use super::*;

fn assert_completed_potato_web_search(output: &[OutputItem]) -> &WebSearchCall {
    assert_eq!(
        count_function_calls(output),
        0,
        "raw web_search function call should not leak"
    );
    let web_search = output
        .iter()
        .find_map(|item| match item {
            OutputItem::WebSearchCall(call) => Some(call),
            _ => None,
        })
        .expect("cassette output should include a web_search_call item");
    assert_eq!(web_search.status.as_str(), "completed");
    assert_eq!(web_search.action.as_search().unwrap().query, "potato");
    assert_eq!(web_search.action.type_str(), "search");
    assert!(web_search.id.starts_with("ws_"));
    assert!(
        output.iter().any(|item| matches!(item, OutputItem::Message(_))),
        "cassette output should include a final assistant message"
    );
    web_search
}

fn assert_matching_web_search_output(openai: &[OutputItem], gateway: &[OutputItem]) {
    for (provider, output) in [("OpenAI", openai), ("gateway", gateway)] {
        let public_types: Vec<&str> = output
            .iter()
            .filter_map(|item| match item {
                OutputItem::Reasoning(_) => None,
                OutputItem::WebSearchCall(_) => Some("web_search_call"),
                OutputItem::Message(_) => Some("message"),
                _ => Some("unexpected"),
            })
            .collect();
        assert_eq!(
            public_types,
            ["web_search_call", "message"],
            "{provider} output should preserve web_search_call before message"
        );
    }

    let openai_call = assert_completed_potato_web_search(openai);
    let gateway_call = assert_completed_potato_web_search(gateway);
    assert_eq!(gateway_call.status, openai_call.status);
    assert_eq!(gateway_call.action.type_str(), openai_call.action.type_str());

    let openai_action = openai_call.action.as_search().unwrap();
    let gateway_action = gateway_call.action.as_search().unwrap();
    assert_eq!(gateway_action.query, openai_action.query);
    assert_eq!(openai_action.queries, ["potato"]);
    assert!(
        gateway_action
            .sources
            .iter()
            .any(|source| source.url == "https://en.wikipedia.org/wiki/Potato"),
        "gateway web_search_call should retain its recorded sources"
    );
}

/// Extracts the `arguments` JSON string from the first function call in output items.

#[test]
fn test_web_search_accumulator_nonstreaming_matches_openai() {
    let (openai, gateway) = load_web_search_cassette_pair(false);
    let openai_output = process_nonstreaming_turn(&openai, 0, WEB_SEARCH_OPENAI_MODEL);
    let gateway_output = process_nonstreaming_turn(&gateway, 0, WEB_SEARCH_GATEWAY_MODEL);

    assert!(
        has_reasoning(&openai_output),
        "reasoning-capable OpenAI reference should include reasoning"
    );
    assert!(
        has_reasoning(&gateway_output),
        "gateway output should include reasoning"
    );
    assert_matching_web_search_output(&openai_output, &gateway_output);
}

#[test]
fn test_web_search_accumulator_streaming_matches_openai() {
    let (openai, gateway) = load_web_search_cassette_pair(true);
    let openai_output = process_streaming_turn(&openai, 0, WEB_SEARCH_OPENAI_MODEL);
    let gateway_output = process_streaming_turn(&gateway, 0, WEB_SEARCH_GATEWAY_MODEL);

    assert!(
        has_reasoning(&gateway_output),
        "gateway output should preserve reasoning around web_search_call"
    );
    assert_matching_web_search_output(&openai_output, &gateway_output);
}
