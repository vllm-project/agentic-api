//! The model-facing call and output a stored `web_search_call` stands for.

use std::borrow::Cow;

use super::input::{FunctionToolResultMessage, InputFunctionToolCall};
use super::output::{WebSearchAction, WebSearchCall};
use crate::tool::web_search::output::{WebSearchResultSections, WebSearchToolOutput};
use crate::tool::web_search::provider::WebSearchResult;
use crate::types::event::MessageStatus;
use crate::types::messages::tool_seam::WEB_SEARCH_EXECUTOR;

/// The model-facing call a stored `web_search_call` stands for, under the item's id: one call carrying every query.
impl From<&WebSearchCall> for InputFunctionToolCall {
    fn from(call: &WebSearchCall) -> Self {
        let arguments = match &call.action {
            WebSearchAction::Search(search) if search.queries.is_empty() => {
                serde_json::json!({ "queries": [search.query] })
            }
            WebSearchAction::Search(search) => serde_json::json!({ "queries": search.queries }),
            // Page actions take no search arguments.
            WebSearchAction::OpenPage(_) | WebSearchAction::FindInPage(_) => serde_json::json!({}),
        };
        Self {
            agent: call.agent.clone(),
            id: None,
            call_id: call.id.clone(),
            name: WEB_SEARCH_EXECUTOR.to_owned(),
            namespace: None,
            arguments: arguments.to_string(),
            status: Some(MessageStatus::Completed),
            async_execution: false,
        }
    }
}

/// The model-facing output of a stored `web_search_call`. The public item keeps the queries and the sources' URLs and
/// titles, not the result text, so the output lists those sources; a page action shows the action taken.
impl From<&WebSearchCall> for FunctionToolResultMessage {
    fn from(call: &WebSearchCall) -> Self {
        let output = match &call.action {
            WebSearchAction::Search(search) => {
                let queries = if search.queries.is_empty() {
                    Cow::Owned(vec![search.query.clone()])
                } else {
                    Cow::Borrowed(search.queries.as_slice())
                };
                let web = search
                    .sources
                    .iter()
                    .map(|source| WebSearchResult {
                        url: source.url.clone(),
                        title: source.title.clone(),
                        ..WebSearchResult::default()
                    })
                    .collect();
                serde_json::to_string(&WebSearchToolOutput {
                    query: Cow::Borrowed(&search.query),
                    queries,
                    results: WebSearchResultSections { web, news: Vec::new() },
                    metadata: None,
                })
            }
            action @ (WebSearchAction::OpenPage(_) | WebSearchAction::FindInPage(_)) => serde_json::to_string(action),
        };
        Self {
            id: None,
            status: Some(MessageStatus::Completed),
            call_id: call.id.clone(),
            output: output.unwrap_or_default().into(),
        }
    }
}
