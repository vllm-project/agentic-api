use super::{IdPairs, TestFixture, Turn, Value, remap_ids, response_fields};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::types::io::InOutItem;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

fn supplied_items(input: &Value) -> Vec<Value> {
    match input {
        Value::String(text) => vec![json!({"role": "user", "content": text})],
        Value::Array(items) => items.clone(),
        _ => panic!("unexpected recorded input: {input}"),
    }
}

fn stored_values(items: Vec<InOutItem>) -> Value {
    Value::Array(
        items
            .into_iter()
            .map(|item| match item {
                InOutItem::Input(item) => serde_json::to_value(item).unwrap(),
                InOutItem::Output(item) => serde_json::to_value(item).unwrap(),
            })
            .collect(),
    )
}

// Input messages may be strings or content arrays. Their storage-generated IDs
// are exposed by the management API but are not part of the forwarded InputItem.
// Output IDs, reasoning, item occurrences, and content-part order remain exact.
fn history_shape(items: &Value) -> Value {
    Value::Array(
        items
            .as_array()
            .expect("history array")
            .iter()
            .map(|item| match item["type"].as_str().unwrap_or("message") {
                "message" => {
                    let content = match &item["content"] {
                        Value::String(text) => json!([{"type": "input_text", "text": text}]),
                        Value::Array(parts) => Value::Array(parts.clone()),
                        _ => panic!("unexpected message content: {item}"),
                    };
                    json!({
                        "type": "message",
                        "role": item["role"],
                        "content": content,
                        "id": if item["role"] == "assistant" { item["id"].clone() } else { Value::Null },
                    })
                }
                "reasoning" => json!({
                    "type": "reasoning", "id": item["id"],
                    "content": item["content"], "summary": item["summary"],
                    "encrypted_content": item["encrypted_content"],
                }),
                kind => panic!("unhandled recorded history item: {kind}"),
            })
            .collect(),
    )
}

#[derive(Default)]
pub(super) struct Histories {
    conversations: HashMap<String, Vec<Value>>,
    responses: HashMap<String, Vec<Value>>,
    payloads: HashMap<String, Value>,
}

impl Histories {
    pub(super) fn response(
        &mut self,
        turn: &Turn,
        expected: &Value,
        forwarded_input: &Value,
        ids: &IdPairs,
        location: &str,
    ) {
        let body = &turn.request.body;
        let conversation = body["conversation"].as_str().or(body["conversation"]["id"].as_str());
        let mut history = if let Some(id) = conversation {
            self.conversations.get(id).expect("recorded conversation").clone()
        } else if let Some(id) = body["previous_response_id"].as_str() {
            self.responses.get(id).expect("recorded parent response").clone()
        } else {
            Vec::new()
        };
        history.extend(supplied_items(&body["input"]));
        assert_eq!(
            history_shape(forwarded_input),
            history_shape(&remap_ids(&Value::Array(history.clone()), ids)),
            "{location}: forwarded history"
        );
        history.extend(expected["output"].as_array().unwrap().iter().cloned());
        if let Some(id) = conversation {
            self.conversations.insert(id.to_owned(), history.clone());
        }
        let id = expected["id"].as_str().unwrap();
        self.responses.insert(id.to_owned(), history);
        self.payloads.insert(id.to_owned(), response_fields(expected));
    }

    pub(super) fn management(&mut self, turn: &Turn, expected: &Value) {
        let segments: Vec<_> = turn.request.path.split('/').filter(|part| !part.is_empty()).collect();
        match (turn.request.method.as_str(), segments.len()) {
            ("POST", 2) => {
                assert!(
                    turn.request.body["items"].is_null(),
                    "fixture creates an empty conversation"
                );
                self.conversations
                    .insert(expected["id"].as_str().unwrap().to_owned(), Vec::new());
            }
            ("POST", 4) => self
                .conversations
                .get_mut(segments[2])
                .unwrap()
                .extend(expected["data"].as_array().unwrap().iter().cloned()),
            ("DELETE", 5) => self
                .conversations
                .get_mut(segments[2])
                .unwrap()
                .retain(|item| item["id"] != segments[4]),
            ("GET", 4) => {}
            _ => panic!("unhandled history mutation: {}", turn.request.path),
        }
    }

    pub(super) async fn assert_stored(&self, fixture: &TestFixture, ids: &IdPairs, location: &str) {
        let conversations = ConversationStore::new(Arc::clone(&fixture.pool));
        let responses = ResponseStore::new(Arc::clone(&fixture.pool));
        for (id, expected) in &self.conversations {
            let actual = conversations.rehydrate(&ids.forward[id]).await.unwrap();
            assert_eq!(
                history_shape(&stored_values(actual)),
                history_shape(&remap_ids(&Value::Array(expected.clone()), ids)),
                "{location}: conversation {id}"
            );
        }
        for (id, expected) in &self.responses {
            let actual = responses.rehydrate(&ids.forward[id]).await.unwrap();
            assert_eq!(
                history_shape(&stored_values(actual)),
                history_shape(&remap_ids(&Value::Array(expected.clone()), ids)),
                "{location}: response snapshot {id}"
            );
            let payload = fixture.exec_ctx.resp_handler.retrieve(&ids.forward[id]).await.unwrap();
            assert_eq!(
                response_fields(&serde_json::to_value(payload).unwrap()),
                remap_ids(&self.payloads[id], ids),
                "{location}: retrievable response snapshot {id}"
            );
        }
    }
}
