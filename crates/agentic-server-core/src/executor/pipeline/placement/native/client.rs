// Reuse the native relay and typed client frames; no experimental emitter.

#![cfg(test)]

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{DriverError, Observation, Output, Ownership, Result, Settings};
use crate::executor::gateway_accumulator::StreamEvent;
use crate::executor::relay::{RelayLimits, StreamRelay};
use crate::executor::request::RequestContext;
use crate::executor::response_budget::ExecutorResponseBudget;
use crate::executor::upstream::tests::request_context;
use crate::tool::ToolRegistry;
use crate::types::request_response::ResponsePayload;

type ClientResult = Result<(Vec<String>, usize, usize)>;

pub(super) struct Consumer {
    relay: Option<StreamRelay>,
    client: Option<JoinHandle<ClientResult>>,
    context: RequestContext,
    registry: ToolRegistry,
    capture: bool,
    trace: Vec<String>,
    trace_bytes: usize,
    pub observations: Vec<Observation>,
    ownership: Ownership,
    gateway_enabled: bool,
    pub budget: ExecutorResponseBudget,
    pub gateway: Option<super::gateway_round::RoundOutcome>,
}

impl Consumer {
    pub fn new(settings: &Settings, cancel: CancellationToken, ownership: Ownership) -> Self {
        let (relay, client) = if settings.relay {
            let (sender, receiver) = mpsc::channel(settings.client_entries);
            let mut relay = StreamRelay::client(
                sender,
                RelayLimits::with_event_bytes(settings.limits.output_item_bytes as usize),
            );
            relay
                .begin_round(0)
                .expect("new native relay cannot have deferred state");
            let options = settings.clone();
            (Some(relay), Some(tokio::spawn(receive(receiver, options, cancel))))
        } else {
            (None, None)
        };
        Self {
            relay,
            client,
            context: request_context(),
            registry: settings.registry.clone().unwrap_or_default(),
            capture: settings.capture,
            trace: Vec::new(),
            trace_bytes: 0,
            observations: Vec::new(),
            ownership,
            gateway_enabled: settings.registry.is_some(),
            budget: ExecutorResponseBudget::new(),
            gateway: None,
        }
    }

    pub async fn accept(&mut self, output: Output, cancel: &CancellationToken) -> Result<()> {
        if let Some(relay) = &mut self.relay {
            tokio::select! {
                // A ready cancellation must not become a simultaneous relay-close error.
                biased;
                () = cancel.cancelled() => return Err(DriverError::Cancelled),
                result = relay.accept(output.translation, &self.context, &self.registry) => result?,
            }
        } else if self.capture {
            let wires: Vec<_> = output.translation.frames.iter().map(|frame| &frame.wire).collect();
            let snapshot = serde_json::to_string(&(output.translation.defer_from_output_index, wires))?;
            self.trace_bytes = self
                .trace_bytes
                .checked_add(snapshot.len())
                .ok_or(DriverError::InvalidLimits)?;
            if self.trace_bytes > 8 * 1024 * 1024 {
                return Err(DriverError::ItemLimit("capture"));
            }
            self.trace.push(snapshot);
        } else {
            std::hint::black_box(output.translation);
        }
        if output.semantic && !self.capture {
            self.observations.push(Observation {
                due_to_ingestion_ns: output
                    .ingestion_finished
                    .saturating_duration_since(output.due)
                    .as_nanos(),
                due_to_relay_return_ns: Instant::now().saturating_duration_since(output.due).as_nanos(),
            });
        }
        Ok(())
    }

    pub async fn complete_round(
        &mut self,
        mut payload: ResponsePayload,
        cancel: &CancellationToken,
    ) -> Result<ResponsePayload> {
        if self.gateway_enabled {
            let relay = self.relay.as_mut().ok_or(DriverError::InvalidLimits)?;
            self.gateway = Some(tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(DriverError::Cancelled),
                result = super::gateway_round::complete(&mut payload, &self.registry, &self.context, relay, &self.budget) => result?,
            });
        }
        Ok(payload)
    }

    pub async fn close(&mut self) -> ClientResult {
        self.relay.take();
        if let Some(client) = self.client.take() {
            let result = client.await.map_err(|_| DriverError::Join);
            self.ownership.client_joined();
            result?
        } else {
            Ok((std::mem::take(&mut self.trace), 0, 0))
        }
    }
}

async fn receive(
    mut receiver: mpsc::Receiver<StreamEvent>,
    settings: Settings,
    cancel: CancellationToken,
) -> ClientResult {
    let mut trace = Vec::new();
    let mut count = 0;
    let mut bytes = 0usize;
    loop {
        let event = tokio::select! {
            // Closed EOF and cancellation may both be ready; cancellation owns teardown.
            biased;
            () = cancel.cancelled() => return Err(DriverError::Cancelled),
            event = receiver.recv() => event,
        };
        match event {
            None => return Ok((trace, count, bytes)),
            Some(StreamEvent::Flush(reply)) => {
                let _ = reply.send(());
            }
            Some(StreamEvent::Frame(frame)) => {
                if let Some(probe) = &settings.client_probe {
                    probe.observe();
                }
                count += 1;
                bytes = bytes
                    .checked_add(frame.content.len())
                    .ok_or(DriverError::InvalidLimits)?;
                std::hint::black_box(frame.sequence_number);
                if settings.capture {
                    if bytes > 8 * 1024 * 1024 {
                        return Err(DriverError::ItemLimit("capture"));
                    }
                    trace.push(frame.content);
                }
                if settings.disconnect_after == Some(count) {
                    return Err(DriverError::ConsumerClosed);
                }
                if !settings.client_delay.is_zero() {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Err(DriverError::Cancelled),
                        () = tokio::time::sleep(settings.client_delay) => {},
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn explicit_cancellation_wins_a_closed_client_channel() {
    for _ in 0..128 {
        let (sender, receiver) = mpsc::channel(1);
        drop(sender);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = receive(receiver, Settings::new(super::Placement::Inline, true), cancel).await;
        assert!(
            matches!(&result, Err(DriverError::Cancelled)),
            "ready cancellation must not become a clean EOF: {result:?}"
        );
    }
    let (sender, receiver) = mpsc::channel(1);
    drop(sender);
    let normal = receive(
        receiver,
        Settings::new(super::Placement::Inline, true),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(normal, (Vec::new(), 0, 0));
}
