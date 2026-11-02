//! Lifecycle state of a background response row.

use std::fmt;
use std::str::FromStr;

/// The `status` of a response created with `background: true`, as stored in
/// `responses.background_status` and reported in its snapshot.
///
/// `queued` and `in_progress` are active; every other status is terminal and
/// is never left once written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackgroundStatus {
    Queued,
    InProgress,
    Completed,
    Incomplete,
    Failed,
    Cancelled,
}

impl BackgroundStatus {
    /// The statuses a job or a cancellation can still move a row out of.
    pub const ACTIVE: [Self; 2] = [Self::Queued, Self::InProgress];

    /// Canonical Responses wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether a job may still run for this row.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::InProgress)
    }

    /// Whether the response can be continued or listed like a foreground response.
    #[must_use]
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Incomplete)
    }
}

impl fmt::Display for BackgroundStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A status string that is not a background response status.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("'{0}' is not a background response status")]
pub struct UnknownBackgroundStatus(pub String);

impl FromStr for BackgroundStatus {
    type Err = UnknownBackgroundStatus;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "queued" => Ok(Self::Queued),
            "in_progress" => Ok(Self::InProgress),
            "completed" => Ok(Self::Completed),
            "incomplete" => Ok(Self::Incomplete),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(UnknownBackgroundStatus(other.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [BackgroundStatus; 6] = [
        BackgroundStatus::Queued,
        BackgroundStatus::InProgress,
        BackgroundStatus::Completed,
        BackgroundStatus::Incomplete,
        BackgroundStatus::Failed,
        BackgroundStatus::Cancelled,
    ];

    #[test]
    fn every_status_round_trips_its_wire_string() {
        for status in ALL {
            assert_eq!(status.as_str().parse::<BackgroundStatus>(), Ok(status));
            assert_eq!(status.to_string(), status.as_str());
        }
        assert_eq!(
            "error".parse::<BackgroundStatus>(),
            Err(UnknownBackgroundStatus("error".to_owned()))
        );
    }

    #[test]
    fn only_queued_and_in_progress_are_active_and_only_finished_rows_continue() {
        let active: Vec<_> = ALL.into_iter().filter(|status| status.is_active()).collect();
        assert_eq!(active, BackgroundStatus::ACTIVE);
        let finished: Vec<_> = ALL.into_iter().filter(|status| status.is_finished()).collect();
        assert_eq!(finished, [BackgroundStatus::Completed, BackgroundStatus::Incomplete]);
    }
}
