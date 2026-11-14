// Test-only input adapter. HTTP framing stays in the production inference module.

#![cfg(test)]

use std::pin::Pin;
use std::time::Duration;

use futures::{Stream, StreamExt};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{ArrivalSchedule, DriverError, Limits, Result, check_size};
use crate::executor::error::ExecutorResult;

pub(super) type LineStream = Pin<Box<dyn Stream<Item = ExecutorResult<String>> + Send>>;

pub(super) struct ReadyLine {
    pub raw: String,
    pub due: Instant,
    pub bytes: u32,
}

#[tokio::test]
async fn replay_eof_does_not_wait_for_a_nonexistent_future_deadline() {
    let mut source = Source::replay(vec!["data: {}".to_owned()], Duration::from_secs(3600));
    let cancel = CancellationToken::new();
    assert!(source.next(&cancel, Limits::default()).await.unwrap().is_some());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), source.next(&cancel, Limits::default()))
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    assert_eq!(source.count(), 1);
}

#[tokio::test]
async fn pending_native_source_is_cancellation_aware() {
    let cancel = CancellationToken::new();
    let waiting = cancel.clone();
    let task = tokio::spawn(async move {
        let mut source = Source::http(Box::pin(futures::stream::pending()));
        source.next(&waiting, Limits::default()).await
    });
    tokio::task::yield_now().await;
    cancel.cancel();
    let joined = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(joined, Err(DriverError::Cancelled)));
}

#[tokio::test]
async fn native_source_rejects_oversized_input_before_ingestion() {
    let mut source = Source::http(Box::pin(futures::stream::iter([Ok("x".repeat(65_537))])));
    assert!(matches!(
        source.next(&CancellationToken::new(), Limits::default()).await,
        Err(DriverError::ItemLimit("input"))
    ));
    assert_eq!(source.count(), 0);
}

pub(in super::super) struct Source {
    lines: LineStream,
    arrival: Option<ArrivalSchedule>,
    count: usize,
    bytes: usize,
    known_lines: Option<usize>,
}

impl Source {
    pub fn replay(lines: Vec<String>, spacing: Duration) -> Self {
        Self::replay_at(lines, ArrivalSchedule::at(Instant::now(), spacing))
    }

    pub fn replay_at(lines: Vec<String>, schedule: ArrivalSchedule) -> Self {
        Self {
            known_lines: Some(lines.len()),
            lines: Box::pin(futures::stream::iter(lines.into_iter().map(Ok))),
            arrival: Some(schedule),
            count: 0,
            bytes: 0,
        }
    }

    pub fn http(lines: LineStream) -> Self {
        Self {
            lines,
            arrival: None,
            count: 0,
            bytes: 0,
            known_lines: None,
        }
    }

    pub fn is_http(&self) -> bool {
        self.arrival.is_none()
    }
    pub fn count(&self) -> usize {
        self.count
    }

    pub(super) fn remaining(&self) -> Option<usize> {
        self.known_lines.map(|count| count.saturating_sub(self.count))
    }

    pub(super) async fn next(&mut self, cancel: &CancellationToken, limits: Limits) -> Result<Option<ReadyLine>> {
        if self.known_lines == Some(self.count) {
            return Ok(None);
        }
        if let Some(arrival) = self.arrival
            && !arrival.wait(self.count, cancel).await?
        {
            return Err(DriverError::Cancelled);
        }
        let next = tokio::select! {
            () = cancel.cancelled() => return Err(DriverError::Cancelled),
            next = self.lines.next() => next,
        };
        let Some(line) = next else { return Ok(None) };
        // HTTP latency begins when native framing is observed ready, not when
        // a byte entered a socket or sat unpolled behind inline backpressure.
        let observed_ready = Instant::now();
        let raw = line.map_err(|error| DriverError::Transport(Box::new(error)))?;
        let bytes = check_size(raw.len(), limits.input_item_bytes, "input")?;
        self.bytes = self
            .bytes
            .checked_add(raw.len())
            .ok_or(DriverError::ItemLimit("source"))?;
        if self.count >= super::MAX_LINES || self.bytes > 16 * 1024 * 1024 {
            return Err(DriverError::ItemLimit("source"));
        }
        let due = self
            .arrival
            .map_or(Ok(observed_ready), |arrival| arrival.deadline(self.count))?;
        self.count += 1;
        Ok(Some(ReadyLine { raw, due, bytes }))
    }
}

#[tokio::test]
async fn shared_origin_remains_fixed_when_streams_are_first_polled_late() {
    let origin = Instant::now() - Duration::from_secs(1);
    let schedule = ArrivalSchedule::jittered_at(origin, Duration::from_millis(1));
    let cancel = CancellationToken::new();
    for _ in 0..4 {
        let mut source = Source::replay_at(vec!["data: {}".to_owned(); 2], schedule);
        assert_eq!(
            source.next(&cancel, Limits::default()).await.unwrap().unwrap().due,
            origin
        );
        assert_eq!(
            source.next(&cancel, Limits::default()).await.unwrap().unwrap().due,
            origin + Duration::from_millis(2)
        );
        assert!(source.next(&cancel, Limits::default()).await.unwrap().is_none());
    }
}
