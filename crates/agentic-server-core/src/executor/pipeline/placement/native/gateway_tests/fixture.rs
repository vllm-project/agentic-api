// Synthetic unit inputs and loopback search provider, never captured model data.

#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::Query;
use axum::http::StatusCode;
use axum::routing::get;
use serde_json::json;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::tool::{GatewayExecutors, ToolDeclaration, ToolRegistry, WebSearchHandler};
use crate::types::tools::WebSearchToolParam;

#[derive(Clone, Copy, Debug)]
pub(super) enum Reply {
    Success,
    Failure,
    WaitForShutdown,
}

#[derive(serde::Deserialize)]
struct SearchQuery {
    query: String,
}

pub(super) struct SearchFixture {
    pub registry: ToolRegistry,
    requests: Arc<AtomicUsize>,
    entered: Arc<Notify>,
    cancel: CancellationToken,
    server: Option<JoinHandle<std::io::Result<()>>>,
}

impl SearchFixture {
    pub async fn start(reply: Reply) -> Self {
        let requests = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let observed = Arc::clone(&requests);
        let ready = Arc::clone(&entered);
        let pending = cancel.clone();
        let app = axum::Router::new().route(
            "/v1/search",
            get(move |Query(query): Query<SearchQuery>| {
                observed.fetch_add(1, Ordering::SeqCst);
                ready.notify_one();
                let pending = pending.clone();
                async move {
                    if matches!(reply, Reply::WaitForShutdown) {
                        pending.cancelled().await;
                    }
                    let status = if matches!(reply, Reply::Failure) {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::OK
                    };
                    (
                        status,
                        axum::Json(
                            json!({"results":{"web":[{"url":"https://example.invalid/search-fixture",
                    "title":"Synthetic local result", "snippets":["controlled provider reply"]}],"news":[]},
                    "metadata":{"query":query.query,"search_uuid":"placement-fixture"}}),
                        ),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stopping = cancel.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(stopping.cancelled_owned())
                .await
        });
        let client = Arc::new(reqwest::Client::builder().no_proxy().build().unwrap());
        let handler = Arc::new(WebSearchHandler::from_values(
            client,
            Some("synthetic-local-fixture-not-a-secret".to_owned()),
            Some(format!("http://{address}")),
            std::num::NonZeroUsize::new(5).unwrap(),
        ));
        let mut executors = GatewayExecutors::default();
        executors.insert(handler);
        let registry = ToolRegistry::build_with_handlers(
            &mut [ToolDeclaration::WebSearch(WebSearchToolParam::default())],
            &mut executors,
        )
        .await
        .unwrap();
        Self {
            registry,
            requests,
            entered,
            cancel,
            server: Some(server),
        }
    }

    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    pub async fn wait_for_request(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .unwrap();
    }

    pub async fn stop(mut self) {
        self.cancel.cancel();
        if let Some(mut server) = self.server.take() {
            let joined = tokio::time::timeout(Duration::from_secs(5), &mut server).await;
            if joined.is_err() {
                server.abort();
                let _ = server.await;
                panic!("local search fixture did not join gracefully");
            }
            joined.unwrap().unwrap().unwrap();
        }
    }
}

impl Drop for SearchFixture {
    fn drop(&mut self) {
        self.cancel.cancel();
        // Panic/drop fallback only; passing checks explicitly stop and observe join.
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

pub(super) fn lines(client_calls: usize, failed: bool) -> Vec<String> {
    assert!(client_calls <= 300, "bounded synthetic stress input");
    let mut events = vec![
        json!({"type":"response.created","response":{"id":"upstream","status":"in_progress"}}),
        json!({"type":"response.in_progress","response":{"id":"upstream","status":"in_progress"}}),
    ];
    let mut completed = Vec::new();
    for index in 0..=client_calls {
        let (id, call_id, name, arguments) = if index == 0 {
            (
                "fc_search".to_owned(),
                "call_search".to_owned(),
                "web_search",
                r#"{"query":"placement fixture"}"#.to_owned(),
            )
        } else {
            (
                format!("fc_echo_{index}"),
                format!("call_echo_{index}"),
                "echo",
                r#"{"value":"after search"}"#.to_owned(),
            )
        };
        let added =
            json!({"type":"function_call","id":id,"call_id":call_id,"name":name,"arguments":"","status":"in_progress"});
        let done = json!({"type":"function_call","id":id,"call_id":call_id,"name":name,"arguments":arguments,"status":"completed"});
        events.extend([
            json!({"type":"response.output_item.added","output_index":index,"item":added}),
            json!({"type":"response.function_call_arguments.delta","output_index":index,"item_id":id,"delta":arguments}),
            json!({"type":"response.function_call_arguments.done","output_index":index,"item_id":id,"name":name,"arguments":arguments}),
            json!({"type":"response.output_item.done","output_index":index,"item":done}),
        ]);
        completed.push(done);
    }
    let status = if failed { "failed" } else { "completed" };
    events.push(
        json!({"type":format!("response.{status}"),"response":{"id":"upstream","status":status,"output":completed}}),
    );
    events.into_iter().map(|event| format!("data: {event}")).collect()
}
