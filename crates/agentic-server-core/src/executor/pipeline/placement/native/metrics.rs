// Observer quantities are named by their scope, not advertised as client receipt.

#![cfg(test)]

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::executor::translate::Translation;

/// Correctness-only progress signal. Timed settings reject this probe.
#[derive(Clone, Default)]
pub(in super::super) struct ClientProbe(Arc<AtomicUsize>);

impl ClientProbe {
    pub fn observe(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    pub fn frames(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(serde::Serialize)]
pub(in super::super) struct Observation {
    pub due_to_ingestion_ns: u128,
    pub due_to_relay_return_ns: u128,
}

#[derive(Clone, Default)]
pub(in super::super) struct Ownership {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    workers: Arc<AtomicUsize>,
    feeders: Arc<AtomicUsize>,
    clients: Arc<AtomicUsize>,
}

impl Ownership {
    pub(super) fn start(&self) -> Active {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        Active(self.clone())
    }
    pub fn worker_joined(&self) {
        self.workers.fetch_add(1, Ordering::SeqCst);
    }
    pub fn feeder_joined(&self) {
        self.feeders.fetch_add(1, Ordering::SeqCst);
    }
    pub fn client_joined(&self) {
        self.clients.fetch_add(1, Ordering::SeqCst);
    }
    pub fn state(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.active.load(Ordering::SeqCst),
            self.peak.load(Ordering::SeqCst),
            self.workers.load(Ordering::SeqCst),
            self.feeders.load(Ordering::SeqCst),
            self.clients.load(Ordering::SeqCst),
        )
    }
}

pub(super) struct Active(Ownership);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

struct ByteCounter(usize);
impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("wire-size overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn logical_wire_bytes(translation: &Translation) -> serde_json::Result<usize> {
    let mut writer = ByteCounter(std::mem::size_of::<Translation>());
    for frame in &translation.frames {
        serde_json::to_writer(&mut writer, &frame.wire)?;
    }
    Ok(writer.0)
}
