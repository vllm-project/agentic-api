// Explicit audit of existing captures. Completion is not blanket workload eligibility.

#![cfg(test)]

use std::time::Duration;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use super::super::fixtures::wire_rounds;
use super::http_server::HttpServer;
use super::{DriverError, Ownership, Placement, Settings, Source, run_source};

macro_rules! capture {
    ($path:literal) => {
        (
            $path,
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/", $path)),
        )
    };
}

fn sources() -> [(&'static str, &'static str); 9] {
    [
        capture!("text_only/responses/resp-single-gpt-4o-streaming.yaml"),
        capture!("reasoning/responses/reasoning-openai-reference-gpt-5.6-streaming.yaml"),
        capture!("reasoning/responses/reasoning-single-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml"),
        capture!("reasoning/responses/reasoning-single-openai-gpt-oss-20b-streaming.yaml"),
        capture!("parallel_tool_calls/parallel-client-openai-reference-gpt-5.6-streaming.yaml"),
        capture!("parallel_tool_calls/parallel-client-gateway-Qwen-Qwen3.5-35B-A3B-FP8-streaming.yaml"),
        capture!("web_search/web-search-openai-reference-gpt-5.6-streaming.yaml"),
        capture!("web_search/web-search-gateway-Qwen-Qwen3.5-35B-A3B-FP8-streaming.yaml"),
        capture!("web_search/gpt_oss_web_search_streaming.yaml"),
    ]
}

#[test]
#[ignore = "explicit native capture audit: report per-arm rejection, do not treat process completion as universal acceptance"]
fn characterize_existing_workload_inputs() {
    audit(&sources());
}

#[test]
#[ignore = "explicit long-capture native acceptance audit; not timing or gateway tool execution evidence"]
fn characterize_long_recordings() {
    audit(&[
        capture!("max_tool_calls/max-tool-calls-counting-gateway-Qwen-Qwen3.6-35B-A3B-streaming.yaml"),
        capture!("max_tool_calls/max-tool-calls-builtin-gateway-Qwen-Qwen3.6-35B-A3B-streaming.yaml"),
        capture!("multi_agent/multi-agent-gateway-review-Qwen-Qwen3.6-35B-A3B-FP8-streaming.yaml"),
    ]);
}

#[test]
#[ignore = "explicit raw-provider capture audit; distinguish direct vLLM from OpenAI multi-agent reference"]
fn characterize_upstream_recordings() {
    audit(&[
        capture!("codex/codex-direct-vllm-http-function-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml"),
        capture!("codex/codex-direct-vllm-http-flat-namespace-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml"),
        capture!("codex/codex-direct-vllm-http-custom-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml"),
        capture!("multi_agent/multi-agent-openai-reference-review-gpt-5.6-sol-streaming.yaml"),
        capture!("multi_agent/multi-agent-openai-reference-code-interpreter-gpt-5.6-sol-streaming.yaml"),
    ]);
}

fn audit(sources: &[(&str, &str)]) {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).max_blocking_threads(8)
        .enable_all().build().unwrap().block_on(async {
            for &(path, source) in sources {
                for (turn, wire) in wire_rounds(source).into_iter().enumerate() {
                    let server = HttpServer::start(vec![Bytes::from(wire)], false, Duration::ZERO).await.unwrap();
                    let mut accepted = Vec::new();
                    let mut errors = Vec::new();
                    for (arm, placement) in [("inline", Placement::Inline), ("worker", Placement::Worker)] {
                        let owner = Ownership::default();
                        let result = run_source(Settings::new(placement, true), Source::http(server.stream(Duration::from_secs(2))),
                            CancellationToken::new(), owner.clone()).await;
                        let workers = usize::from(matches!(placement, Placement::Worker));
                        assert_eq!(owner.state().0, 0);
                        assert_eq!(owner.state().2, workers);
                        assert_eq!(owner.state().3, workers);
                        assert_eq!(owner.state().4, 1);
                        let row = match result {
                            Ok(outcome) => {
                                accepted.push(arm);
                                assert!(matches!(outcome.payload.status.as_str(), "completed" | "failed" | "incomplete"), "accepted stream must be terminal");
                                serde_json::json!({"path": path, "turn": turn, "arm": arm, "accepted": true,
                                    "framed_inputs": outcome.source_lines, "client_frames": outcome.client_frames,
                                    "terminal_status": outcome.payload.status})
                            }
                            Err(error) => {
                                let message = error.to_string();
                                errors.push(message.clone());
                                let kind = match error { DriverError::Ingestion(_) => "native_ingestion", DriverError::Transport(_) => "native_transport", _ => "harness_or_limit" };
                                serde_json::json!({"path": path, "turn": turn, "arm": arm, "accepted": false, "kind": kind, "error": message})
                            }
                        };
                        println!("PLACEMENT_WORKLOAD_AUDIT {row}");
                    }
                    if accepted.len() == 2 {
                        super::http_tests::pair(Settings::new(Placement::Inline, true), &server).await;
                    } else {
                        assert!(accepted.is_empty(), "placement cannot change capture acceptance");
                        assert_eq!(errors[0], errors[1], "both arms must report the same native rejection");
                    }
                    server.stop().await.unwrap();
                    println!("PLACEMENT_WORKLOAD_AUDIT_JOINED {}", serde_json::json!({"path": path, "turn": turn,
                        "full_payload_wire_parity_verified": accepted.len() == 2,
                        "gateway_deferral_execution_covered": false}));
                }
            }
        });
}
