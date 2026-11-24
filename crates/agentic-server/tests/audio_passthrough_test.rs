//! `/v1/audio/transcriptions` and `/v1/audio/translations` stream the client's multipart upload
//! to the upstream as it arrives, within the request-size limit, and relay the answer unchanged.
#[allow(dead_code)]
mod common;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::response::IntoResponse;
use axum::routing::post;
use common::{spawn_gateway, test_config, test_state, test_state_with_max_request_body_size};
use futures::{SinkExt, StreamExt};
use http::{HeaderMap, StatusCode};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify, watch};
use tokio::time::timeout;

const UPLOAD_CONTENT_TYPE: &str = "multipart/form-data; boundary=agentic-audio-boundary";

/// A multipart upload with a binary audio part, so the forwarded body can be compared byte
/// for byte against what the upstream received.
const UPLOAD: &[u8] = b"--agentic-audio-boundary\r\n\
Content-Disposition: form-data; name=\"model\"\r\n\r\n\
whisper-1\r\n\
--agentic-audio-boundary\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"clip.wav\"\r\n\
Content-Type: audio/wav\r\n\r\n\
RIFF\x00\x01\x02\xff\xfe audio\r\n\
--agentic-audio-boundary--\r\n";

/// Irregular whitespace and an unknown field, so the relayed body can be compared byte for byte.
const TRANSCRIPTION: &str = r#"{"text":"hello world",  "unknown_field":{"kept":true}}"#;

const FIRST_EVENT: &str = "data: {\"type\":\"transcript.text.delta\",\"delta\":\"hello\"}\n\n";
const REMAINING_EVENTS: &str = "data: {\"type\":\"transcript.text.done\",\"text\":\"hello\"}\n\ndata: [DONE]\n\n";

/// Bounds every wait on streamed data, so a gateway that buffers fails instead of hanging.
const STREAM_WAIT: Duration = Duration::from_secs(5);

/// The request-size limit for the oversized-upload tests.
const SMALL_LIMIT: usize = 64;

/// What the upstream received for the last request.
#[derive(Default)]
struct Received {
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

/// Upstream audio endpoints that answer with a fixed response and record what they were sent.
#[derive(Clone)]
struct Upstream {
    status: StatusCode,
    content_type: &'static str,
    body: &'static str,
    received: Arc<Mutex<Received>>,
    calls: Arc<AtomicUsize>,
    /// Upload bytes received so far, observable while the request is still streaming.
    bytes_seen: Arc<watch::Sender<usize>>,
    /// Set once the handler has stopped reading the request body, complete or cut off.
    finished: Arc<watch::Sender<bool>>,
}

impl Upstream {
    fn new(status: StatusCode, content_type: &'static str, body: &'static str) -> Self {
        Self {
            status,
            content_type,
            body,
            received: Arc::default(),
            calls: Arc::default(),
            bytes_seen: Arc::new(watch::channel(0).0),
            finished: Arc::new(watch::channel(false).0),
        }
    }

    async fn spawn(&self) -> String {
        let upstream = self.clone();
        let handler = move |req: Request| {
            let upstream = upstream.clone();
            async move {
                // Count the call on arrival, before any of the body is read.
                upstream.calls.fetch_add(1, Ordering::SeqCst);
                let (parts, body) = req.into_parts();
                *upstream.received.lock().await = Received {
                    path: parts.uri.path().to_owned(),
                    headers: parts.headers,
                    body: Vec::new(),
                };
                // Record the upload frame by frame, so a test can watch it arrive and see where
                // a cut-off upload stopped.
                let mut frames = body.into_data_stream();
                while let Some(Ok(frame)) = frames.next().await {
                    upstream.received.lock().await.body.extend_from_slice(&frame);
                    upstream.bytes_seen.send_modify(|seen| *seen += frame.len());
                }
                upstream.finished.send_replace(true);
                let content_type = [(http::header::CONTENT_TYPE, upstream.content_type)];
                (upstream.status, content_type, upstream.body).into_response()
            }
        };
        let app = Router::new()
            .route("/v1/audio/transcriptions", post(handler.clone()))
            .route("/v1/audio/translations", post(handler));
        serve(app).await
    }

    /// Wait until the upstream has received at least `bytes` of the upload.
    async fn wait_for_bytes(&self, bytes: usize) {
        let mut seen = self.bytes_seen.subscribe();
        seen.wait_for(|seen| *seen >= bytes)
            .await
            .expect("upstream stays alive");
    }

    /// Wait until the upstream has stopped reading the request body.
    async fn wait_until_finished(&self) {
        let mut finished = self.finished.subscribe();
        finished
            .wait_for(|finished| *finished)
            .await
            .expect("upstream stays alive");
    }
}

/// Upstream transcription endpoint that streams [`FIRST_EVENT`], then holds the remaining
/// events until `release` is notified.
async fn spawn_gated_sse_upstream(release: Arc<Notify>) -> String {
    let handler = move |req: Request| {
        let release = Arc::clone(&release);
        async move {
            let _upload = axum::body::to_bytes(req.into_body(), usize::MAX).await;
            let (mut events, body) = futures::channel::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
            tokio::spawn(async move {
                let _ = events.send(Ok(Bytes::from_static(FIRST_EVENT.as_bytes()))).await;
                release.notified().await;
                let _ = events.send(Ok(Bytes::from_static(REMAINING_EVENTS.as_bytes()))).await;
            });
            let content_type = [(http::header::CONTENT_TYPE, "text/event-stream")];
            (content_type, Body::from_stream(body)).into_response()
        }
    };
    serve(Router::new().route("/v1/audio/transcriptions", post(handler))).await
}

async fn serve(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn gateway(upstream_url: &str) -> String {
    let (gateway_url, _gateway) = spawn_gateway(test_state(&test_config(upstream_url))).await;
    gateway_url
}

async fn gateway_with_limit(upstream_url: &str, limit: usize) -> String {
    let limit = NonZeroUsize::new(limit).unwrap();
    let (gateway_url, _gateway) =
        spawn_gateway(test_state_with_max_request_body_size(&test_config(upstream_url), limit)).await;
    gateway_url
}

/// The upload split into chunks and sent without a declared length.
fn chunked_upload() -> reqwest::Body {
    let chunks = UPLOAD
        .chunks(16)
        .map(|chunk| Ok::<_, std::io::Error>(Bytes::copy_from_slice(chunk)))
        .collect::<Vec<_>>();
    reqwest::Body::wrap_stream(futures::stream::iter(chunks))
}

async fn upload(url: String, body: impl Into<reqwest::Body>) -> reqwest::Response {
    reqwest::Client::new()
        .post(url)
        .header(http::header::CONTENT_TYPE, UPLOAD_CONTENT_TYPE)
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn transcription_upload_is_forwarded_unchanged() {
    let upstream = Upstream::new(StatusCode::OK, "application/json", TRANSCRIPTION);
    let gateway_url = gateway(&upstream.spawn().await).await;

    let response = upload(format!("{gateway_url}/v1/audio/transcriptions"), UPLOAD).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), TRANSCRIPTION);
    let received = upstream.received.lock().await;
    assert_eq!(received.path, "/v1/audio/transcriptions");
    assert_eq!(received.body, UPLOAD);
    assert_eq!(received.headers[http::header::CONTENT_TYPE], UPLOAD_CONTENT_TYPE);
    assert_eq!(received.headers[http::header::CONTENT_LENGTH], UPLOAD.len().to_string());
}

#[tokio::test]
async fn translation_is_forwarded_to_its_own_path() {
    let upstream = Upstream::new(StatusCode::OK, "application/json", TRANSCRIPTION);
    let gateway_url = gateway(&upstream.spawn().await).await;

    let response = upload(format!("{gateway_url}/v1/audio/translations"), UPLOAD).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), TRANSCRIPTION);
    let received = upstream.received.lock().await;
    assert_eq!(received.path, "/v1/audio/translations");
    assert_eq!(received.body, UPLOAD);
}

#[tokio::test]
async fn an_upload_reaches_the_upstream_before_it_finishes() {
    let upstream = Upstream::new(StatusCode::OK, "application/json", TRANSCRIPTION);
    let gateway_url = gateway(&upstream.spawn().await).await;
    let (first, rest) = UPLOAD.split_at(16);
    let (mut sender, chunks) = futures::channel::mpsc::channel::<Result<Bytes, std::io::Error>>(1);
    let request = tokio::spawn(upload(
        format!("{gateway_url}/v1/audio/transcriptions"),
        reqwest::Body::wrap_stream(chunks),
    ));

    sender.send(Ok(Bytes::from_static(first))).await.unwrap();
    // The rest of the upload is held back until the upstream has the first chunk, so a gateway
    // that buffered the upload would forward nothing and this wait would time out.
    timeout(STREAM_WAIT, upstream.wait_for_bytes(first.len()))
        .await
        .expect("the first chunk must reach the upstream before the upload ends");
    sender.send(Ok(Bytes::from_static(rest))).await.unwrap();
    drop(sender);

    let response = request.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let received = upstream.received.lock().await;
    assert_eq!(received.body, UPLOAD);
    assert!(
        received.headers.get(http::header::CONTENT_LENGTH).is_none(),
        "an undeclared length must stay undeclared upstream"
    );
}

#[tokio::test]
async fn sse_events_reach_the_client_before_the_upstream_finishes() {
    let release = Arc::new(Notify::new());
    let gateway_url = gateway(&spawn_gated_sse_upstream(Arc::clone(&release)).await).await;

    let mut response = upload(format!("{gateway_url}/v1/audio/transcriptions"), UPLOAD).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[http::header::CONTENT_TYPE], "text/event-stream");

    // The upstream holds the remaining events until the client has the first one, so a gateway
    // that buffered the response would deliver nothing and this wait would time out.
    let first = timeout(STREAM_WAIT, async {
        let mut first = Vec::new();
        while first.len() < FIRST_EVENT.len() {
            let chunk = response
                .chunk()
                .await
                .unwrap()
                .expect("stream ended before the first event");
            first.extend_from_slice(&chunk);
        }
        first
    })
    .await
    .expect("the first event must reach the client before the upstream finishes");
    assert_eq!(first, FIRST_EVENT.as_bytes());

    release.notify_one();
    assert_eq!(response.text().await.unwrap(), REMAINING_EVENTS);
}

#[tokio::test]
async fn upstream_errors_are_forwarded_unchanged() {
    const ERROR: &str = r#"{"error":{"message":"unsupported audio format","type":"BadRequestError"}}"#;
    let upstream = Upstream::new(StatusCode::BAD_REQUEST, "application/json", ERROR);
    let gateway_url = gateway(&upstream.spawn().await).await;

    let response = upload(format!("{gateway_url}/v1/audio/transcriptions"), UPLOAD).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(response.text().await.unwrap(), ERROR);
}

#[tokio::test]
async fn a_declared_oversized_upload_is_rejected_before_the_upstream_is_called() {
    let upstream = Upstream::new(StatusCode::OK, "application/json", TRANSCRIPTION);
    let gateway_url = gateway_with_limit(&upstream.spawn().await, SMALL_LIMIT).await;

    let response = upload(format!("{gateway_url}/v1/audio/transcriptions"), UPLOAD).await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(upstream.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_undeclared_oversized_upload_is_cut_off_at_the_limit() {
    let upstream = Upstream::new(StatusCode::OK, "application/json", TRANSCRIPTION);
    let gateway_url = gateway_with_limit(&upstream.spawn().await, SMALL_LIMIT).await;

    let response = upload(format!("{gateway_url}/v1/audio/transcriptions"), chunked_upload()).await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // If the request reached the upstream, let it stop reading before counting what arrived:
    // crossing the limit must stop forwarding, so no more than the limit gets through.
    if upstream.calls.load(Ordering::SeqCst) > 0 {
        timeout(STREAM_WAIT, upstream.wait_until_finished())
            .await
            .expect("a cut-off upload must end the upstream request");
    }
    let forwarded = upstream.received.lock().await.body.len();
    assert!(
        forwarded <= SMALL_LIMIT,
        "forwarded {forwarded} bytes past the {SMALL_LIMIT}-byte limit"
    );
}
