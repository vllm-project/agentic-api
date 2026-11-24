//! Stream-owned orchestration lifetime. No detached producer or async drop reaper.

use std::{future::Future, panic::AssertUnwindSafe};

use async_stream::stream;
use futures::{FutureExt, Stream};
use tokio::sync::mpsc;

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::gateway_accumulator::StreamEvent;

pub(super) enum ProducerEvent<T> {
    Event(StreamEvent),
    Finished(ExecutorResult<T>),
}

/// Poll orchestration and its existing bounded event receiver on the consumer's
/// task. A dropped or unpolled consumer cannot leave a producer running elsewhere.
/// The producer is disposed before buffered events and its outcome are drained.
/// No additional buffering, semantic processing, or client serialization occurs.
pub(super) fn drive<T: Send>(
    producer: impl Future<Output = T> + Send,
    mut events: mpsc::Receiver<StreamEvent>,
) -> impl Stream<Item = ProducerEvent<T>> + Send {
    stream! {
        // Match the previous task's panic isolation without exposing panic data
        // in client errors. This does not replace the process-wide panic hook.
        let mut producer = Box::pin(AssertUnwindSafe(producer).catch_unwind());
        let outcome = loop {
            tokio::select! {
                Some(event) = events.recv() => yield ProducerEvent::Event(event),
                outcome = &mut producer => break outcome,
            }
        };
        // In particular, a caught panic must not retain the request's continuation
        // lease or active tool futures while the client drains queued events.
        drop(producer);
        let outcome = outcome.map_err(|_| ExecutorError::StreamProducerPanicked);
        while let Ok(event) = events.try_recv() {
            yield ProducerEvent::Event(event);
        }
        yield ProducerEvent::Finished(outcome);
    }
}

#[cfg(test)]
mod tests;
