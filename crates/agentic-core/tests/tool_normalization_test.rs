//! Cassette-driven validation of the tool framework wire types and normalization pipeline.
//!
//! Validates that real cassette request bodies — the exact JSON the gateway receives —
//! parse correctly into `Vec<ResponsesTool>` and normalize through the full pipeline.

use serde::Deserialize;

use agentic_core::tool::{ToolRegistry, ToolType};
use agentic_core::types::request_response::RequestPayload;
use agentic_core::types::tools::ResponsesTool;

const MULTI_TURN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/tool_calls/multi_turn");

#[derive(Deserialize)]
struct TurnCassette {
    turns: Vec<Turn>,
}

#[derive(Deserialize)]
struct Turn {
    request: serde_yml::Value,
    #[allow(dead_code)]
    response: serde_yml::Value,
}

fn load_cassette(filename: &str) -> TurnCassette {
    let path = format!("{MULTI_TURN_DIR}/{filename}");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_yml::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn tools_from_turn(turn: &Turn) -> Option<serde_json::Value> {
    let body = turn.request.get("body")?;
    let json: serde_json::Value = serde_json::to_value(body).ok()?;
    json.get("tools").cloned()
}

/// Parse `request.body.tools` from every turn of a cassette into `Vec<ResponsesTool>`.
fn assert_tools_parse(cassette_file: &str) {
    let cassette = load_cassette(cassette_file);
    for (i, turn) in cassette.turns.iter().enumerate() {
        let Some(tools_val) = tools_from_turn(turn) else {
            continue;
        };
        let tools: Vec<ResponsesTool> = serde_json::from_value(tools_val.clone())
            .unwrap_or_else(|e| panic!("{cassette_file} turn {i}: tools parse failed: {e}\nJSON: {tools_val}"));
        assert!(
            !tools.is_empty(),
            "{cassette_file} turn {i}: expected non-empty tools array"
        );
    }
}

/// For every turn that has tools, verify normalization produces only `FunctionTool` entries.
fn assert_tools_normalize(cassette_file: &str) {
    let cassette = load_cassette(cassette_file);
    for (i, turn) in cassette.turns.iter().enumerate() {
        let Some(tools_val) = tools_from_turn(turn) else {
            continue;
        };
        let tools: Vec<ResponsesTool> = serde_json::from_value(tools_val).expect("tools parse");
        let normalized: Vec<_> = tools.iter().filter_map(ResponsesTool::to_function_tool).collect();
        for ft in &normalized {
            assert_eq!(
                ft.type_, "function",
                "{cassette_file} turn {i}: normalized type must be 'function'"
            );
            assert!(
                !ft.name.is_empty(),
                "{cassette_file} turn {i}: normalized name must not be empty"
            );
        }
        // Every Function variant must normalize — count only Function entries
        // so the assertion holds even if future cassettes include gateway-only types.
        let function_count = tools.iter().filter(|t| matches!(t, ResponsesTool::Function(_))).count();
        assert_eq!(
            normalized.len(),
            function_count,
            "{cassette_file} turn {i}: each Function tool must produce exactly one FunctionTool (got {} of {})",
            normalized.len(),
            function_count
        );
    }
}

/// For every turn that has tools, verify `ToolRegistry::build` produces correct entries.
fn assert_registry_lookup(cassette_file: &str) {
    let cassette = load_cassette(cassette_file);
    for (i, turn) in cassette.turns.iter().enumerate() {
        let Some(tools_val) = tools_from_turn(turn) else {
            continue;
        };
        let tools: Vec<ResponsesTool> = serde_json::from_value(tools_val).expect("tools parse");
        let registry = ToolRegistry::build(&tools);
        for tool in &tools {
            if let ResponsesTool::Function(p) = tool {
                let entry = registry
                    .lookup(p.name.as_str())
                    .unwrap_or_else(|| panic!("{cassette_file} turn {i}: tool '{}' not found in registry", p.name));
                assert_eq!(
                    entry.tool_type,
                    ToolType::Function,
                    "{cassette_file} turn {i}: tool '{}' must be Function type",
                    p.name
                );
            }
        }
    }
}

/// Full round-trip: deserialize `request.body` → `RequestPayload` → `to_upstream_request()`
/// → assert upstream tools only contains `FunctionTool` entries.
fn assert_full_roundtrip(cassette_file: &str) {
    let cassette = load_cassette(cassette_file);
    for (i, turn) in cassette.turns.iter().enumerate() {
        let body = turn.request.get("body").expect("turn has body");
        let json: serde_json::Value = serde_json::to_value(body).expect("body to json");
        let payload: RequestPayload = serde_json::from_value(json.clone())
            .unwrap_or_else(|e| panic!("{cassette_file} turn {i}: RequestPayload parse failed: {e}"));
        let upstream = payload.to_upstream_request(false);
        if let Some(tools) = &upstream.tools {
            for ft in tools {
                assert_eq!(
                    ft.type_, "function",
                    "{cassette_file} turn {i}: upstream tools must only contain FunctionTool"
                );
                assert!(
                    !ft.name.is_empty(),
                    "{cassette_file} turn {i}: upstream tool name must not be empty"
                );
            }
        }
    }
}

#[test]
fn tools_parse_3turn() {
    assert_tools_parse("openai_responses_tool_calls_3turn.yaml");
}

#[test]
fn tools_parse_5turn() {
    assert_tools_parse("openai_responses_tool_calls_5turn.yaml");
}

#[test]
fn tools_parse_parallel() {
    assert_tools_parse("openai_responses_tool_calls_parallel.yaml");
}

#[test]
fn tools_normalize_3turn() {
    assert_tools_normalize("openai_responses_tool_calls_3turn.yaml");
}

#[test]
fn tools_normalize_5turn() {
    assert_tools_normalize("openai_responses_tool_calls_5turn.yaml");
}

#[test]
fn tools_normalize_parallel() {
    assert_tools_normalize("openai_responses_tool_calls_parallel.yaml");
}

#[test]
fn registry_lookup_3turn() {
    assert_registry_lookup("openai_responses_tool_calls_3turn.yaml");
}

#[test]
fn registry_lookup_5turn() {
    assert_registry_lookup("openai_responses_tool_calls_5turn.yaml");
}

#[test]
fn registry_lookup_parallel() {
    assert_registry_lookup("openai_responses_tool_calls_parallel.yaml");
}

#[test]
fn roundtrip_3turn() {
    assert_full_roundtrip("openai_responses_tool_calls_3turn.yaml");
}

#[test]
fn roundtrip_5turn() {
    assert_full_roundtrip("openai_responses_tool_calls_5turn.yaml");
}

#[test]
fn roundtrip_parallel() {
    assert_full_roundtrip("openai_responses_tool_calls_parallel.yaml");
}
