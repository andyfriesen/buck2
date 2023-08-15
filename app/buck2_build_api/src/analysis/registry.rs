/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::collections::HashMap;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::sync::Arc;

use allocative::Allocative;
use buck2_artifact::artifact::artifact_type::Artifact;
use buck2_artifact::artifact::artifact_type::DeclaredArtifact;
use buck2_artifact::artifact::artifact_type::OutputArtifact;
use buck2_artifact::deferred::id::DeferredId;
use buck2_core::base_deferred_key::BaseDeferredKey;
use buck2_core::execution_types::execution::ExecutionPlatformResolution;
use buck2_core::fs::buck_out_path::BuckOutPath;
use buck2_core::fs::paths::forward_rel_path::ForwardRelativePath;
use buck2_core::fs::paths::forward_rel_path::ForwardRelativePathBuf;
use buck2_execute::execute::request::OutputType;
use buck2_interpreter::starlark_promise::StarlarkPromise;
use derivative::Derivative;
use dupe::Dupe;
use indexmap::IndexSet;
use starlark::codemap::FileSpan;
use starlark::environment::FrozenModule;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::Heap;
use starlark::values::OwnedFrozenValue;
use starlark::values::Trace;
use starlark::values::Tracer;
use starlark::values::Value;
use starlark::values::ValueTyped;

use crate::actions::registry::ActionsRegistry;
use crate::actions::UnregisteredAction;
use crate::analysis::anon_promises_dyn::AnonPromisesDyn;
use crate::analysis::anon_targets_registry::AnonTargetsRegistryDyn;
use crate::analysis::anon_targets_registry::ANON_TARGET_REGISTRY_NEW;
use crate::analysis::promise_artifacts::PromiseArtifactRegistry;
use crate::artifact_groups::promise::PromiseArtifact;
use crate::artifact_groups::registry::ArtifactGroupRegistry;
use crate::artifact_groups::ArtifactGroup;
use crate::deferred::types::BaseKey;
use crate::deferred::types::DeferredRegistry;
use crate::dynamic::registry::DynamicRegistry;
use crate::interpreter::rule_defs::artifact::associated::AssociatedArtifacts;
use crate::interpreter::rule_defs::artifact::output_artifact_like::OutputArtifactArg;
use crate::interpreter::rule_defs::artifact::StarlarkDeclaredArtifact;

#[derive(Derivative, Trace, Allocative)]
#[derivative(Debug)]
pub struct AnalysisRegistry<'v> {
    #[derivative(Debug = "ignore")]
    deferred: DeferredRegistry,
    #[derivative(Debug = "ignore")]
    actions: ActionsRegistry,
    #[derivative(Debug = "ignore")]
    artifact_groups: ArtifactGroupRegistry,
    #[derivative(Debug = "ignore")]
    dynamic: DynamicRegistry,
    pub anon_targets: Box<dyn AnonTargetsRegistryDyn<'v>>,
    artifact_promises: PromiseArtifactRegistry<'v>,
    analysis_value_storage: AnalysisValueStorage<'v>,
}

#[derive(thiserror::Error, Debug)]
enum DeclaredArtifactError {
    #[error("Can't declare an artifact with an empty filename component")]
    DeclaredEmptyFileName,
}

impl<'v> AnalysisRegistry<'v> {
    pub fn new_from_owner(
        owner: BaseDeferredKey,
        execution_platform: ExecutionPlatformResolution,
    ) -> anyhow::Result<AnalysisRegistry<'v>> {
        Self::new_from_owner_and_deferred(
            owner.dupe(),
            execution_platform,
            DeferredRegistry::new(BaseKey::Base(owner)),
        )
    }

    pub(crate) fn new_from_owner_and_deferred(
        owner: BaseDeferredKey,
        execution_platform: ExecutionPlatformResolution,
        deferred: DeferredRegistry,
    ) -> anyhow::Result<Self> {
        Ok(AnalysisRegistry {
            deferred,
            actions: ActionsRegistry::new(owner.dupe(), execution_platform.dupe()),
            artifact_groups: ArtifactGroupRegistry::new(),
            dynamic: DynamicRegistry::new(owner.dupe()),
            anon_targets: (ANON_TARGET_REGISTRY_NEW.get()?)(PhantomData, execution_platform),
            analysis_value_storage: AnalysisValueStorage::new(),
            artifact_promises: PromiseArtifactRegistry::new(owner),
        })
    }

    pub(crate) fn set_action_key(&mut self, action_key: Arc<str>) {
        self.actions.set_action_key(action_key);
    }

    /// Reserves a path in an output directory. Doesn't declare artifact,
    /// but checks that there is no previously declared artifact with a path
    /// which is in conflict with claimed `path`.
    pub fn claim_output_path(
        &mut self,
        eval: &Evaluator<'_, '_>,
        path: &ForwardRelativePath,
    ) -> anyhow::Result<()> {
        let declaration_location = eval.call_stack_top_location();
        self.actions.claim_output_path(path, declaration_location)
    }

    pub(crate) fn declare_dynamic_output(
        &mut self,
        path: BuckOutPath,
        output_type: OutputType,
    ) -> DeclaredArtifact {
        self.actions.declare_dynamic_output(path, output_type)
    }

    pub fn declare_output(
        &mut self,
        prefix: Option<&str>,
        filename: &str,
        output_type: OutputType,
        declaration_location: Option<FileSpan>,
    ) -> anyhow::Result<DeclaredArtifact> {
        // We want this artifact to be a file/directory inside the current context, which means
        // things like `..` and the empty path `.` can be bad ideas. The `::new` method checks for those
        // things and fails if they are present.

        if filename == "." || filename.is_empty() {
            return Err(DeclaredArtifactError::DeclaredEmptyFileName.into());
        }

        let path = ForwardRelativePath::new(filename)?.to_owned();
        let prefix = match prefix {
            None => None,
            Some(x) => Some(ForwardRelativePath::new(x)?.to_owned()),
        };
        self.actions
            .declare_artifact(prefix, path, output_type, declaration_location)
    }

    /// Takes a string or artifact/output artifact and converts it into an output artifact
    ///
    /// This is handy for functions like `ctx.actions.write` where it's nice to just let
    /// the user give us a string if they want as the output name.
    ///
    /// This function can declare new artifacts depending on the input.
    /// If there is no error, it returns a wrapper around the artifact (ArtifactDeclaration) and the corresponding OutputArtifact
    ///
    /// The valid types for `value` and subsequent actions are as follows:
    ///  - `str`: A new file is declared with this name.
    ///  - `StarlarkOutputArtifact`: The original artifact is returned
    ///  - `StarlarkArtifact`/`StarlarkDeclaredArtifact`: If the artifact is already bound, an error is raised. Otherwise we proceed with the original artifact.
    pub fn get_or_declare_output<'v2>(
        &mut self,
        eval: &Evaluator<'v2, '_>,
        value: OutputArtifactArg<'v2>,
        output_type: OutputType,
    ) -> anyhow::Result<(ArtifactDeclaration<'v2>, OutputArtifact)> {
        let declaration_location = eval.call_stack_top_location();
        let heap = eval.heap();
        let declared_artifact = match value {
            OutputArtifactArg::Str(path) => {
                let artifact =
                    self.declare_output(None, path, output_type, declaration_location.dupe())?;
                heap.alloc_typed(StarlarkDeclaredArtifact::new(
                    declaration_location,
                    artifact,
                    AssociatedArtifacts::new(),
                ))
            }
            OutputArtifactArg::OutputArtifact(output) => output.inner(),
            OutputArtifactArg::DeclaredArtifact(artifact) => artifact,
            OutputArtifactArg::WrongArtifact(artifact) => {
                return Err(artifact.0.as_output_error());
            }
        };

        let output = declared_artifact.output_artifact();
        output.ensure_output_type(output_type)?;
        Ok((
            ArtifactDeclaration {
                artifact: declared_artifact,
                heap,
            },
            output,
        ))
    }

    pub fn register_action<A: UnregisteredAction + 'static>(
        &mut self,
        inputs: IndexSet<ArtifactGroup>,
        outputs: IndexSet<OutputArtifact>,
        action: A,
        associated_value: Option<Value<'v>>,
    ) -> anyhow::Result<()> {
        let id = self
            .actions
            .register(&mut self.deferred, inputs, outputs, action)?;
        if let Some(value) = associated_value {
            self.analysis_value_storage.set_value(id, value);
        }
        Ok(())
    }

    pub fn create_transitive_set(
        &mut self,
        definition: Value<'v>,
        value: Option<Value<'v>>,
        children: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let set = self.artifact_groups.create_transitive_set(
            definition,
            value,
            children,
            &mut self.deferred,
            eval,
        )?;

        let key = set.key().deferred_key().id();
        let set = eval.heap().alloc_complex(set);

        self.analysis_value_storage.set_value(key, set);

        Ok(set)
    }

    pub fn register_dynamic_output(
        &mut self,
        dynamic: IndexSet<Artifact>,
        inputs: IndexSet<Artifact>,
        outputs: IndexSet<OutputArtifact>,
        attributes_plugins_lambda: Value<'v>,
    ) -> anyhow::Result<()> {
        let id = self
            .dynamic
            .register(dynamic, inputs, outputs, &mut self.deferred)?;
        self.analysis_value_storage
            .set_value(id, attributes_plugins_lambda);
        Ok(())
    }

    pub(crate) fn take_promises(&mut self) -> Option<Box<dyn AnonPromisesDyn<'v>>> {
        self.anon_targets.take_promises()
    }

    pub fn assert_no_promises(&self) -> anyhow::Result<()> {
        self.anon_targets.assert_no_promises()
    }

    pub fn register_artifact_promise(
        &mut self,
        promise: ValueTyped<'v, StarlarkPromise<'v>>,
        location: Option<FileSpan>,
        option: Option<ForwardRelativePathBuf>,
    ) -> anyhow::Result<PromiseArtifact> {
        self.artifact_promises.register(promise, location, option)
    }

    /// You MUST pass the same module to both the first function and the second one.
    /// It requires both to get the lifetimes to line up.
    pub fn finalize(
        self,
        env: &'v Module,
    ) -> anyhow::Result<
        impl FnOnce(Module) -> anyhow::Result<(FrozenModule, DeferredRegistry)> + 'static,
    > {
        let AnalysisRegistry {
            mut deferred,
            dynamic,
            actions,
            artifact_groups,
            anon_targets: _,
            analysis_value_storage,
            artifact_promises,
        } = self;
        artifact_promises.resolve_all()?;

        analysis_value_storage.write_to_module(env);
        Ok(move |env: Module| {
            let frozen_env = env.freeze()?;
            let analysis_value_fetcher = AnalysisValueFetcher {
                frozen_module: Some(frozen_env.dupe()),
            };
            actions.ensure_bound(&mut deferred, &analysis_value_fetcher)?;
            artifact_groups.ensure_bound(&mut deferred, &analysis_value_fetcher)?;
            dynamic.ensure_bound(&mut deferred, &analysis_value_fetcher)?;
            Ok((frozen_env, deferred))
        })
    }

    pub fn execution_platform(&self) -> &ExecutionPlatformResolution {
        self.actions.execution_platform()
    }
}

pub struct ArtifactDeclaration<'v> {
    artifact: ValueTyped<'v, StarlarkDeclaredArtifact>,
    heap: &'v Heap,
}

impl<'v> ArtifactDeclaration<'v> {
    pub fn into_declared_artifact(
        self,
        extra_associated_artifacts: AssociatedArtifacts,
    ) -> ValueTyped<'v, StarlarkDeclaredArtifact> {
        self.heap.alloc_typed(
            self.artifact
                .with_extended_associated_artifacts(extra_associated_artifacts),
        )
    }
}

/// Store `Value<'v>` values for actions registered in an implementation function
///
/// Threading lifetimes through the various action registries is kind of a pain. So instead,
/// store the starlark values in this struct, using the `DeferredId` as the key.
///
/// These values eventually are written into the mutable `Module`, and a wrapper is
/// made available to get the `OwnedFrozenValue` back out after that `Module` is frozen.
///
/// Note that this object has internal mutation and is only expected to live for the duration
/// of impl function execution.
///
/// At the end of impl function execution, `write_to_module` should be called to ensure
/// that the values are written the top level of the `Module`.
#[derive(Debug, Allocative)]
struct AnalysisValueStorage<'v> {
    values: HashMap<DeferredId, Value<'v>>,
}

unsafe impl<'v> Trace<'v> for AnalysisValueStorage<'v> {
    fn trace(&mut self, tracer: &Tracer<'v>) {
        for v in self.values.values_mut() {
            tracer.trace(v)
        }
    }
}

/// Simple fetcher that fetches the values written in `AnalysisValueStorage::write_to_module`
///
/// These values are pulled from the `FrozenModule` that results from `env.freeze()`.
/// This is used by the action registry to make an `OwnedFrozenValue` available to
/// Actions' register function.
#[derive(Default)]
pub struct AnalysisValueFetcher {
    frozen_module: Option<FrozenModule>,
}

impl<'v> AnalysisValueStorage<'v> {
    fn new() -> Self {
        Self {
            values: HashMap::new(),
        }
    }

    /// Write all of the values to `module` using an internal name
    fn write_to_module(&self, module: &'v Module) {
        for (id, v) in self.values.iter() {
            let starlark_key = format!("$action_key_{}", id);
            module.set(&starlark_key, *v);
        }
    }

    /// Add a value to the internal hash map that maps ids -> values
    fn set_value(&mut self, id: DeferredId, value: Value<'v>) {
        self.values.insert(id, value);
    }
}

impl AnalysisValueFetcher {
    /// Get the `OwnedFrozenValue` that corresponds to a `DeferredId`, if present
    pub(crate) fn get(&self, id: DeferredId) -> anyhow::Result<Option<OwnedFrozenValue>> {
        match &self.frozen_module {
            None => Ok(None),
            Some(module) => {
                let starlark_key = format!("$action_key_{}", id);
                // This return `Err` is the symbol is private.
                // It is never private, but error is better than panic.
                module.get_option(&starlark_key)
            }
        }
    }
}
