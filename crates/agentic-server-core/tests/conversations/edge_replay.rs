//! Replay every captured management request against a fresh gateway store.

use super::{IdPairs, Turn, Value, cassette, compare_value, remap_error_message};
use agentic_core::executor::{ConversationHandler, ExecutorError};
use agentic_core::storage::{ConversationData, ConversationStore, create_pool_with_schema};
use agentic_core::types::conversations::{
    ConversationResponse, CreateConversationRequest, CreateItemRequest, ListItemsResponse,
};

fn remap_ids(value: &Value, ids: &IdPairs) -> Value {
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

fn conversation_body(data: ConversationData) -> Value {
    serde_json::to_value(ConversationResponse::new(
        data.conversation_id,
        data.created_at,
        data.metadata.map(|metadata| serde_json::from_str(&metadata).unwrap()),
    ))
    .unwrap()
}

async fn execute_step(handler: &ConversationHandler, turn: &Turn, ids: &IdPairs) -> Result<Value, Box<ExecutorError>> {
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
                .create_with_metadata_and_items("default_tenant", request.metadata, request.items.unwrap_or_default())
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
            let limit = query["limit"].as_str().map_or(20, |limit| limit.parse().unwrap());
            let items = handler
                .list_items(
                    "default_tenant",
                    conversation.unwrap(),
                    limit,
                    query["after"].as_str(),
                    query["order"].as_str().unwrap_or("desc"),
                )
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
async fn gateway_matches_all_openai_edge_case_status_codes_and_bodies() {
    let reference = cassette("edge-cases", "openai").expect("OpenAI recording");
    assert_eq!(reference.turns.len(), 19);
    let store = ConversationStore::new(create_pool_with_schema(Some("sqlite::memory:")).await.unwrap());
    let handler = ConversationHandler::new(store);
    let mut ids = IdPairs::default();
    for turn in &reference.turns {
        let (status, actual) = match execute_step(&handler, turn, &ids).await {
            Ok(body) => (200, body),
            Err(error) => (
                error.http_status().as_u16(),
                serde_json::from_slice(&(*error).into_response_body()).unwrap(),
            ),
        };
        assert_eq!(status, turn.response.status_code, "{}: {actual}", turn.filename);
        let mut expected = turn.response.body.clone().unwrap();
        remap_error_message(&mut expected, &ids);
        compare_value(&expected, &actual, &turn.filename, &mut ids, false);
    }
}
