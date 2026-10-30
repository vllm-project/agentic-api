//! Lease timing, recovery boundaries, and cross-instance cancellation, with a
//! lease short enough to observe renewals.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use super::*;
use crate::executor::modes::{ConversationHandler, ResponseHandler};
use crate::executor::{ExecuteRequest, ExecutionContext};
use crate::storage::{ConversationStore, ResponseStore, create_pool_with_schema};

const LIMIT: Duration = Duration::from_secs(10);
const SHORT: LeaseTiming = LeaseTiming {
    lease: Duration::from_millis(300),
    renew_every: Duration::from_millis(50),
};

async fn bounded<F: Future>(what: &str, future: F) -> F::Output {
    tokio::time::timeout(LIMIT, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

struct DropProbe(Arc<AtomicUsize>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A model server whose every request stays open until the gateway drops it.
struct HeldModel {
    url: String,
    started: mpsc::Receiver<()>,
    dropped: Arc<AtomicUsize>,
}

impl HeldModel {
    async fn start() -> Self {
        let (started_tx, started) = mpsc::channel(4);
        let dropped = Arc::new(AtomicUsize::new(0));
        let probe = Arc::clone(&dropped);
        let app = axum::Router::new().route(
            "/v1/responses",
            axum::routing::post(move |_body: axum::body::Bytes| {
                let (started_tx, probe) = (started_tx.clone(), DropProbe(Arc::clone(&probe)));
                async move {
                    let _probe = probe;
                    let _ = started_tx.try_send(());
                    std::future::pending::<&'static str>().await
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind model");
        let url = format!("http://{}", listener.local_addr().expect("model address"));
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        Self { url, started, dropped }
    }

    async fn wait_dropped(&self) {
        bounded("the model request to be dropped", async {
            while self.dropped.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    }
}

async fn instance(model_url: &str) -> (ExecutionContext, ResponseStore) {
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let store = ResponseStore::new(Arc::clone(&pool));
    let exec = ExecutionContext::new(
        ConversationHandler::new(ConversationStore::new(pool)),
        ResponseHandler::new(store.clone()),
        Arc::new(reqwest::Client::new()),
        model_url.to_owned(),
    )
    .with_background_timing(SHORT);
    (exec, store)
}

fn request() -> crate::types::request_response::RequestPayload {
    serde_json::from_value(serde_json::json!({"model": "m", "input": "hi", "background": true})).unwrap()
}

async fn status(store: &ResponseStore, id: &str) -> String {
    store.get(id).await.unwrap().metadata.response_snapshot.unwrap().status
}

#[tokio::test]
async fn renewals_keep_a_long_job_owned_and_a_remote_cancel_stops_it() {
    let mut model = HeldModel::start().await;
    let (exec, store) = instance(&model.url).await;
    let exec = Arc::new(exec);
    // Another instance over the same database: its own owner and no local jobs.
    let other = BackgroundJobs::with_timing(exec.resp_handler.clone(), exec.responses_config.background, SHORT);

    let queued = bounded(
        "admission",
        ExecuteRequest::new(request(), Arc::clone(&exec)).run_background(),
    )
    .await
    .unwrap();
    bounded("the model request", model.started.recv()).await.unwrap();

    // Four leases later the job is still owned: recovery elsewhere leaves it running.
    tokio::time::sleep(SHORT.lease * 4).await;
    assert_eq!(bounded("recovery", other.recover()).await.unwrap(), 0);
    assert_eq!(status(&store, &queued.id).await, "in_progress");

    let cancelled = bounded("a remote cancel", other.cancel(&queued.id)).await.unwrap();
    assert_eq!(cancelled.status, "cancelled");
    model.wait_dropped().await;
    bounded("the job to finish", async {
        while exec.background_jobs().active() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert_eq!(status(&store, &queued.id).await, "cancelled");
}

#[tokio::test]
async fn recovery_fails_a_row_only_once_its_lease_has_expired() {
    let (exec, store) = instance("http://127.0.0.1:9").await;
    let jobs = exec.background_jobs();
    let snapshot: ResponsePayload = serde_json::from_value(serde_json::json!({
        "id": "resp_stale", "object": "response", "created_at": 1, "model": "m", "status": "in_progress",
        "output": [], "usage": null, "incomplete_details": null, "error": null, "previous_response_id": null,
        "instructions": null, "background": true
    }))
    .unwrap();
    let expires_at = 1_000;
    store
        .enqueue_background("resp_stale", &row_metadata(&snapshot), 1, "bgo_stopped", expires_at)
        .await
        .unwrap();

    assert_eq!(
        jobs.recover_at(expires_at).await.unwrap(),
        0,
        "a lease is held through its expiry"
    );
    assert_eq!(status(&store, "resp_stale").await, "in_progress");
    // The conditional update enforces the same boundary on its own.
    let unchanged = row_metadata(&snapshot);
    for (now, moves) in [(expires_at, false), (expires_at + 1, true)] {
        let moved = store
            .transition_background(
                "resp_stale",
                &BackgroundStatus::ACTIVE,
                Writer::Recovery { now },
                BackgroundStatus::InProgress,
                &unchanged,
            )
            .await
            .unwrap();
        assert_eq!(moved, moves, "recovery at {now}");
    }
    assert_eq!(jobs.recover_at(expires_at + 1).await.unwrap(), 1);
    let failed = store
        .get("resp_stale")
        .await
        .unwrap()
        .metadata
        .response_snapshot
        .unwrap();
    assert_eq!(failed.status, "failed");
    assert_eq!(failed.error.as_ref().unwrap()["code"], "server_error");
    assert_eq!(
        jobs.recover_at(expires_at + 1).await.unwrap(),
        0,
        "a failed row stays failed"
    );
    assert!(
        !store
            .renew_background("resp_stale", "bgo_stopped", i64::MAX)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn recovery_reads_past_one_full_batch_of_expired_rows() {
    let (exec, store) = instance("http://127.0.0.1:9").await;
    let snapshot: ResponsePayload = serde_json::from_value(serde_json::json!({
        "id": "resp_x", "object": "response", "created_at": 1, "model": "m", "status": "queued",
        "output": [], "usage": null, "incomplete_details": null, "error": null, "previous_response_id": null,
        "instructions": null, "background": true
    }))
    .unwrap();
    let rows = usize::try_from(super::jobs::RECOVERY_BATCH).unwrap() + 1;
    for index in 0..rows {
        store
            .enqueue_background(
                &format!("resp_{index:03}"),
                &row_metadata(&snapshot),
                1,
                "bgo_stopped",
                1,
            )
            .await
            .unwrap();
    }
    assert_eq!(exec.background_jobs().recover_at(2).await.unwrap(), rows);
}

#[tokio::test]
async fn a_turn_that_lost_its_row_stores_none_of_its_items() {
    use crate::storage::response::ResponseWrite;
    use crate::types::io::{InOutItem, InputItem};

    let (_, store) = instance("http://127.0.0.1:9").await;
    let snapshot: ResponsePayload = serde_json::from_value(serde_json::json!({
        "id": "resp_lost", "object": "response", "created_at": 1, "model": "m", "status": "in_progress",
        "output": [], "usage": null, "incomplete_details": null, "error": null, "previous_response_id": null,
        "instructions": null, "background": true
    }))
    .unwrap();
    store
        .enqueue_background("resp_lost", &row_metadata(&snapshot), 1, "bgo_live", i64::MAX)
        .await
        .unwrap();
    let cancelled = with_status(snapshot, BackgroundStatus::Cancelled, None);
    assert!(
        store
            .transition_background(
                "resp_lost",
                &BackgroundStatus::ACTIVE,
                Writer::Client,
                BackgroundStatus::Cancelled,
                &row_metadata(&cancelled),
            )
            .await
            .unwrap()
    );

    let item: InputItem = serde_json::from_value(
        serde_json::json!({"type": "message", "id": "msg_kept_out", "role": "user", "content": "x"}),
    )
    .unwrap();
    let write = ResponseWrite::FinalizeBackground {
        owner: "bgo_live",
        status: BackgroundStatus::Completed,
    };
    let error = store
        .persist_with_conversation_id(
            "resp_lost",
            None,
            None,
            vec![InOutItem::Input(item)],
            &ResponseMetadata::default(),
            write,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::storage::StorageError::BackgroundClaimLost { .. }),
        "{error:?}"
    );
    assert!(
        store.get_input_item("msg_kept_out").await.unwrap().is_none(),
        "the item insert rolled back"
    );
    assert_eq!(status(&store, "resp_lost").await, "cancelled");
}
