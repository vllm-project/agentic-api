use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::response::IntoResponse as _;
use futures::{SinkExt as _, StreamExt as _};
use opentelemetry::Value;
use opentelemetry::trace::SpanId;
use serde_json::json;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;

use super::harness::{Server, attr, finished, gateway, trace_test};

const CREATED: &str = "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_upstream\",\"status\":\"in_progress\",\"output\":[]}}\n\n";
const COMPLETED: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_upstream\",\"status\":\"completed\",\"output\":[]}}\n\ndata: [DONE]\n\n";

fn create(generate: bool) -> Message {
    Message::Text(json!({"type":"response.create","model":"test","input":"private websocket prompt","store":false,"generate":generate}).to_string().into())
}

#[tokio::test]
async fn websocket_requests_are_new_traces_linked_to_session_with_queue_wait() {
    let _guard = trace_test().await;
    let release = Arc::new(Notify::new());
    let requests = Arc::new(AtomicUsize::new(0));
    let (notify, count) = (Arc::clone(&release), Arc::clone(&requests));
    let upstream = Server::start(axum::Router::new().route(
        "/v1/responses",
        axum::routing::post(move || {
            let (notify, count) = (Arc::clone(&notify), Arc::clone(&count));
            async move {
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    notify.notified().await;
                }
                ([("content-type", "text/event-stream")], format!("{CREATED}{COMPLETED}"))
            }
        }),
    ))
    .await;
    let gateway = gateway(&upstream.url).await;
    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("{}/v1/responses", gateway.url.replace("http:", "ws:")))
            .await
            .unwrap();
    socket.send(create(true)).await.unwrap();
    socket.send(create(true)).await.unwrap();
    socket.send(create(false)).await.unwrap();
    socket.send(Message::Ping("barrier".into())).await.unwrap();
    while !matches!(socket.next().await.unwrap().unwrap(), Message::Pong(_)) {}
    tokio::time::sleep(Duration::from_millis(25)).await;
    release.notify_one();
    let mut completed = 0;
    tokio::time::timeout(Duration::from_secs(5), async {
        while completed < 3 {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                let event: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_ne!(event["type"], "error", "{event}");
                completed += usize::from(event["type"] == "response.completed");
            }
        }
    })
    .await
    .unwrap();
    socket.close(None).await.unwrap();
    let spans = finished("agentic.websocket.session", 1).await;
    let session = spans
        .iter()
        .find(|span| span.name == "agentic.websocket.session")
        .unwrap();
    let executions: Vec<_> = spans.iter().filter(|span| span.name == "agentic.execute").collect();
    assert_eq!(executions.len(), 3, "includes local generate:false completion");
    let mut trace_ids = std::collections::HashSet::new();
    for execution in &executions {
        assert_eq!(execution.parent_span_id, SpanId::INVALID);
        assert!(trace_ids.insert(execution.span_context.trace_id()));
        assert_ne!(execution.span_context.trace_id(), session.span_context.trace_id());
        assert_eq!(execution.links.links.len(), 1);
        assert_eq!(execution.links.links[0].span_context, session.span_context);
        assert_eq!(
            attr(execution, "agentic.execution.outcome"),
            Some(&Value::from("completed"))
        );
        assert_eq!(
            attr(execution, "agentic.delivery.outcome"),
            Some(&Value::from("delivered"))
        );
    }
    assert!(
        executions
            .iter()
            .filter(|span| matches!(attr(span, "agentic.queue.wait"), Some(Value::F64(wait)) if *wait >= 0.02))
            .count()
            >= 2
    );
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    gateway.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn websocket_disconnect_closes_active_and_queued_traces_without_tracing_rejected_frames() {
    let _guard = trace_test().await;
    let upstream = Server::start(axum::Router::new().route(
        "/v1/responses",
        axum::routing::post(|| async {
            let body = futures::stream::once(std::future::ready(Ok::<_, std::io::Error>(
                axum::body::Bytes::from_static(CREATED.as_bytes()),
            )))
            .chain(futures::stream::pending());
            ([("content-type", "text/event-stream")], Body::from_stream(body)).into_response()
        }),
    ))
    .await;
    let gateway = gateway(&upstream.url).await;
    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("{}/v1/responses", gateway.url.replace("http:", "ws:")))
            .await
            .unwrap();
    socket
        .send(Message::Text("{\"type\":\"invalid\"}".into()))
        .await
        .unwrap();
    assert!(
        socket
            .next()
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap()
            .contains("error")
    );
    socket.send(create(true)).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(first.into_text().unwrap().contains("response.created"));
    socket.send(create(true)).await.unwrap();
    socket.send(Message::Ping("barrier".into())).await.unwrap();
    while !matches!(socket.next().await.unwrap().unwrap(), Message::Pong(_)) {}
    drop(socket);
    let spans = finished("agentic.websocket.session", 1).await;
    let executions: Vec<_> = spans.iter().filter(|span| span.name == "agentic.execute").collect();
    assert_eq!(executions.len(), 2);
    for execution in executions {
        assert_eq!(
            attr(execution, "agentic.execution.outcome"),
            Some(&Value::from("cancelled"))
        );
        assert_eq!(
            attr(execution, "agentic.delivery.outcome"),
            Some(&Value::from("disconnected"))
        );
        assert_eq!(execution.links.links.len(), 1);
        assert!(matches!(attr(execution, "agentic.queue.wait"), Some(Value::F64(wait)) if *wait >= 0.0));
    }
    gateway.stop().await;
    upstream.stop().await;
}
