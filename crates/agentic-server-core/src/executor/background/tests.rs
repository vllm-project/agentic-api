//! Lease timing, recovery boundaries, and cross-instance cancellation, with a
//! lease short enough to observe renewals.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use super::*;
use crate::executor::modes::{ConversationHandler, ResponseHandler};
use crate::executor::{ExecuteRequest, ExecutionContext};
use crate::storage::models::background::Claim;
use crate::storage::{
    ConversationStore, FinishedTurn, QueuedTurn, ResponseMetadata, ResponseStore, StorageError, create_pool_with_schema,
};
use crate::types::io::{InOutItem, InputItem, OutputItem};

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

fn request() -> RequestPayload {
    serde_json::from_value(serde_json::json!({"model": "m", "input": "hi", "background": true})).unwrap()
}

async fn status(store: &ResponseStore, id: &str) -> String {
    stored(store, id).await.status
}

async fn stored(store: &ResponseStore, id: &str) -> ResponsePayload {
    store.get_snapshot(id).await.unwrap().expect("background snapshot")
}

fn snapshot(id: &str, status: &str) -> ResponsePayload {
    serde_json::from_value(serde_json::json!({
        "id": id, "object": "response", "created_at": 1, "model": "m", "status": status,
        "output": [], "usage": null, "incomplete_details": null, "error": null, "previous_response_id": null,
        "instructions": null, "background": true
    }))
    .unwrap()
}

/// Store `snapshot` as a background row of `owner` whose lease expires at `lease_expires_at`, with `items`.
async fn enqueue(
    store: &ResponseStore,
    snapshot: &ResponsePayload,
    items: Vec<InOutItem>,
    owner: &str,
    lease_expires_at: i64,
) {
    let turn = QueuedTurn {
        response_id: &snapshot.id,
        items,
        snapshot,
        metadata: &ResponseMetadata::default(),
        claim: Claim {
            owner,
            lease_expires_at,
        },
    };
    store.enqueue_background(None, turn).await.unwrap();
}

fn message(id: &str) -> InOutItem {
    InOutItem::Input(
        serde_json::from_value::<InputItem>(
            serde_json::json!({"type": "message", "id": id, "role": "user", "content": id}),
        )
        .unwrap(),
    )
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
    let snapshot = snapshot("resp_stale", "in_progress");
    let expires_at = 1_000;
    enqueue(&store, &snapshot, Vec::new(), "bgo_stopped", expires_at).await;

    assert_eq!(
        jobs.recover_at(expires_at).await.unwrap(),
        0,
        "a lease is held through its expiry"
    );
    assert_eq!(status(&store, "resp_stale").await, "in_progress");
    // The conditional update enforces the same boundary on its own.
    for (now, moves) in [(expires_at, false), (expires_at + 1, true)] {
        let moved = store
            .transition_background(
                "resp_stale",
                &BackgroundStatus::ACTIVE,
                Writer::Recovery { now },
                BackgroundStatus::InProgress,
                &snapshot,
            )
            .await
            .unwrap();
        assert_eq!(moved, moves, "recovery at {now}");
    }
    assert_eq!(jobs.recover_at(expires_at + 1).await.unwrap(), 1);
    let failed = stored(&store, "resp_stale").await;
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
    let rows = usize::try_from(super::jobs::RECOVERY_BATCH).unwrap() + 1;
    for index in 0..rows {
        enqueue(
            &store,
            &snapshot(&format!("resp_{index:03}"), "queued"),
            Vec::new(),
            "bgo_stopped",
            1,
        )
        .await;
    }
    assert_eq!(exec.background_jobs().recover_at(2).await.unwrap(), rows);
}

#[tokio::test]
async fn a_queued_turn_lists_its_input_and_finishing_appends_its_output() {
    let (_, store) = instance("http://127.0.0.1:9").await;
    let queued = snapshot("resp_turn", "queued");
    enqueue(&store, &queued, vec![message("msg_input")], "bgo_live", i64::MAX).await;
    let listed = |items: Option<Vec<InOutItem>>| -> Vec<String> {
        items
            .unwrap()
            .iter()
            .filter_map(InOutItem::id)
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(listed(store.get_input_items("resp_turn").await.unwrap()), ["msg_input"]);

    let running = with_status(queued, BackgroundStatus::InProgress, None);
    let claimed = store
        .transition_background(
            "resp_turn",
            &[BackgroundStatus::Queued],
            Writer::Owner("bgo_live"),
            BackgroundStatus::InProgress,
            &running,
        )
        .await
        .unwrap();
    assert!(claimed);
    let output: OutputItem = serde_json::from_value(serde_json::json!({
        "type": "message", "id": "msg_output", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": "OK", "annotations": []}]
    }))
    .unwrap();
    let mut completed = with_status(running, BackgroundStatus::Completed, None);
    completed.output = vec![output.clone()];
    let history = store
        .finish_background(
            "resp_turn",
            "bgo_live",
            FinishedTurn {
                status: BackgroundStatus::Completed,
                queued: vec![message("msg_input")],
                items: vec![InOutItem::Output(output)],
                snapshot: &completed,
                metadata: &ResponseMetadata::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(history.len(), 2, "the queued input, then the output");
    assert_eq!(listed(store.get_input_items("resp_turn").await.unwrap()), ["msg_input"]);
    let (_, continued) = store.rehydrate("resp_turn").await.unwrap();
    let continued: Vec<_> = continued.iter().filter_map(InOutItem::id).collect();
    assert_eq!(continued, ["msg_input", "msg_output"]);
    assert_eq!(status(&store, "resp_turn").await, "completed");
}

#[tokio::test]
async fn a_turn_that_lost_its_row_stores_none_of_its_items() {
    let (_, store) = instance("http://127.0.0.1:9").await;
    let running = snapshot("resp_lost", "in_progress");
    enqueue(&store, &running, Vec::new(), "bgo_live", i64::MAX).await;
    let cancelled = with_status(running, BackgroundStatus::Cancelled, None);
    assert!(
        store
            .transition_background(
                "resp_lost",
                &BackgroundStatus::ACTIVE,
                Writer::Client,
                BackgroundStatus::Cancelled,
                &cancelled,
            )
            .await
            .unwrap()
    );

    let error = store
        .finish_background(
            "resp_lost",
            "bgo_live",
            FinishedTurn {
                status: BackgroundStatus::Completed,
                queued: Vec::new(),
                items: vec![message("msg_kept_out")],
                snapshot: &cancelled,
                metadata: &ResponseMetadata::default(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::BackgroundClaimLost { .. }), "{error:?}");
    assert!(
        store
            .get_input_items_by_id(&["msg_kept_out".to_owned()])
            .await
            .unwrap()
            .is_empty(),
        "the item insert rolled back"
    );
    assert_eq!(status(&store, "resp_lost").await, "cancelled");
}
