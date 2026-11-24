//! A native call's outcome shown to the client, and a returned result block
//! given back to the model: both project the model's `tool_result` from the
//! block, so the two always agree.

use std::borrow::Cow;

use serde_json::Value;

use super::model_text::{
    SealedSearchResult, SearchSection, UNAVAILABLE_RESULTS_TEXT, projected_tool_result, search_error_text,
    search_queries, search_result_aad, search_text,
};
use super::sealing::{MESSAGES_RESULT_KEY_ENV, MessagesResultKey, OpenError};
use super::{NativeOutcome, NativeTool, ProjectedResult, Refusal, ReplayFailure, client_tool_use_id};
use crate::executor::messages_usage::ServerToolUses;
use crate::tool::web_fetch::{FailureOutput, WebFetchToolOutput};
use crate::tool::web_search::output::WebSearchToolOutput;
use crate::types::messages::{
    GatewayToolResult, ServerToolResultBlock, WebFetchDocument, WebFetchErrorCode, WebFetchResult,
    WebFetchToolResultContent, WebSearchErrorCode, WebSearchResultBlock, WebSearchToolResultContent,
    WebSearchToolResultError, tool_result_block,
};

fn search_error(client_id: String, code: WebSearchErrorCode) -> ServerToolResultBlock {
    ServerToolResultBlock::WebSearchToolResult {
        tool_use_id: client_id,
        content: WebSearchToolResultContent::Error(WebSearchToolResultError::new(code)),
    }
}

fn fetch_error(client_id: String, error_code: WebFetchErrorCode, message: Option<String>) -> ServerToolResultBlock {
    ServerToolResultBlock::WebFetchToolResult {
        tool_use_id: client_id,
        content: WebFetchToolResultContent::WebFetchToolResultError { error_code, message },
    }
}

/// Show one native call's outcome to the client, and project the model's
/// `tool_result` from what the client is shown.
pub(crate) fn project_native_call(
    tool: NativeTool,
    upstream_id: &str,
    input: &Value,
    outcome: NativeOutcome,
    key: &MessagesResultKey,
) -> ProjectedResult {
    let client_id = client_tool_use_id(upstream_id);
    match tool {
        NativeTool::WebSearch => project_search(&client_id, upstream_id, input, outcome, key),
        NativeTool::WebFetch => {
            let block = match outcome {
                NativeOutcome::Output { text, failed: false } => {
                    match serde_json::from_str::<WebFetchToolOutput<'_>>(&text) {
                        Ok(page) => ServerToolResultBlock::WebFetchToolResult {
                            tool_use_id: client_id,
                            content: WebFetchToolResultContent::WebFetchResult(WebFetchResult {
                                url: page.url.into_owned(),
                                content: WebFetchDocument::new(
                                    page.content.into_owned(),
                                    page.title.map(Cow::into_owned),
                                ),
                                retrieved_at: page.retrieved_at.map(Cow::into_owned),
                                content_type: page.content_type.map(Cow::into_owned),
                                truncated: page.truncated,
                            }),
                        },
                        Err(error) => {
                            tracing::warn!(%error, "web_fetch output did not parse; reporting the call as unavailable");
                            fetch_error(client_id, WebFetchErrorCode::Unavailable, None)
                        }
                    }
                }
                NativeOutcome::Output { text, failed: true } => {
                    if let Ok(failure) = serde_json::from_str::<FailureOutput<'_>>(&text) {
                        fetch_error(client_id, failure.error_code, Some(failure.message.into_owned()))
                    } else {
                        tracing::warn!(output = %text, "native web_fetch call failed");
                        fetch_error(client_id, WebFetchErrorCode::Unavailable, None)
                    }
                }
                NativeOutcome::Refused(refusal) => fetch_error(
                    client_id,
                    match refusal {
                        Refusal::InvalidInput => WebFetchErrorCode::InvalidToolInput,
                        Refusal::MaxUsesExceeded => WebFetchErrorCode::MaxUsesExceeded,
                        Refusal::UrlNotInPriorContext => WebFetchErrorCode::UrlNotInPriorContext,
                    },
                    None,
                ),
                NativeOutcome::Failed => fetch_error(client_id, WebFetchErrorCode::Unavailable, None),
            };
            let fetched = matches!(
                block,
                ServerToolResultBlock::WebFetchToolResult {
                    content: WebFetchToolResultContent::WebFetchResult(_),
                    ..
                }
            );
            let tool_result = projected_tool_result(upstream_id, &block)
                .unwrap_or_else(|| tool_result_block(upstream_id, String::new(), true));
            ProjectedResult {
                block,
                tool_result,
                uses: ServerToolUses {
                    web_fetch_requests: u64::from(fetched),
                    ..ServerToolUses::default()
                },
            }
        }
    }
}

fn project_search(
    client_id: &str,
    upstream_id: &str,
    input: &Value,
    outcome: NativeOutcome,
    key: &MessagesResultKey,
) -> ProjectedResult {
    let code = match outcome {
        NativeOutcome::Output { text, failed: false } => match seal_search(client_id, upstream_id, input, &text, key) {
            Ok(projected) => return projected,
            Err(reason) => {
                tracing::warn!(
                    reason,
                    "native web_search result could not be returned; reporting it as unavailable"
                );
                WebSearchErrorCode::Unavailable
            }
        },
        NativeOutcome::Output { failed: true, .. } | NativeOutcome::Failed => WebSearchErrorCode::Unavailable,
        NativeOutcome::Refused(Refusal::MaxUsesExceeded) => WebSearchErrorCode::MaxUsesExceeded,
        NativeOutcome::Refused(Refusal::InvalidInput | Refusal::UrlNotInPriorContext) => {
            WebSearchErrorCode::InvalidToolInput
        }
    };
    let block = search_error(client_id.to_owned(), code);
    let tool_result = tool_result_block(upstream_id, search_error_text(code).to_owned(), true);
    ProjectedResult {
        block,
        tool_result,
        uses: ServerToolUses::default(),
    }
}

/// Seal a successful search's results and project its `tool_result` from the
/// sealed bytes, exactly as a replay will.
fn seal_search(
    client_id: &str,
    upstream_id: &str,
    input: &Value,
    text: &str,
    key: &MessagesResultKey,
) -> Result<ProjectedResult, &'static str> {
    let output: WebSearchToolOutput<'_> =
        serde_json::from_str(text).map_err(|_| "the executor output did not parse")?;
    let queries = search_queries(input).ok_or("the call input did not parse")?;
    let sealed: Vec<SealedSearchResult> = output
        .results
        .web
        .into_iter()
        .map(|result| (SearchSection::Web, result))
        .chain(
            output
                .results
                .news
                .into_iter()
                .map(|result| (SearchSection::News, result)),
        )
        .map(|(section, result)| SealedSearchResult { section, result })
        .collect();
    let count = sealed.len();
    let mut payloads = Vec::with_capacity(count);
    let mut results = Vec::with_capacity(count);
    for (index, entry) in sealed.iter().enumerate() {
        let payload = serde_json::to_vec(entry).map_err(|_| "a result did not serialize")?;
        let encrypted = key
            .seal(&search_result_aad(upstream_id, &queries, index, count), payload.clone())
            .map_err(|_| "a result could not be sealed")?;
        results.push(WebSearchResultBlock::new(
            entry.result.url.clone(),
            entry.result.title.clone().unwrap_or_default(),
            encrypted,
            entry.result.page_age.clone(),
        ));
        payloads.push(payload);
    }
    let text = search_text(&queries, &payloads).ok_or("a sealed result did not parse")?;
    Ok(ProjectedResult {
        block: ServerToolResultBlock::WebSearchToolResult {
            tool_use_id: client_id.to_owned(),
            content: WebSearchToolResultContent::Results(results),
        },
        tool_result: tool_result_block(upstream_id, text, false),
        uses: ServerToolUses {
            web_search_requests: u64::try_from(queries.len()).unwrap_or(u64::MAX),
            ..ServerToolUses::default()
        },
    })
}

/// The `tool_result` a replayed result block gives the model: the same text the
/// live round projected, or a fixed note when the search content was sealed
/// under a key this gateway no longer holds.
pub(crate) fn replayed_tool_result(
    upstream_id: &str,
    input: &Value,
    block: &ServerToolResultBlock,
    key: &MessagesResultKey,
) -> Result<GatewayToolResult, ReplayFailure> {
    let ServerToolResultBlock::WebSearchToolResult {
        content: WebSearchToolResultContent::Results(results),
        ..
    } = block
    else {
        return projected_tool_result(upstream_id, block).ok_or(ReplayFailure::Tampered);
    };
    let queries = search_queries(input).ok_or(ReplayFailure::InvalidInput)?;
    let count = results.len();
    let opened: Vec<_> = results
        .iter()
        .enumerate()
        .map(|(index, result)| {
            key.open(
                &search_result_aad(upstream_id, &queries, index, count),
                &result.encrypted_content,
            )
        })
        .collect();
    if !opened.is_empty() && opened.iter().all(|result| *result == Err(OpenError::OtherKey)) {
        tracing::warn!(
            tool_use_id = upstream_id,
            "replayed web_search results were sealed under another key; set {} to keep them across restarts and \
             replicas",
            MESSAGES_RESULT_KEY_ENV
        );
        return Ok(tool_result_block(
            upstream_id,
            UNAVAILABLE_RESULTS_TEXT.to_owned(),
            true,
        ));
    }
    let payloads = opened
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ReplayFailure::Tampered)?;
    let text = search_text(&queries, &payloads).ok_or(ReplayFailure::Tampered)?;
    Ok(tool_result_block(upstream_id, text, false))
}
