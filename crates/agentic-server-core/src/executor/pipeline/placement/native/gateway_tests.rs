// Controlled native search execution, not pass-through replay or a full gateway test.

#![cfg(test)]

mod fixture;

use std::time::Duration;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use super::http_server::HttpServer;
use super::{DriverError, FinishFence, Outcome, Ownership, Placement, Settings, Source, run_source};
use crate::events::normalize_sse_line;
use fixture::{Reply, SearchFixture, lines};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .enable_all()
        .build()
        .unwrap()
}

fn settings(placement: Placement, fixture: &SearchFixture) -> Settings {
    let mut settings = Settings::new(placement, true);
    settings.capture = true;
    settings.registry = Some(fixture.registry.clone());
    settings
}

fn source(lines: &[String], server: Option<&HttpServer>) -> Source {
    server.map_or_else(
        || Source::replay(lines.to_vec(), Duration::ZERO),
        |server| Source::http(server.stream(Duration::from_secs(2))),
    )
}

fn assert_joins(owner: &Ownership, placement: Placement) {
    let worker = usize::from(matches!(placement, Placement::Worker));
    let state = owner.state();
    assert_eq!(state.0, 0);
    assert_eq!(state.2, worker);
    assert_eq!(state.3, worker);
    assert_eq!(state.4, 1);
}

fn assert_executed(outcome: &Outcome, reply: Reply) {
    let gateway = outcome.gateway.as_ref().unwrap();
    assert!(
        gateway.deferred_before_tools,
        "hidden call must really open a deferred window"
    );
    assert!(
        !gateway.deferred_after_tools,
        "closing a sender is not draining deferred frames"
    );
    assert!(gateway.retained_bytes_after_ingestion > 0);
    assert!(
        gateway.retained_bytes_after_tools > gateway.retained_bytes_after_ingestion,
        "tool outputs charge the same request budget"
    );
    assert_eq!(gateway.tool_outputs.len(), 1);
    let tool = serde_json::to_value(&gateway.tool_outputs[0]).unwrap();
    assert_eq!(tool["call_id"], "call_search");
    let returned = tool["output"].as_str().unwrap();
    if matches!(reply, Reply::Failure) {
        assert!(returned.contains("503"));
    } else {
        assert!(returned.contains("controlled provider reply"));
    }
    let output = serde_json::to_value(&outcome.payload.output).unwrap();
    assert_eq!(output[0]["type"], "web_search_call");
    assert_eq!(output[0]["id"], "ws_search");
    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["name"], "echo");
    let frames: Vec<_> = outcome
        .trace
        .iter()
        .map(|wire| {
            let frame = wire.lines().find_map(normalize_sse_line).unwrap();
            serde_json::to_value(frame.wire).unwrap()
        })
        .collect();
    assert!(
        frames.iter().all(|frame| frame["item"]["id"] != "fc_search"),
        "canonical built-in call must not leak"
    );
    let search_done = frames
        .iter()
        .position(|frame| frame["type"] == "response.output_item.done" && frame["item"]["id"] == "ws_search")
        .unwrap();
    let echo_added = frames
        .iter()
        .position(|frame| frame["type"] == "response.output_item.added" && frame["item"]["id"] == "fc_echo_1")
        .unwrap();
    assert!(
        search_done < echo_added,
        "native per-item release follows the built-in lifecycle"
    );
    for (sequence, frame) in frames.iter().enumerate() {
        assert_eq!(
            frame["sequence_number"].as_u64(),
            Some(u64::try_from(sequence).unwrap())
        );
    }
}

async fn pair(http: bool, entries: usize, reply: Reply, failed: bool) {
    let fixture = SearchFixture::start(reply).await;
    let input = lines(1, failed);
    let server = if http {
        let wire = input.join("\n\n") + "\n\n";
        Some(
            HttpServer::start(vec![Bytes::from(wire)], false, Duration::ZERO)
                .await
                .unwrap(),
        )
    } else {
        None
    };
    let fence = FinishFence::new();
    let control = CancellationToken::new();
    let mut inline = settings(Placement::Inline, &fixture);
    inline.finish_fence = Some(fence.clone());
    inline.limits.input_entries = entries;
    inline.limits.output_entries = entries;
    let mut worker = inline.clone();
    worker.placement = Placement::Worker;
    let inline_owner = Ownership::default();
    let worker_owner = Ownership::default();
    let (inline, worker, released) = Box::pin(tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            run_source(
                inline,
                source(&input, server.as_ref()),
                control.clone(),
                inline_owner.clone()
            ),
            run_source(
                worker,
                source(&input, server.as_ref()),
                control.clone(),
                worker_owner.clone()
            ),
            fence.release(&control),
        )
    }))
    .await
    .unwrap();
    let requests = fixture.requests();
    fixture.stop().await;
    if let Some(server) = server {
        assert_eq!(server.requests(), 2);
        server.stop().await.unwrap();
    }
    released.unwrap();
    assert_joins(&inline_owner, Placement::Inline);
    assert_joins(&worker_owner, Placement::Worker);
    let (inline, worker) = (inline.unwrap(), worker.unwrap());
    assert_eq!(
        serde_json::to_string(&inline.payload).unwrap(),
        serde_json::to_string(&worker.payload).unwrap()
    );
    assert_eq!(inline.trace, worker.trace);
    assert_eq!(inline.client_frames, worker.client_frames);
    assert_eq!(inline.client_bytes, worker.client_bytes);
    assert_eq!(inline.source_lines, input.len());
    assert_eq!(worker.source_lines, input.len());
    assert_eq!(
        serde_json::to_string(&inline.gateway).unwrap(),
        serde_json::to_string(&worker.gateway).unwrap()
    );
    if failed {
        assert_eq!(requests, 0, "failed upstream never executes tools");
        let gateway = inline.gateway.unwrap();
        assert!(gateway.deferred_before_tools && !gateway.deferred_after_tools);
        assert!(gateway.tool_outputs.is_empty());
        assert_eq!(
            gateway.retained_bytes_after_ingestion,
            gateway.retained_bytes_after_tools
        );
    } else {
        assert_eq!(requests, 2, "one real loopback provider request per arm");
        assert_executed(&inline, reply);
        assert_executed(&worker, reply);
    }
    println!(
        "PLACEMENT_GATEWAY_CONTRACT {}",
        serde_json::json!({"http_inference":http,"queue_entries":entries,
        "reply":format!("{reply:?}"),"failed_upstream":failed,"actual_provider_requests":requests,
        "full_payload_wire_equal":true,"gateway_mock_joined":true,"recorded_model_workload":false})
    );
}

#[test]
fn native_search_execution_and_deferred_release_match_in_both_placements() {
    runtime().block_on(async {
        for http in [false, true] {
            for entries in [1, 16] {
                for reply in [Reply::Success, Reply::Failure] {
                    Box::pin(pair(http, entries, reply, false)).await;
                }
            }
        }
    });
}

#[test]
fn failed_upstream_releases_deferred_output_without_executing_search() {
    runtime().block_on(async {
        for http in [false, true] {
            Box::pin(pair(http, 1, Reply::Success, true)).await;
        }
    });
}

#[test]
fn cancelling_pending_native_search_joins_owned_tasks_and_fixture() {
    runtime().block_on(async {
        for placement in [Placement::Inline, Placement::Worker] {
            let fixture = SearchFixture::start(Reply::WaitForShutdown).await;
            let owner = Ownership::default();
            let cancel = CancellationToken::new();
            let task = tokio::spawn(run_source(
                settings(placement, &fixture),
                source(&lines(1, false), None),
                cancel.clone(),
                owner.clone(),
            ));
            fixture.wait_for_request().await;
            cancel.cancel();
            let result = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            let requests = fixture.requests();
            fixture.stop().await;
            assert_eq!(requests, 1);
            assert!(matches!(result, Err(DriverError::Cancelled)));
            assert_joins(&owner, placement);
        }
    });
}

#[test]
fn disconnect_during_post_execution_release_does_not_retry_the_tool() {
    runtime().block_on(async {
        for placement in [Placement::Inline, Placement::Worker] {
            let fixture = SearchFixture::start(Reply::Success).await;
            let mut settings = settings(placement, &fixture);
            settings.disconnect_after = Some(7);
            let owner = Ownership::default();
            let result = run_source(
                settings,
                source(&lines(1, false), None),
                CancellationToken::new(),
                owner.clone(),
            )
            .await;
            let requests = fixture.requests();
            fixture.stop().await;
            assert_eq!(
                requests, 1,
                "disconnect cannot roll back a provider call or silently retry it"
            );
            let Err(DriverError::Ingestion(error)) = result else {
                panic!("retain native release failure")
            };
            assert!(error.to_string().contains("stream receiver closed"));
            assert_joins(&owner, placement);
        }
    });
}

#[test]
fn deferred_buffer_limit_is_an_error_not_a_dropped_frame_or_tool_execution() {
    runtime().block_on(async {
        for placement in [Placement::Inline, Placement::Worker] {
            let fixture = SearchFixture::start(Reply::Success).await;
            let owner = Ownership::default();
            let result = run_source(
                settings(placement, &fixture),
                source(&lines(300, false), None),
                CancellationToken::new(),
                owner.clone(),
            )
            .await;
            let requests = fixture.requests();
            fixture.stop().await;
            assert_eq!(requests, 0);
            let Err(DriverError::Ingestion(error)) = result else {
                panic!("retain native deferred limit error")
            };
            assert!(error.to_string().contains("deferred stream exceeded"));
            assert_joins(&owner, placement);
        }
    });
}

#[test]
fn registry_without_relay_is_rejected_before_starting_any_owner() {
    runtime().block_on(async {
        let fixture = SearchFixture::start(Reply::Success).await;
        let mut settings = settings(Placement::Worker, &fixture);
        settings.relay = false;
        let owner = Ownership::default();
        let result = run_source(settings, source(&[], None), CancellationToken::new(), owner.clone()).await;
        let requests = fixture.requests();
        fixture.stop().await;
        assert_eq!(requests, 0);
        assert!(matches!(result, Err(DriverError::InvalidLimits)));
        assert_eq!(owner.state(), (0, 0, 0, 0, 0));
    });
}

#[test]
fn malformed_input_preserves_ingestion_error_and_never_executes_a_tool() {
    runtime().block_on(async {
        for placement in [Placement::Inline, Placement::Worker] {
            let fixture = SearchFixture::start(Reply::Success).await;
            let owner = Ownership::default();
            let result = run_source(
                settings(placement, &fixture),
                source(&["data: {".to_owned()], None),
                CancellationToken::new(),
                owner.clone(),
            )
            .await;
            let requests = fixture.requests();
            fixture.stop().await;
            assert_eq!(requests, 0);
            let Err(DriverError::Ingestion(error)) = result else {
                panic!("retain malformed-input failure")
            };
            assert!(error.to_string().contains("malformed data frame"));
            assert_joins(&owner, placement);
        }
    });
}
