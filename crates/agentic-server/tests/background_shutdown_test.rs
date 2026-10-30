//! Process-level coverage of background jobs at gateway shutdown: the signal
//! stops admission and running jobs, and the drain records each row `failed`.

#![cfg(unix)]

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const TEST_TIMEOUT: Duration = Duration::from_secs(15);

/// A model server whose requests stay open; it reports each one it receives.
async fn held_model() -> (String, mpsc::Receiver<()>) {
    let (started_tx, started) = mpsc::channel(4);
    let app = axum::Router::new().route(
        "/v1/responses",
        axum::routing::post(move |_body: axum::body::Bytes| {
            let started_tx = started_tx.clone();
            async move {
                let _ = started_tx.try_send(());
                std::future::pending::<&'static str>().await
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind model");
    let url = format!("http://{}", listener.local_addr().expect("model address"));
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    (url, started)
}

struct Gateway {
    child: Child,
    directory: tempfile::TempDir,
}

impl Gateway {
    fn start(model_url: &str) -> Self {
        let directory = tempfile::tempdir().expect("temporary server home");
        let log = std::fs::File::create(directory.path().join("server.log")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentic-server"))
            .env_clear()
            .env("AGENTIC_API_HOME", directory.path())
            .env("RUST_LOG", "info")
            .env("NO_COLOR", "1")
            .args(["--llm-api-base", model_url, "--skip-llm-ready-check"])
            .args(["--gateway-host", "127.0.0.1", "--gateway-port", "0"])
            .args(["--db-url", &Self::database_url(&directory)])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("agentic-server must start");
        Self { child, directory }
    }

    fn database_url(directory: &tempfile::TempDir) -> String {
        format!("sqlite://{}", directory.path().join("agentic.db").display())
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.directory.path().join("server.log")).unwrap()
    }

    async fn address(&self) -> String {
        timeout(TEST_TIMEOUT, async {
            loop {
                let log = self.log();
                if let Some((_, rest)) = log.split_once("gateway listening on ") {
                    return rest
                        .chars()
                        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | ':'))
                        .collect();
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("gateway did not start: {}", self.log()))
    }

    async fn terminate(&mut self) -> std::process::ExitStatus {
        let killed = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        timeout(TEST_TIMEOUT, async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return status;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("gateway did not exit: {}", self.log()))
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn sigterm_records_a_running_background_response_failed() {
    let (model_url, mut started) = held_model().await;
    let mut gateway = Gateway::start(&model_url);
    let address = gateway.address().await;

    let queued: Value = timeout(TEST_TIMEOUT, async {
        reqwest::Client::new()
            .post(format!("http://{address}/v1/responses"))
            .json(&json!({"model": "test-model", "input": "hi", "background": true}))
            .send()
            .await
            .expect("gateway reachable")
            .json()
            .await
            .expect("queued response")
    })
    .await
    .expect("admission");
    assert_eq!(queued["status"], "queued", "{queued}");
    timeout(TEST_TIMEOUT, started.recv())
        .await
        .expect("the job reached the model")
        .unwrap();

    let status = gateway.terminate().await;
    assert!(status.success(), "shutdown failed: {status}; {}", gateway.log());

    let pool = agentic_core::storage::create_pool(Some(&Gateway::database_url(&gateway.directory)))
        .await
        .expect("open the gateway database");
    let (row_status, metadata): (String, String) =
        sqlx::query_as("SELECT background_status, metadata FROM responses WHERE id = $1")
            .bind(queued["id"].as_str().unwrap())
            .fetch_one(pool.as_ref())
            .await
            .expect("the background row");
    assert_eq!(row_status, "failed");
    let metadata: Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(
        metadata["response_snapshot"]["error"]["message"],
        "the gateway instance running this background response stopped before it finished"
    );
}
