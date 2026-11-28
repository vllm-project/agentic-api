// Explicit long-provider correctness gate; all capture fields and assertions retained.

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::task::JoinSet;

use super::super::fixtures::{Workload, named_workloads};
use super::http_server::HttpServer;
use super::transport::named_inputs;
use super::{Placement, Settings, gate, http_tests};

fn settings(entries: usize, budget: u32, relay: bool) -> Settings {
    let mut settings = Settings::new(Placement::Inline, relay);
    settings.limits.input_entries = entries;
    settings.limits.output_entries = entries;
    settings.limits.input_bytes = budget;
    settings.limits.output_bytes = budget;
    settings
}

async fn server(workload: &Workload, spacing_ms: u64) -> HttpServer {
    let chunks = Bytes::from(workload.wire.clone())
        .chunks(4096)
        .map(Bytes::copy_from_slice)
        .collect();
    HttpServer::start(chunks, false, Duration::from_millis(spacing_ms))
        .await
        .unwrap()
}

async fn concurrent(workload: &Workload, lines: &[String]) {
    let server = Arc::new(server(workload, 0).await);
    let mut tasks = JoinSet::new();
    for _ in 0..4 {
        tasks.spawn(Box::pin(gate::pair(settings(16, 65_536, true), lines.to_vec())));
        let server = Arc::clone(&server);
        let expected = workload.identity.expected_framed_lines;
        tasks.spawn(Box::pin(async move {
            assert_eq!(http_tests::pair(settings(16, 65_536, true), &server).await, expected);
        }));
    }
    while let Some(joined) = tasks.join_next().await {
        joined.unwrap();
    }
    assert_eq!(server.requests(), 8);
    let Ok(server) = Arc::try_unwrap(server) else {
        panic!("all HTTP pair owners must join")
    };
    server.stop().await.unwrap();
}

#[test]
#[ignore = "explicit extended workload gate: required before timing the complete named catalog"]
fn provider_long_matrix_preserves_entire_payload_and_native_wire() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .enable_all()
        .build()
        .unwrap()
        .block_on(Box::pin(async {
            let prepared = named_inputs().await.unwrap();
            let workloads: Vec<_> = named_workloads()
                .into_iter()
                .filter(|workload| workload.identity.extended_gate)
                .collect();
            assert_eq!(workloads.len(), 1);
            for workload in workloads {
                let framed = prepared
                    .iter()
                    .find(|framed| framed.identity == workload.identity)
                    .unwrap();
                let mut isolated_pairs = 0;
                let mut http_pairs = 0;
                for entries in [1, 16, 64, 256] {
                    for budget in [65_536, 262_144] {
                        for (relay, arrival, client) in
                            [(false, 0, 0), (false, 1, 0), (true, 0, 0), (true, 1, 0), (true, 0, 1)]
                        {
                            let mut options = settings(entries, budget, relay);
                            options.arrival_spacing = Duration::from_millis(arrival);
                            options.client_delay = Duration::from_millis(client);
                            gate::pair(options, framed.lines.clone()).await;
                            isolated_pairs += 1;
                        }
                    }
                }
                for spacing in [0, 1] {
                    let server = server(&workload, spacing).await;
                    for entries in [1, 16, 64, 256] {
                        for budget in [65_536, 262_144] {
                            for client in [0, 1] {
                                let mut options = settings(entries, budget, true);
                                options.client_delay = Duration::from_millis(client);
                                assert_eq!(
                                    http_tests::pair(options, &server).await,
                                    workload.identity.expected_framed_lines
                                );
                                http_pairs += 1;
                            }
                        }
                    }
                    assert_eq!(server.requests(), 32);
                    server.stop().await.unwrap();
                }
                concurrent(&workload, &framed.lines).await;
                println!(
                    "PLACEMENT_EXTENDED_GATE {}",
                    serde_json::json!({"workload":workload.identity,
                    "isolated_pairs":isolated_pairs,"http_pairs":http_pairs,
                    "concurrent_isolated_pairs":4,"concurrent_http_pairs":4,"all_mock_servers_joined":true})
                );
            }
        }));
}
