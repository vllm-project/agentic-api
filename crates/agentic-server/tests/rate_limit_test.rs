//! Per-caller request rate limiting on the gateway's HTTP and WebSocket routes.
//!
//! Each test uses a rate of one request per minute, so a bucket cannot refill
//! while the test runs and every admit or reject is deterministic. The advertised
//! wait shrinks by however long the test takes, so the tests bound it by the
//! measured elapsed time; the exact rounding is covered by the limiter's
//! paused-time unit tests.

#[allow(dead_code)]
mod common;

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use agentic_server::app::{ServerConfig, build_router};
use agentic_server::rate_limit::RateLimitConfig;
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use axum::http::StatusCode;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tower::ServiceExt as _;

use common::{test_config, test_state};

/// An upstream that answers every request with an empty JSON object and
/// counts how many requests reached it.
async fn counting_upstream() -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let app = Router::new().fallback(move || {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            axum::Json(json!({"object": "list", "data": []}))
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, calls)
}

async fn gateway(upstream_url: &str, rate_limit: Option<RateLimitConfig>) -> String {
    gateway_with_server_key(upstream_url, rate_limit, None).await
}

async fn gateway_with_server_key(
    upstream_url: &str,
    rate_limit: Option<RateLimitConfig>,
    server_key: Option<&str>,
) -> String {
    let mut config = test_config(upstream_url);
    config.openai_api_key = server_key.map(str::to_owned);
    let state = test_state(&config);
    let router = build_router(
        state,
        &ServerConfig {
            rate_limit,
            ..ServerConfig::from_env()
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    // As in production, each request carries its connection's peer address.
    let service = router.into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    url
}

/// The spacing between requests at one request per minute.
const INTERVAL: Duration = Duration::from_secs(60);

/// One request per minute, with `burst` allowed at once.
fn per_minute(burst: u32) -> RateLimitConfig {
    RateLimitConfig::new(NonZeroU32::MIN, NonZeroU32::new(burst)).unwrap()
}

/// The bounds on a limited request's whole-second wait, given the time since
/// just before its bucket's first request.
///
/// The next request is due one interval after the bucket's first, so the wait
/// is the interval less the time that has passed since then: at most the
/// interval, and at least the interval less `elapsed`.
fn wait_seconds_bounds(elapsed: Duration) -> std::ops::RangeInclusive<u64> {
    INTERVAL.saturating_sub(elapsed).as_secs()..=INTERVAL.as_secs()
}

/// Milliseconds in an `x-ratelimit-reset-requests` value such as `250ms`,
/// `1.5s`, or `1m59.996s`.
fn reset_millis(value: &str) -> u64 {
    if let Some(millis) = value.strip_suffix("ms") {
        return millis.parse().unwrap();
    }
    let value = value.strip_suffix('s').unwrap();
    let (minutes, seconds) = value.split_once('m').unwrap_or(("0", value));
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let fraction: u64 = format!("{fraction:0<3}").parse().unwrap();
    minutes.parse::<u64>().unwrap() * 60_000 + whole.parse::<u64>().unwrap() * 1000 + fraction
}

/// Checks a limited response's retry headers, given the time since just before
/// its bucket's first request, and returns `retry-after`.
///
/// The headers describe one wait, so they agree exactly whatever the timing:
/// `retry-after` is `retry-after-ms` rounded up to whole seconds, and the full
/// burst returns one interval per extra burst request after that.
fn assert_retry_headers(response: &reqwest::Response, burst: u32, elapsed: Duration) -> u64 {
    let seconds: u64 = header(response, "retry-after").parse().unwrap();
    let millis: u64 = header(response, "retry-after-ms").parse().unwrap();
    assert!(
        wait_seconds_bounds(elapsed).contains(&seconds),
        "retry-after {seconds} after {elapsed:?}"
    );
    let lowest_millis = u64::try_from(INTERVAL.saturating_sub(elapsed).as_millis()).unwrap();
    assert!(
        (lowest_millis..=60_000).contains(&millis),
        "retry-after-ms {millis} after {elapsed:?}"
    );
    assert_eq!(seconds, millis.div_ceil(1000), "both hints round up the same wait");
    assert_eq!(header(response, "x-ratelimit-limit-requests"), "1");
    assert_eq!(header(response, "x-ratelimit-remaining-requests"), "0");
    let reset = header(response, "x-ratelimit-reset-requests");
    assert_eq!(
        reset_millis(reset),
        millis + 60_000 * u64::from(burst - 1),
        "the full burst returns one interval per extra burst request later: {reset}"
    );
    seconds
}

fn models(gateway: &str, bearer: Option<&str>) -> reqwest::RequestBuilder {
    let request = reqwest::Client::new().get(format!("{gateway}/v1/models"));
    match bearer {
        Some(bearer) => request.bearer_auth(bearer),
        None => request,
    }
}

fn header<'a>(response: &'a reqwest::Response, name: &str) -> &'a str {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .to_str()
        .unwrap()
}

#[tokio::test]
async fn a_caller_over_its_limit_gets_an_openai_429_with_retry_headers() {
    let (upstream, calls) = counting_upstream().await;
    let gateway = gateway(&upstream, Some(per_minute(2))).await;

    let started = Instant::now();
    for _ in 0..2 {
        let response = models(&gateway, Some("caller-a")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers().get("x-ratelimit-limit-requests").is_none(),
            "only a limited response carries rate-limit headers"
        );
    }
    let limited = models(&gateway, Some("caller-a")).send().await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    let seconds = assert_retry_headers(&limited, 2, elapsed);
    assert_eq!(
        limited.json::<Value>().await.unwrap(),
        json!({"error": {
            "message": format!("rate limit exceeded; retry after {seconds} seconds"),
            "type": "rate_limit_error",
            "param": null,
            "code": "rate_limit_exceeded"
        }})
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the limited request never reached the upstream"
    );
}

/// A credential the gateway does not verify must not choose the bucket: on the
/// executor route the gateway calls the upstream with its own key, so nothing
/// would ever check a made-up `x-api-key` or a lowercase `bearer` token.
#[tokio::test]
async fn changing_the_credential_does_not_reset_the_bucket() {
    let (upstream, calls) = counting_upstream().await;
    let gateway = gateway_with_server_key(&upstream, Some(per_minute(1)), Some("server-key")).await;
    let create = |credential: Option<(&str, String)>| {
        let request = reqwest::Client::new()
            .post(format!("{gateway}/v1/responses"))
            .json(&json!({"model": "m", "input": "hi"}));
        match credential {
            Some((name, value)) => request.header(name, value),
            None => request,
        }
    };

    let first = create(Some(("x-api-key", "made-up-0".to_owned())))
        .send()
        .await
        .unwrap();
    assert_ne!(first.status(), StatusCode::TOO_MANY_REQUESTS);
    for credential in [
        Some(("x-api-key", "made-up-1".to_owned())),
        Some(("authorization", "Bearer made-up-2".to_owned())),
        Some(("authorization", "bearer made-up-3".to_owned())),
        None,
    ] {
        let response = create(credential.clone()).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{credential:?}");
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "only the first request reached the upstream"
    );
}

/// The status of `GET /v1/models` from `peer`, through the router directly.
async fn status_from(router: &Router, peer: &str) -> StatusCode {
    let request = Request::builder()
        .uri("/v1/models")
        .extension(ConnectInfo(peer.parse::<SocketAddr>().unwrap()))
        .body(Body::empty())
        .unwrap();
    router.clone().oneshot(request).await.unwrap().status()
}

#[tokio::test]
async fn separate_peer_addresses_have_separate_buckets() {
    let (upstream, _calls) = counting_upstream().await;
    let router = build_router(
        test_state(&test_config(&upstream)),
        &ServerConfig {
            rate_limit: Some(per_minute(1)),
            ..ServerConfig::from_env()
        },
    );

    assert_eq!(status_from(&router, "192.0.2.1:4000").await, StatusCode::OK);
    assert_eq!(
        status_from(&router, "192.0.2.1:4001").await,
        StatusCode::TOO_MANY_REQUESTS,
        "a new connection from the same address is the same caller"
    );
    assert_eq!(
        status_from(&router, "192.0.2.2:4000").await,
        StatusCode::OK,
        "another address has its own bucket"
    );
    assert_eq!(status_from(&router, "[2001:db8::1]:4000").await, StatusCode::OK);
    assert_eq!(
        status_from(&router, "[2001:db8::2]:4000").await,
        StatusCode::TOO_MANY_REQUESTS,
        "one IPv6 /64 is one caller"
    );
}

#[tokio::test]
async fn health_endpoints_are_never_limited() {
    let (upstream, _calls) = counting_upstream().await;
    let gateway = gateway(&upstream, Some(per_minute(1))).await;
    let client = reqwest::Client::new();

    for path in ["/health", "/ready"] {
        for _ in 0..5 {
            let response = client.get(format!("{gateway}{path}")).send().await.unwrap();
            assert_ne!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{path}");
        }
    }
    assert_eq!(models(&gateway, None).send().await.unwrap().status(), StatusCode::OK);
}

#[tokio::test]
async fn messages_routes_answer_in_the_anthropic_format() {
    let (upstream, calls) = counting_upstream().await;
    let gateway = gateway(&upstream, Some(per_minute(1))).await;
    let message = || {
        reqwest::Client::new()
            .post(format!("{gateway}/v1/messages"))
            .header("x-api-key", "anthropic-caller")
            .json(&json!({"model": "m", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]}))
    };

    let started = Instant::now();
    assert_ne!(message().send().await.unwrap().status(), StatusCode::TOO_MANY_REQUESTS);
    let limited = message().send().await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    let seconds = assert_retry_headers(&limited, 1, elapsed);
    let request_id = header(&limited, "request-id").to_owned();
    assert!(request_id.starts_with("req_"), "{request_id}");
    assert_eq!(
        limited.json::<Value>().await.unwrap(),
        json!({
            "type": "error",
            "error": {
                "type": "rate_limit_error",
                "message": format!("rate limit exceeded; retry after {seconds} seconds")
            },
            "request_id": request_id
        })
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rate_limiting_is_off_by_default() {
    let (upstream, calls) = counting_upstream().await;
    let gateway = gateway(&upstream, None).await;

    for _ in 0..10 {
        let response = models(&gateway, Some("caller-a")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get("retry-after").is_none());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 10);
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn connect(gateway: &str, bearer: Option<&str>) -> Result<Socket, WsError> {
    let mut request = format!("{}/v1/responses", gateway.replacen("http://", "ws://", 1))
        .into_client_request()
        .unwrap();
    if let Some(bearer) = bearer {
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {bearer}").parse().unwrap());
    }
    connect_async(request).await.map(|(socket, _)| socket)
}

async fn recv(socket: &mut Socket) -> Value {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("timed out waiting for a websocket message")
            .expect("websocket closed")
            .expect("websocket error");
        if let Message::Text(text) = message {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

async fn send(socket: &mut Socket, value: Value) {
    socket.send(Message::Text(value.to_string().into())).await.unwrap();
}

/// A response the gateway completes locally, without calling the upstream.
fn local_create() -> Value {
    json!({"type": "response.create", "model": "test-model", "input": [], "generate": false, "store": false, "stream": true})
}

async fn create_completes(socket: &mut Socket) {
    send(socket, local_create()).await;
    assert_eq!(recv(socket).await["type"], "response.created");
    assert_eq!(recv(socket).await["type"], "response.completed");
}

/// Checks a WebSocket rate-limit error frame, given the time since just before
/// its bucket's first request.
fn assert_ws_rate_limit_error(frame: &Value, elapsed: Duration) {
    let seconds: u64 = frame["error"]["message"]
        .as_str()
        .and_then(|message| message.strip_prefix("rate limit exceeded; retry after "))
        .and_then(|rest| rest.strip_suffix(" seconds"))
        .and_then(|seconds| seconds.parse().ok())
        .unwrap_or_else(|| panic!("not a rate-limit message: {frame}"));
    assert!(
        wait_seconds_bounds(elapsed).contains(&seconds),
        "retry after {seconds} s after {elapsed:?}"
    );
    assert_eq!(
        frame,
        &json!({"type": "error", "status": 429, "error": {
            "message": format!("rate limit exceeded; retry after {seconds} seconds"),
            "type": "rate_limit_error",
            "code": "rate_limit_exceeded"
        }})
    );
}

#[tokio::test]
async fn websocket_creates_are_charged_and_a_limited_create_keeps_the_connection() {
    let (upstream, _calls) = counting_upstream().await;
    let gateway = gateway(&upstream, Some(per_minute(3))).await;

    // The upgrade is the first request; two creates use the rest of the burst.
    let started = Instant::now();
    let mut socket = connect(&gateway, Some("ws-caller")).await.unwrap();
    create_completes(&mut socket).await;
    create_completes(&mut socket).await;
    send(&mut socket, local_create()).await;
    assert_ws_rate_limit_error(&recv(&mut socket).await, started.elapsed());

    // Steering acts on an admitted response and is not charged; it is answered
    // on the same, still open, connection.
    send(
        &mut socket,
        json!({"type": "response.steer", "previous_response_id": "resp_unknown", "input": "hi"}),
    )
    .await;
    let steer = recv(&mut socket).await;
    assert_eq!(steer["type"], "response.steer.failed", "{steer}");
}

#[tokio::test]
async fn http_and_websocket_share_a_bucket_and_the_upgrade_is_charged() {
    let (upstream, _calls) = counting_upstream().await;
    let gateway = gateway(&upstream, Some(per_minute(2))).await;

    // Different credentials, one peer: the WebSocket shares the HTTP caller's bucket.
    let started = Instant::now();
    assert_eq!(
        models(&gateway, Some("http-credential")).send().await.unwrap().status(),
        StatusCode::OK
    );
    let mut socket = connect(&gateway, Some("websocket-credential")).await.unwrap();
    send(&mut socket, local_create()).await;
    assert_ws_rate_limit_error(&recv(&mut socket).await, started.elapsed());

    match connect(&gateway, None).await {
        Err(WsError::Http(response)) => assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS),
        other => panic!("a limited upgrade is rejected before switching protocols: {other:?}"),
    }
}
