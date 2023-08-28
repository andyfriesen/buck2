/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::sync::Arc;

use async_trait::async_trait;
use buck2_artifact::actions::key::ActionKey;
use buck2_build_api::actions::query::ActionQueryNode;
use buck2_build_api::actions::query::ActionQueryNodeRef;
use buck2_build_api::artifact_groups::ArtifactGroup;
use buck2_core::configuration::compatibility::MaybeCompatible;
use buck2_query::query::environment::QueryEnvironment;
use buck2_query::query::syntax::simple::eval::error::QueryError;
use buck2_query::query::syntax::simple::eval::file_set::FileSet;
use buck2_query::query::syntax::simple::eval::set::TargetSet;
use buck2_query::query::syntax::simple::functions::docs::QueryEnvironmentDescription;
use buck2_query::query::syntax::simple::functions::DefaultQueryFunctionsModule;
use buck2_query::query::syntax::simple::functions::HasModuleDescription;
use buck2_query::query::traversal::async_depth_first_postorder_traversal;
use buck2_query::query::traversal::async_depth_limited_traversal;
use buck2_query::query::traversal::AsyncNodeLookup;
use buck2_query::query::traversal::AsyncTraversalDelegate;
use dice::DiceComputations;

use crate::aquery::functions::AqueryFunctions;
use crate::cquery::environment::CqueryDelegate;
use crate::uquery::environment::QueryLiterals;

/// CqueryDelegate resolves information needed by the QueryEnvironment.
#[async_trait]
pub trait AqueryDelegate: Send + Sync {
    fn cquery_delegate(&self) -> &dyn CqueryDelegate;

    fn ctx(&self) -> &DiceComputations;

    async fn get_node(&self, key: &ActionKey) -> anyhow::Result<ActionQueryNode>;

    async fn expand_artifacts(
        &self,
        artifacts: &[ArtifactGroup],
    ) -> anyhow::Result<Vec<ActionQueryNode>>;
}

pub struct AqueryEnvironment<'c> {
    pub(super) delegate: Arc<dyn AqueryDelegate + 'c>,
    literals: Arc<dyn QueryLiterals<ActionQueryNode> + 'c>,
}

impl<'c> AqueryEnvironment<'c> {
    pub fn new(
        delegate: Arc<dyn AqueryDelegate + 'c>,
        literals: Arc<dyn QueryLiterals<ActionQueryNode> + 'c>,
    ) -> Self {
        Self { delegate, literals }
    }

    async fn get_node(&self, label: &ActionQueryNodeRef) -> anyhow::Result<ActionQueryNode> {
        // We do not allow traversing edges in targets in aquery
        self.delegate.get_node(label.require_action()?).await
    }

    pub(crate) fn describe() -> QueryEnvironmentDescription {
        QueryEnvironmentDescription {
            name: "Aquery Environment".to_owned(),
            mods: vec![
                DefaultQueryFunctionsModule::<Self>::describe(),
                AqueryFunctions::describe(),
            ],
        }
    }
}

#[async_trait]
impl<'a> AsyncNodeLookup<ActionQueryNode> for AqueryEnvironment<'a> {
    async fn get(&self, label: &ActionQueryNodeRef) -> anyhow::Result<ActionQueryNode> {
        self.get_node(label).await
    }
}

#[async_trait]
impl<'c> QueryEnvironment for AqueryEnvironment<'c> {
    type Target = ActionQueryNode;

    async fn get_node(&self, node_ref: &ActionQueryNodeRef) -> anyhow::Result<Self::Target> {
        AqueryEnvironment::get_node(self, node_ref).await
    }

    async fn get_node_for_default_configured_target(
        &self,
        _node_ref: &ActionQueryNodeRef,
    ) -> anyhow::Result<MaybeCompatible<Self::Target>> {
        Err(QueryError::FunctionUnimplemented(
            "get_node_for_default_configured_target() only for CqueryEnvironment",
        )
        .into())
    }

    async fn eval_literals(&self, literals: &[&str]) -> anyhow::Result<TargetSet<Self::Target>> {
        self.literals
            .eval_literals(literals, self.delegate.ctx())
            .await
    }

    async fn eval_file_literal(&self, literal: &str) -> anyhow::Result<FileSet> {
        self.delegate
            .cquery_delegate()
            .uquery_delegate()
            .eval_file_literal(literal)
            .await
    }

    async fn dfs_postorder(
        &self,
        root: &TargetSet<Self::Target>,
        traversal_delegate: &mut dyn AsyncTraversalDelegate<Self::Target>,
    ) -> anyhow::Result<()> {
        // TODO(cjhopman): The query nodes deps are going to flatten the tset structure for its deps. In a typical
        // build graph, a traversal over just the graph of ActionQueryNode ends up being an `O(n)` operation at each
        // node and ends up with an `O(n^2)` cost. If instead we were to not flatten the structure and traverse the
        // mixed graph of action nodes and tset nodes, we'd get closer to `O(n + e)` which in practice is much better
        // (hence the whole point of tsets). While we can't change the ActionQueryNode deps() function to not flatten
        // the tset, we aren't required to do these traversal's using that function.
        async_depth_first_postorder_traversal(self, root.iter_names(), traversal_delegate).await
    }

    async fn depth_limited_traversal(
        &self,
        root: &TargetSet<Self::Target>,
        delegate: &mut dyn AsyncTraversalDelegate<Self::Target>,
        depth: u32,
    ) -> anyhow::Result<()> {
        // TODO(cjhopman): See above.
        async_depth_limited_traversal(self, root.iter_names(), delegate, depth).await
    }

    async fn owner(&self, _paths: &FileSet) -> anyhow::Result<TargetSet<Self::Target>> {
        Err(QueryError::NotAvailableInContext("owner").into())
    }
}
