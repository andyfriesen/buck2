/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::time::Instant;

use buck2_build_api::interpreter::rule_defs::artifact::StarlarkArtifact;
use buck2_build_api::interpreter::rule_defs::cmd_args::value_as::ValueAsCommandLineLike;
use buck2_node::nodes::configured::ConfiguredTargetNode;
use buck2_node::nodes::unconfigured::TargetNode;
use buck2_query::query::syntax::simple::eval::set::TargetSet;
use dupe::Dupe;
use futures::FutureExt;
use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::values::none::NoneType;
use starlark::values::Heap;
use starlark::values::StringValue;
use starlark::values::Value;
use starlark::values::ValueLike;
use thiserror::Error;

use super::artifacts::visit_artifact_path_without_associated_deduped;
use super::context::output::get_artifact_path_display;
use super::context::output::get_cmd_line_inputs;
use crate::bxl::starlark_defs::context::BxlContext;
use crate::bxl::starlark_defs::targetset::StarlarkTargetSet;
use crate::bxl::starlark_defs::time::StarlarkInstant;

/// Global methods on the target set.
#[starlark_module]
pub(crate) fn register_target_function(builder: &mut GlobalsBuilder) {
    /// Creates an empty target set for configured nodes.
    ///
    /// Sample usage:
    /// ```text
    /// def _impl_ctarget_set(ctx):
    ///     targets = ctarget_set()
    ///     ctx.output.print(type(targets))
    ///     ctx.output.print(len(targets))
    /// ```
    fn ctarget_set() -> anyhow::Result<StarlarkTargetSet<ConfiguredTargetNode>> {
        Ok(StarlarkTargetSet::from(TargetSet::new()))
    }

    /// Creates an empty target set for unconfigured nodes.
    ///
    /// Sample usage:
    /// ```text
    /// def _impl_utarget_set(ctx):
    ///     targets = utarget_set()
    ///     ctx.output.print(type(targets))
    ///     ctx.output.print(len(targets))
    /// ```
    fn utarget_set() -> anyhow::Result<StarlarkTargetSet<TargetNode>> {
        Ok(StarlarkTargetSet::from(TargetSet::new()))
    }
}

/// Global methods on artifacts.
#[starlark_module]
pub(crate) fn register_artifact_function(builder: &mut GlobalsBuilder) {
    /// The output path of a source or build artifact. Takes an optional boolean to print the absolute or relative path.
    /// Note that this method returns an artifact path without asking for the artifact to be materialized,
    /// (i.e. it may not actually exist on the disk yet).
    ///
    /// This is a risky function to call because you may accidentally pass this path to further BXL actions
    /// that expect the artifact to be materialized. If this happens, the BXL script will error out.
    /// If you want the path without materialization for other uses that don’t involve passing them into
    /// further actions, then it’s safe.
    ///
    /// Sample usage:
    /// ```text
    /// def _impl_get_path_without_materialization(ctx):
    ///     owner = ctx.cquery().owner("cell//path/to/file")[0]
    ///     artifact = owner.get_source("cell//path/to/file", ctx)
    ///     source_artifact_project_rel_path = get_path_without_materialization(artifact, ctx)
    ///     ctx.output.print(source_artifact_project_rel_path) # Note this artifact is NOT ensured or materialized
    /// ```
    fn get_path_without_materialization<'v>(
        this: &'v StarlarkArtifact,
        ctx: &'v BxlContext<'v>,
        #[starlark(require = named, default = false)] abs: bool,
        heap: &'v Heap,
    ) -> anyhow::Result<StringValue<'v>> {
        let path = get_artifact_path_display(
            this.artifact().get_path(),
            abs,
            &ctx.data.project_fs,
            &ctx.data.artifact_fs,
        )?;

        Ok(heap.alloc_str(&path))
    }

    /// The output paths of a `cmd_args()` inputs. The output paths will be returned as a list.
    /// Takes an optional boolean to print the absolute or relative path.
    /// Note that this method returns an artifact path without asking for the artifact to be materialized,
    /// (i.e. it may not actually exist on the disk yet).
    ///
    /// This is a risky function to call because you may accidentally pass this path to further BXL actions
    /// that expect the artifact to be materialized. If this happens, the BXL script will error out.
    /// If you want the path without materialization for other uses that don’t involve passing them into
    /// further actions, then it’s safe.
    ///
    /// Sample usage:
    /// ```text
    /// def _impl_get_paths_without_materialization(ctx):
    ///     node = ctx.configured_targets("root//bin:the_binary")
    ///     providers = ctx.analysis(node).providers()
    ///     path = get_paths_without_materialization(providers[RunInfo], abs=True) # Note this artifact is NOT ensured or materialized
    ///     ctx.output.print(path)
    /// ```
    fn get_paths_without_materialization<'v>(
        this: Value<'v>,
        ctx: &'v BxlContext<'v>,
        #[starlark(require = named, default = false)] abs: bool,
        heap: &'v Heap,
    ) -> anyhow::Result<Value<'v>> {
        match this.as_command_line() {
            Some(cmd_line) => {
                let inputs = get_cmd_line_inputs(cmd_line)?;
                let mut result = Vec::new();

                for artifact_group in &inputs.inputs {
                    result.push(artifact_group.dupe());
                }

                let mut paths = Vec::new();

                let _result = ctx.via_dice(|mut dice_ctx, bxl_ctx| {
                    dice_ctx.via(|dice_ctx| {
                        visit_artifact_path_without_associated_deduped(
                            &result,
                            abs,
                            |artifact_path, abs| {
                                let path = get_artifact_path_display(
                                    artifact_path,
                                    abs,
                                    &bxl_ctx.project_fs,
                                    &bxl_ctx.artifact_fs,
                                )?;

                                paths.push(path);
                                Ok(())
                            },
                            dice_ctx,
                        )
                        .boxed_local()
                    })
                });
                Ok(heap.alloc(paths))
            }
            None => Err(anyhow::anyhow!("Expected a cmd_args()")),
        }
    }
}

/// Global methods for Instant.
#[starlark_module]
pub(crate) fn register_instant_function(builder: &mut GlobalsBuilder) {
    /// Creates an Instant at the current time.
    ///
    /// Sample usage:
    /// ```text
    /// def _impl_elapsed_millis(ctx):
    ///     instant = now()
    ///     time_a = instant.elapsed_millis()
    ///     # do something that takes a long time
    ///     time_b = instant.elapsed_millis()
    ///
    ///     ctx.output.print(time_a)
    ///     ctx.output.print(time_b)
    /// ```
    fn now() -> anyhow::Result<StarlarkInstant> {
        Ok(StarlarkInstant(Instant::now()))
    }
}

/// This is used to mark the error returned by `fail_no_stacktrace()` (via context chaining).
/// We check if this marker is present after finishing BXL evaluation. If this marker is present,
/// then we hide the stacktrace. Otherwise, we emit the stacktrace to users.
#[derive(Debug, Error, Clone)]
#[error("fail:{0}")]
pub(crate) struct BxlErrorWithoutStacktrace(String);

/// Global method for error handling.
#[starlark_module]
pub(crate) fn register_error_handling_function(builder: &mut GlobalsBuilder) {
    fn fail_no_stacktrace(#[starlark(args)] args: Vec<Value>) -> anyhow::Result<NoneType> {
        let mut s = String::new();
        for x in args {
            s.push(' ');
            match x.unpack_str() {
                Some(x) => s.push_str(x),
                None => x.collect_repr(&mut s),
            }
        }
        Err(BxlErrorWithoutStacktrace(s).into())
    }
}
