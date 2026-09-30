//! The gateway listener disables Nagle's algorithm on accepted connections,
//! so streamed frames are not held back by the client's delayed
//! acknowledgements (#376).

use std::convert::Infallible;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::routing::get;
use axum::serve::Listener as _;
use futures::StreamExt as _;
use tokio::net::{TcpListener, TcpStream};

use agentic_server::app::gateway_listener;

/// A delayed acknowledgement holds a frame back for about 40 ms on Linux; an
/// unstalled response on loopback takes a few milliseconds.
const STALL_THRESHOLD: Duration = Duration::from_millis(25);

#[tokio::test]
async fn accepted_connections_disable_nagle() {
    let mut listener = gateway_listener(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let client = tokio::spawn(TcpStream::connect(listener.local_addr().unwrap()));

    let (accepted, _) = listener.accept().await;

    assert!(accepted.nodelay().unwrap(), "TCP_NODELAY is set on the accepted socket");
    client.await.unwrap().unwrap();
}

/// Five small frames written a millisecond apart, as a streamed response is.
async fn stream_frames() -> Body {
    let frames = futures::stream::iter(0..5).then(|frame| async move {
        tokio::time::sleep(Duration::from_millis(1)).await;
        Ok::<_, Infallible>(Bytes::from(format!("data: {{\"frame\":{frame}}}\n\n")))
    });
    Body::from_stream(frames)
}

#[tokio::test]
async fn streamed_frames_are_not_held_for_delayed_acknowledgements() {
    let listener = gateway_listener(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let url = format!("http://{}/stream", listener.local_addr().unwrap());
    let server = tokio::spawn(axum::serve(listener, Router::new().route("/stream", get(stream_frames))).into_future());
    // One keep-alive connection. Linux acknowledges the first segments of a
    // connection immediately, so warm it up before measuring.
    let client = reqwest::Client::new();
    let fetch = || async {
        let started = Instant::now();
        let body = client.get(&url).send().await.unwrap().bytes().await.unwrap();
        assert!(body.ends_with(b"{\"frame\":4}\n\n"));
        started.elapsed()
    };
    for _ in 0..20 {
        fetch().await;
    }
    let mut elapsed = Vec::new();
    for _ in 0..9 {
        elapsed.push(fetch().await);
    }
    elapsed.sort();
    server.abort();

    let median = elapsed[elapsed.len() / 2];
    assert!(
        median < STALL_THRESHOLD,
        "streamed responses took a median {median:?} ({elapsed:?}); frames are waiting for delayed ACKs"
    );
}
