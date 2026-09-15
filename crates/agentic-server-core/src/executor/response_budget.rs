//! Cumulative retained-byte budget for one Responses request.
//!
//! Two owners share this module. [`ExecutorResponseBudget`] is the per-request
//! ceiling, charged by synchronous ingestion for retained output items and by
//! the gateway for tool outputs. [`RetainedSize`] is the single measurement of
//! how many bytes an output item keeps in memory; ingestion charges growth
//! incrementally and reconciles against this measurement at completion, so a
//! new variable-sized field is added in exactly one place.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

use crate::executor::error::{ExecutorError, ExecutorResult, ResourceLimit};
use crate::types::io::output::{McpListTool, McpListTools, McpToolExecutionError, ReasoningTextContent};
use crate::types::io::{
    CompactionItem, CustomToolCall, FunctionToolCall, McpCall, McpCallError, OutputItem, OutputMessage,
    OutputTextContent, ReasoningOutput, ShellCall, ToolSearchCall, WebSearchAction, WebSearchCall,
};
#[cfg(test)]
use crate::types::request_response::ResponsePayload;

#[cfg(test)]
pub use crate::config::DEFAULT_MAX_RETAINED_RESPONSE_BYTES;
#[cfg(test)]
pub(super) const MAX_EXECUTOR_RESPONSE_BYTES: usize = DEFAULT_MAX_RETAINED_RESPONSE_BYTES;

/// Fixed charge for every retained container: an output item, a content part,
/// a shell command, a JSON array or object. It bounds structural growth that
/// carries no text, such as an empty content part.
pub(super) const RETAINED_CONTAINER_OVERHEAD_BYTES: usize = 32;

#[derive(Clone, Debug)]
pub(super) struct ExecutorResponseBudget {
    limit: usize,
    used: Arc<AtomicUsize>,
}

impl ExecutorResponseBudget {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::with_limit(MAX_EXECUTOR_RESPONSE_BYTES)
    }

    pub(super) fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            used: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[cfg(test)]
    pub(super) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    pub(super) fn consume(&self, bytes: usize) -> ExecutorResult<()> {
        let limit = self.limit;
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                let next = used.checked_add(bytes)?;
                if next > limit { None } else { Some(next) }
            })
            .map(|_| ())
            .map_err(|_| ExecutorError::ResourceLimitExceeded {
                limit: ResourceLimit::ResponseBudget,
                max_bytes: limit,
            })
    }
}

/// Bytes accounted so far for one retained slot, and the only way to change them.
///
/// Every lifecycle operation uses one of three moves:
/// - [`charge`](Self::charge) for growth known before it happens (a delta, a new
///   container), checked against the budget before the state grows;
/// - [`grow`](Self::grow) for a mutation whose retained growth is only known by
///   measuring the affected state before and after, so accounting observes the
///   same completion policy that mutates the item instead of re-deriving it;
/// - [`reconcile`](Self::reconcile) for a comprehensive measurement of the
///   completed item, charging only what incremental accounting has not yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct RetainedAccount {
    accounted: usize,
}

impl RetainedAccount {
    #[cfg(test)]
    pub(super) fn accounted(self) -> usize {
        self.accounted
    }

    /// Charge growth that is known before the retained state grows.
    pub(super) fn charge(&mut self, budget: Option<&ExecutorResponseBudget>, bytes: usize) -> ExecutorResult<()> {
        if bytes == 0 {
            return Ok(());
        }
        if let Some(budget) = budget {
            budget.consume(bytes)?;
        }
        self.accounted = self.accounted.saturating_add(bytes);
        Ok(())
    }

    /// Apply `mutate` and charge the measured growth of `state`.
    ///
    /// `measure` must cover every field `mutate` can grow; measuring a superset
    /// is harmless. A rejected charge fails the round, so the state that grew
    /// past the budget is dropped with it and the overshoot is bounded by the
    /// single payload that caused it.
    pub(super) fn grow<T>(
        &mut self,
        budget: Option<&ExecutorResponseBudget>,
        state: &mut T,
        measure: impl Fn(&T) -> usize,
        mutate: impl FnOnce(&mut T),
    ) -> ExecutorResult<()> {
        let before = measure(state);
        mutate(state);
        let after = measure(state);
        self.charge(budget, after.saturating_sub(before))
    }

    /// Bring the account up to a comprehensive measurement of the retained item.
    ///
    /// Incremental accounting may exceed the measurement (a discarded empty
    /// part, a streamed buffer superseded by its completion); it never refunds.
    pub(super) fn reconcile(&mut self, budget: Option<&ExecutorResponseBudget>, measured: usize) -> ExecutorResult<()> {
        self.charge(budget, measured.saturating_sub(self.accounted))
    }
}

/// Bytes an item keeps in executor memory once retained.
///
/// Counts every variable-sized field (identifiers, text, arguments, nested
/// JSON) plus [`RETAINED_CONTAINER_OVERHEAD_BYTES`] per container. Fixed-vocabulary
/// fields (`type`, `role`, `status`) are not counted: they are bounded by the
/// enum they encode and would make in-progress and completed snapshots of the
/// same item measure differently.
pub(in crate::executor) trait RetainedSize {
    fn retained_bytes(&self) -> usize;
}

fn opt_len(value: Option<&String>) -> usize {
    value.map_or(0, String::len)
}

fn sum_retained<'a, T: RetainedSize + 'a>(items: impl IntoIterator<Item = &'a T>) -> usize {
    items.into_iter().map(RetainedSize::retained_bytes).sum()
}

impl RetainedSize for Value {
    /// Approximates the serialized footprint of arbitrary JSON without serializing it.
    fn retained_bytes(&self) -> usize {
        match self {
            Value::Null => 4,
            Value::Bool(_) => 5,
            Value::Number(_) => 8,
            Value::String(text) => text.len(),
            Value::Array(items) => RETAINED_CONTAINER_OVERHEAD_BYTES + sum_retained(items),
            Value::Object(map) => {
                RETAINED_CONTAINER_OVERHEAD_BYTES
                    + map
                        .iter()
                        .map(|(key, value)| key.len() + value.retained_bytes())
                        .sum::<usize>()
            }
        }
    }
}

impl<T: RetainedSize> RetainedSize for Option<T> {
    fn retained_bytes(&self) -> usize {
        self.as_ref().map_or(0, RetainedSize::retained_bytes)
    }
}

impl RetainedSize for OutputTextContent {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES + self.text.len() + sum_retained(&self.annotations)
    }
}

impl RetainedSize for OutputMessage {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES + self.id.len() + sum_retained(&self.content)
    }
}

impl RetainedSize for FunctionToolCall {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + self.id.len()
            + self.call_id.len()
            + self.name.len()
            + opt_len(self.namespace.as_ref())
            + self.arguments.len()
    }
}

impl RetainedSize for CustomToolCall {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES + self.id.len() + self.call_id.len() + self.name.len() + self.input.len()
    }
}

impl RetainedSize for ShellCall {
    fn retained_bytes(&self) -> usize {
        let commands = self
            .action
            .commands
            .iter()
            .map(|command| RETAINED_CONTAINER_OVERHEAD_BYTES + command.len())
            .sum::<usize>();
        let extras = self
            .extra
            .iter()
            .chain(self.action.extra.iter())
            .map(|(key, value)| key.len() + value.retained_bytes())
            .sum::<usize>();
        RETAINED_CONTAINER_OVERHEAD_BYTES + opt_len(self.id.as_ref()) + self.call_id.len() + commands + extras
    }
}

impl RetainedSize for ReasoningTextContent {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES + self.text.len()
    }
}

impl RetainedSize for ReasoningOutput {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + self.id.len()
            + self.encrypted_content.retained_bytes()
            + sum_retained(&self.content)
            + sum_retained(&self.summary)
    }
}

impl RetainedSize for ToolSearchCall {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES + self.id.len() + self.call_id.len() + self.arguments.retained_bytes()
    }
}

impl RetainedSize for WebSearchAction {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Search(search) => {
                search.query.len()
                    + search.queries.iter().map(String::len).sum::<usize>()
                    + search
                        .sources
                        .iter()
                        .map(|source| {
                            RETAINED_CONTAINER_OVERHEAD_BYTES + source.url.len() + opt_len(source.title.as_ref())
                        })
                        .sum::<usize>()
            }
            Self::OpenPage(open) => opt_len(open.url.as_ref()),
            Self::FindInPage(find) => find.pattern.len() + find.url.len(),
        }
    }
}

impl RetainedSize for WebSearchCall {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES + self.id.len() + self.action.retained_bytes()
    }
}

impl RetainedSize for McpToolExecutionError {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + self
                .content
                .iter()
                .map(|content| {
                    RETAINED_CONTAINER_OVERHEAD_BYTES
                        + content.text.len()
                        + content.annotations.retained_bytes()
                        + content.meta.retained_bytes()
                })
                .sum::<usize>()
    }
}

impl RetainedSize for McpCallError {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::ToolExecution(error) => error.retained_bytes(),
            Self::Unknown(value) => value.retained_bytes(),
        }
    }
}

impl RetainedSize for McpCall {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + self.id.len()
            + self.server_label.len()
            + self.name.len()
            + self.arguments.len()
            + opt_len(self.approval_request_id.as_ref())
            + opt_len(self.output.as_ref())
            + self.error.retained_bytes()
    }
}

impl RetainedSize for McpListTool {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + self.name.len()
            + opt_len(self.description.as_ref())
            + self.input_schema.retained_bytes()
            + self.annotations.retained_bytes()
    }
}

impl RetainedSize for McpListTools {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + self.id.len()
            + self.server_label.len()
            + opt_len(self.error.as_ref())
            + sum_retained(&self.tools)
    }
}

impl RetainedSize for CompactionItem {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES + opt_len(self.id.as_ref()) + self.encrypted_content.len()
    }
}

impl RetainedSize for OutputItem {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Message(item) => item.retained_bytes(),
            Self::FunctionCall(item) => item.retained_bytes(),
            Self::CustomToolCall(item) => item.retained_bytes(),
            Self::ShellCall(item) => item.retained_bytes(),
            Self::Reasoning(item) => item.retained_bytes(),
            Self::ToolSearchCall(item) => item.retained_bytes(),
            Self::WebSearchCall(item) => item.retained_bytes(),
            Self::McpCall(item) => item.retained_bytes(),
            Self::McpListTools(item) => item.retained_bytes(),
            Self::Compaction(item) => item.retained_bytes(),
            Self::Unknown => RETAINED_CONTAINER_OVERHEAD_BYTES,
        }
    }
}

#[cfg(test)]
pub(in crate::executor) fn retained_output_item_bytes(item: &OutputItem) -> usize {
    item.retained_bytes()
}

pub(in crate::executor) fn retained_response_parts_bytes(response_id: &str, output: &[OutputItem]) -> usize {
    RETAINED_CONTAINER_OVERHEAD_BYTES + response_id.len() + sum_retained(output)
}

#[cfg(test)]
pub(in crate::executor) fn retained_response_bytes(response: &ResponsePayload) -> usize {
    retained_response_parts_bytes(&response.id, &response.output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::io::output::{
        McpListTool, McpListTools, ReasoningOutput, ReasoningTextContent, WebSearchActionOpenPage,
        WebSearchActionSearch, WebSearchCall, WebSearchCallStatus,
    };
    use crate::types::io::{McpCall, McpCallStatus};

    #[test]
    fn retained_response_bytes_accounts_for_id_and_items() {
        let payload = ResponsePayload {
            id: "resp_test".to_owned(),
            object: "response".to_owned(),
            created_at: 1000,
            model: "test".to_owned(),
            status: "completed".to_owned(),
            output: vec![OutputItem::Unknown],
            usage: None,
            incomplete_details: None,
            error: None,
            previous_response_id: None,
            conversation_id: None,
            instructions: None,
            tools: None,
            tool_choice: None,
        };
        let expected = RETAINED_CONTAINER_OVERHEAD_BYTES + "resp_test".len() + RETAINED_CONTAINER_OVERHEAD_BYTES;
        assert_eq!(retained_response_bytes(&payload), expected);
        assert_eq!(retained_response_parts_bytes(&payload.id, &payload.output), expected);
    }

    #[test]
    fn retained_accounting_for_web_search_mcp_and_reasoning() {
        let ws_search = OutputItem::WebSearchCall(WebSearchCall {
            id: "ws_1".to_owned(),
            status: WebSearchCallStatus::Completed,
            action: WebSearchAction::Search(
                WebSearchActionSearch::try_new(vec!["query1".to_owned(), "q2".to_owned()], vec![]).unwrap(),
            ),
        });
        // `query` mirrors the first entry of `queries` and is retained separately.
        assert_eq!(
            retained_output_item_bytes(&ws_search),
            RETAINED_CONTAINER_OVERHEAD_BYTES + "ws_1".len() + "query1".len() + ("query1".len() + "q2".len())
        );

        let ws_open = OutputItem::WebSearchCall(WebSearchCall {
            id: "ws_2".to_owned(),
            status: WebSearchCallStatus::Completed,
            action: WebSearchAction::OpenPage(WebSearchActionOpenPage {
                url: Some("https://example.com".to_owned()),
            }),
        });
        assert_eq!(
            retained_output_item_bytes(&ws_open),
            RETAINED_CONTAINER_OVERHEAD_BYTES + "ws_2".len() + "https://example.com".len()
        );

        let mcp_call = OutputItem::McpCall(McpCall {
            id: "mcp_1".to_owned(),
            server_label: "srv".to_owned(),
            name: "tool1".to_owned(),
            arguments: "{}".to_owned(),
            status: Some(McpCallStatus::Completed),
            approval_request_id: None,
            output: Some("result_output".to_owned()),
            error: Some(McpCallError::Text("error_msg".to_owned())),
        });
        assert_eq!(
            retained_output_item_bytes(&mcp_call),
            RETAINED_CONTAINER_OVERHEAD_BYTES
                + "mcp_1".len()
                + "srv".len()
                + "tool1".len()
                + "{}".len()
                + "result_output".len()
                + "error_msg".len()
        );

        let mcp_list = OutputItem::McpListTools(McpListTools {
            id: "list_1".to_owned(),
            server_label: "srv".to_owned(),
            tools: vec![McpListTool {
                name: "test_tool".to_owned(),
                description: Some("a tool".to_owned()),
                input_schema: serde_json::json!({"type": "object", "prop": "val"}),
                annotations: None,
            }],
            error: Some("list_err".to_owned()),
        });
        assert!(retained_output_item_bytes(&mcp_list) > RETAINED_CONTAINER_OVERHEAD_BYTES + "list_1".len());

        let reasoning = OutputItem::Reasoning(ReasoningOutput {
            id: "rs_1".to_owned(),
            status: Some("completed".to_owned()),
            content: vec![ReasoningTextContent::new("thought")],
            summary: vec![],
            encrypted_content: Some(Value::String("encrypted_blob".to_owned())),
        });
        assert_eq!(
            retained_output_item_bytes(&reasoning),
            RETAINED_CONTAINER_OVERHEAD_BYTES
                + "rs_1".len()
                + "encrypted_blob".len()
                + RETAINED_CONTAINER_OVERHEAD_BYTES
                + "thought".len()
        );
    }

    #[test]
    fn retained_accounting_covers_annotations_compaction_and_nested_arguments() {
        let annotation = serde_json::json!({"type": "url_citation", "url": "https://example.com", "title": "x"});
        let mut part = OutputTextContent::new("body");
        part.annotations = vec![annotation.clone()];
        let mut message = OutputMessage::new("msg_1", crate::types::event::MessageStatus::Completed);
        message.content = vec![part];
        assert_eq!(
            retained_output_item_bytes(&OutputItem::Message(message)),
            RETAINED_CONTAINER_OVERHEAD_BYTES
                + "msg_1".len()
                + RETAINED_CONTAINER_OVERHEAD_BYTES
                + "body".len()
                + annotation.retained_bytes()
        );
        assert!(annotation.retained_bytes() >= "url_citation".len() + "https://example.com".len());

        let compaction = OutputItem::Compaction(CompactionItem {
            id: Some("cmp_1".to_owned()),
            encrypted_content: "x".repeat(1000),
        });
        assert_eq!(
            retained_output_item_bytes(&compaction),
            RETAINED_CONTAINER_OVERHEAD_BYTES + "cmp_1".len() + 1000
        );

        let nested = serde_json::json!({"query": {"terms": ["a".repeat(100), "b".repeat(200)]}});
        let tool_search = OutputItem::ToolSearchCall(ToolSearchCall {
            id: "ts_1".to_owned(),
            call_id: "call_1".to_owned(),
            execution: crate::types::tools::ToolSearchExecution::Client,
            arguments: nested.clone(),
            status: crate::types::tools::ToolSearchStatus::Completed,
        });
        assert!(nested.retained_bytes() >= 300);
        assert_eq!(
            retained_output_item_bytes(&tool_search),
            RETAINED_CONTAINER_OVERHEAD_BYTES + "ts_1".len() + "call_1".len() + nested.retained_bytes()
        );
    }

    #[test]
    fn retained_account_charges_grows_and_reconciles_once() {
        let budget = ExecutorResponseBudget::with_limit(100);
        let mut account = RetainedAccount::default();
        account.charge(Some(&budget), 10).unwrap();
        assert_eq!(account.accounted(), 10);

        // Growth is the measured difference of one mutation, never the payload size.
        let mut text = String::from("abc");
        account
            .grow(Some(&budget), &mut text, String::len, |text| text.push_str("de"))
            .unwrap();
        assert_eq!(account.accounted(), 12);
        account
            .grow(Some(&budget), &mut text, String::len, |text| *text = "ab".to_owned())
            .unwrap();
        assert_eq!(account.accounted(), 12, "shrinking retains the prior charge");

        // Reconciliation charges only the unaccounted remainder, then is idempotent.
        account.reconcile(Some(&budget), 40).unwrap();
        assert_eq!(account.accounted(), 40);
        account.reconcile(Some(&budget), 40).unwrap();
        account.reconcile(Some(&budget), 30).unwrap();
        assert_eq!(account.accounted(), 40);
        assert_eq!(budget.used(), 40);

        let error = account
            .charge(Some(&budget), 61)
            .expect_err("budget ceiling is enforced");
        assert!(matches!(
            error,
            ExecutorError::ResourceLimitExceeded {
                limit: ResourceLimit::ResponseBudget,
                max_bytes: 100
            }
        ));
        assert_eq!(
            account.accounted(),
            40,
            "a rejected charge leaves the account unchanged"
        );
    }
}
