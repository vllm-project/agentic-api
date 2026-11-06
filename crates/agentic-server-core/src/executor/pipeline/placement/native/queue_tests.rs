// Observed-channel correctness, with complete native payload/wire comparisons.

#![cfg(test)]

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::gate::pair;
use super::profile::pilot_runtime;
use super::transport::named_inputs;
use super::{DriverError, Ownership, Placement, Settings, run};

#[test]
#[ignore = "explicit observer-enabled complete-catalog gate required before queue measurements"]
fn queue_envelope_preserves_full_payload_and_native_wire() {
    pilot_runtime().block_on(async {
        let workloads = named_inputs().await.unwrap();
        for entries in [1, 16] {
            for (arrival, client) in [(0, 0), (1, 0), (0, 1)] {
                let mut settings = Settings::new(Placement::Inline, true);
                settings.queue_observation = true;
                settings.limits.input_entries = entries;
                settings.limits.output_entries = entries;
                settings.limits.input_bytes = 65_536;
                settings.limits.output_bytes = 65_536;
                settings.arrival_spacing = Duration::from_millis(arrival);
                settings.client_delay = Duration::from_millis(client);
                for workload in &workloads {
                    pair(settings.clone(), workload.lines.clone()).await;
                }
            }
        }
        println!(
            "PLACEMENT_QUEUE_GATE {}",
            serde_json::json!({
                "workloads": workloads.len(), "full_payload_wire_pairs": 96,
                "queue_entries": [1, 16], "logical_byte_budget": 65536,
                "observer_enabled": true, "default_production_change": false,
            })
        );
    });
}

#[test]
fn observer_does_not_replace_primary_failure_or_pre_cancelled_cleanup() {
    pilot_runtime().block_on(async {
        for cancelled in [false, true] {
            let mut settings = Settings::new(Placement::Worker, true);
            settings.queue_observation = true;
            let cancel = CancellationToken::new();
            if cancelled {
                cancel.cancel();
            }
            let owner = Ownership::default();
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                run(settings, vec!["data: {".to_owned()], cancel, owner.clone()),
            )
            .await
            .unwrap();
            if cancelled {
                assert!(matches!(result, Err(DriverError::Cancelled)));
            } else {
                assert!(matches!(result, Err(DriverError::Ingestion(_))));
            }
            assert_eq!(owner.state().0, 0);
            assert_eq!((owner.state().2, owner.state().3, owner.state().4), (1, 1, 1));
        }
    });
}
