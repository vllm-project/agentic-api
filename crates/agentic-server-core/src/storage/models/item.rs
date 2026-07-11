//! Conversation history item stored in the database.

use serde_json::Value;
use tracing::warn;

use super::super::pool::{DbPool, DbResult, DbTransaction};
use super::super::types::item::{InOutItem, ItemKind, STORED_ITEM_KIND_KEY};
use crate::types::io::{InputItem, OutputItem};
use crate::utils::common::{deserialize_from_str_opt, utcnow_str};

/// Conversation history item stored in the database.
///
/// Maps to the `items` table and represents a single message/event
/// in a conversation timeline.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Item {
    /// Unique identifier for this item.
    pub id: String,

    /// Item data stored as JSON text.
    /// Deserialized based on context (`message`, `tool_call`, etc.)
    pub data: String,

    /// Creation timestamp as Unix timestamp in seconds.
    pub created_at: i64,

    /// Optional conversation ID for grouping items.
    pub conversation_id: Option<String>,

    /// Optional sequence number within conversation.
    pub seq: Option<i64>,
}

impl Item {
    /// Deserialize data column as `InputItem`.
    #[must_use]
    pub fn as_input(&self) -> Option<InputItem> {
        deserialize_from_str_opt(&self.data)
    }

    /// Deserialize data column as `OutputItem`.
    #[must_use]
    pub fn as_output(&self) -> Option<OutputItem> {
        deserialize_from_str_opt(&self.data)
    }

    /// Deserialize data column as either `InputItem` or `OutputItem`.
    #[must_use]
    pub fn as_inout(&self) -> Option<InOutItem> {
        if let Some(kind) = self.stored_item_kind() {
            match kind {
                ItemKind::Input => {
                    if let Some(input) = self.as_input() {
                        return Some(InOutItem::Input(input));
                    }
                }
                ItemKind::Output => {
                    if let Some(output) = self.as_output() {
                        return Some(InOutItem::Output(output));
                    }
                }
            }
        }

        let output = self.as_output();
        if output.as_ref().is_some_and(|item| !matches!(item, OutputItem::Unknown)) {
            return output.map(InOutItem::Output);
        }

        let input = self.as_input();
        if input.as_ref().is_some_and(|item| !matches!(item, InputItem::Unknown)) {
            return input.map(InOutItem::Input);
        }

        match (input, output) {
            (Some(input), _) => Some(InOutItem::Input(input)),
            (_, Some(output)) => Some(InOutItem::Output(output)),
            _ => {
                warn!(item_id = %self.id, "unrecognized item type in stored data");
                None
            }
        }
    }

    fn stored_item_kind(&self) -> Option<ItemKind> {
        let value = deserialize_from_str_opt::<Value>(&self.data)?;
        ItemKind::from_stored_str(value.get(STORED_ITEM_KIND_KEY)?.as_str()?)
    }
}

/// Create items in a transaction with optional conversation context.
///
/// If `conversation_id` is provided, the next sequence range is computed in the insert statement so
/// concurrent `SQLite` writers do not take a stale read snapshot before writing.
///
/// # Errors
/// Returns `DbResult::Err` if the database insertion fails.
pub async fn create_in_tx(
    tx: &mut DbTransaction<'_>,
    items: Vec<(String, String)>,
    conversation_id: Option<&str>,
) -> DbResult<Vec<Item>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }

    if let Some(conversation_id) = conversation_id {
        return create_in_tx_with_next_conversation_seq(tx, items, conversation_id).await;
    }

    let now = utcnow_str();
    let placeholders: Vec<&str> = vec!["(?, ?, ?, ?, ?)"; items.len()];
    let values_clause = placeholders.join(", ");
    let sql =
        format!("INSERT INTO items (id, data, created_at, conversation_id, seq) VALUES {values_clause} RETURNING *");

    let mut query = sqlx::query_as::<_, Item>(&sql);
    for (id, data) in &items {
        query = query.bind(id).bind(data).bind(now).bind(None::<&str>).bind(None::<i64>);
    }

    query.fetch_all(&mut **tx).await
}

async fn create_in_tx_with_next_conversation_seq(
    tx: &mut DbTransaction<'_>,
    items: Vec<(String, String)>,
    conversation_id: &str,
) -> DbResult<Vec<Item>> {
    let now = utcnow_str();
    let placeholders: Vec<&str> = vec!["(?, ?, ?, ?, (SELECT start + ? FROM next_seq))"; items.len()];
    let values_clause = placeholders.join(", ");
    let sql = format!(
        "WITH next_seq AS ( \
             SELECT COALESCE(MAX(seq), -1) + 1 AS start \
             FROM items \
             WHERE conversation_id = ? \
         ) \
         INSERT INTO items (id, data, created_at, conversation_id, seq) \
         VALUES {values_clause} \
         RETURNING *"
    );

    let mut query = sqlx::query_as::<_, Item>(&sql).bind(conversation_id);
    #[allow(clippy::cast_possible_wrap)]
    for (idx, (id, data)) in items.iter().enumerate() {
        query = query
            .bind(id)
            .bind(data)
            .bind(now)
            .bind(conversation_id)
            .bind(idx as i64);
    }

    query.fetch_all(&mut **tx).await
}

/// Get items by IDs.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get_items(pool: &DbPool, ids: &[String]) -> DbResult<Vec<Item>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!("SELECT * FROM items WHERE id IN ({placeholders})");
    let mut q = sqlx::query_as::<_, Item>(&sql);
    for id in ids {
        q = q.bind(id);
    }
    q.fetch_all(pool).await
}

/// Get items by conversation ID ordered by sequence.
///
/// # Errors
/// Returns `DbResult::Err` if the database query fails.
pub async fn get_items_by_conversation(pool: &DbPool, conversation_id: &str) -> DbResult<Vec<Item>> {
    sqlx::query_as::<_, Item>("SELECT * FROM items WHERE conversation_id = ? ORDER BY seq ASC")
        .bind(conversation_id)
        .fetch_all(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::event::MessageStatus;
    use crate::types::io::{InputItem, OutputItem, ReasoningOutput, ReasoningTextContent};

    #[test]
    fn test_item_basic() {
        let item = Item {
            id: "item_123".to_string(),
            data: r#"{"role":"user","content":"hello"}"#.to_string(),
            created_at: 1_704_067_200,
            conversation_id: Some("conv_456".to_string()),
            seq: Some(1),
        };

        assert_eq!(item.id, "item_123");
        assert_eq!(item.conversation_id, Some("conv_456".to_string()));
        assert_eq!(item.seq, Some(1));
    }

    #[test]
    fn test_item_optional_fields() {
        let item = Item {
            id: "item_789".to_string(),
            data: r#"{"role":"assistant"}"#.to_string(),
            created_at: 1_704_067_200,
            conversation_id: None,
            seq: None,
        };

        assert!(item.conversation_id.is_none());
        assert!(item.seq.is_none());
    }

    #[test]
    fn test_as_inout_uses_stored_kind_for_reasoning_output() {
        let mut reasoning = ReasoningOutput::new("rs_1");
        reasoning.content.push(ReasoningTextContent::new("thinking..."));
        let stored = InOutItem::Output(OutputItem::Reasoning(reasoning));
        let item = Item {
            id: "item_reasoning".to_string(),
            data: String::try_from(&stored).expect("serialization failed"),
            created_at: 1_704_067_200,
            conversation_id: None,
            seq: None,
        };

        assert!(matches!(
            item.as_inout(),
            Some(InOutItem::Output(OutputItem::Reasoning(_)))
        ));
    }

    #[test]
    fn test_legacy_output_message_rehydrates_as_output_before_unknown_input() {
        let item = Item {
            id: "item_message".to_string(),
            data: serde_json::json!({
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "hello", "annotations": []}]
            })
            .to_string(),
            created_at: 1_704_067_200,
            conversation_id: None,
            seq: None,
        };

        let stored = item.as_inout().expect("stored item");
        assert!(matches!(stored, InOutItem::Output(OutputItem::Message(_))));

        let inputs = InOutItem::into_input_items(vec![stored]);
        assert!(matches!(inputs[0], InputItem::Message(_)));
    }

    #[test]
    fn test_namespaced_function_call_rehydrates_without_storage_marker() {
        let stored = InOutItem::Output(OutputItem::FunctionCall(crate::types::io::FunctionToolCall {
            id: "fc_1".to_string(),
            call_id: "call_1".to_string(),
            name: "run".to_string(),
            namespace: Some("mcp__shell".to_string()),
            arguments: "{\"cmd\":\"pwd\"}".to_string(),
            status: MessageStatus::Completed,
        }));
        let item = Item {
            id: "item_function_call".to_string(),
            data: String::try_from(&stored).expect("serialization failed"),
            created_at: 1_704_067_200,
            conversation_id: None,
            seq: None,
        };

        let inputs = InOutItem::into_input_items(vec![item.as_inout().expect("stored item")]);
        let value = serde_json::to_value(&inputs[0]).expect("input value");

        assert_eq!(value["type"], "function_call");
        assert_eq!(value["namespace"], "mcp__shell");
        assert_eq!(value["name"], "run");
        assert!(value.get(STORED_ITEM_KIND_KEY).is_none());

        println!("namespace round-trip: mcp__shell.run -> storage -> input function_call");
        println!("storage marker stripped: _agentic_item_kind absent");
    }

    #[test]
    fn test_multiple_namespaced_function_calls_rehydrate_without_storage_marker() {
        let stored_items = [
            InOutItem::Output(OutputItem::FunctionCall(crate::types::io::FunctionToolCall {
                id: "fc_1".to_string(),
                call_id: "call_1".to_string(),
                name: "run".to_string(),
                namespace: Some("mcp__shell".to_string()),
                arguments: "{\"cmd\":\"pwd\"}".to_string(),
                status: MessageStatus::Completed,
            })),
            InOutItem::Output(OutputItem::FunctionCall(crate::types::io::FunctionToolCall {
                id: "fc_2".to_string(),
                call_id: "call_2".to_string(),
                name: "run".to_string(),
                namespace: Some("mcp__git".to_string()),
                arguments: "{\"args\":[\"status\",\"--short\"]}".to_string(),
                status: MessageStatus::Completed,
            })),
        ];
        let rows: Vec<InOutItem> = stored_items
            .iter()
            .enumerate()
            .map(|(idx, stored)| Item {
                id: format!("item_function_call_{idx}"),
                data: String::try_from(stored).expect("serialization failed"),
                created_at: 1_704_067_200,
                conversation_id: None,
                seq: Some(idx.try_into().expect("seq")),
            })
            .map(|item| item.as_inout().expect("stored item"))
            .collect();

        let inputs = InOutItem::into_input_items(rows);
        let values = serde_json::to_value(&inputs).expect("input values");

        assert_eq!(values[0]["type"], "function_call");
        assert_eq!(values[0]["namespace"], "mcp__shell");
        assert_eq!(values[0]["name"], "run");
        assert_eq!(values[0]["call_id"], "call_1");
        assert!(values[0].get(STORED_ITEM_KIND_KEY).is_none());

        assert_eq!(values[1]["type"], "function_call");
        assert_eq!(values[1]["namespace"], "mcp__git");
        assert_eq!(values[1]["name"], "run");
        assert_eq!(values[1]["call_id"], "call_2");
        assert!(values[1].get(STORED_ITEM_KIND_KEY).is_none());

        println!("namespace round-trip: mcp__shell.run -> call_1");
        println!("namespace round-trip: mcp__git.run -> call_2");
        println!("same tool name preserved under separate namespaces");
    }

    #[test]
    fn test_unknown_rehydrated_items_are_omitted() {
        let stored = InOutItem::Output(OutputItem::Unknown);
        let item = Item {
            id: "item_unknown".to_string(),
            data: String::try_from(&stored).expect("serialization failed"),
            created_at: 1_704_067_200,
            conversation_id: None,
            seq: None,
        };

        let inputs = InOutItem::into_input_items(vec![item.as_inout().expect("stored item")]);

        assert!(inputs.is_empty());
    }
}
