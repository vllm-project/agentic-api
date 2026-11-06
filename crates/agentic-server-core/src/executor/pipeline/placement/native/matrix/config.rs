// Controlled full-factor plan; the initial pilot is an explicit subset.

#![cfg(test)]

use std::time::Duration;
use tokio::time::Instant;

use super::super::{ArrivalSchedule, Placement, Settings};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(in super::super) enum Arrival {
    Burst,
    Paced,
    Jittered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(in super::super) struct Config {
    pub queue_entries: usize,
    pub logical_byte_budget: u32,
    pub concurrency: usize,
    pub relay: bool,
    pub arrival: Arrival,
    pub client_ms: u64,
}

impl Config {
    pub fn settings(self, placement: Placement, origin: Instant) -> Settings {
        let mut settings = Settings::new(placement, self.relay);
        settings.limits.input_entries = self.queue_entries;
        settings.limits.output_entries = self.queue_entries;
        settings.limits.input_bytes = self.logical_byte_budget;
        settings.limits.output_bytes = self.logical_byte_budget;
        settings.client_delay = Duration::from_millis(self.client_ms);
        settings.replay_schedule = Some(match self.arrival {
            Arrival::Burst => ArrivalSchedule::at(origin, Duration::ZERO),
            Arrival::Paced => ArrivalSchedule::at(origin, Duration::from_millis(1)),
            Arrival::Jittered => ArrivalSchedule::jittered_at(origin, Duration::from_millis(1)),
        });
        settings
    }
}

pub(in super::super) fn full_plan() -> Vec<Config> {
    let mut configs = Vec::new();
    for queue_entries in [1, 16, 64, 256] {
        for logical_byte_budget in [65_536, 262_144] {
            for concurrency in [1, 4] {
                for arrival in [Arrival::Burst, Arrival::Paced, Arrival::Jittered] {
                    for (relay, client_ms) in [(false, 0), (true, 0), (true, 1)] {
                        configs.push(Config {
                            queue_entries,
                            logical_byte_budget,
                            concurrency,
                            relay,
                            arrival,
                            client_ms,
                        });
                    }
                }
            }
        }
    }
    configs
}

pub(in super::super) fn pilot() -> Vec<Config> {
    vec![
        Config {
            queue_entries: 1,
            logical_byte_budget: 65_536,
            concurrency: 1,
            relay: false,
            arrival: Arrival::Burst,
            client_ms: 0,
        },
        Config {
            queue_entries: 16,
            logical_byte_budget: 65_536,
            concurrency: 4,
            relay: true,
            arrival: Arrival::Burst,
            client_ms: 0,
        },
        Config {
            queue_entries: 64,
            logical_byte_budget: 262_144,
            concurrency: 4,
            relay: false,
            arrival: Arrival::Paced,
            client_ms: 0,
        },
        Config {
            queue_entries: 256,
            logical_byte_budget: 262_144,
            concurrency: 4,
            relay: true,
            arrival: Arrival::Jittered,
            client_ms: 0,
        },
        Config {
            queue_entries: 1,
            logical_byte_budget: 65_536,
            concurrency: 4,
            relay: true,
            arrival: Arrival::Jittered,
            client_ms: 1,
        },
    ]
}

#[test]
fn full_plan_has_all_144_valid_factor_combinations_and_pilot_is_not_the_full_plan() {
    let full = full_plan();
    assert_eq!(full.len(), 144);
    for (index, config) in full.iter().enumerate() {
        assert!(full[..index].iter().all(|other| other != config));
        config.settings(Placement::Inline, Instant::now()).validate(0).unwrap();
    }
    let pilot = pilot();
    assert_eq!(pilot.len(), 5);
    assert!(pilot.iter().all(|config| full.contains(config)));
}
