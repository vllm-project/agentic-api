#[path = "support/multi_agent_contract.rs"]
mod multi_agent_contract;
mod support;

use multi_agent_contract::{ComparisonPolicy, RecordedSession, assert_multi_agent_contract};
use std::path::Path;

#[test]
fn reference_cassettes_have_consistent_multi_agent_contract() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes/multi_agent");
    for suffix in [
        "review-gpt-5.6-sol-nonstreaming",
        "review-gpt-5.6-sol-streaming",
        "proposals-gpt-5.6-sol-nonstreaming",
        "proposals-gpt-5.6-sol-streaming",
        "mixed-tools-gpt-5.6-sol-nonstreaming",
        "mixed-tools-gpt-5.6-sol-streaming",
    ] {
        let path = directory.join(format!("multi-agent-openai-reference-{suffix}.yaml"));
        RecordedSession::load(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    }
}

fn assert_qwen_case(scenario: &str, mode: &str) {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cassettes/multi_agent");
    let reference_path = directory.join(format!(
        "multi-agent-openai-reference-{scenario}-gpt-5.6-sol-{mode}.yaml"
    ));
    let gateway_path = directory.join(format!(
        "multi-agent-gateway-{scenario}-Qwen-Qwen3.6-35B-A3B-FP8-{mode}.yaml"
    ));
    let reference =
        RecordedSession::load(&reference_path).unwrap_or_else(|error| panic!("{}: {error}", reference_path.display()));
    let gateway =
        RecordedSession::load(&gateway_path).unwrap_or_else(|error| panic!("{}: {error}", gateway_path.display()));
    assert_multi_agent_contract(
        &reference,
        &gateway,
        &ComparisonPolicy {
            require_reference_tool_kinds: true,
        },
    )
    .unwrap_or_else(|error| panic!("{scenario} {mode}: {error}"));
}

#[test]
fn review_nonstreaming_matches_reference_contract() {
    assert_qwen_case("review", "nonstreaming");
}

#[test]
fn review_streaming_matches_reference_contract() {
    assert_qwen_case("review", "streaming");
}

#[test]
fn proposals_nonstreaming_matches_reference_contract() {
    assert_qwen_case("proposals", "nonstreaming");
}

#[test]
fn proposals_streaming_matches_reference_contract() {
    assert_qwen_case("proposals", "streaming");
}

#[test]
fn mixed_tools_nonstreaming_matches_reference_contract() {
    assert_qwen_case("mixed-tools", "nonstreaming");
}

#[test]
fn mixed_tools_streaming_matches_reference_contract() {
    assert_qwen_case("mixed-tools", "streaming");
}

#[test]
#[ignore = "requires independently recorded OpenAI and gateway cassettes"]
fn compare_recorded_multi_agent_contract() {
    let reference = std::env::var("MULTI_AGENT_REFERENCE_CASSETTE").expect("set reference cassette path");
    let gateway = std::env::var("MULTI_AGENT_GATEWAY_CASSETTE").expect("set gateway cassette path");
    let reference = RecordedSession::load(Path::new(&reference)).unwrap();
    let gateway = RecordedSession::load(Path::new(&gateway)).unwrap();
    assert_multi_agent_contract(
        &reference,
        &gateway,
        &ComparisonPolicy {
            require_reference_tool_kinds: true,
        },
    )
    .unwrap();
}
