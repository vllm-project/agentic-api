//! What the model sees of tool-search history.
//!
//! The public `tool_search_call` and `tool_search_output` items are the canonical record; the model called a
//! `tool_search` function and read its output. The upstream projection therefore carries each search item as that
//! function call or output. A client search pairs its items by `call_id`, which the pending-call ledger checks. A
//! hosted search has no call id: its output answers the oldest earlier hosted call still open, and the model's call
//! id is that call's item id, so the projection is the same live, on continuation, and on replay.

use std::borrow::Borrow;
use std::collections::{HashSet, VecDeque};

use serde::Serialize;
use serde_json::Value;

use super::{
    FunctionToolResultMessage, InputFunctionToolCall, InputItem, InputToolSearchCall, ResponsesInput, ToolCallOutput,
    ToolSearchOutputMessage,
};
use crate::types::event::MessageStatus;
use crate::types::io::{AgentAttribution, InOutItem, OutputItem};
use crate::types::tools::{FunctionToolParam, ResponsesTool, ToolSearchExecution, ToolSearchStatus};
use crate::utils::common::serialize_to_string;

/// The function the model calls to search; see `tool::tool_search`.
pub(crate) const TOOL_SEARCH_NAME: &str = "tool_search";

/// The model-facing text of a hosted search that loaded nothing. It says why, so a model does
/// not keep repeating a filter that matches nothing or a path it has already loaded.
const HOSTED_EMPTY_SEARCH_OUTPUT: &str = r#"{"tools":[],"note":"No tools were loaded. `query` keeps only tools whose name or description contains it, and tools loaded earlier in this conversation are not returned again. Search the path again without `query`, or call a tool that is already loaded."}"#;

/// Stands in for a search output whose tools cannot be serialized; typed definitions always can.
const UNSERIALIZABLE_SEARCH_OUTPUT: &str = r#"{"tools":[]}"#;

#[derive(Serialize)]
struct CanonicalToolSearchOutput<'a> {
    tools: &'a [ModelVisibleLoadedTool<'a>],
}

/// Typed model-output projection, deliberately separate from the raw
/// credential-sensitive definition retained for equality and later execution.
#[derive(Serialize)]
#[serde(untagged)]
enum ModelVisibleLoadedTool<'a> {
    Function(ModelVisibleFunction<'a>),
    Namespace(ModelVisibleNamespace<'a>),
}

#[derive(Serialize)]
struct ModelVisibleFunction<'a> {
    #[serde(rename = "type")]
    type_: &'static str,
    #[serde(flatten)]
    definition: &'a FunctionToolParam,
}

#[derive(Serialize)]
struct ModelVisibleNamespace<'a> {
    #[serde(rename = "type")]
    type_: &'static str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
}

/// How one search item pairs with its counterpart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SearchPairing {
    /// The call id the model sees for this item.
    Paired(String),
    /// A call that no later output answers.
    OpenCall,
    /// An output that answers no earlier call.
    UnmatchedOutput,
}

/// A search item no history may hold, as the Responses API pairs search items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SearchHistoryDefect {
    #[error("duplicate tool_search_call item id")]
    DuplicateCallItemId,
    /// A hosted call's model call id is its item id; another call already uses it, so the model would see two calls
    /// with one id.
    #[error("duplicate tool_search_call call_id")]
    DuplicateHostedCallId,
    #[error("hosted tool_search_output has no preceding hosted tool_search_call")]
    HostedOutputWithoutCall,
    /// Only a completed output loads definitions.
    #[error("tool_search_output must be completed before it may load tool definitions")]
    IncompleteOutput,
    /// A hosted call no later output answers.
    #[error("unresolved tool_search_call requires a matching completed tool_search_output")]
    OpenHostedCall,
}

/// The pairing of every search item in one item sequence, by position, and the first defect at each position.
///
/// A client search's call and output pair by `call_id`, which the pending-call ledger checks; this owns the rest:
/// item ids, hosted pairs and output status.
pub(crate) struct SearchHistory {
    pairings: Vec<Option<SearchPairing>>,
    defects: Vec<Option<SearchHistoryDefect>>,
    open_hosted_call: bool,
}

impl SearchHistory {
    /// Pairs the search items of `items`. It rejects nothing: callers that must refuse an unpaired item read
    /// [`Self::pairing`], and the projection leaves one out.
    pub(crate) fn scan<T: Borrow<InputItem>>(items: &[T]) -> Self {
        let mut pairings = vec![None; items.len()];
        let mut open_hosted_calls = VecDeque::new();
        for (position, item) in items.iter().enumerate() {
            pairings[position] = match item.borrow() {
                InputItem::ToolSearchCall(call) => Some(match (call.execution, &call.call_id) {
                    (ToolSearchExecution::Server, _) => {
                        open_hosted_calls.push_back(position);
                        SearchPairing::Paired(hosted_private_call_id(&call.id))
                    }
                    (ToolSearchExecution::Client, Some(call_id)) => SearchPairing::Paired(call_id.clone()),
                    // Deserialization requires a client call id; without one nothing can answer it.
                    (ToolSearchExecution::Client, None) => SearchPairing::OpenCall,
                }),
                InputItem::ToolSearchOutput(output) => Some(match (output.execution, &output.call_id) {
                    (ToolSearchExecution::Server, _) => {
                        open_hosted_calls
                            .pop_front()
                            .map_or(SearchPairing::UnmatchedOutput, |call_position| {
                                match &pairings[call_position] {
                                    Some(SearchPairing::Paired(call_id)) => SearchPairing::Paired(call_id.clone()),
                                    _ => SearchPairing::UnmatchedOutput,
                                }
                            })
                    }
                    (ToolSearchExecution::Client, Some(call_id)) => SearchPairing::Paired(call_id.clone()),
                    (ToolSearchExecution::Client, None) => SearchPairing::UnmatchedOutput,
                }),
                _ => None,
            };
        }
        let open_hosted_call = !open_hosted_calls.is_empty();
        for position in open_hosted_calls {
            pairings[position] = Some(SearchPairing::OpenCall);
        }
        let defects = defects(items, &pairings);
        Self {
            pairings,
            defects,
            open_hosted_call,
        }
    }

    /// The first defect of the search item at `position`, in the order history is checked.
    pub(crate) fn defect(&self, position: usize) -> Option<SearchHistoryDefect> {
        self.defects.get(position).copied().flatten()
    }

    /// Whether a hosted call is still open at the end of the history.
    pub(crate) const fn has_open_hosted_call(&self) -> bool {
        self.open_hosted_call
    }

    /// The pairing of the search item at `position`, or `None` for any other item.
    pub(crate) fn pairing(&self, position: usize) -> Option<&SearchPairing> {
        self.pairings.get(position).and_then(Option::as_ref)
    }

    /// The model's view of `items`: each paired search item becomes the `tool_search` function call or output the
    /// model saw, and an unpaired hosted item is left out, since the model never saw half of a hosted pair.
    pub(crate) fn lower(items: Vec<InputItem>) -> Vec<InputItem> {
        let history = Self::scan(&items);
        items
            .into_iter()
            .enumerate()
            .filter_map(|(position, item)| {
                if is_search_item(&item) {
                    history.lowered(position, &item)
                } else {
                    Some(item)
                }
            })
            .collect()
    }

    /// The model's form of the search item `item` at `position`, or `None` for an unpaired one or any other item.
    pub(crate) fn lowered(&self, position: usize, item: &InputItem) -> Option<InputItem> {
        let Some(SearchPairing::Paired(call_id)) = self.pairing(position) else {
            return None;
        };
        match item {
            InputItem::ToolSearchCall(call) => Some(model_search_call(call, call_id.clone())),
            InputItem::ToolSearchOutput(output) => Some(model_search_output(output, call_id.clone())),
            _ => None,
        }
    }
}

/// The first defect of each search item, checked in history order: item ids and hosted call ids are unique among
/// earlier items (a hosted call id also among every client call id), and an output answers a call and is complete.
fn defects<T: Borrow<InputItem>>(items: &[T], pairings: &[Option<SearchPairing>]) -> Vec<Option<SearchHistoryDefect>> {
    let client_call_ids: HashSet<&str> = items.iter().filter_map(|item| item.borrow().call_id()).collect();
    let mut call_item_ids = HashSet::new();
    let mut hosted_call_ids = HashSet::new();
    items
        .iter()
        .zip(pairings)
        .map(|(item, pairing)| match item.borrow() {
            InputItem::ToolSearchCall(call) => {
                if !call_item_ids.insert(call.id.as_str()) {
                    return Some(SearchHistoryDefect::DuplicateCallItemId);
                }
                match pairing {
                    Some(SearchPairing::Paired(call_id))
                        if call.execution == ToolSearchExecution::Server
                            && (client_call_ids.contains(call_id.as_str())
                                || !hosted_call_ids.insert(call_id.as_str())) =>
                    {
                        Some(SearchHistoryDefect::DuplicateHostedCallId)
                    }
                    _ => None,
                }
            }
            InputItem::ToolSearchOutput(output) => {
                if output.execution == ToolSearchExecution::Server && pairing == &Some(SearchPairing::UnmatchedOutput) {
                    Some(SearchHistoryDefect::HostedOutputWithoutCall)
                } else if output.status != ToolSearchStatus::Completed {
                    Some(SearchHistoryDefect::IncompleteOutput)
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect()
}

/// Whether the upstream projection rewrites `item`.
pub(crate) const fn is_search_item(item: &InputItem) -> bool {
    matches!(item, InputItem::ToolSearchCall(_) | InputItem::ToolSearchOutput(_))
}

impl ResponsesInput {
    /// The top-level functions this input's completed search outputs loaded. A namespace member may share its
    /// namespace's name, so these record which calls naming themselves were to a searched function.
    pub(crate) fn searched_function_names(&self) -> HashSet<&str> {
        let Self::Items(items) = self else {
            return HashSet::new();
        };
        items
            .iter()
            .filter_map(|item| match item {
                InOutItem::Input(InputItem::ToolSearchOutput(output))
                | InOutItem::Output(OutputItem::ToolSearchOutput(output)) => Some(output),
                InOutItem::Input(_) | InOutItem::Output(_) => None,
            })
            .filter(|output| output.status == ToolSearchStatus::Completed)
            .flat_map(|output| &output.tools)
            .filter_map(|tool| match tool {
                ResponsesTool::Function(function) => Some(function.name.as_str()),
                ResponsesTool::Namespace(_)
                | ResponsesTool::ToolSearch(_)
                | ResponsesTool::Mcp(_)
                | ResponsesTool::WebSearch(_)
                | ResponsesTool::FileSearch(_)
                | ResponsesTool::CodeInterpreter(_)
                | ResponsesTool::Shell(_)
                | ResponsesTool::Custom(_)
                | ResponsesTool::Unknown => None,
            })
            .collect()
    }
}

fn model_search_call(call: &InputToolSearchCall, call_id: String) -> InputItem {
    private_search_call(call.agent.clone(), call_id, &call.arguments)
}

fn model_search_output(output: &ToolSearchOutputMessage, call_id: String) -> InputItem {
    let text = match output.execution {
        ToolSearchExecution::Server => hosted_search_output_text(&output.tools),
        ToolSearchExecution::Client => canonical_search_output(&output.tools),
    };
    InputItem::FunctionCallOutput(FunctionToolResultMessage {
        id: None,
        status: None,
        call_id,
        output: ToolCallOutput::Text(text),
    })
}

/// The private call id of a hosted search, derived from its public item id.
fn hosted_private_call_id(item_id: &str) -> String {
    item_id.to_owned()
}

/// The private function call the model sees for a public search call. It carries no item id: an upstream such as
/// `OpenAI` ties the model's own item id to the `call_id` it issued, which the public pair does not keep, so the call is
/// identified by the derived `call_id` alone.
fn private_search_call(agent: Option<AgentAttribution>, call_id: String, arguments: &Value) -> InputItem {
    InputItem::FunctionCall(InputFunctionToolCall {
        async_execution: false,
        agent,
        id: None,
        call_id,
        name: TOOL_SEARCH_NAME.to_owned(),
        namespace: None,
        // A JSON value always serializes; an empty object stands in if that ever fails.
        arguments: serialize_to_string(arguments).unwrap_or_else(|_| "{}".to_owned()),
        status: Some(MessageStatus::Completed),
    })
}

/// The model-facing text of a hosted search output, live or rebuilt from history.
pub(crate) fn hosted_search_output_text(tools: &[ResponsesTool]) -> String {
    if tools.is_empty() {
        return HOSTED_EMPTY_SEARCH_OUTPUT.to_owned();
    }
    canonical_search_output(tools)
}

/// The model-facing text of a search output: functions with their schemas, namespaces by name and description
/// (members are loaded as tools). Loading accepts only functions and namespaces, so no other kind is shown.
fn canonical_search_output(tools: &[ResponsesTool]) -> String {
    let projected_tools: Vec<_> = tools
        .iter()
        .filter_map(|tool| match tool {
            ResponsesTool::Function(definition) => Some(ModelVisibleLoadedTool::Function(ModelVisibleFunction {
                type_: "function",
                definition,
            })),
            ResponsesTool::Namespace(namespace) => Some(ModelVisibleLoadedTool::Namespace(ModelVisibleNamespace {
                type_: "namespace",
                name: &namespace.name,
                description: namespace.description.as_deref(),
            })),
            ResponsesTool::ToolSearch(_)
            | ResponsesTool::Mcp(_)
            | ResponsesTool::WebSearch(_)
            | ResponsesTool::FileSearch(_)
            | ResponsesTool::CodeInterpreter(_)
            | ResponsesTool::Shell(_)
            | ResponsesTool::Custom(_)
            | ResponsesTool::Unknown => None,
        })
        .collect();
    serialize_to_string(&CanonicalToolSearchOutput {
        tools: &projected_tools,
    })
    .unwrap_or_else(|_| UNSERIALIZABLE_SEARCH_OUTPUT.to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{HOSTED_EMPTY_SEARCH_OUTPUT, SearchHistory, SearchPairing};
    use crate::types::io::InputItem;

    fn items(value: Value) -> Vec<InputItem> {
        serde_json::from_value(value).expect("search items")
    }

    fn hosted_call(id: &str) -> Value {
        json!({"type": "tool_search_call", "id": id, "call_id": null, "execution": "server",
            "arguments": {"paths": ["f"]}, "status": "completed"})
    }

    fn hosted_output(id: &str) -> Value {
        json!({"type": "tool_search_output", "id": id, "call_id": null, "execution": "server",
            "status": "completed", "tools": []})
    }

    fn client_call(call_id: &str) -> Value {
        json!({"type": "tool_search_call", "id": format!("tsc_{call_id}"), "call_id": call_id, "execution": "client",
            "arguments": {"query": "f"}, "status": "completed"})
    }

    fn client_output(call_id: &str) -> Value {
        json!({"type": "tool_search_output", "call_id": call_id, "execution": "client", "status": "completed",
            "tools": []})
    }

    fn paired(id: &str) -> SearchPairing {
        SearchPairing::Paired(id.to_owned())
    }

    #[test]
    fn hosted_outputs_answer_the_oldest_open_hosted_call() {
        let items = items(json!([
            hosted_call("tsc_a"),
            hosted_call("tsc_b"),
            client_call("call_c"),
            hosted_output("tso_1"),
            client_output("call_c"),
            hosted_output("tso_2")
        ]));
        let history = SearchHistory::scan(&items);
        let expected = ["tsc_a", "tsc_b", "call_c", "tsc_a", "call_c", "tsc_b"].map(paired);
        for (position, expected) in expected.iter().enumerate() {
            assert_eq!(history.pairing(position), Some(expected), "item {position}");
        }
    }

    #[test]
    fn unpaired_hosted_items_are_left_out_of_the_model_input() {
        let items = items(
            json!([hosted_output("tso_orphan"), hosted_call("tsc_a"), hosted_output("tso_a"),
            hosted_call("tsc_open"), {"type": "message", "role": "user", "content": "next"}]),
        );
        let history = SearchHistory::scan(&items);
        assert_eq!(history.pairing(0), Some(&SearchPairing::UnmatchedOutput));
        assert_eq!(history.pairing(3), Some(&SearchPairing::OpenCall));
        assert_eq!(history.pairing(4), None);
        let lowered = serde_json::to_value(SearchHistory::lower(items)).expect("lowered items");
        assert_eq!(
            lowered,
            json!([
                {"type": "function_call", "call_id": "tsc_a", "name": "tool_search",
                    "arguments": "{\"paths\":[\"f\"]}", "status": "completed"},
                {"type": "function_call_output", "call_id": "tsc_a", "output": HOSTED_EMPTY_SEARCH_OUTPUT},
                {"type": "message", "role": "user", "content": "next"}
            ])
        );
    }

    #[test]
    fn a_client_search_keeps_its_call_id_and_says_plainly_that_nothing_loaded() {
        let lowered = serde_json::to_value(SearchHistory::lower(items(json!([
            client_call("call_c"),
            client_output("call_c")
        ]))))
        .expect("lowered items");
        assert_eq!(lowered[0]["call_id"], "call_c");
        assert_eq!(
            lowered[1],
            json!({"type": "function_call_output", "call_id": "call_c", "output": "{\"tools\":[]}"})
        );
    }
}
