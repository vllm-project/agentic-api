// Optional, bounded send/receive wrappers; never add state to channel packets.

#![cfg(test)]

use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::mpsc;

use super::Limits;
use super::queue_math::{Envelope, Interval, summarize};

#[derive(Clone, Copy)]
struct Record {
    send_before: u128,
    send_after: Option<u128>,
    receive_before: Option<u128>,
    receive_after: Option<u128>,
    logical_bytes: u32,
}

struct History {
    records: Vec<Record>,
    invalid: bool,
}

#[derive(Clone)]
struct Recorder {
    origin: Instant,
    maximum: usize,
    history: Arc<Mutex<History>>,
}

impl Recorder {
    fn new(origin: Instant, maximum: usize) -> Self {
        Self {
            origin,
            maximum,
            history: Arc::new(Mutex::new(History {
                records: Vec::new(),
                invalid: false,
            })),
        }
    }
    fn begin_send(&self, bytes: u32) -> Option<usize> {
        let at = self.origin.elapsed().as_nanos();
        let mut history = self.history.lock().expect("observer mutex poisoned");
        if history.records.len() == self.maximum {
            history.invalid = true;
            return None;
        }
        let index = history.records.len();
        history.records.push(Record {
            send_before: at,
            send_after: None,
            receive_before: None,
            receive_after: None,
            logical_bytes: bytes,
        });
        Some(index)
    }
    fn end_send(&self, index: Option<usize>) {
        let at = self.origin.elapsed().as_nanos();
        let mut history = self.history.lock().expect("observer mutex poisoned");
        if let Some(record) = index.and_then(|index| history.records.get_mut(index)) {
            record.send_after = Some(at);
        }
    }
    fn received(&self, index: usize, before: Instant, after: Instant) {
        let mut history = self.history.lock().expect("observer mutex poisoned");
        if let Some(record) = history.records.get_mut(index) {
            record.receive_before = Some(before.duration_since(self.origin).as_nanos());
            record.receive_after = Some(after.duration_since(self.origin).as_nanos());
        } else {
            history.invalid = true;
        }
    }
    fn snapshot(&self, window: u128, capacity: usize, budget: u32) -> Result<Envelope, &'static str> {
        let history = self.history.lock().expect("observer mutex poisoned");
        if history.invalid {
            return Err("observer storage bound or FIFO correspondence failed");
        }
        let intervals: Result<Vec<Interval>, &'static str> = history
            .records
            .iter()
            .map(|record| {
                Ok(Interval {
                    send_before: record.send_before,
                    send_after: record.send_after.ok_or("send did not finish")?,
                    receive_before: record.receive_before.ok_or("record not received")?,
                    receive_after: record.receive_after.ok_or("record not received")?,
                    logical_bytes: record.logical_bytes,
                })
            })
            .collect();
        summarize(intervals?, window, capacity, budget)
    }
}

pub(super) struct SenderObserver(Option<Recorder>);
impl SenderObserver {
    pub fn send<T>(&self, slot: mpsc::Permit<'_, T>, value: T, bytes: u32) {
        if let Some(recorder) = &self.0 {
            let index = recorder.begin_send(bytes);
            slot.send(value);
            recorder.end_send(index);
        } else {
            slot.send(value);
        }
    }
}

pub(super) struct ReceiverObserver {
    recorder: Option<Recorder>,
    next: usize,
}
impl ReceiverObserver {
    fn observed<T>(&mut self, value: Option<T>, before: Option<Instant>) -> Option<T> {
        if value.is_some()
            && let (Some(recorder), Some(before)) = (&self.recorder, before)
        {
            recorder.received(self.next, before, Instant::now());
            self.next += 1;
        }
        value
    }
    pub fn blocking_recv<T>(&mut self, receiver: &mut mpsc::Receiver<T>) -> Option<T> {
        let before = self.recorder.as_ref().map(|_| Instant::now());
        let value = receiver.blocking_recv();
        self.observed(value, before)
    }
    pub async fn recv<T>(&mut self, receiver: &mut mpsc::Receiver<T>) -> Option<T> {
        let before = self.recorder.as_ref().map(|_| Instant::now());
        let value = receiver.recv().await;
        self.observed(value, before)
    }
}

pub(super) struct Pair {
    origin: Option<Instant>,
    input: Option<Recorder>,
    output: Option<Recorder>,
    limits: Limits,
}
pub(super) struct Trace {
    pair: Pair,
    window: u128,
}

#[derive(serde::Serialize)]
pub(super) struct Summary {
    pub scope: &'static str,
    pub input: Envelope,
    pub output: Envelope,
}

impl Pair {
    pub fn new(enabled: bool, limits: Limits, maximum: usize) -> Self {
        let origin = enabled.then(Instant::now);
        Self {
            origin,
            input: origin.map(|origin| Recorder::new(origin, maximum)),
            output: origin.map(|origin| Recorder::new(origin, maximum)),
            limits,
        }
    }
    pub fn input_sender(&self) -> SenderObserver {
        SenderObserver(self.input.clone())
    }
    pub fn output_sender(&self) -> SenderObserver {
        SenderObserver(self.output.clone())
    }
    pub fn input_receiver(&self) -> ReceiverObserver {
        ReceiverObserver {
            recorder: self.input.clone(),
            next: 0,
        }
    }
    pub fn output_receiver(&self) -> ReceiverObserver {
        ReceiverObserver {
            recorder: self.output.clone(),
            next: 0,
        }
    }
    pub fn finish(self) -> Option<Trace> {
        let origin = self.origin?;
        Some(Trace {
            window: origin.elapsed().as_nanos(),
            pair: self,
        })
    }
}
impl Trace {
    /// Called after trial clocks: sorting and histogram allocation are untimed.
    pub fn summarize(&self) -> Result<Summary, &'static str> {
        let limits = self.pair.limits;
        Ok(Summary {
            scope: "native_mpsc_queue_residence_linearization_bounds_excludes_reservations_and_service",
            input: self.pair.input.as_ref().ok_or("missing input observer")?.snapshot(
                self.window,
                limits.input_entries,
                limits.input_bytes,
            )?,
            output: self.pair.output.as_ref().ok_or("missing output observer")?.snapshot(
                self.window,
                limits.output_entries,
                limits.output_bytes,
            )?,
        })
    }
}

#[test]
fn incomplete_and_overflowed_recorders_cannot_issue_an_envelope() {
    let recorder = Recorder::new(Instant::now(), 1);
    let index = recorder.begin_send(1);
    recorder.end_send(index);
    assert!(recorder.snapshot(recorder.origin.elapsed().as_nanos(), 1, 10).is_err());
    assert_eq!(recorder.begin_send(1), None);
    assert!(recorder.snapshot(recorder.origin.elapsed().as_nanos(), 1, 10).is_err());
}
