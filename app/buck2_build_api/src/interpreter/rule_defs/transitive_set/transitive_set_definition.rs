/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::cell::RefCell;
use std::fmt;
use std::fmt::Display;
use std::hash::Hash;
use std::sync::Arc;

use allocative::Allocative;
use anyhow::Context;
use buck2_core::bzl::ImportPath;
use buck2_interpreter::build_context::STARLARK_PATH_FROM_BUILD_CONTEXT;
use buck2_interpreter::path::StarlarkPath;
use derive_more::Display;
use dupe::Dupe;
use serde::Serialize;
use serde::Serializer;
use starlark::any::ProvidesStaticType;
use starlark::coerce::coerce;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::collections::StarlarkHasher;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::values::starlark_value;
use starlark::values::AllocValue;
use starlark::values::Freeze;
use starlark::values::Freezer;
use starlark::values::FrozenValue;
use starlark::values::Heap;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::Tracer;
use starlark::values::Value;
use starlark::values::ValueLike;

use crate::interpreter::rule_defs::transitive_set::TransitiveSetError;

#[derive(Debug, thiserror::Error)]
enum TransitiveSetDefinitionError {
    #[error("`transitive_set()` can only be used in `bzl` files")]
    TransitiveSetOnlyInBzl,
}

#[derive(Debug, Clone, Dupe, Copy, Trace, Freeze, PartialEq, Allocative)]
pub enum TransitiveSetProjectionKind {
    Args,
    Json,
}

impl TransitiveSetProjectionKind {
    pub fn short_name(&self) -> &'static str {
        match self {
            TransitiveSetProjectionKind::Args => "args",
            TransitiveSetProjectionKind::Json => "json",
        }
    }

    pub fn function_name(&self) -> &'static str {
        match self {
            TransitiveSetProjectionKind::Args => "project_as_args",
            TransitiveSetProjectionKind::Json => "project_as_json",
        }
    }
}

// The Coerce derivation doesn't work if this is just a tuple in the SmallMap value.
#[derive(Debug, Clone, Trace, Coerce, Freeze, Allocative)]
#[repr(C)]
pub struct TransitiveSetProjectionSpec<V> {
    pub kind: TransitiveSetProjectionKind,
    pub projection: V,
}

/// A unique identity for a given [`TransitiveSetDefinition`].
#[derive(Debug, Clone, Display, Allocative, Hash)]
#[display(fmt = "{}", "name")]
struct TransitiveSetId {
    module_id: ImportPath,
    name: String,
}

#[derive(Debug, ProvidesStaticType, Allocative)]
pub struct TransitiveSetDefinition<'v> {
    /// The name of this transitive set. This is filed in by `export_as` when it's assigned to a
    /// top-level variable. This must be set before this is used.
    id: RefCell<Option<Arc<TransitiveSetId>>>,

    /// The module id where this `TransitiveSetDefinition` is created and assigned
    module_id: ImportPath,

    operations: TransitiveSetOperationsGen<Value<'v>>,
}

#[derive(Debug, Clone, Trace, Coerce, Freeze, Allocative)]
#[repr(C)]
pub struct TransitiveSetOperationsGen<V> {
    /// Callables that will project the values contained in transitive sets of this type to
    /// cmd_args or json. This can be used to include a transitive set into a command or json file.
    pub(crate) projections: SmallMap<String, TransitiveSetProjectionSpec<V>>,

    /// Callables that will reduce the values contained in transitive sets to a single value per
    /// node. This can be used to e.g. aggregate flags throughout a transitive set;
    pub(crate) reductions: SmallMap<String, V>,
}

pub type TransitiveSetOperations<'v> = TransitiveSetOperationsGen<Value<'v>>;

impl<V> TransitiveSetOperationsGen<V> {
    pub fn valid_projections(&self, kind: TransitiveSetProjectionKind) -> Vec<String> {
        self.projections
            .iter()
            .filter_map(|(k, spec)| {
                if kind == spec.kind {
                    Some(k.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    }

    pub fn get_index_of_projection(
        &self,
        kind: TransitiveSetProjectionKind,
        proj: &str,
    ) -> anyhow::Result<usize> {
        let index = match self.projections.get_index_of(proj) {
            Some(index) => index,
            None => {
                return Err(TransitiveSetError::ProjectionDoesNotExist {
                    projection: proj.to_owned(),
                    valid_projections: self.valid_projections(TransitiveSetProjectionKind::Args),
                }
                .into());
            }
        };

        let (_, spec) = self.projections.get_index(index).unwrap();
        if spec.kind != kind {
            return Err(TransitiveSetError::ProjectionKindMismatch {
                projection: proj.to_owned(),
                expected_kind: kind,
                actual_kind: spec.kind,
            }
            .into());
        }

        Ok(index)
    }
}

unsafe impl<'v> Trace<'v> for TransitiveSetDefinition<'v> {
    fn trace(&mut self, tracer: &Tracer<'v>) {
        self.operations.trace(tracer)
    }
}

impl<'v> Display for TransitiveSetDefinition<'v> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.id.try_borrow() {
            Ok(val) => match val.as_deref() {
                Some(id) => write!(f, "{}", id),
                None => write!(f, "unnamed transitive set"),
            },
            Err(..) => write!(f, "borrowed transitive set"),
        }
    }
}

impl<'v> Serialize for TransitiveSetDefinition<'v> {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&format!("{}", self))
    }
}

impl<'v> TransitiveSetDefinition<'v> {
    fn new(module_id: ImportPath, operations: TransitiveSetOperations<'v>) -> Self {
        Self {
            id: RefCell::new(None),
            module_id,
            operations,
        }
    }

    pub fn has_id(&self) -> bool {
        self.id.borrow().is_some()
    }
}

impl<'v> AllocValue<'v> for TransitiveSetDefinition<'v> {
    fn alloc_value(self, heap: &'v Heap) -> Value<'v> {
        heap.alloc_complex(self)
    }
}

#[starlark_value(type = "transitive_set_definition")]
impl<'v> StarlarkValue<'v> for TransitiveSetDefinition<'v> {
    fn export_as(&self, variable_name: &str, _: &mut Evaluator<'v, '_>) {
        // First export wins
        let mut id = self.id.borrow_mut();
        if id.is_none() {
            let new_id = Arc::new(TransitiveSetId {
                module_id: self.module_id.clone(),
                name: variable_name.to_owned(),
            });
            *id = Some(new_id.dupe());
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["type".to_owned()]
    }

    fn has_attr(&self, attribute: &str, _heap: &'v Heap) -> bool {
        attribute == "type"
    }

    fn get_attr(&self, attribute: &str, heap: &'v Heap) -> Option<Value<'v>> {
        if attribute == "type" {
            let id = self.id.borrow();
            let typ = id
                .as_ref()
                .map_or("transitive_set_definition", |id| id.name.as_str());
            Some(heap.alloc(typ))
        } else {
            None
        }
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> anyhow::Result<()> {
        let id = self.id.borrow();
        let id = id
            .as_deref()
            .context("cannot hash a transitive_set_definition without id")?;
        id.hash(hasher);
        Ok(())
    }

    // TODO (torozco): extra_memory()?
}

#[derive(Display, ProvidesStaticType, Allocative)]
#[display(fmt = "{}", id)]
pub struct FrozenTransitiveSetDefinition {
    /// The name of this transitive set. This is filed in by `export_as` when it's assigned to a
    /// top-level variable. This must be set before this is used.
    id: Arc<TransitiveSetId>,

    operations: TransitiveSetOperationsGen<FrozenValue>,
}

impl fmt::Debug for FrozenTransitiveSetDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TransitiveSetDefinition({} declared in {})",
            self.id.name, self.id.module_id
        )
    }
}

impl Serialize for FrozenTransitiveSetDefinition {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&format!("{}", self))
    }
}

#[starlark_value(type = "transitive_set_definition")]
impl<'v> StarlarkValue<'v> for FrozenTransitiveSetDefinition {
    fn dir_attr(&self) -> Vec<String> {
        vec!["type".to_owned()]
    }

    fn has_attr(&self, attribute: &str, _heap: &'v Heap) -> bool {
        attribute == "type"
    }

    fn get_attr(&self, attribute: &str, heap: &'v Heap) -> Option<Value<'v>> {
        if attribute == "type" {
            let typ = self.id.name.as_str();
            Some(heap.alloc(typ))
        } else {
            None
        }
    }

    fn write_hash(&self, hasher: &mut StarlarkHasher) -> anyhow::Result<()> {
        self.id.hash(hasher);
        Ok(())
    }
}

starlark_simple_value!(FrozenTransitiveSetDefinition);

impl<'v> Freeze for TransitiveSetDefinition<'v> {
    type Frozen = FrozenTransitiveSetDefinition;

    fn freeze(self, freezer: &Freezer) -> anyhow::Result<Self::Frozen> {
        let Self {
            id,
            module_id: _,
            operations,
        } = self;

        let id = match id.into_inner() {
            Some(x) => x,
            None => {
                // Unfortunately we have no name or location for the definition at this point.
                return Err(TransitiveSetError::TransitiveSetNotAssigned.into());
            }
        };

        let operations = operations.freeze(freezer)?;

        Ok(FrozenTransitiveSetDefinition { id, operations })
    }
}

pub fn transitive_set_definition_from_value<'v>(
    x: Value<'v>,
) -> Option<&dyn TransitiveSetDefinitionLike<'v>> {
    if let Some(x) = x.downcast_ref::<TransitiveSetDefinition>() {
        Some(x as &dyn TransitiveSetDefinitionLike<'v>)
    } else if let Some(x) = x.downcast_ref::<FrozenTransitiveSetDefinition>() {
        Some(x as &dyn TransitiveSetDefinitionLike<'v>)
    } else {
        None
    }
}

pub trait TransitiveSetDefinitionLike<'v> {
    fn has_id(&self) -> bool;

    fn as_debug(&self) -> &dyn fmt::Debug;

    fn matches_type(&self, ty: &str) -> bool;

    fn operations(&self) -> &TransitiveSetOperations<'v>;
}

impl<'v> TransitiveSetDefinitionLike<'v> for TransitiveSetDefinition<'v> {
    fn has_id(&self) -> bool {
        Self::has_id(self)
    }

    fn as_debug(&self) -> &dyn fmt::Debug {
        self
    }

    fn matches_type(&self, ty: &str) -> bool {
        self.id.borrow().as_ref().map_or(false, |id| id.name == ty)
    }

    fn operations(&self) -> &TransitiveSetOperations<'v> {
        &self.operations
    }
}

impl<'v> TransitiveSetDefinitionLike<'v> for FrozenTransitiveSetDefinition {
    fn has_id(&self) -> bool {
        true
    }

    fn as_debug(&self) -> &dyn fmt::Debug {
        self
    }

    fn matches_type(&self, ty: &str) -> bool {
        self.id.name == ty
    }

    fn operations(&self) -> &TransitiveSetOperations<'v> {
        coerce(&self.operations)
    }
}

#[starlark_module]
pub fn register_transitive_set(builder: &mut GlobalsBuilder) {
    fn transitive_set<'v>(
        args_projections: Option<SmallMap<String, Value<'v>>>,
        json_projections: Option<SmallMap<String, Value<'v>>>,
        reductions: Option<SmallMap<String, Value<'v>>>,
        eval: &mut Evaluator,
    ) -> anyhow::Result<TransitiveSetDefinition<'v>> {
        // TODO(cjhopman): Reductions could do similar signature checking.
        let projections: SmallMap<_, _> = args_projections
            .into_iter()
            .flat_map(|v| v.into_iter())
            .map(|(k, v)| {
                (
                    k,
                    TransitiveSetProjectionSpec {
                        kind: TransitiveSetProjectionKind::Args,
                        projection: v,
                    },
                )
            })
            .chain(
                json_projections
                    .into_iter()
                    .flat_map(|v| v.into_iter())
                    .map(|(k, v)| {
                        (
                            k,
                            TransitiveSetProjectionSpec {
                                kind: TransitiveSetProjectionKind::Json,
                                projection: v,
                            },
                        )
                    }),
            )
            .collect();

        // Both kinds of projections take functions with the same signature.
        for (name, spec) in projections.iter() {
            // We should probably be able to require that the projection returns a parameters_spec, but
            // we don't depend on this type-checking and we'd just error out later when calling it if it
            // were wrong.
            if let Some(v) = spec.projection.parameters_spec() {
                if v.len() != 1 {
                    return Err(TransitiveSetError::ProjectionSignatureError {
                        name: name.clone(),
                    }
                    .into());
                }
            };
        }

        let starlark_path: StarlarkPath = (STARLARK_PATH_FROM_BUILD_CONTEXT.get()?)(eval)?;
        Ok(TransitiveSetDefinition::new(
            match starlark_path {
                StarlarkPath::LoadFile(import_path) => import_path.clone(),
                _ => return Err(TransitiveSetDefinitionError::TransitiveSetOnlyInBzl.into()),
            },
            TransitiveSetOperations {
                projections,
                reductions: reductions.unwrap_or_default(),
            },
        ))
    }
}
