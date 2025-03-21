/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fmt::Debug;
use std::ptr;

use allocative::Allocative;
use anyhow::Context;
use buck2_artifact::artifact::artifact_type::Artifact;
use buck2_build_api_derive::internal_provider;
use dupe::Dupe;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::values::dict::Dict;
use starlark::values::dict::FrozenDictRef;
use starlark::values::list::AllocList;
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::type_repr::DictType;
use starlark::values::Freeze;
use starlark::values::FrozenRef;
use starlark::values::FrozenValue;
use starlark::values::FrozenValueTyped;
use starlark::values::Trace;
use starlark::values::UnpackValue;
use starlark::values::Value;
use starlark::values::ValueError;
use starlark::values::ValueLike;
use thiserror::Error;

use crate::artifact_groups::ArtifactGroup;
use crate::interpreter::rule_defs::artifact::StarlarkArtifact;
use crate::interpreter::rule_defs::artifact::StarlarkArtifactLike;
use crate::interpreter::rule_defs::artifact::ValueAsArtifactLike;
use crate::interpreter::rule_defs::cmd_args::value_as::ValueAsCommandLineLike;
use crate::interpreter::rule_defs::cmd_args::CommandLineArgLike;
use crate::interpreter::rule_defs::cmd_args::SimpleCommandLineArtifactVisitor;
use crate::interpreter::rule_defs::provider::collection::FrozenProviderCollection;
use crate::interpreter::rule_defs::provider::ProviderCollection;

/// A provider that all rules' implementations must return
///
/// In many simple cases, this can be inferred for the user.
///
/// Example of a rule's implementation function and how these fields are used by the framework:
///
/// ```starlark
/// # //foo_binary.bzl
/// def impl(ctx):
///     ctx.action.run([ctx.attrs._cc[RunInfo], "-o", ctx.attrs.out.as_output()] + ctx.attrs.srcs)
///     ctx.action.run([
///         ctx.attrs._strip[RunInfo],
///         "--binary",
///         ctx.attrs.out,
///         "--stripped-out",
///         ctx.attrs.stripped.as_output(),
///         "--debug-symbols-out",
///         ctx.attrs.debug_info.as_output(),
///     ])
///     return [
///         DefaultInfo(
///             sub_targets = {
///                 "stripped": [
///                     DefaultInfo(default_outputs = [ctx.attrs.stripped, ctx.attrs.debug_info]),
///                 ],
///             },
///             default_output = ctx.attrs.out,
///     ]
///
/// foo_binary = rule(
///     impl=impl,
///     attrs={
///         "srcs": attrs.list(attrs.source()),
///         "out": attrs.output(),
///         "stripped": attrs.output(),
///         "debug_info": attrs.output(),
///         "_cc": attrs.dep(default="//tools:cc", providers=[RunInfo]),
///         "_strip_script": attrs.dep(default="//tools:strip", providers=[RunInfo])
/// )
///
/// def foo_binary_wrapper(name, srcs):
///     foo_binary(
///         name = name,
///         srcs = src,
///         out = name,
///         stripped = name + ".stripped",
///         debug_info = name + ".debug_info",
///     )
///
/// # //subdir/BUCK
/// load("//:foo_binary.bzl", "foo_binary_wrapper")
///
/// genrule(name = "gen_stuff", ...., default_outs = ["foo.cpp"])
///
/// # ":gen_stuff" pulls the default_outputs for //subdir:gen_stuff
/// foo_binary_wrapper(name = "foo", srcs = glob(["*.cpp"]) + [":gen_stuff"])
///
/// # Builds just 'foo' binary. The strip command is never invoked.
/// $ buck build //subdir:foo
///
/// # builds the 'foo' binary, because it is needed by the 'strip' command. Ensures that
/// # both the stripped binary and the debug symbols are built.
/// $ buck build //subdir:foo[stripped]
/// ```
#[internal_provider(default_info_creator)]
#[derive(Clone, Debug, Freeze, Trace, Coerce, ProvidesStaticType, Allocative)]
#[freeze(validator = validate_default_info, bounds = "V: ValueLike<'freeze>")]
#[repr(C)]
pub struct DefaultInfoGen<V> {
    /// A mapping of names to `ProviderCollection`s. The keys are used when resolving the
    /// `ProviderName` portion of a `ProvidersLabel` in order to access the providers for a
    /// subtarget, such as when doing `buck2 build cell//foo:bar[baz]`. Just like any
    /// `ProviderCollection`, this collection must include at least a `DefaultInfo` provider. The
    /// subtargets can have their own subtargets as well, which can be accessed by chaining them,
    /// e.g.: `buck2 build cell//foo:bar[baz][qux]`.
    #[provider(field_type = DictType<String, ProviderCollection>)]
    sub_targets: V,
    /// A list of `Artifact`s that are built by default if this rule is requested
    /// explicitly, or depended on as as a "source".
    #[provider(field_type = Vec<StarlarkArtifact>)]
    default_outputs: V,
    /// A list of `ArtifactTraversable`. The underlying `Artifact`s they define will
    /// be built by default if this rule is requested, but _not_ when it's depended
    /// on as as a "source". `ArtifactTraversable` can be an `Artifact` (which yields
    /// itself), or `cmd_args`, which expand to all their inputs.
    #[provider(field_type = Vec<StarlarkArtifact>)]
    other_outputs: V,
}

fn validate_default_info(info: &FrozenDefaultInfo) -> anyhow::Result<()> {
    // Check length of default outputs
    let default_output_list = ListRef::from_value(info.default_outputs.to_value())
        .expect("should be a list from constructor");
    if default_output_list.len() > 1 {
        tracing::info!("DefaultInfo.default_output should only have a maximum of 1 item.");
        // TODO use soft_error when landed
        // TODO error rather than soft warning
        // return Err(anyhow::anyhow!(
        //     "DefaultInfo.default_output can only have a maximum of 1 item."
        // ));
    }

    // Check mutable data hasn't been modified.
    for output in info.default_outputs_impl()? {
        output?;
    }
    for sub_target in info.sub_targets_impl()? {
        sub_target?;
    }

    Ok(())
}

impl FrozenDefaultInfo {
    fn get_sub_target_providers_impl(
        &self,
        name: &str,
    ) -> anyhow::Result<Option<FrozenValueTyped<'static, FrozenProviderCollection>>> {
        FrozenDictRef::from_frozen_value(self.sub_targets)
            .context("sub_targets should be a dict-like object")?
            .get_str(name)
            .map(|v| {
                FrozenValueTyped::new(v).context(
                    "Values inside of a frozen provider should be frozen provider collection",
                )
            })
            .transpose()
    }

    pub fn get_sub_target_providers(
        &self,
        name: &str,
    ) -> Option<FrozenValueTyped<'static, FrozenProviderCollection>> {
        self.get_sub_target_providers_impl(name).unwrap()
    }

    fn default_outputs_impl(
        &self,
    ) -> anyhow::Result<impl Iterator<Item = anyhow::Result<StarlarkArtifact>> + '_> {
        let list = ListRef::from_frozen_value(self.default_outputs)
            .context("Should be list of artifacts")?;

        Ok(list.iter().map(|v| {
            let frozen_value = v.unpack_frozen().context("should be frozen")?;

            Ok(
                if let Some(starlark_artifact) = frozen_value.downcast_ref::<StarlarkArtifact>() {
                    starlark_artifact.dupe()
                } else {
                    // This code path is for StarlarkPromiseArtifact. We have to create a `StarlarkArtifact` object here.
                    let artifact_like = ValueAsArtifactLike::unpack_value(frozen_value.to_value())
                        .context("Should be list of artifacts")?;
                    artifact_like.0.get_bound_starlark_artifact()?
                },
            )
        }))
    }

    pub fn default_outputs<'a>(&'a self) -> Vec<StarlarkArtifact> {
        self.default_outputs_impl()
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    pub fn default_outputs_raw(&self) -> FrozenValue {
        self.default_outputs
    }

    fn sub_targets_impl(
        &self,
    ) -> anyhow::Result<
        impl Iterator<Item = anyhow::Result<(&str, FrozenRef<'static, FrozenProviderCollection>)>> + '_,
    > {
        let sub_targets = FrozenDictRef::from_frozen_value(self.sub_targets)
            .context("sub_targets should be a dict-like object")?;

        Ok(sub_targets.iter().map(|(k, v)| {
            anyhow::Ok((
                k.to_value()
                    .unpack_str()
                    .context("sub_targets should have string keys")?,
                v.downcast_frozen_ref::<FrozenProviderCollection>()
                    .context(
                        "Values inside of a frozen provider should be frozen provider collection",
                    )?,
            ))
        }))
    }

    pub fn sub_targets(&self) -> SmallMap<&str, FrozenRef<'static, FrozenProviderCollection>> {
        self.sub_targets_impl()
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    pub fn sub_targets_raw(&self) -> FrozenValue {
        self.sub_targets
    }

    pub fn for_each_default_output_artifact_only(
        &self,
        processor: &mut dyn FnMut(Artifact) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.for_each_in_list(self.default_outputs, |value| {
            processor(
                ValueAsArtifactLike::unpack_value(value)
                    .ok_or_else(|| anyhow::anyhow!("not an artifact"))?
                    .0
                    .get_bound_artifact()?,
            )
        })
    }

    pub fn for_each_default_output_other_artifacts_only(
        &self,
        processor: &mut dyn FnMut(ArtifactGroup) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.for_each_in_list(self.default_outputs, |value| {
            let others = ValueAsArtifactLike::unpack_value(value)
                .ok_or_else(|| anyhow::anyhow!("not an artifact"))?
                .0
                .get_associated_artifacts();
            others
                .iter()
                .flat_map(|v| v.iter())
                .for_each(|other| processor(other.dupe()).unwrap());
            Ok(())
        })
    }

    // TODO(marwhal): We can remove this once we migrate all other outputs to be handled with Artifacts directly
    pub fn for_each_other_output(
        &self,
        processor: &mut dyn FnMut(ArtifactGroup) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.for_each_in_list(self.other_outputs, |value| {
            value
                .as_artifact_traversable()
                .with_context(|| format!("Expected artifact traversable, got: {:?}", value))?
                .traverse(processor)
        })
    }

    pub fn for_each_output(
        &self,
        processor: &mut dyn FnMut(ArtifactGroup) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.for_each_default_output_artifact_only(&mut |a| processor(ArtifactGroup::Artifact(a)))?;
        self.for_each_default_output_other_artifacts_only(processor)?;
        // TODO(marwhal): We can remove this once we migrate all other outputs to be handled with Artifacts directly
        self.for_each_other_output(processor)
    }

    fn for_each_in_list(
        &self,
        value: FrozenValue,
        mut processor: impl FnMut(Value) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let outputs_list = ListRef::from_frozen_value(value)
            .unwrap_or_else(|| panic!("expected list, got `{:?}` from info `{:?}`", value, self));

        for value in outputs_list.iter() {
            processor(value)?;
        }

        Ok(())
    }
}

impl PartialEq for FrozenDefaultInfo {
    // frozen default infos can be compared by ptr for a simple equality
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self, other)
    }
}

trait ArtifactTraversable {
    fn traverse(
        &self,
        processor: &mut dyn FnMut(ArtifactGroup) -> anyhow::Result<()>,
    ) -> anyhow::Result<()>;
}

// TODO: This is a hack. We need a way to express "the inputs of that other thing", but at the
// moment we don't have one, so we allow adding a command line (which is often the input container
// we care about) as an "other" output on DefaultInfo. We could use a better abstraction for this.
impl ArtifactTraversable for &dyn CommandLineArgLike {
    fn traverse(
        &self,
        processor: &mut dyn FnMut(ArtifactGroup) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut acc = SimpleCommandLineArtifactVisitor::new();
        CommandLineArgLike::visit_artifacts(*self, &mut acc)?;
        for input in acc.inputs {
            processor(input)?;
        }
        Ok(())
    }
}

impl ArtifactTraversable for &dyn StarlarkArtifactLike {
    fn traverse(
        &self,
        processor: &mut dyn FnMut(ArtifactGroup) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        processor(ArtifactGroup::Artifact(self.get_bound_artifact()?))?;
        Ok(())
    }
}

trait ValueAsArtifactTraversable<'v> {
    fn as_artifact_traversable(&self) -> Option<Box<dyn ArtifactTraversable + 'v>>;
}

impl<'v, V: ValueLike<'v>> ValueAsArtifactTraversable<'v> for V {
    fn as_artifact_traversable(&self) -> Option<Box<dyn ArtifactTraversable + 'v>> {
        if let Some(artifact) = ValueAsArtifactLike::unpack_value(self.to_value()) {
            return Some(Box::new(artifact.0));
        }

        if let Some(cli) = self.to_value().as_command_line() {
            return Some(Box::new(cli));
        }

        None
    }
}

#[derive(Debug, Error)]
enum DefaultOutputError {
    #[error("Cannot specify both `default_output` and `default_outputs`.")]
    ConflictingArguments,
}

#[starlark_module]
fn default_info_creator(builder: &mut GlobalsBuilder) {
    #[starlark(as_type = FrozenDefaultInfo)]
    fn DefaultInfo<'v>(
        #[starlark(default = NoneType)] default_output: Value<'v>,
        #[starlark(default = NoneType)] default_outputs: Value<'v>,
        #[starlark(default = AllocList::EMPTY)] other_outputs: Value<'v>,
        #[starlark(default = SmallMap::new())] sub_targets: SmallMap<String, Value<'v>>,
        eval: &mut Evaluator<'v, '_>,
    ) -> anyhow::Result<DefaultInfo<'v>> {
        let heap = eval.heap();
        let default_info_creator = || {
            let default_outputs = heap.alloc(AllocList::EMPTY);
            let other_outputs = heap.alloc(AllocList::EMPTY);
            let sub_targets = heap.alloc(Dict::default());
            heap.alloc(DefaultInfo {
                sub_targets,
                default_outputs,
                other_outputs,
            })
        };

        // support both list and singular options for now until we migrate all the rules.
        let valid_default_outputs = if !default_outputs.is_none() {
            match ListRef::from_value(default_outputs) {
                Some(list) => {
                    if !default_output.is_none() {
                        return Err(anyhow::anyhow!(DefaultOutputError::ConflictingArguments));
                    }

                    if list
                        .iter()
                        .all(|v| ValueAsArtifactLike::unpack_value(v).is_some())
                    {
                        default_outputs
                    } else {
                        return Err(anyhow::anyhow!(ValueError::IncorrectParameterTypeNamed(
                            "default_outputs".to_owned()
                        )));
                    }
                }
                None => {
                    return Err(anyhow::anyhow!(ValueError::IncorrectParameterTypeNamed(
                        "default_outputs".to_owned()
                    )));
                }
            }
        } else {
            // handle where we didn't specify `default_outputs`, which means we should use the new
            // `default_output`.
            if default_output.is_none() {
                eval.heap().alloc(AllocList::EMPTY)
            } else if ValueAsArtifactLike::unpack_value(default_output).is_some() {
                eval.heap().alloc(AllocList([default_output]))
            } else {
                return Err(anyhow::anyhow!(ValueError::IncorrectParameterTypeNamed(
                    "default_output".to_owned()
                )));
            }
        };

        let valid_other_outputs = match ListRef::from_value(other_outputs) {
            Some(list) => {
                if list.iter().all(|v| v.as_artifact_traversable().is_some()) {
                    Ok(other_outputs)
                } else {
                    Err(())
                }
            }
            None => Err(()),
        }
        .map_err(|_| ValueError::IncorrectParameterTypeNamed("other_outputs".to_owned()))?;

        let valid_sub_targets = sub_targets
            .into_iter()
            .map(|(k, v)| {
                let as_provider_collection =
                    ProviderCollection::try_from_value_with_default_info(v, default_info_creator)?;
                Ok((
                    heap.alloc_str(&k).get_hashed_value(),
                    heap.alloc(as_provider_collection),
                ))
            })
            .collect::<anyhow::Result<SmallMap<Value<'v>, Value<'v>>>>()?;

        Ok(DefaultInfo {
            default_outputs: valid_default_outputs,
            other_outputs: valid_other_outputs,
            sub_targets: heap.alloc(Dict::new(valid_sub_targets)),
        })
    }
}
