//! Background admission bounds, the per-job timeout, shutdown, and a job that
//! lost its row: each limit is exercised at its edge.
#[allow(dead_code)]
mod background_support;
#[allow(dead_code)]
mod common;

use std::num::NonZeroUsize;
use std::time::Duration;

use agentic_core::config::{BackgroundConfig, Config};
use background_support::{Gateway, Model, TestDb, Turn, bounded, id};
use http::StatusCode;
use serde_json::{Value, json};

fn config_with(model: &Model, db: &TestDb, background: BackgroundConfig) -> Config {
    let mut config = Gateway::config(&model.url, db);
    config.responses.background = background;
    config
}

fn limits(max_concurrent: usize, max_queued: usize) -> BackgroundConfig {
    BackgroundConfig {
        max_concurrent: NonZeroUsize::new(max_concurrent).expect("nonzero"),
        max_queued,
        ..BackgroundConfig::default()
    }
}

async fn wait_idle_jobs(gateway: &Gateway, active: usize) {
    bounded("background jobs to finish", async {
        while gateway.exec.background_jobs().active() != active {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

fn capacity_error() -> Value {
    json!({"error": {
        "message": "background capacity is full; retry the request later",
        "type": "server_error", "code": "background_capacity_exceeded"
    }})
}

async fn create_raw(gateway: &Gateway) -> (StatusCode, Value) {
    gateway
        .create(json!({"model": "test-model", "input": "hi", "background": true}))
        .await
}

#[tokio::test]
async fn admission_holds_running_plus_queued_jobs_and_rejects_one_more() {
    let mut model = Model::start(vec![Turn::Hold("first"), Turn::Answer("later")]).await;
    let db = TestDb::new().await;
    let gateway = Gateway::start(&config_with(&model, &db, limits(1, 1))).await;

    let running = gateway.create_background(json!({})).await;
    model.wait_started(0).await;
    let queued = gateway.create_background(json!({})).await;
    assert_eq!(
        create_raw(&gateway).await,
        (StatusCode::SERVICE_UNAVAILABLE, capacity_error())
    );
    assert_eq!(gateway.get(id(&queued)).await.1["status"], "queued");

    // Cancelled before it started: the queued job never reaches the model and frees its place.
    let (status, cancelled) = gateway.cancel(id(&queued)).await;
    assert_eq!((status, &cancelled["status"]), (StatusCode::OK, &json!("cancelled")));
    wait_idle_jobs(&gateway, 1).await;
    let next = gateway.create_background(json!({})).await;
    assert_eq!(create_raw(&gateway).await.0, StatusCode::SERVICE_UNAVAILABLE);

    model.release();
    gateway.wait_for_status(id(&running), "completed").await;
    let finished = gateway.wait_for_status(id(&next), "completed").await;
    assert_eq!(finished["output"][0]["content"][0]["text"], "later");
    assert_eq!(
        model.request_count().await,
        2,
        "the cancelled queued job never called the model"
    );
    assert_eq!(gateway.get(id(&queued)).await.1, cancelled);
}

#[tokio::test]
async fn max_concurrent_jobs_run_together_and_a_zero_queue_rejects_the_next() {
    let mut model = Model::start(vec![Turn::Hold("a"), Turn::Hold("b")]).await;
    let db = TestDb::new().await;
    let gateway = Gateway::start(&config_with(&model, &db, limits(2, 0))).await;

    let first = gateway.create_background(json!({})).await;
    let second = gateway.create_background(json!({})).await;
    model.wait_started(1).await;
    assert_eq!(model.request_count().await, 2, "both jobs run at once");
    assert_eq!(
        create_raw(&gateway).await,
        (StatusCode::SERVICE_UNAVAILABLE, capacity_error())
    );

    model.release();
    model.release();
    gateway.wait_for_status(id(&first), "completed").await;
    gateway.wait_for_status(id(&second), "completed").await;
    wait_idle_jobs(&gateway, 0).await;
    let third = gateway.create_background(json!({})).await;
    gateway.wait_for_status(id(&third), "completed").await;
}

#[tokio::test]
async fn a_job_past_its_timeout_fails_and_one_within_it_completes() {
    let model = Model::start(vec![Turn::Answer("quick"), Turn::Hold("slow")]).await;
    let db = TestDb::new().await;
    let background = BackgroundConfig {
        timeout: Duration::from_secs(1),
        ..BackgroundConfig::default()
    };
    let gateway = Gateway::start(&config_with(&model, &db, background)).await;

    let quick = gateway.create_background(json!({})).await;
    gateway.wait_for_status(id(&quick), "completed").await;
    let slow = gateway.create_background(json!({})).await;
    let failed = gateway.wait_for_status(id(&slow), "failed").await;
    assert_eq!(
        failed["error"],
        json!({
            "message": "the background response exceeded its 1-second timeout",
            "type": "server_error", "code": "server_error"
        })
    );
    model.wait_dropped(1).await;
}

#[tokio::test]
async fn shutdown_fails_running_and_queued_jobs_and_rejects_new_ones() {
    let mut model = Model::start(vec![Turn::Hold("never")]).await;
    let db = TestDb::new().await;
    let gateway = Gateway::start(&config_with(&model, &db, limits(1, 1))).await;
    let running = gateway.create_background(json!({})).await;
    model.wait_started(0).await;
    let queued = gateway.create_background(json!({})).await;

    bounded("background shutdown", gateway.exec.background_jobs().shutdown()).await;
    assert_eq!(gateway.exec.background_jobs().active(), 0);
    model.wait_dropped(1).await;
    for response in [&running, &queued] {
        let (_, body) = gateway.get(id(response)).await;
        assert_eq!(body["status"], "failed", "{body}");
        assert_eq!(
            body["error"]["message"],
            "the gateway instance running this background response stopped before it finished"
        );
    }
    assert_eq!(model.request_count().await, 1, "the queued job never started");
    assert_eq!(
        create_raw(&gateway).await,
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": {
                "message": "the gateway is shutting down; retry the request on another instance",
                "type": "server_error", "code": "server_shutting_down"
            }})
        )
    );
}

#[tokio::test]
async fn a_job_that_lost_its_lease_cannot_overwrite_the_failed_row() {
    let mut model = Model::start(vec![Turn::Hold("too late")]).await;
    let db = TestDb::new().await;
    let gateway = Gateway::start(&Gateway::config(&model.url, &db)).await;
    let held = gateway.create_background(json!({})).await;
    model.wait_started(0).await;
    gateway.wait_for_status(id(&held), "in_progress").await;

    let pool = gateway.exec.storage_pool().expect("storage pool");
    sqlx::query("UPDATE responses SET background_lease_expires_at = 0 WHERE id = $1")
        .bind(id(&held))
        .execute(pool)
        .await
        .expect("expire the lease");
    let (_, failed) = gateway.get(id(&held)).await;
    assert_eq!(failed["status"], "failed", "retrieval fails a row whose lease expired");

    // The job still holds its model call; let it finish and try to store its result.
    model.release();
    wait_idle_jobs(&gateway, 0).await;
    assert_eq!(gateway.get(id(&held)).await.1, failed);
    let (status, items) = gateway.input_items(id(&held)).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "no history was stored for the failed row: {items}"
    );
}
