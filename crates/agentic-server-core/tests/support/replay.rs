//! Compare recorded and executed public values while preserving ID relationships.

use serde_json::Value;
use std::collections::HashMap;

#[derive(Default)]
pub struct IdPairs {
    pub forward: HashMap<String, String>,
    reverse: HashMap<String, String>,
}

impl IdPairs {
    pub fn compare(&mut self, a: &str, b: &str, location: &str) {
        if let Some(previous) = self.forward.insert(a.to_owned(), b.to_owned()) {
            assert_eq!(previous, b, "{location}: one OpenAI ID mapped to two gateway IDs");
        }
        if let Some(previous) = self.reverse.insert(b.to_owned(), a.to_owned()) {
            assert_eq!(previous, a, "{location}: two OpenAI IDs mapped to one gateway ID");
        }
    }
}

pub fn remap_error_message(body: &mut Value, ids: &IdPairs) {
    if let Some(message) = body["error"]["message"].as_str() {
        let message = ids
            .forward
            .iter()
            .fold(message.to_owned(), |message, (recorded, actual)| {
                message.replace(&format!("'{recorded}'"), &format!("'{actual}'"))
            });
        body["error"]["message"] = Value::String(message);
    }
}

fn is_id_field(key: &str) -> bool {
    matches!(
        key,
        "id" | "conversation"
            | "conversation_id"
            | "previous_response_id"
            | "first_id"
            | "last_id"
            | "after"
            | "call_id"
    )
}

pub fn compare_value(a: &Value, b: &Value, path: &str, ids: &mut IdPairs, generated_text: bool) {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(a.len(), b.len(), "{path}: field count differs");
            for (key, av) in a {
                let bv = b.get(key).unwrap_or_else(|| panic!("{path}: missing field {key}"));
                if key == "model" || key == "created_at" || key == "updated_at" || key == "usage" {
                    assert_eq!(
                        std::mem::discriminant(av),
                        std::mem::discriminant(bv),
                        "{path}.{key}: type differs"
                    );
                } else if is_id_field(key) && av.is_string() && bv.is_string() {
                    ids.compare(av.as_str().unwrap(), bv.as_str().unwrap(), &format!("{path}.{key}"));
                } else if generated_text && key == "text" && av.is_string() && bv.is_string() {
                    // Model wording varies. History visibility is asserted separately.
                } else {
                    compare_value(av, bv, &format!("{path}.{key}"), ids, generated_text);
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{path}: item count differs");
            for (i, (av, bv)) in a.iter().zip(b).enumerate() {
                compare_value(av, bv, &format!("{path}[{i}]"), ids, generated_text);
            }
        }
        _ => assert_eq!(a, b, "{path}: values differ"),
    }
}

pub fn remap_ids(value: &Value, ids: &IdPairs) -> Value {
    match value {
        Value::String(value) => Value::String(ids.forward.get(value).unwrap_or(value).clone()),
        Value::Array(values) => Value::Array(values.iter().map(|value| remap_ids(value, ids)).collect()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), remap_ids(value, ids)))
                .collect(),
        ),
        value => value.clone(),
    }
}
