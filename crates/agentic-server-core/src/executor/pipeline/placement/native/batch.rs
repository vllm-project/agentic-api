// One owned native replay batch; cancellation is cooperative and every task joins.

#![cfg(test)]

use std::time::Duration;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{ArrivalSchedule, DriverError, Outcome, Ownership, Result, Settings, run as stream_run};

pub(super) async fn run(settings: &Settings, prepared: Vec<Vec<String>>) -> Result<(Vec<Outcome>, Ownership)> {
    let owner = Ownership::default();
    let mut settings = settings.clone();
    if settings.replay_schedule.is_none() {
        settings.replay_schedule = Some(ArrivalSchedule::at(Instant::now(), settings.arrival_spacing));
        settings.arrival_spacing = Duration::ZERO;
    }
    let outcomes = with_deadline(
        &settings,
        prepared,
        CancellationToken::new(),
        owner.clone(),
        Duration::from_secs(20),
    )
    .await?;
    Ok((outcomes, owner))
}

pub(super) async fn with_deadline(
    settings: &Settings,
    prepared: Vec<Vec<String>>,
    cancel: CancellationToken,
    owner: Ownership,
    budget: Duration,
) -> Result<Vec<Outcome>> {
    let pending = controlled(settings, prepared, cancel.clone(), owner);
    tokio::pin!(pending);
    tokio::select! {
        result = &mut pending => result,
        () = tokio::time::sleep(budget) => {
            cancel.cancel();
            // Do not drop the batch/JoinSet: observe every native owner teardown.
            let result = pending.await;
            result.and(Err(DriverError::Cancelled))
        }
    }
}

pub(super) async fn controlled(
    settings: &Settings,
    prepared: Vec<Vec<String>>,
    cancel: CancellationToken,
    owner: Ownership,
) -> Result<Vec<Outcome>> {
    let mut tasks = JoinSet::new();
    for source in prepared {
        tasks.spawn(stream_run(settings.clone(), source, cancel.clone(), owner.clone()));
    }
    let mut results = Vec::new();
    let mut error = None;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(value)) => results.push(value),
            Ok(Err(cause)) => {
                cancel.cancel();
                error.get_or_insert(cause);
            }
            Err(_) => {
                cancel.cancel();
                error.get_or_insert(DriverError::Join);
            }
        }
    }
    if let Some(error) = error {
        return Err(error);
    }
    Ok(results)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cooperative_deadline_observes_worker_feeder_and_client_join() {
    let owner = Ownership::default();
    let mut settings = Settings::new(super::Placement::Worker, true);
    settings.replay_schedule = Some(ArrivalSchedule::at(Instant::now(), Duration::from_secs(3600)));
    let lines = super::super::fixtures::recorded_rounds().remove(0);
    let result = with_deadline(
        &settings,
        vec![lines; 4],
        CancellationToken::new(),
        owner.clone(),
        Duration::from_millis(10),
    )
    .await;
    assert!(matches!(result, Err(DriverError::Cancelled)));
    assert_eq!(
        (owner.state().0, owner.state().2, owner.state().3, owner.state().4),
        (0, 4, 4, 4)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn primary_native_batch_error_is_retained_after_all_owners_join() {
    let owner = Ownership::default();
    let settings = Settings::new(super::Placement::Worker, true);
    let result = controlled(
        &settings,
        vec![vec!["data: {".to_owned()]; 4],
        CancellationToken::new(),
        owner.clone(),
    )
    .await;
    assert!(matches!(result, Err(DriverError::Ingestion(_))));
    assert_eq!(
        (owner.state().0, owner.state().2, owner.state().3, owner.state().4),
        (0, 4, 4, 4)
    );
}
