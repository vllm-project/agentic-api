//! Upstream frames withheld behind a hidden gateway call, and their byte account.
use std::collections::VecDeque;

use super::RelayLimits;
use crate::events::EventFrame;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::utils::common::serialize_to_string;

/// Which deferred frames a release admits. Indexes are the round's local
/// output indexes, before the relay applies the round's offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::executor) enum Release {
    /// Indexed frames below this output index; index-less frames stay deferred.
    Below(u64),
    /// Frames through one output item, with that item's frames presented at
    /// `public_index`: refused calls omitted from the public output shift later
    /// items down. Index-less frames stay deferred.
    Item { index: u64, public_index: u64 },
    /// Every frame: indexed frames in output order, then index-less frames.
    All,
}

impl Release {
    fn admits(self, frame: &EventFrame) -> bool {
        match self {
            Self::Below(bound) => frame.wire.output_index.is_some_and(|index| index < bound),
            Self::Item { index, .. } => frame
                .wire
                .output_index
                .is_some_and(|output_index| output_index <= index),
            Self::All => true,
        }
    }

    /// Where an admitted frame is presented within the round.
    pub(super) fn position(self, output_index: Option<u64>) -> Option<u64> {
        match self {
            Self::Item { index, public_index } if output_index == Some(index) => Some(public_index),
            Self::Below(_) | Self::Item { .. } | Self::All => output_index,
        }
    }
}

struct DeferredFrame {
    frame: EventFrame,
    /// Serialized wire bytes, charged on entry and refunded on release.
    bytes: usize,
}

#[derive(Default)]
pub(super) struct DeferredFrames {
    /// Output index of the first hidden gateway call, while one is open.
    window: Option<u64>,
    /// Release order: indexed frames by output index, then index-less frames,
    /// each in arrival order.
    frames: VecDeque<DeferredFrame>,
    bytes: usize,
}

impl DeferredFrames {
    pub(super) fn begin_round(&mut self) -> ExecutorResult<()> {
        if !self.frames.is_empty() {
            return Err(ExecutorError::StreamError(format!(
                "previous inference round left {} deferred stream events unreleased",
                self.frames.len()
            )));
        }
        self.window = None;
        Ok(())
    }

    /// Adopt ingestion's current window, returning the release it allows if it moved.
    pub(super) fn move_window(&mut self, window: Option<u64>) -> Option<Release> {
        if self.window == window {
            return None;
        }
        self.window = window;
        Some(window.map_or(Release::All, Release::Below))
    }

    /// An open window hides index-less frames and frames at or after its first hidden call.
    pub(super) fn withholds(&self, frame: &EventFrame) -> bool {
        self.window.is_some_and(|first_hidden| {
            frame
                .wire
                .output_index
                .is_none_or(|output_index| output_index >= first_hidden)
        })
    }

    pub(super) fn push(&mut self, frame: EventFrame, limits: &RelayLimits) -> ExecutorResult<()> {
        if self.frames.len() >= limits.deferred_frames {
            return Err(ExecutorError::StreamError(format!(
                "deferred stream exceeded {} buffered events",
                limits.deferred_frames
            )));
        }
        let bytes = serialize_to_string(&frame.wire)
            .map_err(ExecutorError::JsonError)?
            .len();
        if bytes > limits.deferred_bytes {
            return Err(ExecutorError::StreamError(format!(
                "deferred stream event of {bytes} bytes exceeds the {} buffered-byte limit",
                limits.deferred_bytes
            )));
        }
        let total = self.bytes.saturating_add(bytes);
        if total > limits.deferred_bytes {
            return Err(ExecutorError::StreamError(format!(
                "deferred stream exceeded {} buffered bytes",
                limits.deferred_bytes
            )));
        }
        let key = release_key(&frame);
        let at = self.frames.partition_point(|queued| release_key(&queued.frame) <= key);
        self.frames.insert(at, DeferredFrame { frame, bytes });
        self.bytes = total;
        Ok(())
    }

    /// The next frame in release order, if `release` admits it.
    pub(super) fn next(&self, release: Release) -> Option<&EventFrame> {
        self.frames
            .front()
            .map(|deferred| &deferred.frame)
            .filter(|frame| release.admits(frame))
    }

    /// Drop the frame [`Self::next`] returned once the sink has accepted it.
    pub(super) fn pop_front(&mut self) {
        if let Some(released) = self.frames.pop_front() {
            self.bytes = self.bytes.saturating_sub(released.bytes);
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.frames.len()
    }

    #[cfg(test)]
    pub(super) const fn bytes(&self) -> usize {
        self.bytes
    }
}

fn release_key(frame: &EventFrame) -> (bool, u64) {
    frame
        .wire
        .output_index
        .map_or((true, 0), |output_index| (false, output_index))
}
