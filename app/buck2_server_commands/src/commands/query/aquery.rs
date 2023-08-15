/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::io::Write;

use anyhow::Context;
use async_trait::async_trait;
use buck2_build_api::query::oneshot::QUERY_FRONTEND;
use buck2_common::dice::cells::HasCellResolver;
use buck2_query::query::syntax::simple::eval::values::QueryEvaluationResult;
use buck2_server_ctx::ctx::ServerCommandContextTrait;
use buck2_server_ctx::partial_result_dispatcher::PartialResultDispatcher;
use buck2_server_ctx::pattern::target_platform_from_client_context;
use buck2_server_ctx::template::run_server_command;
use buck2_server_ctx::template::ServerCommandTemplate;
use dice::DiceTransaction;

use crate::commands::query::printer::QueryResultPrinter;
use crate::commands::query::printer::ShouldPrintProviders;

pub(crate) async fn aquery_command(
    ctx: &dyn ServerCommandContextTrait,
    partial_result_dispatcher: PartialResultDispatcher<buck2_cli_proto::StdoutBytes>,
    req: buck2_cli_proto::AqueryRequest,
) -> anyhow::Result<buck2_cli_proto::AqueryResponse> {
    run_server_command(AqueryServerCommand { req }, ctx, partial_result_dispatcher).await
}

struct AqueryServerCommand {
    req: buck2_cli_proto::AqueryRequest,
}

#[async_trait]
impl ServerCommandTemplate for AqueryServerCommand {
    type StartEvent = buck2_data::AqueryCommandStart;
    type EndEvent = buck2_data::AqueryCommandEnd;
    type Response = buck2_cli_proto::AqueryResponse;
    type PartialResult = buck2_cli_proto::StdoutBytes;

    async fn command(
        &self,
        server_ctx: &dyn ServerCommandContextTrait,
        mut partial_result_dispatcher: PartialResultDispatcher<Self::PartialResult>,
        ctx: DiceTransaction,
    ) -> anyhow::Result<Self::Response> {
        aquery(
            server_ctx,
            partial_result_dispatcher.as_writer(),
            ctx,
            &self.req,
        )
        .await
    }

    fn is_success(&self, response: &Self::Response) -> bool {
        response.error_messages.is_empty()
    }
}

async fn aquery(
    server_ctx: &dyn ServerCommandContextTrait,
    mut stdout: impl Write,
    mut ctx: DiceTransaction,
    request: &buck2_cli_proto::AqueryRequest,
) -> anyhow::Result<buck2_cli_proto::AqueryResponse> {
    let cell_resolver = ctx.get_cell_resolver().await?;

    let output_configuration = QueryResultPrinter::from_request_options(
        &cell_resolver,
        &request.output_attributes,
        request.unstable_output_format,
    )?;

    let buck2_cli_proto::AqueryRequest {
        query,
        query_args,
        context,
        ..
    } = request;

    let client_ctx = context
        .as_ref()
        .context("No client context (internal error)")?;
    let global_target_platform =
        target_platform_from_client_context(client_ctx, server_ctx, &mut ctx).await?;

    let query_result = QUERY_FRONTEND
        .get()?
        .eval_aquery(
            &ctx,
            server_ctx.working_dir(),
            query,
            query_args,
            global_target_platform,
        )
        .await?;

    let result = match query_result {
        QueryEvaluationResult::Single(targets) => {
            output_configuration
                .print_single_output(&mut stdout, targets, false, ShouldPrintProviders::No)
                .await
        }
        QueryEvaluationResult::Multiple(results) => {
            output_configuration
                .print_multi_output(&mut stdout, results, false, ShouldPrintProviders::No)
                .await
        }
    };
    let error_messages = match result {
        Ok(_) => vec![],
        Err(e) => vec![format!("{:#}", e)],
    };
    Ok(buck2_cli_proto::AqueryResponse { error_messages })
}
