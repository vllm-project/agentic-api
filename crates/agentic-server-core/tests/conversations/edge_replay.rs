//! Replay every captured management request against a fresh gateway store.

use super::{IdPairs, Turn, Value, cassette, compare_value, remap_error_message};
use agentic_core::executor::{ConversationHandler, ExecutorError};
use agentic_core::storage::{ConversationData, ConversationStore, create_pool_with_schema};
use agentic_core::types::conversations::{ConversationResponse, CreateConversationRequest, CreateItemRequest};
use agentic_core::types::io::{ItemLimit, ItemOrder, ItemPage, ListItemsResponse};
use serde_json::json;

pub(super) use crate::support::replay::remap_ids;

fn conversation_body(data: ConversationData) -> Value {
    serde_json::to_value(ConversationResponse::new(
        data.conversation_id,
        data.created_at,
        data.metadata,
    ))
    .unwrap()
}

pub(super) async fn execute_step(
    handler: &ConversationHandler,
    turn: &Turn,
    ids: &IdPairs,
) -> Result<Value, Box<ExecutorError>> {
    let segments: Vec<_> = turn.request.path.split('/').filter(|part| !part.is_empty()).collect();
    let body = remap_ids(&turn.request.body, ids);
    let query = remap_ids(&turn.request.query_params, ids);
    let conversation = segments
        .get(2)
        .map(|id| ids.forward.get(*id).expect("known conversation").as_str());
    match (turn.request.method.as_str(), segments.len()) {
        ("POST", 2) => {
            let request: CreateConversationRequest = serde_json::from_value(body).unwrap();
            let data = handler
                .create(
                    "default_tenant",
                    &request.metadata.unwrap_or_default(),
                    &request.items.unwrap_or_default(),
                )
                .await?;
            Ok(conversation_body(data))
        }
        ("POST", 4) => {
            let request: CreateItemRequest = serde_json::from_value(body).unwrap();
            let items = handler
                .create_items("default_tenant", conversation.unwrap(), request.items)
                .await?;
            Ok(serde_json::to_value(ListItemsResponse::new(items, false)).unwrap())
        }
        ("GET", 4) => {
            let page = ItemPage {
                limit: query["limit"].as_str().map_or_else(ItemLimit::default, |limit| {
                    ItemLimit::try_from(limit.parse::<i64>().unwrap()).unwrap()
                }),
                after: query["after"].as_str(),
                order: query["order"].as_str().map_or_else(ItemOrder::default, |order| {
                    serde_json::from_value(Value::from(order)).unwrap()
                }),
            };
            let items = handler
                .list_items("default_tenant", conversation.unwrap(), page)
                .await?;
            Ok(serde_json::to_value(items).unwrap())
        }
        ("DELETE", 5) => {
            let item = ids.forward.get(segments[4]).expect("known item");
            Ok(conversation_body(
                handler
                    .delete_item("default_tenant", conversation.unwrap(), item)
                    .await?,
            ))
        }
        _ => panic!(
            "unhandled recorded request: {} {}",
            turn.request.method, turn.request.path
        ),
    }
}

#[tokio::test]
async fn gateway_matches_openai_item_status_codes_and_bodies() {
    // `OpenAI` stores tool items with fields the gateway omits (an `in_progress` status, null shell limits), so the
    // prefix scenario compares the stored items' identity and kind rather than their full bodies.
    for (scenario, turns, full_bodies) in [("edge-cases", 19, true), ("item-id-prefixes", 18, false)] {
        replay_openai_recording(scenario, turns, full_bodies).await;
    }
}

async fn replay_openai_recording(scenario: &str, turns: usize, full_bodies: bool) {
    let reference = cassette(scenario, "openai").expect("OpenAI recording");
    assert_eq!(reference.turns.len(), turns);
    let store = ConversationStore::new(create_pool_with_schema(Some("sqlite::memory:")).await.unwrap());
    let handler = ConversationHandler::new(store);
    let mut ids = IdPairs::default();
    for turn in &reference.turns {
        let (status, mut actual) = match execute_step(&handler, turn, &ids).await {
            Ok(body) => (200, body),
            Err(error) => (
                error.http_status().as_u16(),
                serde_json::from_slice(&(*error).into_response_body()).unwrap(),
            ),
        };
        assert_eq!(
            status, turn.response.status_code,
            "{scenario} {}: {actual}",
            turn.filename
        );
        let mut expected = turn.response.body.clone().unwrap();
        remap_error_message(&mut expected, &ids);
        if !full_bodies && let Some(items) = expected["data"].as_array() {
            let identity = |item: &Value| json!({"id": item["id"], "type": item["type"], "call_id": item["call_id"]});
            expected = items.iter().map(identity).collect();
            actual = actual["data"].as_array().unwrap().iter().map(identity).collect();
        }
        compare_value(&expected, &actual, &turn.filename, &mut ids, false);
    }
}
