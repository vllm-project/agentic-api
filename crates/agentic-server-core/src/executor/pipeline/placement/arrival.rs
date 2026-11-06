// Controlled replay arrival schedule; not a claim about recorded model timings.

#![cfg(test)]

use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(super) enum ArrivalError {
    #[error("replay arrival schedule has too many lines")]
    TooManyLines,
    #[error("replay arrival schedule exceeds the monotonic clock range")]
    Overflow,
}

#[derive(Clone, Copy)]
pub(super) struct ArrivalSchedule {
    origin: Instant,
    spacing: Duration,
    jittered: bool,
}

impl ArrivalSchedule {
    pub fn at(origin: Instant, spacing: Duration) -> Self {
        Self {
            origin,
            spacing,
            jittered: false,
        }
    }

    /// Repeating controlled gaps [2, 0, 1, 1] * spacing, not recorded model jitter.
    pub fn jittered_at(origin: Instant, spacing: Duration) -> Self {
        Self {
            origin,
            spacing,
            jittered: true,
        }
    }

    /// Every offset is anchored to one origin, never to the previous poll or
    /// consumer completion. Backpressure can delay admission but not arrival.
    pub fn deadline(self, index: usize) -> Result<Instant, ArrivalError> {
        let index = u32::try_from(index).map_err(|_| ArrivalError::TooManyLines)?;
        let steps = if self.jittered {
            let prefix = [0u32, 2, 2, 3][usize::try_from(index % 4).map_err(|_| ArrivalError::Overflow)?];
            (index / 4)
                .checked_mul(4)
                .and_then(|cycle| cycle.checked_add(prefix))
                .ok_or(ArrivalError::Overflow)?
        } else {
            index
        };
        let offset = self.spacing.checked_mul(steps).ok_or(ArrivalError::Overflow)?;
        self.origin.checked_add(offset).ok_or(ArrivalError::Overflow)
    }

    /// False means cancellation, including a burst whose deadline is already due.
    pub async fn wait(self, index: usize, cancel: &CancellationToken) -> Result<bool, ArrivalError> {
        if cancel.is_cancelled() {
            return Ok(false);
        }
        let deadline = self.deadline(index)?;
        if deadline <= Instant::now() {
            return Ok(true);
        }
        tokio::select! {
            () = cancel.cancelled() => Ok(false),
            () = tokio::time::sleep_until(deadline) => Ok(true),
        }
    }
}

#[test]
fn controlled_jitter_offsets_repeat_without_reanchoring_or_randomness() {
    let origin = Instant::now();
    let schedule = ArrivalSchedule::jittered_at(origin, Duration::from_millis(1));
    for (index, millis) in [0, 2, 2, 3, 4, 6, 6, 7, 8].into_iter().enumerate() {
        assert_eq!(
            schedule.deadline(index).unwrap(),
            origin + Duration::from_millis(millis)
        );
    }
    assert_eq!(
        ArrivalSchedule::jittered_at(origin, Duration::MAX).deadline(1),
        Err(ArrivalError::Overflow)
    );
}

#[test]
fn fixed_offsets_do_not_restart_after_a_consumer_poll() {
    let origin = Instant::now();
    let schedule = ArrivalSchedule::at(origin, Duration::from_millis(10));
    assert_eq!(schedule.deadline(0).unwrap(), origin);
    assert_eq!(schedule.deadline(4).unwrap(), origin + Duration::from_millis(40));
}

#[test]
fn late_poll_keeps_the_original_due_time() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let origin = Instant::now() - Duration::from_secs(1);
            let schedule = ArrivalSchedule::at(origin, Duration::from_millis(10));
            let due = origin + Duration::from_millis(40);
            assert_eq!(schedule.deadline(4).unwrap(), due);
            assert!(schedule.wait(4, &CancellationToken::new()).await.unwrap());
            assert_eq!(schedule.deadline(4).unwrap(), due);
        });
}

#[test]
fn cancellation_wakes_a_long_future_arrival() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let schedule = ArrivalSchedule::at(Instant::now(), Duration::from_secs(3600));
            let cancel = CancellationToken::new();
            let waiting = cancel.clone();
            let task = tokio::spawn(async move { schedule.wait(1, &waiting).await });
            tokio::task::yield_now().await;
            cancel.cancel();
            assert!(
                !tokio::time::timeout(Duration::from_secs(1), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
            );
        });
}

#[test]
fn cancelled_burst_does_not_admit_another_line() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let schedule = ArrivalSchedule::at(Instant::now(), Duration::ZERO);
            let cancel = CancellationToken::new();
            cancel.cancel();
            assert!(!schedule.wait(0, &cancel).await.unwrap());
        });
}

#[test]
fn impossible_offsets_are_errors_not_panics_or_saturation() {
    let schedule = ArrivalSchedule::at(Instant::now(), Duration::MAX);
    assert_eq!(schedule.deadline(2), Err(ArrivalError::Overflow));
    if let Ok(index) = usize::try_from(u64::from(u32::MAX) + 1) {
        assert_eq!(schedule.deadline(index), Err(ArrivalError::TooManyLines));
    }
}
