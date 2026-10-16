// Test-only replay fixtures; captured data is kept unchanged.

#![cfg(test)]

use serde::{Deserialize, Serialize};

/// Stable request identity shared by correctness gates and both timing scopes.
/// A turn is an independent captured response, not a replayed agent conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) struct WorkloadIdentity {
    pub id: &'static str,
    pub family: &'static str,
    pub cassette: &'static str,
    pub source_turn: usize,
    pub expected_framed_lines: usize,
    pub capture_boundary: &'static str,
    pub extended_gate: bool,
}

pub(super) struct Workload {
    pub identity: WorkloadIdentity,
    pub wire: String,
}

/// Only native-audited captures enter the formal matrix. Reasoning is a short
/// visible-summary sample, and function calls do not execute client tools here.
/// Web-search pass-through captures are deliberately not gateway-execution tests.
macro_rules! capture {
    ($boundary:literal, $family:literal, $extended:literal, $path:literal, $turns:expr) => {
        workloads(
            $boundary,
            $family,
            $extended,
            $path,
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/", $path)),
            $turns,
        )
    };
}

pub(super) fn named_workloads() -> Vec<Workload> {
    reference_workloads()
        .into_iter()
        .chain(direct_vllm_workloads())
        .chain(provider_long_workload())
        .collect()
}

fn reference_workloads() -> Vec<Workload> {
    [
        capture!(
            "provider_http_reference",
            "reasoning_sample",
            false,
            "reasoning/responses/reasoning-openai-reference-gpt-5.6-streaming.yaml",
            &[("reasoning-reference-0", 118)]
        ),
        capture!(
            "provider_http_reference",
            "multi_turn_function",
            false,
            "tool_calls/multi_turn/openai_responses_tool_calls_3turn_streaming.yaml",
            &[
                ("function-multiturn-0", 14),
                ("function-multiturn-1", 24),
                ("function-multiturn-2", 20)
            ]
        ),
        capture!(
            "provider_http_reference",
            "text_short",
            false,
            "text_only/responses/resp-single-gpt-4o-streaming.yaml",
            &[("text-short-0", 11)]
        ),
        capture!(
            "provider_http_reference",
            "parallel_client_function",
            false,
            "parallel_tool_calls/parallel-client-openai-reference-gpt-5.6-streaming.yaml",
            &[
                ("parallel-client-0", 11),
                ("parallel-client-1", 13),
                ("parallel-client-2", 13),
                ("parallel-client-3", 28)
            ]
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn direct_vllm_workloads() -> Vec<Workload> {
    [
        capture!(
            "direct_vllm_http",
            "direct_vllm_function",
            false,
            "codex/codex-direct-vllm-http-function-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            &[("direct-vllm-function-0", 200), ("direct-vllm-function-1", 31)]
        ),
        capture!(
            "direct_vllm_http",
            "direct_vllm_flat_namespace",
            false,
            "codex/codex-direct-vllm-http-flat-namespace-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            &[
                ("direct-vllm-flat-namespace-0", 92),
                ("direct-vllm-flat-namespace-1", 59)
            ]
        ),
        capture!(
            "direct_vllm_http",
            "direct_vllm_custom",
            false,
            "codex/codex-direct-vllm-http-custom-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml",
            &[("direct-vllm-custom-0", 151), ("direct-vllm-custom-1", 47)]
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn provider_long_workload() -> Vec<Workload> {
    // A provider-specific multi-agent public response, not a raw vLLM single
    // inference round or evidence of executing agents/tools in this harness.
    capture!(
        "openai_provider_multi_agent_http",
        "provider_multi_agent_long_text",
        true,
        "multi_agent/multi-agent-openai-reference-review-gpt-5.6-sol-streaming.yaml",
        &[("provider-multi-agent-long-text-0", 1761)]
    )
}

fn workloads(
    capture_boundary: &'static str,
    family: &'static str,
    extended_gate: bool,
    cassette: &'static str,
    source: &str,
    expected: &[(&'static str, usize)],
) -> Vec<Workload> {
    let turns = parse_cassette(source).turns;
    assert_eq!(
        turns.len(),
        expected.len(),
        "do not silently drop recorded turns: {cassette}"
    );
    turns
        .into_iter()
        .zip(expected)
        .enumerate()
        .map(|(source_turn, (turn, &(id, expected_framed_lines)))| {
            assert!(!turn.response.sse.is_empty());
            Workload {
                identity: WorkloadIdentity {
                    id,
                    family,
                    cassette,
                    source_turn,
                    expected_framed_lines,
                    capture_boundary,
                    extended_gate,
                },
                wire: turn.response.sse.concat(),
            }
        })
        .collect()
}

#[test]
fn named_workload_manifest_preserves_every_audited_request() {
    let all = named_workloads();
    let ids: Vec<_> = all.iter().map(|workload| workload.identity.id).collect();
    assert_eq!(
        ids,
        [
            "reasoning-reference-0",
            "function-multiturn-0",
            "function-multiturn-1",
            "function-multiturn-2",
            "text-short-0",
            "parallel-client-0",
            "parallel-client-1",
            "parallel-client-2",
            "parallel-client-3",
            "direct-vllm-function-0",
            "direct-vllm-function-1",
            "direct-vllm-flat-namespace-0",
            "direct-vllm-flat-namespace-1",
            "direct-vllm-custom-0",
            "direct-vllm-custom-1",
            "provider-multi-agent-long-text-0"
        ]
    );
    assert_eq!(ids.iter().collect::<std::collections::BTreeSet<_>>().len(), ids.len());
    assert_eq!(
        all.iter()
            .map(|workload| workload.identity.family)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "reasoning_sample",
            "multi_turn_function",
            "text_short",
            "parallel_client_function",
            "direct_vllm_function",
            "direct_vllm_flat_namespace",
            "direct_vllm_custom",
            "provider_multi_agent_long_text"
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(
        all.iter()
            .take(4)
            .map(|workload| workload.wire.clone())
            .collect::<Vec<_>>(),
        recorded_wire_rounds()
    );
    assert!(all.iter().all(|workload| workload.identity.expected_framed_lines > 0));
    assert_eq!(
        all.iter()
            .filter(|workload| workload.identity.capture_boundary == "direct_vllm_http")
            .count(),
        6
    );
    let extended: Vec<_> = all.iter().filter(|workload| workload.identity.extended_gate).collect();
    assert_eq!(extended.len(), 1);
    assert_eq!(extended[0].identity.id, "provider-multi-agent-long-text-0");
    println!(
        "PLACEMENT_NAMED_MANIFEST {}",
        serde_json::to_string(&all.iter().map(|workload| workload.identity).collect::<Vec<_>>()).unwrap()
    );
}

#[derive(Deserialize)]
struct Cassette {
    turns: Vec<Turn>,
}

#[derive(Deserialize)]
struct Turn {
    response: Response,
}

#[derive(Deserialize)]
struct Response {
    sse: Vec<String>,
}

/// Replay the checked-in capture unchanged and keep each request's lifecycle separate.
pub(super) fn recorded_rounds() -> Vec<Vec<String>> {
    // Use recorded reasoning and multi-turn function calls, not fabricated captures.
    // The older benchmark cassette is retained separately as a strict-error case.
    recorded_sources().into_iter().flat_map(parse_rounds).collect()
}

/// Preserve every recorded SSE byte for the native inference transport.
pub(super) fn recorded_wire_rounds() -> Vec<String> {
    recorded_sources().into_iter().flat_map(wire_rounds).collect()
}

/// Shared cassette decoding, matching the repository's integration-test loader.
/// This reads original SSE strings; it performs no protocol framing or filtering.
pub(super) fn wire_rounds(source: &str) -> Vec<String> {
    cassette_wire(parse_cassette(source))
}

fn cassette_wire(cassette: Cassette) -> Vec<String> {
    cassette
        .turns
        .into_iter()
        .map(|turn| {
            assert!(!turn.response.sse.is_empty());
            turn.response.sse.concat()
        })
        .collect()
}

fn recorded_sources() -> [&'static str; 2] {
    [
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/cassettes/reasoning/responses/reasoning-openai-reference-gpt-5.6-streaming.yaml"
        )),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/cassettes/tool_calls/multi_turn/openai_responses_tool_calls_3turn_streaming.yaml"
        )),
    ]
}

pub(super) fn rejected_benchmark_rounds() -> Vec<Vec<String>> {
    parse_rounds(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/benches/cassettes/reasoning-and-tool-call-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml"
    )))
}

fn parse_cassette(source: &str) -> Cassette {
    let cassette: Cassette = serde_yaml::from_str(source).expect("checked-in recording must parse");
    assert!(!cassette.turns.is_empty());
    cassette
}

#[test]
fn canonical_decoder_preserves_legacy_small_capture_wire_bytes() {
    let direct = [
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/cassettes/codex/codex-direct-vllm-http-function-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml"
        )),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/cassettes/codex/codex-direct-vllm-http-flat-namespace-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml"
        )),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/cassettes/codex/codex-direct-vllm-http-custom-tool-Qwen-Qwen3.6-35B-A3B-streaming.yaml"
        )),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/benches/cassettes/reasoning-and-tool-call-Qwen-Qwen3-30B-A3B-FP8-streaming.yaml"
        )),
    ];
    for source in recorded_sources().into_iter().chain(direct) {
        let legacy: Cassette = serde_yml::from_str(source).unwrap();
        assert_eq!(cassette_wire(legacy), wire_rounds(source));
    }
}

#[test]
fn long_provider_captures_match_independent_utf8_wire_digests() {
    // Digests independently calculated with PyYAML + hashlib; no clipping,
    // event conversion or rewriting of the captured terminal snapshot.
    let cases = [
        (
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/cassettes/multi_agent/multi-agent-openai-reference-review-gpt-5.6-sol-streaming.yaml"
            )),
            700_723,
            [
                0x70, 0x03, 0x85, 0x20, 0x8b, 0x41, 0xb4, 0x30, 0xc1, 0xab, 0x8b, 0x64, 0x7b, 0x9f, 0xe9, 0x18, 0xe0,
                0x26, 0xfe, 0x18, 0xdd, 0x7c, 0xf7, 0x9f, 0xb2, 0xc6, 0x8d, 0x4c, 0x56, 0x4d, 0xff, 0x3c,
            ],
        ),
        (
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/cassettes/multi_agent/multi-agent-openai-reference-code-interpreter-gpt-5.6-sol-streaming.yaml"
            )),
            925_108,
            [
                0xe9, 0x65, 0xaf, 0x50, 0xe8, 0x1f, 0x6b, 0x1d, 0xb3, 0x6c, 0x05, 0xe1, 0x63, 0xa5, 0x64, 0xf4, 0xd2,
                0xe2, 0xa1, 0xb6, 0x81, 0xb4, 0x65, 0x70, 0x2e, 0x8b, 0x88, 0x14, 0x48, 0x90, 0x37, 0x17,
            ],
        ),
    ];
    for (source, bytes, expected) in cases {
        let all = wire_rounds(source);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].len(), bytes);
        assert_eq!(
            ring::digest::digest(&ring::digest::SHA256, all[0].as_bytes()).as_ref(),
            &expected
        );
    }
}

fn parse_rounds(source: &str) -> Vec<Vec<String>> {
    parse_cassette(source)
        .turns
        .into_iter()
        .map(|turn| {
            let lines: Vec<_> = turn
                .response
                .sse
                .into_iter()
                .flat_map(|entry| entry.lines().map(str::to_owned).collect::<Vec<_>>())
                .collect();
            assert!(!lines.is_empty(), "a recorded streaming turn must not disappear");
            lines
        })
        .collect()
}
