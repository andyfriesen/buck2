/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use buck2_client_ctx::client_ctx::ClientCommandContext;
use buck2_client_ctx::exit_result::ExitResult;
use buck2_query::query::syntax::simple::functions::description::QueryType;
use buck2_query::query::syntax::simple::functions::description::QUERY_ENVIRONMENT_DESCRIPTION_BY_TYPE;
use buck2_query::query::syntax::simple::functions::docs::MarkdownOptions;
use buck2_query::query::syntax::simple::functions::docs::QueryEnvironmentDescription;

use crate::commands::docs::output::DocsOutputFormatOptions;

#[derive(Debug, clap::Parser)]
#[clap(name = "docs-uquery", about = "Print documentation for uquery")]
pub(crate) struct DocsUqueryCommand {
    #[clap(flatten)]
    docs_options: DocsOutputFormatOptions,
}

#[derive(Debug, clap::Parser)]
#[clap(name = "docs-cquery", about = "Print documentation for cquery")]
pub(crate) struct DocsCqueryCommand {
    #[clap(flatten)]
    docs_options: DocsOutputFormatOptions,
}

#[derive(Debug, clap::Parser)]
#[clap(name = "docs-aquery", about = "Print documentation for aquery")]
pub(crate) struct DocsAqueryCommand {
    #[clap(flatten)]
    docs_options: DocsOutputFormatOptions,
}

fn output(
    options: DocsOutputFormatOptions,
    description: QueryEnvironmentDescription,
) -> ExitResult {
    let markdown = description.render_markdown(&MarkdownOptions {
        include_alt_text: true,
    });
    options.emit_markdown(&markdown)?;
    ExitResult::success()
}

impl DocsUqueryCommand {
    pub(crate) fn exec(
        self,
        _matches: &clap::ArgMatches,
        _ctx: ClientCommandContext<'_>,
    ) -> ExitResult {
        let description = (QUERY_ENVIRONMENT_DESCRIPTION_BY_TYPE.get()?)(QueryType::Uquery);
        output(self.docs_options, description)
    }
}

impl DocsCqueryCommand {
    pub(crate) fn exec(
        self,
        _matches: &clap::ArgMatches,
        _ctx: ClientCommandContext<'_>,
    ) -> ExitResult {
        let description = (QUERY_ENVIRONMENT_DESCRIPTION_BY_TYPE.get()?)(QueryType::Cquery);
        output(self.docs_options, description)
    }
}

impl DocsAqueryCommand {
    pub(crate) fn exec(
        self,
        _matches: &clap::ArgMatches,
        _ctx: ClientCommandContext<'_>,
    ) -> ExitResult {
        let description = (QUERY_ENVIRONMENT_DESCRIPTION_BY_TYPE.get()?)(QueryType::Aquery);
        output(self.docs_options, description)
    }
}
