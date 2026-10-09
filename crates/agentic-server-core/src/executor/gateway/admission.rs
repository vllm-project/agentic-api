//! `max_tool_calls` admission for gateway-executed built-in tool calls.
//!
//! The Responses API counts every dispatched built-in tool call toward one
//! response-wide limit, whether or not the call succeeds. Client-executed tool
//! calls never count. The scheduler admits calls in model output order while it
//! plans a round, before any call runs, so concurrent execution cannot exceed
//! the limit.

use std::num::NonZeroU64;

use crate::types::io::output::WebSearchCallStatus;
use crate::types::io::{CodeInterpreterCall, CodeInterpreterCallStatus, OutputItem, WebSearchCall};

/// Admission decision for one planned gateway call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admission {
    /// Execute the call; it counts toward the limit.
    Dispatch,
    /// Do not execute the call. `visible` holds until a refused call with a
    /// public item is shown: the Responses API never shows more than one.
    Refuse { limit: NonZeroU64, visible: bool },
}

/// Built-in tool calls remaining in one public response.
#[derive(Debug, Clone)]
pub(crate) struct BuiltInToolCallBudget {
    limit: Option<NonZeroU64>,
    dispatched: u64,
    refused: bool,
    refusal_shown: bool,
}

impl BuiltInToolCallBudget {
    /// A budget for one response; `None` admits every call.
    #[must_use]
    pub(crate) const fn new(limit: Option<NonZeroU64>) -> Self {
        Self {
            limit,
            dispatched: 0,
            refused: false,
            refusal_shown: false,
        }
    }

    pub(super) fn admit(&mut self) -> Admission {
        match self.limit {
            Some(limit) if self.dispatched >= limit.get() => {
                self.refused = true;
                Admission::Refuse {
                    limit,
                    visible: !self.refusal_shown,
                }
            }
            _ => {
                self.dispatched = self.dispatched.saturating_add(1);
                Admission::Dispatch
            }
        }
    }

    /// Records that a refused call is shown, hiding every later refusal.
    pub(super) fn mark_refusal_shown(&mut self) {
        self.refusal_shown = true;
    }

    /// Whether a call was refused; later rounds stop offering built-in tools.
    #[must_use]
    pub(crate) const fn has_refused(&self) -> bool {
        self.refused
    }
}

/// Tool call output returned to the model for a refused call, as the Responses
/// API phrases it (a `gpt-5.6` reference recording quoted it verbatim).
pub(super) fn limit_reached_message(limit: NonZeroU64) -> String {
    format!("UserError: Reached tool call limit of {limit}")
}

/// The public item for a visible refused call, or `None` to omit it.
///
/// Matches the Responses API: `web_search_call` stops at `searching`,
/// `code_interpreter_call` stops at `interpreting`, and an MCP call is omitted.
pub(super) fn refused_output(started: &OutputItem) -> Option<OutputItem> {
    match started {
        OutputItem::WebSearchCall(call) => Some(OutputItem::WebSearchCall(WebSearchCall {
            status: WebSearchCallStatus::Searching,
            ..call.clone()
        })),
        OutputItem::CodeInterpreterCall(call) => Some(OutputItem::CodeInterpreterCall(CodeInterpreterCall {
            status: CodeInterpreterCallStatus::Interpreting,
            ..call.clone()
        })),
        OutputItem::McpCall(_)
        | OutputItem::McpListTools(_)
        | OutputItem::Message(_)
        | OutputItem::FunctionCall(_)
        | OutputItem::ToolSearchCall(_)
        | OutputItem::CustomToolCall(_)
        | OutputItem::ShellCall(_)
        | OutputItem::Reasoning(_)
        | OutputItem::Compaction(_)
        | OutputItem::MultiAgentCall(_)
        | OutputItem::MultiAgentCallOutput(_)
        | OutputItem::AgentMessage(_)
        | OutputItem::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(value: u64) -> Option<NonZeroU64> {
        NonZeroU64::new(value)
    }

    #[test]
    fn unlimited_budget_dispatches_every_call() {
        let mut budget = BuiltInToolCallBudget::new(None);
        for _ in 0..100 {
            assert_eq!(budget.admit(), Admission::Dispatch);
        }
        assert!(!budget.has_refused());
    }

    #[test]
    fn limit_dispatches_in_order_then_shows_only_one_refusal() {
        let mut budget = BuiltInToolCallBudget::new(limit(2));
        let max = NonZeroU64::new(2).unwrap();
        let refuse = |visible| Admission::Refuse { limit: max, visible };
        assert_eq!(budget.admit(), Admission::Dispatch);
        assert_eq!(budget.admit(), Admission::Dispatch);
        // A refusal without a public item leaves the visible slot for a later one.
        assert_eq!(budget.admit(), refuse(true));
        assert_eq!(budget.admit(), refuse(true));
        budget.mark_refusal_shown();
        assert_eq!(budget.admit(), refuse(false));
        assert!(budget.has_refused());
    }

    #[test]
    fn refused_projection_matches_the_responses_api_per_tool_kind() {
        let search =
            WebSearchCall::try_new("ws_1", WebSearchCallStatus::InProgress, vec!["q".into()], Vec::new()).unwrap();
        let Some(OutputItem::WebSearchCall(refused)) = refused_output(&OutputItem::WebSearchCall(search)) else {
            panic!("web search stays visible");
        };
        assert_eq!(refused.status, WebSearchCallStatus::Searching);
        let mcp: OutputItem = serde_json::from_value(serde_json::json!({
            "type": "mcp_call", "id": "mcp_1", "server_label": "s", "name": "t", "arguments": "{}"
        }))
        .unwrap();
        assert!(refused_output(&mcp).is_none());
        assert_eq!(
            limit_reached_message(NonZeroU64::new(1).unwrap()),
            "UserError: Reached tool call limit of 1"
        );
    }
}
