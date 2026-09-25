use super::*;
use axum::{
    Router,
    body::Body,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use futures::StreamExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::task::JoinHandle;

struct Server {
    url: String,
    task: JoinHandle<()>,
}

impl Server {
    async fn new(router: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, task }
    }

    async fn stop(mut self) {
        self.task.abort();
        assert!((&mut self.task).await.unwrap_err().is_cancelled());
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn local_transport() -> ResponsesTransport {
    // Only the unit fixture disables HTTPS. All other production client settings
    // remain in effect; no runtime endpoint override or gate bypass is exposed.
    ResponsesTransport {
        client: Arc::new(opaque_client_builder().https_only(false).build().unwrap()),
        response_policy: ResponsePolicy::Opaque,
        fixture_address: None,
    }
}

#[tokio::test]
async fn opaque_transport_refuses_http_before_contacting_the_server() {
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let server = Server::new(Router::new().route(
        "/",
        post(move || async move {
            observed.fetch_add(1, Ordering::SeqCst);
            "unexpected"
        }),
    ))
    .await;
    let error = ResponsesTransport::opaque()
        .unwrap()
        .fetch_json(&server.url, "{}".into(), Some("synthetic-key"), 1024)
        .await
        .unwrap_err();
    assert_eq!(error.http_status(), StatusCode::BAD_GATEWAY);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.stop().await;
}

#[tokio::test]
async fn redirects_are_not_followed_and_reflected_bodies_are_never_forwarded() {
    for status in [
        StatusCode::MOVED_PERMANENTLY,
        StatusCode::FOUND,
        StatusCode::SEE_OTHER,
        StatusCode::TEMPORARY_REDIRECT,
        StatusCode::PERMANENT_REDIRECT,
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::TOO_MANY_REQUESTS,
    ] {
        let reached = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&reached);
        let router = Router::new()
            .route(
                "/",
                post(move || async move {
                    (
                        status,
                        [("location", "/leak"), ("x-private", "opaque-secret")],
                        "opaque-secret",
                    )
                }),
            )
            .route(
                "/leak",
                axum::routing::any(move || async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    "unexpected"
                }),
            );
        let server = Server::new(router).await;
        let error = local_transport()
            .fetch_json(&server.url, "opaque-secret".into(), Some("synthetic-key"), 1024)
            .await
            .unwrap_err();
        assert_eq!(
            error.http_status(),
            if status.is_redirection() {
                StatusCode::BAD_GATEWAY
            } else {
                status
            }
        );
        assert!(!format!("{error:?} {error}").contains("opaque-secret"));
        assert_eq!(reached.load(Ordering::SeqCst), 0);
        server.stop().await;
    }
}

#[tokio::test]
async fn only_explicit_bearer_identity_and_content_type_are_sent() {
    let observed = Arc::new(tokio::sync::Mutex::new(None));
    let capture = Arc::clone(&observed);
    let server = Server::new(Router::new().route(
        "/",
        post(move |headers: HeaderMap| async move {
            *capture.lock().await = Some(headers);
            "{}"
        }),
    ))
    .await;
    assert_eq!(
        local_transport()
            .fetch_json(&server.url, "{}".into(), Some("synthetic-key"), 1024)
            .await
            .unwrap(),
        "{}"
    );
    let headers = observed.lock().await.take().unwrap();
    assert_eq!(headers["authorization"], "Bearer synthetic-key");
    assert_eq!(headers["content-type"], "application/json");
    for name in ["openai-organization", "openai-project", "cookie", "proxy-authorization"] {
        assert!(!headers.contains_key(name));
    }
    server.stop().await;
}

#[tokio::test]
async fn opaque_transport_preserves_existing_json_and_sse_byte_limits() {
    let server = Server::new(Router::new().route("/", post(|| async { "data: oversized\n\n" }))).await;
    let error = local_transport()
        .fetch_json(&server.url, "{}".into(), None, 4)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ExecutorError::ResourceLimitExceeded {
            limit: crate::executor::error::ResourceLimit::UpstreamJsonBody,
            ..
        }
    ));
    let stream = crate::executor::inference::call_inference_with_transport(
        "{}".into(),
        server.url.clone(),
        local_transport(),
        None,
        Duration::ZERO,
        4,
    );
    futures::pin_mut!(stream);
    assert!(matches!(
        stream.next().await.unwrap(),
        Err(ExecutorError::ResourceLimitExceeded {
            limit: crate::executor::error::ResourceLimit::UpstreamSseLine,
            ..
        })
    ));
    assert!(stream.next().await.is_none());
    server.stop().await;
}

#[tokio::test]
async fn read_timeout_bounds_headers_and_body_without_a_caller_chunk_timeout() {
    let router = Router::new()
        .route(
            "/headers",
            post(|| async {
                std::future::pending::<()>().await;
                "unreachable"
            }),
        )
        .route(
            "/body",
            post(|| async {
                Body::from_stream(futures::stream::pending::<Result<bytes::Bytes, std::io::Error>>()).into_response()
            }),
        );
    let server = Server::new(router).await;
    let transport = ResponsesTransport {
        client: Arc::new(
            opaque_client_builder()
                .https_only(false)
                .read_timeout(Duration::from_millis(25))
                .build()
                .unwrap(),
        ),
        response_policy: ResponsePolicy::Opaque,
        fixture_address: None,
    };
    for path in ["headers", "body"] {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            transport.fetch_json(&format!("{}/{path}", server.url), "{}".into(), None, 1024),
        )
        .await
        .unwrap();
        assert!(result.is_err());
    }
    server.stop().await;
}

#[tokio::test]
async fn candidate_rejects_invalid_utf8_and_partial_lines_in_the_shared_framer() {
    let server = Server::new(
        Router::new()
            .route(
                "/invalid",
                post(|| async { vec![b'd', b'a', b't', b'a', b':', b' ', 0xff, b'\n'] }),
            )
            .route("/partial", post(|| async { "data: unfinished" })),
    )
    .await;
    for path in ["invalid", "partial"] {
        let stream = crate::executor::inference::call_inference_with_transport(
            "{}".into(),
            format!("{}/{path}", server.url),
            local_transport(),
            None,
            Duration::ZERO,
            1024,
        );
        futures::pin_mut!(stream);
        assert!(matches!(
            stream.next().await.unwrap(),
            Err(ExecutorError::StreamError(_))
        ));
        assert!(stream.next().await.is_none());
        // The default adapter retains its previous lenient framing behavior.
        let stream = crate::executor::inference::call_inference_limited(
            "{}".into(),
            format!("{}/{path}", server.url),
            Arc::new(reqwest::Client::new()),
            None,
            Duration::ZERO,
            1024,
        );
        futures::pin_mut!(stream);
        assert!(stream.next().await.is_none());
    }
    server.stop().await;
}
