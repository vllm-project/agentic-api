//! Public history, continuation metadata, and SQL budgets through the same typed store APIs on both databases.

mod support;

use std::fmt::Debug;
use std::sync::{Arc, Mutex};

use agentic_core::config::{PostgresConfig, SqliteConfig};
use agentic_core::executor::{ConversationHandler, ExecutionContext, ResponseHandler, execute};
use agentic_core::storage::{
    ConversationStore, DbPool, ResponseMetadata, ResponseStore, create_pool_with_schema_and_configs,
};
use agentic_core::types::conversations::ConversationMetadata;
use agentic_core::types::io::{InOutItem, InputItem, OutputItem, ResponsesInput};
use agentic_core::types::request_response::{RequestPayload, ResponsePayload};
use serde_json::{Value, json};
use support::{MockServer, make_request, text_response, unwrap_blocking};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, Registry};

#[derive(Clone, Default)]
struct Queries(Arc<Mutex<Vec<String>>>);

impl<S: Subscriber> Layer<S> for Queries {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        if event.metadata().target() == "sqlx::query" {
            event.record(&mut self.clone());
        }
    }
}

impl Visit for Queries {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if field.name() == "summary" {
            self.0.lock().unwrap().push(format!("{value:?}"));
        }
    }
}

impl Queries {
    fn reset(&self) {
        self.0.lock().unwrap().clear();
    }

    fn budget(&self, operation: &str, maximum: usize) {
        let queries = std::mem::take(&mut *self.0.lock().unwrap());
        let statements: Vec<_> = queries
            .iter()
            .filter(|query| {
                let sql = query.trim_matches('"').to_ascii_uppercase();
                sql != "SELECT 1"
                    && sql != "SELECT 1;"
                    && !["BEGIN", "COMMIT", "ROLLBACK", "PRAGMA"]
                        .iter()
                        .any(|prefix| sql.starts_with(prefix))
            })
            .collect();
        assert!(
            (1..=maximum).contains(&statements.len()),
            "{operation}: expected 1..={maximum} SQL statements, observed {statements:?}"
        );
    }
}

fn user(id: &str) -> InOutItem {
    InOutItem::Input(
        serde_json::from_value(json!({
            "type": "message", "id": id, "role": "user", "status": "completed",
        "content": [{"type": "input_text", "text": id}]
        }))
        .unwrap(),
    )
}

// Expected public tool records come from unchanged OpenAI recordings, rather than gateway-generated expectations.
fn reference_outputs() -> Vec<OutputItem> {
    let recording: Value =
        serde_yaml::from_str(include_str!("cassettes/responses/response-tool-items-openai.yaml")).unwrap();
    ["web_search_call", "mcp_call", "code_interpreter_call"]
        .into_iter()
        .map(|kind| {
            let item = recording["turns"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|turn| turn["response"]["body"]["output"].as_array().into_iter().flatten())
                .find(|item| item["type"] == kind)
                .unwrap_or_else(|| panic!("OpenAI recording must cover {kind}"));
            serde_json::from_value(item.clone()).unwrap()
        })
        .collect()
}

fn snapshot(id: &str, output: &[OutputItem]) -> ResponsePayload {
    serde_json::from_value(json!({
        "id": id, "object": "response", "created_at": 0, "model": "test", "status": "completed", "output": output
    }))
    .unwrap()
}

async fn assert_rows(pool: &DbPool, ids: &[String], expected: &[InOutItem]) {
    assert_eq!(ids.len(), expected.len());
    for (id, item) in ids.iter().zip(expected) {
        let (data, metadata, public_id): (String, String, String) =
            sqlx::query_as("SELECT data, metadata, public_id FROM items WHERE id = $1")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&data).unwrap(),
            serde_json::to_value(item).unwrap()
        );
        assert_eq!(public_id, item.id().unwrap());
        assert_eq!(
            serde_json::from_str::<Value>(&metadata).unwrap(),
            json!({
                "kind": match item { InOutItem::Input(_) => "input", InOutItem::Output(_) => "output" }
            })
        );
    }
}

async fn response_contract(
    pool: &Arc<DbPool>,
    queries: &Queries,
    history: &[InOutItem],
    outputs: &[OutputItem],
    metadata: &ResponseMetadata,
) {
    let responses = ResponseStore::new(Arc::clone(pool));
    let parent = format!("resp_contract_{}", uuid::Uuid::now_v7());
    let payload = snapshot(&parent, outputs);

    queries.reset();
    responses
        .persist(&parent, None, history.to_vec(), Some(&payload), metadata)
        .await
        .unwrap();
    queries.budget("persist first response", 2);
    queries.reset();
    let (stored, restored) = responses.rehydrate(&parent).await.unwrap();
    queries.budget("response rehydration", 2);
    assert_eq!(restored, history);
    assert_eq!(stored.metadata.effective_instructions.as_deref(), Some("keep this"));
    assert_rows(pool, &stored.history_item_ids, history).await;
    assert_eq!(
        responses.get_input_items(&parent).await.unwrap().unwrap(),
        vec![user("msg_first")]
    );
    let (stored_metadata, stored_snapshot): (String, String) =
        sqlx::query_as("SELECT metadata, snapshot FROM responses WHERE id = $1")
            .bind(&parent)
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&stored_metadata).unwrap(),
        serde_json::to_value(metadata).unwrap()
    );
    assert_eq!(
        serde_json::from_str::<Value>(&stored_snapshot).unwrap(),
        serde_json::to_value(&payload).unwrap()
    );

    // The model projection may lower public tool records, but persistence and retrieval retain the originals.
    let request = RequestPayload {
        model: "test".into(),
        input: ResponsesInput::from(InOutItem::into_input_items(restored)),
        ..Default::default()
    };
    let model_items = Vec::<InputItem>::from(request.to_upstream_request(false).unwrap().input.into_owned());
    for output in outputs {
        assert!(
            model_items.iter().any(
                |item| matches!(item, InputItem::FunctionCall(call) if Some(call.call_id.as_str()) == output.id())
            )
        );
    }
    assert_eq!(responses.rehydrate(&parent).await.unwrap().1, history);

    let child = format!("resp_contract_{}", uuid::Uuid::now_v7());
    queries.reset();
    responses
        .persist(
            &child,
            Some(&parent),
            vec![user("msg_second")],
            Some(&snapshot(&child, &[])),
            metadata,
        )
        .await
        .unwrap();
    queries.budget("persist continued response", 3);
    let mut continued = history.to_vec();
    continued.push(user("msg_second"));
    assert_eq!(responses.get_input_items(&child).await.unwrap().unwrap(), continued);
    assert_eq!(responses.rehydrate(&child).await.unwrap().1, continued);

    // Retrieval snapshots must stay out of the continuation decoder, even when malformed or large.
    sqlx::query("UPDATE responses SET snapshot = $1 WHERE id = $2")
        .bind("invalid snapshot JSON")
        .bind(&parent)
        .execute(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(responses.rehydrate(&parent).await.unwrap().1, history);
    assert!(responses.get_snapshot(&parent).await.is_err());
}

async fn conversation_contract(
    pool: &Arc<DbPool>,
    queries: &Queries,
    history: &[InOutItem],
    outputs: &[OutputItem],
    metadata: &ResponseMetadata,
    postgres: bool,
) {
    let responses = ResponseStore::new(Arc::clone(pool));
    let conversations = ConversationStore::new(Arc::clone(pool));
    let payload = snapshot("resp_conversation", outputs);
    let mut continued = history.to_vec();
    continued.push(user("msg_second"));
    let conversation = conversations
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .unwrap();
    queries.reset();
    let checkpoint = conversations
        .rehydrate(&conversation.conversation_id)
        .await
        .unwrap()
        .checkpoint;
    queries.budget("empty conversation rehydration", 1);
    let conv_response = format!("resp_contract_{}", uuid::Uuid::now_v7());
    queries.reset();
    conversations
        .persist(
            &conversation.conversation_id,
            &checkpoint,
            &conv_response,
            history.to_vec(),
            Some(&payload),
            metadata,
        )
        .await
        .unwrap();
    queries.budget("persist conversation turn", if postgres { 4 } else { 5 });
    queries.reset();
    let restored_conversation = conversations.rehydrate(&conversation.conversation_id).await.unwrap();
    queries.budget("populated conversation rehydration", 1);
    assert_eq!(restored_conversation.items, history);
    assert_rows(pool, &restored_conversation.checkpoint.history_item_ids, history).await;
    assert_eq!(
        responses.get_input_items(&conv_response).await.unwrap().unwrap(),
        vec![user("msg_first")]
    );
    let conv_child = format!("resp_contract_{}", uuid::Uuid::now_v7());
    conversations
        .persist(
            &conversation.conversation_id,
            &restored_conversation.checkpoint,
            &conv_child,
            vec![user("msg_second")],
            Some(&snapshot(&conv_child, &[])),
            metadata,
        )
        .await
        .unwrap();
    assert_eq!(
        responses.get_input_items(&conv_child).await.unwrap().unwrap(),
        continued
    );
    assert_eq!(
        conversations
            .rehydrate(&conversation.conversation_id)
            .await
            .unwrap()
            .items,
        continued
    );
}

async fn executor_contract(pool: &Arc<DbPool>, queries: &Queries, postgres: bool) {
    let upstream = MockServer::start_deque(
        [
            "parent answer",
            "child answer",
            "conversation answer",
            "continued answer",
        ]
        .map(text_response)
        .into(),
    )
    .await;
    let responses = ResponseStore::new(Arc::clone(pool));
    let conversations = ConversationStore::new(Arc::clone(pool));
    let context = Arc::new(ExecutionContext::new(
        ConversationHandler::new(conversations.clone()),
        ResponseHandler::new(responses.clone()),
        Arc::new(reqwest::Client::new()),
        upstream.url().to_owned(),
    ));
    let first = unwrap_blocking(
        execute(make_request("first", true, false, None, None), Arc::clone(&context))
            .await
            .unwrap(),
    );
    queries.reset();
    let child = unwrap_blocking(
        execute(
            make_request("second", true, false, Some(first.id), None),
            Arc::clone(&context),
        )
        .await
        .unwrap(),
    );
    queries.budget("complete previous_response_id turn", 5);
    assert_eq!(responses.get_input_items(&child.id).await.unwrap().unwrap().len(), 3);

    let conversation = conversations
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .unwrap();
    execute(
        make_request("first", true, false, None, Some(conversation.conversation_id.clone())),
        Arc::clone(&context),
    )
    .await
    .unwrap();
    queries.reset();
    let child = unwrap_blocking(
        execute(
            make_request("second", true, false, None, Some(conversation.conversation_id)),
            context,
        )
        .await
        .unwrap(),
    );
    queries.budget("complete conversation turn", if postgres { 6 } else { 7 });
    assert_eq!(responses.get_input_items(&child.id).await.unwrap().unwrap().len(), 3);
    let requests = upstream.request_bodies().await;
    for index in [1, 3] {
        let items = requests[index]["input"].as_array().unwrap();
        let roles: Vec<_> = items.iter().map(|item| item["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
    }
}

#[tokio::test]
async fn storage_history_contract_and_query_budgets() {
    let queries = Queries::default();
    tracing::subscriber::set_global_default(Registry::default().with(queries.clone())).unwrap();
    let database = std::env::var("TEST_POSTGRES_URL").unwrap_or_else(|_| "sqlite::memory:".into());
    // One warmed connection isolates operation counts from pool growth and after-connect setup queries.
    let pool = create_pool_with_schema_and_configs(
        Some(&database),
        SqliteConfig::default(),
        PostgresConfig {
            max_connections: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    sqlx::query("SELECT COUNT(*) FROM responses")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    // A disabled SQL logger must fail the test instead of silently accepting every budget.
    queries.reset();
    sqlx::query("SELECT COUNT(*) FROM responses")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    queries.budget("query counter calibration", 1);
    let outputs = reference_outputs();
    let mut history = vec![user("msg_first")];
    history.extend(outputs.iter().cloned().map(InOutItem::Output));
    let metadata = ResponseMetadata {
        model: "test".into(),
        effective_instructions: Some("keep this".into()),
        ..Default::default()
    };
    response_contract(&pool, &queries, &history, &outputs, &metadata).await;
    conversation_contract(
        &pool,
        &queries,
        &history,
        &outputs,
        &metadata,
        database.starts_with("postgres"),
    )
    .await;
    executor_contract(&pool, &queries, database.starts_with("postgres")).await;
    pool.close().await;
}
