// Bounded typed worker transport around the same native synchronous ingestion.

#![cfg(test)]

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use super::queue::{Pair, ReceiverObserver, SenderObserver, Trace};
use super::{
    Consumer, DriverError, Input, MAX_LINES, Ownership, QueuedOutput, Result, Settings, Source, finish, pipeline, push,
};
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::types::request_response::ResponsePayload;

type Finished = (ResponsePayload, Vec<usize>, Vec<usize>, usize, Option<Trace>);

async fn feed(
    sender: mpsc::Sender<Input>,
    mut source: Source,
    budget: Arc<Semaphore>,
    settings: Settings,
    cancel: CancellationToken,
    observer: SenderObserver,
) -> Result<(Vec<usize>, usize)> {
    let mut occupancy = Vec::new();
    while let Some(next) = source.next(&cancel, settings.limits).await? {
        if source.count() == 2
            && let Some(probe) = &settings.early_failure_probe
        {
            probe.pause_before_second_input(&cancel).await;
        }
        let bytes = tokio::select! {
            () = cancel.cancelled() => return Err(DriverError::Cancelled),
            bytes = Arc::clone(&budget).acquire_many_owned(next.bytes) => bytes.map_err(|_| DriverError::ChannelClosed)?,
        };
        let slot = tokio::select! {
            () = cancel.cancelled() => return Err(DriverError::Cancelled),
            slot = sender.reserve() => slot.map_err(|_| DriverError::ChannelClosed)?,
        };
        occupancy.push(sender.max_capacity() - sender.capacity());
        observer.send(
            slot,
            Input {
                raw: next.raw,
                due: next.due,
                _bytes: bytes,
            },
            next.bytes,
        );
    }
    Ok((occupancy, source.count()))
}

struct Worker {
    runtime: Handle,
    settings: Settings,
    cancel: CancellationToken,
    owner: Ownership,
    response_budget: ExecutorResponseBudget,
    input: mpsc::Receiver<Input>,
    output: mpsc::Sender<QueuedOutput>,
    output_budget: Arc<Semaphore>,
    input_observer: ReceiverObserver,
    output_observer: SenderObserver,
}

impl Worker {
    fn process(mut self) -> Result<(ResponsePayload, Vec<usize>)> {
        let _active = self.owner.start();
        let mut round = pipeline(&self.settings, &self.response_budget);
        let mut occupancy = Vec::new();
        while let Some(packet) = self.input_observer.blocking_recv(&mut self.input) {
            if self.cancel.is_cancelled() {
                return Err(DriverError::Cancelled);
            }
            if let Some(probe) = &self.settings.early_failure_probe
                && !self.runtime.block_on(probe.wait_until_paused(&self.cancel))
            {
                return Err(DriverError::Cancelled);
            }
            let translated = push(&mut round, &packet.raw, packet.due, self.settings.limits)?;
            drop(packet);
            self.runtime.block_on(async {
                let bytes = tokio::select! {
                    () = self.cancel.cancelled() => return Err(DriverError::Cancelled),
                    bytes = Arc::clone(&self.output_budget).acquire_many_owned(translated.logical_bytes) => bytes.map_err(|_| DriverError::ChannelClosed)?,
                };
                let slot = tokio::select! {
                    () = self.cancel.cancelled() => return Err(DriverError::Cancelled),
                    slot = self.output.reserve() => slot.map_err(|_| DriverError::ChannelClosed)?,
                };
                occupancy.push(self.output.max_capacity() - self.output.capacity());
                let size = translated.logical_bytes;
                self.output_observer.send(slot, QueuedOutput { output: translated, _bytes: bytes }, size);
                Ok(())
            })?;
        }
        Ok((
            self.runtime.block_on(finish(round, &self.settings, &self.cancel))?,
            occupancy,
        ))
    }
}

pub(super) async fn run(
    settings: &Settings,
    source: Source,
    cancel: &CancellationToken,
    ownership: &Ownership,
    consumer: &mut Consumer,
) -> Result<Finished> {
    let (sender, input) = mpsc::channel::<Input>(settings.limits.input_entries);
    let (output, mut receiver) = mpsc::channel::<QueuedOutput>(settings.limits.output_entries);
    let input_budget = Arc::new(Semaphore::new(settings.limits.input_bytes as usize));
    let output_budget = Arc::new(Semaphore::new(settings.limits.output_bytes as usize));
    let observers = Pair::new(settings.queue_observation, settings.limits, MAX_LINES);
    let mut output_observer = observers.output_receiver();
    let feeder = tokio::spawn(feed(
        sender,
        source,
        input_budget,
        settings.clone(),
        cancel.clone(),
        observers.input_sender(),
    ));
    let worker = Worker {
        runtime: Handle::current(),
        settings: settings.clone(),
        cancel: cancel.clone(),
        owner: ownership.clone(),
        response_budget: consumer.budget.clone(),
        input,
        output,
        output_budget,
        input_observer: observers.input_receiver(),
        output_observer: observers.output_sender(),
    };
    let worker = tokio::task::spawn_blocking(move || worker.process());
    let mut consumer_error = None;
    loop {
        let packet = tokio::select! {
            // Preserve explicit cancellation when output EOF is ready in the same poll.
            biased;
            () = cancel.cancelled() => { consumer_error = Some(DriverError::Cancelled); break; },
            packet = output_observer.recv(&mut receiver) => packet,
        };
        let Some(packet) = packet else { break };
        if let Err(error) = consumer.accept(packet.output, cancel).await {
            consumer_error = Some(error);
            break;
        }
    }
    drop(receiver);
    if consumer_error.is_some() {
        cancel.cancel();
        feeder.abort();
    }
    let worker_result = worker.await.unwrap_or(Err(DriverError::Join));
    ownership.worker_joined();
    if worker_result.is_err() {
        cancel.cancel();
    }
    let feeder_result = feeder.await;
    ownership.feeder_joined();
    if let Some(error) = consumer_error {
        return Err(error);
    }
    let (input_occupancy, source_lines) = match feeder_result {
        Ok(Ok(values)) => values,
        Ok(Err(error))
            if matches!(error, DriverError::Cancelled | DriverError::ChannelClosed) && worker_result.is_err() =>
        {
            (Vec::new(), 0)
        }
        Ok(Err(error)) => return Err(error),
        Err(_) => return Err(DriverError::Join),
    };
    let (payload, output_occupancy) = worker_result?;
    Ok((
        payload,
        input_occupancy,
        output_occupancy,
        source_lines,
        observers.finish(),
    ))
}
