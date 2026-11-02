use serde_json::{Value, json};

use super::model_text::UNAVAILABLE_RESULTS_TEXT;
use super::replay::replay_server_tool_blocks;
use super::*;
use crate::executor::ExecutorError;
use crate::executor::messages_request::{
    web_fetch_budget_exhausted_result, web_fetch_refused_result, web_search_budget_exhausted_result,
};
use crate::tool::web_fetch::failure_output;
use crate::types::messages::{MessagesRequest, WebFetchErrorCode, WebSearchResultBlock, WebSearchToolResultContent};

fn key() -> MessagesResultKey {
    MessagesResultKey::from_secret(b"0123456789abcdef0123456789abcdef").unwrap()
}

fn other_key() -> MessagesResultKey {
    MessagesResultKey::from_secret(b"fedcba9876543210fedcba9876543210").unwrap()
}

/// An executor `web_search` output with the provider metadata the model no
/// longer receives for a native call.
fn search_output() -> String {
    json!({
        "query": "rust release",
        "queries": ["rust release", "rust 2024 edition"],
        "results": {
            "web": [
                {"url": "https://blog.rust-lang.org/", "title": "Rust Blog", "description": "Announcing \"Rust\" 1.89",
                 "snippets": ["Rust 1.89.0 is out", "ünïcödé — \u{2028} line"], "page_age": "2025-08-07",
                 "contents": {"markdown": "# Rust 1.89", "highlights": ["stable"]}},
                {"url": "https://www.rust-lang.org/"}
            ],
            "news": [{"url": "https://news.example/rust", "title": "Rust ships", "contents": {}}]
        },
        "metadata": [{"query": "rust release", "search_uuid": "u-1", "latency": 0.12}]
    })
    .to_string()
}

fn search_input() -> Value {
    json!({"queries": ["rust release", "rust 2024 edition"], "count": 3})
}

/// Serialize a block as the client receives it and parse it back.
fn through_client(block: &ServerToolResultBlock) -> ServerToolResultBlock {
    serde_json::from_str(&serde_json::to_string(block).unwrap()).unwrap()
}

fn replay_of(projected: &ProjectedResult, input: &Value, key: &MessagesResultKey) -> GatewayToolResult {
    replayed_tool_result("toolu_1", input, &through_client(&projected.block), key).unwrap()
}

#[test]
fn only_native_declarations_show_their_calls() {
    let tools: MessagesRequest = serde_json::from_value(json!({
        "model": "m", "max_tokens": 1, "messages": [],
        "tools": [{"type": "web_search_20250305", "name": "web_search"},
                  {"type": "web_fetch_20250910", "name": "web_fetch"},
                  {"name": "WebSearch", "input_schema": {"type": "object"}}]
    }))
    .unwrap();
    let native = NativeServerTools::from_declarations(tools.tools.as_ref());
    assert_eq!(native.tool("web_search"), Some(NativeTool::WebSearch));
    assert_eq!(native.tool("web_fetch"), Some(NativeTool::WebFetch));
    assert_eq!(native.tool("WebSearch"), None, "an alias stays hidden");

    let plain: MessagesRequest = serde_json::from_value(json!({
        "model": "m", "max_tokens": 1, "messages": [],
        "tools": [{"name": "web_search", "input_schema": {"type": "object"}},
                  {"type": "web_search_20250305", "name": "search_the_web"}]
    }))
    .unwrap();
    let native = NativeServerTools::from_declarations(plain.tools.as_ref());
    assert_eq!(
        native.tool("web_search"),
        None,
        "a plain function named web_search stays hidden"
    );
    assert_eq!(native.tool("search_the_web"), None);
    assert_eq!(NativeServerTools::from_declarations(None).tool("web_fetch"), None);
}

#[test]
fn client_ids_carry_the_upstream_id() {
    let id = client_tool_use_id("toolu_01abc");
    assert_eq!(id, "srvtoolu_gw_toolu_01abc");
    assert_eq!(upstream_tool_use_id(&id), Some("toolu_01abc"));
    assert_eq!(
        upstream_tool_use_id("srvtoolu_01abc"),
        None,
        "Anthropic's own ids are not ours"
    );
    assert!(may_carry_gateway_server_tool_blocks(br#"{"id":"srvtoolu_gw_x"}"#));
    assert!(!may_carry_gateway_server_tool_blocks(br#"{"id":"srvtoolu_01x"}"#));
}

#[test]
fn a_search_replays_to_the_text_its_live_round_projected() {
    let key = key();
    let projected = project_native_call(
        NativeTool::WebSearch,
        "toolu_1",
        &search_input(),
        NativeOutcome::Output {
            text: search_output(),
            failed: false,
        },
        &key,
    );
    assert!(!projected.tool_result.is_error);
    assert_eq!(projected.tool_result.tool_use_id, "toolu_1");
    assert_eq!(projected.uses.web_search_requests, 2, "each query is one search");
    assert_eq!(projected.uses.web_fetch_requests, 0);

    // The model sees the queries and every result as the executor returned it,
    // without provider metadata.
    let mut expected: Value = serde_json::from_str(&search_output()).unwrap();
    expected.as_object_mut().unwrap().remove("metadata");
    let text: Value = serde_json::from_str(&projected.tool_result.content).unwrap();
    assert_eq!(text, expected);
    assert!(
        projected
            .tool_result
            .content
            .starts_with(r#"{"query":"rust release","queries":["#),
        "{}",
        projected.tool_result.content
    );

    // The client sees url, title, and page age; the content only sealed.
    let client = serde_json::to_value(&projected.block).unwrap();
    assert_eq!(client["type"], "web_search_tool_result");
    assert_eq!(client["tool_use_id"], "srvtoolu_gw_toolu_1");
    let results = client["content"].as_array().unwrap();
    assert_eq!(results.len(), 3, "web results, then news");
    assert_eq!(results[0]["title"], "Rust Blog");
    assert_eq!(results[0]["page_age"], "2025-08-07");
    assert_eq!(results[1]["title"], "", "a result without a title");
    assert_eq!(results[1]["page_age"], Value::Null);
    assert_eq!(results[2]["url"], "https://news.example/rust");
    assert!(!client.to_string().contains("Rust 1.89.0 is out"));

    assert_eq!(replay_of(&projected, &search_input(), &key), projected.tool_result);
}

#[test]
fn a_search_without_results_replays_without_sealed_content() {
    let key = key();
    let output = json!({"query": "q", "queries": ["q"], "results": {"web": [], "news": []},
                        "metadata": [{"query": "q"}]})
    .to_string();
    let input = json!({"query": "q"});
    let projected = project_native_call(
        NativeTool::WebSearch,
        "toolu_1",
        &input,
        NativeOutcome::Output {
            text: output,
            failed: false,
        },
        &key,
    );
    assert_eq!(
        serde_json::to_value(&projected.block).unwrap()["content"],
        json!([]),
        "no results is an empty list, not an error"
    );
    assert_eq!(
        projected.tool_result.content,
        r#"{"query":"q","queries":["q"],"results":{"web":[],"news":[]}}"#
    );
    assert_eq!(projected.uses.web_search_requests, 1);
    assert_eq!(replay_of(&projected, &input, &other_key()), projected.tool_result);
}

#[test]
fn search_errors_carry_their_documented_code() {
    let key = key();
    let input = json!({"query": "q"});
    for (outcome, code, text) in [
        (
            NativeOutcome::Refused(Refusal::MaxUsesExceeded),
            "max_uses_exceeded",
            web_search_budget_exhausted_result("toolu_1").content,
        ),
        (
            NativeOutcome::Refused(Refusal::InvalidInput),
            "invalid_tool_input",
            "web_search input was invalid; search was not run".to_owned(),
        ),
        (
            NativeOutcome::Failed,
            "unavailable",
            "web_search is unavailable; search did not complete".to_owned(),
        ),
        (
            NativeOutcome::Output {
                text: "tool execution failed".to_owned(),
                failed: true,
            },
            "unavailable",
            "web_search is unavailable; search did not complete".to_owned(),
        ),
        (
            NativeOutcome::Output {
                text: "not json".to_owned(),
                failed: false,
            },
            "unavailable",
            "web_search is unavailable; search did not complete".to_owned(),
        ),
    ] {
        let projected = project_native_call(NativeTool::WebSearch, "toolu_1", &input, outcome, &key);
        let client = serde_json::to_value(&projected.block).unwrap();
        assert_eq!(
            client["content"],
            json!({"type": "web_search_tool_result_error", "error_code": code})
        );
        assert!(projected.tool_result.is_error);
        assert_eq!(projected.tool_result.content, text, "{code}");
        assert_eq!(projected.uses, ServerToolUses::default(), "an error is not a use");
        assert_eq!(replay_of(&projected, &input, &key), projected.tool_result);
    }
}

#[test]
fn a_fetched_page_replays_from_the_fields_its_block_carries() {
    let key = key();
    let output = json!({"type": "web_fetch_result", "url": "https://example.com/a", "title": "A \"page\"",
                        "content_type": "text/html", "retrieved_at": "2026-10-30T12:00:00Z",
                        "truncated": true, "content": "Body\nwith ünïcödé"})
    .to_string();
    assert!(
        output.starts_with(r#"{"type":"web_fetch_result","url":"#),
        "the executor's field order"
    );
    let input = json!({"url": "https://example.com/a"});
    let projected = project_native_call(
        NativeTool::WebFetch,
        "toolu_1",
        &input,
        NativeOutcome::Output {
            text: output.clone(),
            failed: false,
        },
        &key,
    );
    assert_eq!(
        serde_json::to_value(&projected.block).unwrap(),
        json!({"type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_gw_toolu_1",
               "content": {"type": "web_fetch_result", "url": "https://example.com/a",
                           "content": {"type": "document",
                                       "source": {"type": "text", "media_type": "text/plain",
                                                  "data": "Body\nwith ünïcödé"},
                                       "title": "A \"page\""},
                           "retrieved_at": "2026-10-30T12:00:00Z",
                           "content_type": "text/html", "truncated": true}})
    );
    // The model reads exactly the executor's output, projected from the block.
    assert_eq!(projected.tool_result.content, output);
    assert!(!projected.tool_result.is_error);
    assert_eq!(projected.uses.web_fetch_requests, 1);
    assert_eq!(replay_of(&projected, &input, &other_key()), projected.tool_result);

    // A client that keeps only Anthropic's fields still replays the page.
    let mut documented = serde_json::to_value(&projected.block).unwrap();
    for field in ["content_type", "truncated"] {
        documented["content"].as_object_mut().unwrap().remove(field);
    }
    let replayed = replayed_tool_result("toolu_1", &input, &serde_json::from_value(documented).unwrap(), &key).unwrap();
    assert_eq!(
        replayed.content,
        r#"{"type":"web_fetch_result","url":"https://example.com/a","title":"A \"page\"","retrieved_at":"2026-10-30T12:00:00Z","content":"Body\nwith ünïcödé"}"#
    );
}

#[test]
fn fetch_errors_keep_the_loop_texts_for_codes_the_loop_answers() {
    let key = key();
    let input = json!({"url": "https://example.com/a"});
    let refused = |refusal| {
        project_native_call(
            NativeTool::WebFetch,
            "toolu_1",
            &input,
            NativeOutcome::Refused(refusal),
            &key,
        )
    };
    assert_eq!(
        refused(Refusal::MaxUsesExceeded).tool_result,
        web_fetch_budget_exhausted_result("toolu_1")
    );
    assert_eq!(
        refused(Refusal::UrlNotInPriorContext).tool_result,
        web_fetch_refused_result(
            "toolu_1",
            WebFetchErrorCode::UrlNotInPriorContext,
            fetch_error_message(WebFetchErrorCode::UrlNotInPriorContext)
        )
    );

    // An executor refusal keeps its code and the message the model reads.
    let failure = failure_output(WebFetchErrorCode::UrlNotAccessible, "HTTP 404 Not Found");
    let projected = project_native_call(
        NativeTool::WebFetch,
        "toolu_1",
        &input,
        NativeOutcome::Output {
            text: failure.clone(),
            failed: true,
        },
        &key,
    );
    assert_eq!(
        serde_json::to_value(&projected.block).unwrap()["content"],
        json!({"type": "web_fetch_tool_result_error", "error_code": "url_not_accessible",
               "message": "HTTP 404 Not Found"})
    );
    assert_eq!(projected.tool_result.content, failure);
    assert!(projected.tool_result.is_error);
    assert_eq!(projected.uses, ServerToolUses::default());
    assert_eq!(replay_of(&projected, &input, &key), projected.tool_result);

    // Without a message, the model reads the code's fixed message.
    let mut documented = serde_json::to_value(&projected.block).unwrap();
    documented["content"].as_object_mut().unwrap().remove("message");
    assert_eq!(
        replayed_tool_result("toolu_1", &input, &serde_json::from_value(documented).unwrap(), &key)
            .unwrap()
            .content,
        r#"{"type":"web_fetch_tool_result_error","error_code":"url_not_accessible","message":"the page could not be retrieved"}"#
    );
    for (outcome, code) in [
        (NativeOutcome::Failed, "unavailable"),
        (NativeOutcome::Refused(Refusal::InvalidInput), "invalid_tool_input"),
        (
            NativeOutcome::Output {
                text: "not json".to_owned(),
                failed: true,
            },
            "unavailable",
        ),
    ] {
        let projected = project_native_call(NativeTool::WebFetch, "toolu_1", &input, outcome, &key);
        assert_eq!(
            serde_json::to_value(&projected.block).unwrap()["content"],
            json!({"type": "web_fetch_tool_result_error", "error_code": code}),
            "executor internals are not shown"
        );
        assert!(projected.tool_result.is_error);
        assert_eq!(replay_of(&projected, &input, &key), projected.tool_result);
    }
}

fn sealed_search(key: &MessagesResultKey) -> ProjectedResult {
    project_native_call(
        NativeTool::WebSearch,
        "toolu_1",
        &search_input(),
        NativeOutcome::Output {
            text: search_output(),
            failed: false,
        },
        key,
    )
}

fn results_of(block: &ServerToolResultBlock) -> Vec<WebSearchResultBlock> {
    match block {
        ServerToolResultBlock::WebSearchToolResult {
            content: WebSearchToolResultContent::Results(results),
            ..
        } => results.clone(),
        other => panic!("not search results: {other:?}"),
    }
}

fn with_results(results: Vec<WebSearchResultBlock>) -> ServerToolResultBlock {
    ServerToolResultBlock::WebSearchToolResult {
        tool_use_id: "srvtoolu_gw_toolu_1".to_owned(),
        content: WebSearchToolResultContent::Results(results),
    }
}

#[test]
fn altered_search_content_is_refused() {
    let key = key();
    let projected = sealed_search(&key);
    let results = results_of(&projected.block);
    let replay = |block: &ServerToolResultBlock, id: &str, input: &Value| replayed_tool_result(id, input, block, &key);

    let mut flipped = results.clone();
    let mut envelope = flipped[0].encrypted_content.clone().into_bytes();
    let last = envelope.len() - 1;
    envelope[last] = if envelope[last] == b'A' { b'B' } else { b'A' };
    flipped[0].encrypted_content = String::from_utf8(envelope).unwrap();
    let mut reordered = results.clone();
    reordered.swap(0, 1);
    let dropped = results[..2].to_vec();
    let mut missing = results.clone();
    missing[1].encrypted_content = String::new();
    for (name, block) in [
        ("flipped", with_results(flipped)),
        ("reordered", with_results(reordered)),
        ("dropped", with_results(dropped)),
        ("missing", with_results(missing)),
    ] {
        assert_eq!(
            replay(&block, "toolu_1", &search_input()),
            Err(ReplayFailure::Tampered),
            "{name}"
        );
    }
    // Moved to another call, or shown with other queries.
    assert_eq!(
        replay(&projected.block, "toolu_2", &search_input()),
        Err(ReplayFailure::Tampered)
    );
    assert_eq!(
        replay(
            &projected.block,
            "toolu_1",
            &json!({"queries": ["rust release", "go release"]})
        ),
        Err(ReplayFailure::Tampered)
    );
    assert_eq!(
        replay(&projected.block, "toolu_1", &json!({"query": ""})),
        Err(ReplayFailure::InvalidInput)
    );
    // Display fields are not what the model reads, so changing them changes nothing.
    let mut retitled = results;
    retitled[0].title = "Something else".to_owned();
    assert_eq!(
        replay(&with_results(retitled), "toolu_1", &search_input()),
        Ok(projected.tool_result)
    );
}

#[test]
fn results_sealed_under_another_key_become_a_note() {
    let projected = sealed_search(&key());
    let replayed = replay_of(&projected, &search_input(), &other_key());
    assert!(replayed.is_error);
    assert_eq!(replayed.content, UNAVAILABLE_RESULTS_TEXT);
    assert_eq!(replayed.tool_use_id, "toolu_1");

    // Results sealed under two keys were assembled by hand.
    let mut mixed = results_of(&projected.block);
    mixed[1] = results_of(&sealed_search(&other_key()).block)[1].clone();
    assert_eq!(
        replayed_tool_result("toolu_1", &search_input(), &with_results(mixed), &key()),
        Err(ReplayFailure::Tampered)
    );
}

/// The assistant turn a client returns for rounds the gateway ran.
fn client_turn(content: &[Value]) -> Value {
    json!({"role": "assistant", "content": content})
}

fn call(upstream_id: &str, name: &str, input: &Value) -> Value {
    json!({"type": "server_tool_use", "id": client_tool_use_id(upstream_id), "name": name, "input": input})
}

fn fetch_result(upstream_id: &str, key: &MessagesResultKey) -> ProjectedResult {
    project_native_call(
        NativeTool::WebFetch,
        upstream_id,
        &json!({"url": "https://example.com/a"}),
        NativeOutcome::Output {
            text: json!({"type": "web_fetch_result", "url": "https://example.com/a", "content_type": "text/plain",
                         "retrieved_at": "2026-10-30T12:00:00Z", "truncated": false, "content": upstream_id})
            .to_string(),
            failed: false,
        },
        key,
    )
}

fn tool_use(id: &str, name: &str, input: &Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}

fn results_message(results: &[&ProjectedResult]) -> Value {
    json!({"role": "user", "content": results.iter().map(|result| &result.tool_result).collect::<Vec<_>>()})
}

#[test]
fn a_returned_turn_splits_back_into_the_rounds_the_upstream_saw() {
    let key = key();
    let fetch_input = json!({"url": "https://example.com/a"});
    let (first, second, third) = (
        fetch_result("toolu_1", &key),
        fetch_result("toolu_2", &key),
        fetch_result("toolu_3", &key),
    );
    let search = project_native_call(
        NativeTool::WebSearch,
        "toolu_s",
        &search_input(),
        NativeOutcome::Output {
            text: search_output(),
            failed: false,
        },
        &key,
    );
    let block = |projected: &ProjectedResult| serde_json::to_value(&projected.block).unwrap();
    let mut request = json!({
        "model": "m", "max_tokens": 1,
        "messages": [
            {"role": "user", "content": "Read https://example.com/a"},
            client_turn(&[
                json!({"type": "thinking", "thinking": "t", "signature": "s"}),
                json!({"type": "text", "text": "Fetching."}),
                call("toolu_1", "web_fetch", &fetch_input),
                call("toolu_2", "web_fetch", &fetch_input),
                block(&first),
                block(&second),
                call("toolu_3", "web_fetch", &fetch_input),
                json!({"type": "text", "text": "and after the call"}),
                block(&third),
                call("toolu_s", "web_search", &search_input()),
                // Anthropic's own blocks and client calls are not the gateway's.
                json!({"type": "server_tool_use", "id": "srvtoolu_01real", "name": "web_search", "input": {}}),
                block(&search),
                json!({"type": "text", "text": "Done.", "citations": null}),
                json!({"type": "tool_use", "id": "toolu_c", "name": "client_fn", "input": {}})
            ]),
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_c", "content": "ok"}]}
        ]
    });

    assert!(replay_server_tool_blocks(&mut request, &key).unwrap());
    assert_eq!(
        request["messages"],
        json!([
            {"role": "user", "content": "Read https://example.com/a"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "t", "signature": "s"},
                {"type": "text", "text": "Fetching."},
                tool_use("toolu_1", "web_fetch", &fetch_input),
                tool_use("toolu_2", "web_fetch", &fetch_input)
            ]},
            results_message(&[&first, &second]),
            {"role": "assistant", "content": [
                tool_use("toolu_3", "web_fetch", &fetch_input),
                {"type": "text", "text": "and after the call"}
            ]},
            results_message(&[&third]),
            {"role": "assistant", "content": [
                tool_use("toolu_s", "web_search", &search_input()),
                {"type": "server_tool_use", "id": "srvtoolu_01real", "name": "web_search", "input": {}}
            ]},
            results_message(&[&search]),
            {"role": "assistant", "content": [
                {"type": "text", "text": "Done.", "citations": null},
                {"type": "tool_use", "id": "toolu_c", "name": "client_fn", "input": {}}
            ]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_c", "content": "ok"}]}
        ])
    );
}

#[test]
fn results_that_end_a_turn_join_the_next_user_message() {
    let key = key();
    let fetched = fetch_result("toolu_1", &key);
    let mut request = json!({"messages": [
        {"role": "user", "content": "go"},
        client_turn(&[call("toolu_1", "web_fetch", &json!({"url": "https://example.com/a"})),
                      serde_json::to_value(&fetched.block).unwrap()]),
        {"role": "user", "content": "thanks"}
    ]});
    assert!(replay_server_tool_blocks(&mut request, &key).unwrap());
    assert_eq!(request["messages"].as_array().unwrap().len(), 3);
    assert_eq!(
        request["messages"][2]["content"],
        json!([fetched.tool_result, {"type": "text", "text": "thanks"}])
    );
}

#[test]
fn a_request_without_gateway_blocks_is_left_alone() {
    let mut request = json!({"messages": [
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [
            {"type": "server_tool_use", "id": "srvtoolu_01", "name": "web_search", "input": {}},
            {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_01", "content": []}
        ]}
    ]});
    let before = request.clone();
    assert!(!replay_server_tool_blocks(&mut request, &key()).unwrap());
    assert_eq!(request, before);
    assert!(!replay_server_tool_blocks(&mut json!({"model": "m"}), &key()).unwrap());
}

#[test]
fn malformed_or_unpaired_gateway_blocks_are_refused() {
    let key = key();
    let fetch_input = json!({"url": "https://example.com/a"});
    let fetched = serde_json::to_value(fetch_result("toolu_1", &key).block).unwrap();
    let mut wrong_kind = fetched.clone();
    wrong_kind["type"] = json!("web_search_tool_result");
    let mut tampered = serde_json::to_value(sealed_search(&key).block).unwrap();
    tampered["content"][0]["encrypted_content"] = json!("AAAA");
    for (name, content, message) in [
        (
            "call without result",
            vec![call("toolu_1", "web_fetch", &fetch_input)],
            "`web_fetch` tool use with id `srvtoolu_gw_toolu_1` was found without a corresponding \
             `web_fetch_tool_result` block",
        ),
        (
            "result without call",
            vec![fetched.clone()],
            "web_fetch_tool_result block for `srvtoolu_gw_toolu_1` does not follow its server_tool_use block",
        ),
        (
            "result before call",
            vec![fetched.clone(), call("toolu_1", "web_fetch", &fetch_input)],
            "does not follow its server_tool_use block",
        ),
        (
            "repeated result",
            vec![
                call("toolu_1", "web_fetch", &fetch_input),
                fetched.clone(),
                fetched.clone(),
            ],
            "does not follow its server_tool_use block",
        ),
        (
            "repeated call",
            vec![
                call("toolu_1", "web_fetch", &fetch_input),
                fetched.clone(),
                call("toolu_1", "web_fetch", &fetch_input),
                fetched.clone(),
            ],
            "appears more than once",
        ),
        (
            "unknown tool",
            vec![call("toolu_1", "code_execution", &json!({}))],
            "which the gateway does not run",
        ),
        (
            "kind mismatch",
            vec![call("toolu_1", "web_fetch", &fetch_input), wrong_kind],
            "invalid gateway web_search_tool_result block",
        ),
        (
            "search answering a fetch",
            vec![call("toolu_1", "web_fetch", &fetch_input), tampered.clone()],
            "web_search_tool_result block for `srvtoolu_gw_toolu_1` answers a `web_fetch` call",
        ),
        (
            "tampered content",
            vec![call("toolu_1", "web_search", &search_input()), tampered],
            "web_search_tool_result block for `srvtoolu_gw_toolu_1` was modified or was not issued by this gateway",
        ),
        (
            "malformed call",
            vec![json!({"type": "server_tool_use", "id": "srvtoolu_gw_x"})],
            "invalid gateway server_tool_use block",
        ),
    ] {
        let mut request = json!({"messages": [{"role": "user", "content": "go"}, client_turn(&content)]});
        let error = replay_server_tool_blocks(&mut request, &key).unwrap_err();
        assert!(matches!(error, ExecutorError::InvalidRequest(_)), "{name}: {error:?}");
        assert!(error.to_string().contains(message), "{name}: {error}");
    }
}
