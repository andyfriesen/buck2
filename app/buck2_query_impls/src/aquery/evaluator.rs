/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Implementation of the cli and query_* attr query language.
use std::sync::Arc;

use buck2_build_api::actions::query::ActionQueryNode;
use buck2_core::fs::project_rel_path::ProjectRelativePath;
use buck2_core::target::label::TargetLabel;
use buck2_query::query::syntax::simple::eval::values::QueryEvaluationResult;
use dice::DiceComputations;
use dupe::Dupe;

use crate::analysis::evaluator::eval_query;
use crate::aquery::environment::AqueryDelegate;
use crate::aquery::environment::AqueryEnvironment;
use crate::aquery::functions::aquery_functions;
use crate::dice::aquery::DiceAqueryDelegate;
use crate::dice::get_dice_query_delegate;
use crate::uquery::environment::PreresolvedQueryLiterals;

pub(crate) struct AqueryEvaluator<'c> {
    dice_query_delegate: Arc<DiceAqueryDelegate<'c>>,
}

impl AqueryEvaluator<'_> {
    pub(crate) async fn eval_query(
        &self,
        query: &str,
        query_args: &[String],
    ) -> anyhow::Result<QueryEvaluationResult<ActionQueryNode>> {
        let functions = aquery_functions();

        eval_query(&functions, query, query_args, async move |literals| {
            let resolved_literals = PreresolvedQueryLiterals::pre_resolve(
                &**self.dice_query_delegate.query_data(),
                &literals,
                self.dice_query_delegate.ctx(),
            )
            .await;
            Ok(AqueryEnvironment::new(
                self.dice_query_delegate.dupe(),
                Arc::new(resolved_literals),
            ))
        })
        .await
    }
}

/// Evaluates some query expression. TargetNodes are resolved via the interpreter from
/// the provided DiceCtx.
pub(crate) async fn get_aquery_evaluator<'a, 'c: 'a>(
    ctx: &'c DiceComputations,
    working_dir: &'a ProjectRelativePath,
    global_target_platform: Option<TargetLabel>,
) -> anyhow::Result<AqueryEvaluator<'c>> {
    let dice_query_delegate =
        get_dice_aquery_delegate(ctx, working_dir, global_target_platform).await?;
    Ok(AqueryEvaluator {
        dice_query_delegate,
    })
}

// Provides the dice query delegate for aquery evaluator
pub(crate) async fn get_dice_aquery_delegate<'a, 'c: 'a>(
    ctx: &'c DiceComputations,
    working_dir: &'a ProjectRelativePath,
    global_target_platform: Option<TargetLabel>,
) -> anyhow::Result<Arc<DiceAqueryDelegate<'c>>> {
    let dice_query_delegate =
        get_dice_query_delegate(ctx, working_dir, global_target_platform).await?;
    let dice_query_delegate = Arc::new(DiceAqueryDelegate::new(dice_query_delegate).await?);
    Ok(dice_query_delegate)
}
