//! Admission and run-time limits for background responses.

use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::error::Error;

pub const DEFAULT_BACKGROUND_MAX_CONCURRENT: NonZeroUsize = NonZeroUsize::new(16).expect("default is nonzero");
pub const DEFAULT_BACKGROUND_MAX_QUEUED: usize = 64;
pub const DEFAULT_BACKGROUND_TIMEOUT_SECONDS: NonZeroU64 = NonZeroU64::new(1800).expect("default is nonzero");

pub const BACKGROUND_MAX_CONCURRENT_ENV: &str = "AGENTIC_BACKGROUND_MAX_CONCURRENT";
pub const BACKGROUND_MAX_QUEUED_ENV: &str = "AGENTIC_BACKGROUND_MAX_QUEUED";
pub const BACKGROUND_TIMEOUT_SECONDS_ENV: &str = "AGENTIC_BACKGROUND_TIMEOUT_SECONDS";

/// Limits on the background jobs one gateway instance runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackgroundConfig {
    /// Jobs that run at once. Further admitted jobs wait as `queued`.
    pub max_concurrent: NonZeroUsize,
    /// Admitted jobs that may wait for a run slot. A request beyond both
    /// limits is rejected with `503` instead of waiting.
    pub max_queued: usize,
    /// Run time allowed per job, from `in_progress` to its terminal status.
    pub timeout: Duration,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            max_concurrent: DEFAULT_BACKGROUND_MAX_CONCURRENT,
            max_queued: DEFAULT_BACKGROUND_MAX_QUEUED,
            timeout: Duration::from_secs(DEFAULT_BACKGROUND_TIMEOUT_SECONDS.get()),
        }
    }
}

impl BackgroundConfig {
    /// Jobs admitted at once: running plus queued.
    ///
    /// # Errors
    /// Returns [`Error::Config`] when the sum exceeds what one semaphore can hold.
    pub fn max_admitted(&self) -> Result<usize, Error> {
        self.max_concurrent
            .get()
            .checked_add(self.max_queued)
            .filter(|admitted| *admitted <= Semaphore::MAX_PERMITS)
            .ok_or_else(|| {
                Error::Config(format!(
                    "background_max_concurrent ({}) plus background_max_queued ({}) cannot exceed {}",
                    self.max_concurrent,
                    self.max_queued,
                    Semaphore::MAX_PERMITS
                ))
            })
    }

    /// Validates the admission bound and a non-zero timeout.
    ///
    /// # Errors
    /// Returns [`Error::Config`] for an admission bound past [`Semaphore::MAX_PERMITS`] or a zero timeout.
    pub fn validate(&self) -> Result<(), Error> {
        self.max_admitted()?;
        if self.timeout.is_zero() {
            return Err(Error::Config(
                "background_timeout_seconds must be at least 1".to_owned(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_admit_running_plus_queued_jobs() {
        let config = BackgroundConfig::default();
        assert_eq!(config.max_concurrent.get(), 16);
        assert_eq!(config.max_queued, 64);
        assert_eq!(config.timeout.as_secs(), 1800);
        assert_eq!(config.max_admitted().unwrap(), 80);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn the_admission_bound_is_checked_at_the_semaphore_limit() {
        let one = NonZeroUsize::new(1).unwrap();
        let at_limit = BackgroundConfig {
            max_concurrent: one,
            max_queued: Semaphore::MAX_PERMITS - 1,
            ..BackgroundConfig::default()
        };
        assert_eq!(at_limit.max_admitted().unwrap(), Semaphore::MAX_PERMITS);
        let past_limit = BackgroundConfig {
            max_queued: Semaphore::MAX_PERMITS,
            ..at_limit
        };
        assert!(past_limit.validate().is_err());
        let overflow = BackgroundConfig {
            max_concurrent: NonZeroUsize::MAX,
            max_queued: 1,
            ..at_limit
        };
        assert!(overflow.validate().is_err());
        let no_queue = BackgroundConfig {
            max_queued: 0,
            ..at_limit
        };
        assert_eq!(no_queue.max_admitted().unwrap(), 1);
    }

    #[test]
    fn a_zero_timeout_is_rejected_and_one_second_is_accepted() {
        let zero = BackgroundConfig {
            timeout: Duration::ZERO,
            ..BackgroundConfig::default()
        };
        assert!(zero.validate().is_err());
        let one = BackgroundConfig {
            timeout: Duration::from_secs(1),
            ..BackgroundConfig::default()
        };
        assert!(one.validate().is_ok());
    }
}
