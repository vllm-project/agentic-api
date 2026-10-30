//! Recorded hosted tool search (`execution: "server"`) behavior for issue #448.
//!
//! Legs are documented in `record_hosted_tool_search_cassettes.sh`. The `OpenAI`
//! cassettes pin the reference contract; the gateway cassettes (Qwen on vLLM)
//! show the gateway meeting it. The two models choose different searches, so the
//! gateway checks compare shapes, lifecycles, and results for identical arguments.

use serde_json::{Value, json};

mod support;

const CASSETTE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/hosted_tool_search");
const OPENAI: &str = "openai-reference-gpt-5.6";
const GATEWAY: &str = "gateway-Qwen-Qwen3.6-35B-A3B-FP8";
const MCP_NAMESPACE: &str = "mcp_gitmcp_tiktoken";
const SERVER_DESCRIPTION: &str = "Documentation and code search for the openai/tiktoken tokenizer repository.";

fn cassette(group: &str, mode: &str) -> support::Cassette {
    provider_cassette(OPENAI, group, mode)
}

fn provider_cassette(provider: &str, group: &str, mode: &str) -> support::Cassette {
    support::load_cassette(&format!(
        "{CASSETTE_DIR}/hosted-tool-search-{group}-{provider}{mode}.yaml"
    ))
}

fn terminal(turn: &support::Turn) -> Value {
    if let Some(body) = &turn.response.body {
        return body.clone();
    }
    // A recorded model may exhaust the round limit; its items still have to be well formed.
    support::recorded_named_sse_events(turn)
        .into_iter()
        .find(|event| event["type"] == "response.completed" || event["type"] == "response.incomplete")
        .and_then(|event| event.get("response").cloned())
        .expect("streaming turn records a terminal response")
}

fn items<'a>(response: &'a Value, kind: &str) -> Vec<&'a Value> {
    response["output"]
        .as_array()
        .expect("response output")
        .iter()
        .filter(|item| item["type"] == kind)
        .collect()
}

/// Output item types in order, without reasoning items.
fn item_types(response: &Value) -> Vec<&str> {
    response["output"]
        .as_array()
        .expect("response output")
        .iter()
        .filter_map(|item| item["type"].as_str())
        .filter(|kind| *kind != "reasoning")
        .collect()
}

/// `(type, name, description, member names)` of one loaded tool.
type Loaded = (String, String, String, Vec<String>);

/// Each tool a search loaded.
fn loaded(output: &Value) -> Vec<Loaded> {
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    output["tools"]
        .as_array()
        .expect("tool_search_output.tools")
        .iter()
        .map(|tool| {
            let members = tool["tools"]
                .as_array()
                .map(|members| members.iter().map(|member| text(&member["name"])).collect())
                .unwrap_or_default();
            (
                text(&tool["type"]),
                text(&tool["name"]),
                text(&tool["description"]),
                members,
            )
        })
        .collect()
}

fn namespace(name: &str, description: &str, members: &[&str]) -> Loaded {
    (
        "namespace".to_owned(),
        name.to_owned(),
        description.to_owned(),
        members.iter().map(|member| (*member).to_owned()).collect(),
    )
}

fn function(name: &str, description: &str) -> Loaded {
    (
        "function".to_owned(),
        name.to_owned(),
        description.to_owned(),
        Vec::new(),
    )
}

fn assert_hosted(item: &Value) {
    assert_eq!(item["execution"], "server");
    assert_eq!(item["call_id"], Value::Null);
    assert_eq!(item["status"], "completed");
}

#[test]
fn validation_errors_match_openai() {
    let recorded = cassette("validation", "-nonstreaming");
    let responses: Vec<_> = recorded.turns.iter().map(terminal).collect();
    assert_eq!(responses.len(), 6, "validation legs: see the recorder header");

    // explicit-server: the echo drops `execution` even though the request sent it.
    assert_eq!(
        recorded.turns[0].request.body.tools[0],
        json!({"type": "tool_search", "execution": "server"})
    );
    let echoed: Vec<_> = responses[0]["tools"]
        .as_array()
        .expect("echoed tools")
        .iter()
        .filter(|tool| tool["type"] == "tool_search")
        .collect();
    assert_eq!(
        echoed,
        [&json!({"type": "tool_search", "description": null, "parameters": null})]
    );
    assert_hosted(items(&responses[0], "tool_search_call")[0]);

    let error = |index: usize, param: &str, message: &str| {
        let body = &responses[index];
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["param"], param);
        assert_eq!(body["error"]["message"], message);
    };
    error(
        1,
        "tools.tool_search",
        "Invalid Value: 'tools.tool_search'. tools.tool_search requires at least one deferred tool.",
    );
    error(
        2,
        "tools.defer_loading",
        "Invalid Value: 'tools.defer_loading'. Deferred tools require tools.tool_search.",
    );
    error(
        3,
        "tool_choice",
        "Tool choice 'get_weather' not found in 'tools' parameter.",
    );
    // tool-choice-mcp: OpenAI fails with a server error; the gateway returns a 400 instead.
    assert_eq!(responses[4]["error"]["type"], "server_error");

    // tool-choice-required: the model searches first.
    assert_eq!(
        item_types(&responses[5])[..3],
        ["mcp_list_tools", "tool_search_call", "tool_search_output"]
    );
}

#[test]
fn deferred_mcp_server_loads_as_a_namespace() {
    for mode in ["-nonstreaming", "-streaming"] {
        let recorded = cassette("contract", mode);
        let responses: Vec<_> = recorded.turns.iter().map(terminal).collect();
        assert_eq!(responses.len(), 11, "contract legs: see the recorder header");

        // mcp-deferred and mcp-no-description: tools are listed before the search.
        for (response, description) in [(&responses[0], SERVER_DESCRIPTION), (&responses[1], "")] {
            assert_eq!(
                item_types(response)[..4],
                ["mcp_list_tools", "tool_search_call", "tool_search_output", "mcp_call"]
            );
            let call = items(response, "tool_search_call")[0];
            assert_hosted(call);
            assert_eq!(call["arguments"], json!({"paths": [MCP_NAMESPACE]}));
            let output = items(response, "tool_search_output")[0];
            assert_hosted(output);
            // No fallback to the MCP server's own description.
            assert_eq!(
                loaded(output),
                [namespace(
                    MCP_NAMESPACE,
                    description,
                    &["search_tiktoken_documentation"]
                )]
            );
            let member = &output["tools"][0]["tools"][0];
            assert_eq!(member["type"], "function");
            assert_eq!(member["defer_loading"], true);
            assert_eq!(member["strict"], Value::Null);
            assert_eq!(member["output_schema"], Value::Null);
            assert_eq!(items(response, "mcp_call")[0]["server_label"], "gitmcp_tiktoken");
        }
    }
}

#[test]
fn query_filters_loaded_namespace_members() {
    for mode in ["-nonstreaming", "-streaming"] {
        let recorded = cassette("contract", mode);
        let responses: Vec<_> = recorded.turns.iter().map(terminal).collect();

        // mixed-catalog turn 1: the query keeps one member and drops the named function.
        let searches = items(&responses[2], "tool_search_call");
        let outputs = items(&responses[2], "tool_search_output");
        searches.iter().chain(&outputs).for_each(|item| assert_hosted(item));
        assert_eq!(
            searches[0]["arguments"],
            json!({"paths": ["get_weather", "travel"], "query": "get_timezone"})
        );
        assert_eq!(
            loaded(outputs[0]),
            [namespace("travel", "Travel location tools", &["get_timezone"])]
        );
        assert_eq!(searches[1]["arguments"], json!({"paths": ["get_weather"]}));
        assert_eq!(
            loaded(outputs[1]),
            [function("get_weather", "Get the current weather for a city")]
        );

        // mixed-parallel-off turn 1: without a query, a namespace path loads every member.
        let search = items(&responses[5], "tool_search_call")[0];
        let output = items(&responses[5], "tool_search_output")[0];
        assert_hosted(search);
        assert_hosted(output);
        assert_eq!(search["arguments"], json!({"paths": ["get_weather", "travel"]}));
        assert_eq!(
            loaded(output),
            [
                function("get_weather", "Get the current weather for a city"),
                namespace(
                    "travel",
                    "Travel location tools",
                    &["get_timezone", "get_coordinates", "calculate_distance"]
                ),
            ]
        );
    }
}

/// The `query` group: one search per leg over `tools-query.json`. `OpenAI` keeps a namespace member whose
/// name or description contains the whole query, ignoring case, and keeps or drops a requested function by
/// the same match (only name matches were recorded). It does not split words, stem, match parameters, or
/// search outside `paths`; an empty query keeps every member, and a namespace with no match is dropped.
/// Legs 1-13 returned the same results in two recordings; legs 14-21 were recorded once.
#[test]
fn query_is_a_case_insensitive_substring_filter_within_paths() {
    const TRAVEL: &str = "Travel location tools";
    let travel = |members: &[&str]| vec![namespace("travel", TRAVEL, members)];
    let none = Vec::new;
    let cases: Vec<(Value, Vec<Loaded>)> = vec![
        (
            json!({"paths": ["travel"], "query": "get_timezone"}),
            travel(&["get_timezone"]),
        ),
        (
            json!({"paths": ["travel"], "query": "timezone"}),
            travel(&["get_timezone"]),
        ),
        (
            json!({"paths": ["travel"], "query": "latitude"}),
            travel(&["get_coordinates"]),
        ),
        (json!({"paths": ["travel"], "query": "airplane ticket"}), none()),
        (json!({"paths": ["travel"], "query": "timezone distance"}), none()),
        (
            json!({"paths": ["travel"], "query": "GET_TIMEZONE"}),
            travel(&["get_timezone"]),
        ),
        (json!({"paths": ["travel"], "query": "chocolate cake recipe"}), none()),
        (
            json!({"paths": ["travel"], "query": ""}),
            travel(&["get_timezone", "get_coordinates", "calculate_distance", "book_flight"]),
        ),
        (
            json!({"paths": ["get_weather"], "query": "weather"}),
            vec![function("get_weather", "Get the current weather for a city")],
        ),
        (json!({"paths": ["get_weather"], "query": "timezone"}), none()),
        (json!({"paths": ["get_weather"], "query": "get_exchange_rate"}), none()),
        (Value::Null, none()),
        (
            json!({"paths": [MCP_NAMESPACE], "query": "code"}),
            vec![namespace(MCP_NAMESPACE, SERVER_DESCRIPTION, &["search_tiktoken_code"])],
        ),
        (
            json!({"paths": ["travel"], "query": "coord"}),
            travel(&["get_coordinates"]),
        ),
        // "cities" in calculate_distance does not match "city".
        (
            json!({"paths": ["travel"], "query": "city"}),
            travel(&["get_timezone", "get_coordinates"]),
        ),
        (json!({"paths": ["travel"], "query": "latitude city"}), none()),
        (json!({"paths": ["travel"], "query": "get timezone"}), none()),
        (
            json!({"paths": ["travel"], "query": "time zone"}),
            travel(&["get_timezone"]),
        ),
        (json!({"paths": ["travel"], "query": "destination_airport"}), none()),
        (json!({"paths": ["no_such_tool"]}), none()),
        (json!({"paths": [], "query": "timezone"}), none()),
    ];

    let recorded = cassette("query", "-nonstreaming");
    assert_eq!(recorded.turns.len(), cases.len(), "query legs: see the recorder header");
    for (turn, (arguments, expected)) in recorded.turns.iter().zip(cases) {
        let response = terminal(turn);
        assert_eq!(response["status"], "completed");
        let searches = items(&response, "tool_search_call");
        if arguments.is_null() {
            // query-without-paths: asked to omit `paths`, the model did not search at all. This pins the
            // model's choice, not whether the API requires `paths`.
            assert!(searches.is_empty());
            continue;
        }
        assert_eq!(searches.len(), 1);
        assert_eq!(searches[0]["arguments"], arguments);
        let output = items(&response, "tool_search_output")[0];
        assert_hosted(output);
        assert_eq!(loaded(output), expected, "loaded tools for {arguments}");
    }
}

#[test]
fn loaded_functions_report_their_namespace() {
    for mode in ["-nonstreaming", "-streaming"] {
        let recorded = cassette("contract", mode);
        let responses: Vec<_> = recorded.turns.iter().map(terminal).collect();
        let namespaces = |response: &Value| -> Vec<(String, String)> {
            items(response, "function_call")
                .iter()
                .map(|call| {
                    let field = |key: &str| call[key].as_str().unwrap_or_default().to_owned();
                    (field("namespace"), field("name"))
                })
                .collect()
        };
        // A top-level function reports its own name as its namespace.
        assert_eq!(
            namespaces(&responses[2]),
            [("get_weather".into(), "get_weather".into())]
        );
        assert_eq!(namespaces(&responses[3]), [("travel".into(), "get_timezone".into())]);
    }
}

#[test]
fn search_does_not_count_toward_max_tool_calls() {
    for mode in ["-nonstreaming", "-streaming"] {
        let recorded = cassette("contract", mode);
        let response = terminal(&recorded.turns[8]);
        assert_eq!(recorded.turns[8].request.body.extra["max_tool_calls"], 1);
        assert_eq!(recorded.turns[8].request.body.parallel_tool_calls, Some(false));
        assert_eq!(response["status"], "completed");
        assert_hosted(items(&response, "tool_search_call")[0]);
        assert_eq!(
            items(&response, "mcp_call").len(),
            1,
            "the search left the one call slot"
        );
    }
}

#[test]
fn loaded_tools_persist_without_a_new_search() {
    // contract continuation: turn 2 chains by previous_response_id.
    for mode in ["-nonstreaming", "-streaming"] {
        let recorded = cassette("contract", mode);
        assert!(recorded.turns[10].request.body.previous_response_id.is_some());
        assert_eq!(item_types(&terminal(&recorded.turns[10])), ["mcp_call", "message"]);
    }

    // replay: turn 2 resends the hosted transcript with store=false.
    let recorded = cassette("replay", "-nonstreaming");
    let replayed = &recorded.turns[1].request.body;
    assert!(!replayed.store);
    let replayed_types: Vec<_> = replayed.input.as_array().expect("replayed input").iter().collect();
    assert!(replayed_types.iter().any(|item| item["type"] == "tool_search_output"));
    assert!(replayed_types.iter().any(|item| item["type"] == "mcp_list_tools"));
    assert_eq!(item_types(&terminal(&recorded.turns[1])), ["mcp_call", "message"]);
}

#[test]
fn streaming_has_no_dedicated_tool_search_events() {
    let check = |turn: &support::Turn| {
        let events = support::recorded_named_sse_events(turn);
        let search_events: Vec<(&str, &str, &str)> = events
            .iter()
            .filter(|event| {
                event["item"]["type"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("tool_search"))
            })
            .map(|event| {
                (
                    event["type"].as_str().unwrap_or_default(),
                    event["item"]["type"].as_str().unwrap_or_default(),
                    event["item"]["status"].as_str().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            search_events,
            [
                ("response.output_item.added", "tool_search_call", "in_progress"),
                ("response.output_item.done", "tool_search_call", "completed"),
                ("response.output_item.added", "tool_search_output", "in_progress"),
                ("response.output_item.done", "tool_search_output", "completed"),
            ]
        );
        assert!(
            events
                .iter()
                .all(|event| !event["type"].as_str().unwrap_or_default().contains("tool_search")),
            "no dedicated tool_search event types"
        );
        // Arguments arrive whole on `done`; `added` carries an empty object.
        let added = events
            .iter()
            .find(|event| event["type"] == "response.output_item.added" && event["item"]["type"] == "tool_search_call")
            .expect("tool_search_call added");
        assert_eq!(added["item"]["arguments"], json!({}));
    };

    let recorded = cassette("contract", "-streaming");
    check(&recorded.turns[0]);
    let websocket = support::load_cassette(&format!("{CASSETTE_DIR}/hosted-tool-search-websocket-{OPENAI}.yaml"));
    check(&websocket.turns[0]);
    assert_eq!(
        item_types(&terminal(&websocket.turns[0]))[..4],
        ["mcp_list_tools", "tool_search_call", "tool_search_output", "mcp_call"]
    );
}

/// The `edges` group: legs are documented in the recorder header.
#[test]
fn edge_cases_match_openai() {
    const TRAVEL: &str = "Travel location tools";
    const WEATHER: &str = "Get the current weather for a city";
    let recorded = cassette("edges", "-nonstreaming");
    let responses: Vec<_> = recorded.turns.iter().map(terminal).collect();
    assert_eq!(responses.len(), 7, "edges legs: see the recorder header");
    let searched = |response: &Value| -> Vec<(Value, Vec<Loaded>)> {
        items(response, "tool_search_call")
            .into_iter()
            .zip(items(response, "tool_search_output"))
            .map(|(call, output)| (call["arguments"].clone(), loaded(output)))
            .collect()
    };

    // query-function-description: a top-level function also matches on its description.
    assert_eq!(
        searched(&responses[0]),
        [(
            json!({"paths": ["get_weather"], "query": "CURRENT"}),
            vec![function("get_weather", WEATHER)]
        )]
    );

    // repeat-same-response: within one response, a loaded tool is returned again.
    let all_travel = ["get_timezone", "get_coordinates", "calculate_distance", "book_flight"];
    assert_eq!(
        searched(&responses[1]),
        [
            (
                json!({"paths": ["get_weather"]}),
                vec![function("get_weather", WEATHER)]
            ),
            (
                json!({"paths": ["get_weather"]}),
                vec![function("get_weather", WEATHER)]
            ),
            (
                json!({"paths": ["travel"], "query": "timezone"}),
                vec![namespace("travel", TRAVEL, &["get_timezone"])]
            ),
            (
                json!({"paths": ["travel"]}),
                vec![namespace("travel", TRAVEL, &all_travel)]
            ),
        ]
    );

    // repeat-continuation: tools loaded by an earlier response are left out.
    assert!(recorded.turns[3].request.body.previous_response_id.is_some());
    assert!(items(&responses[3], "mcp_list_tools").is_empty());
    assert_eq!(
        searched(&responses[3]),
        [
            (json!({"paths": ["travel"], "query": "timezone"}), Vec::new()),
            (
                json!({"paths": ["travel"]}),
                vec![namespace(
                    "travel",
                    TRAVEL,
                    &["get_coordinates", "calculate_distance", "book_flight"]
                )]
            ),
        ]
    );

    // mcp-unreachable-unused, -used and -eager: a failed tool list fails the request,
    // whether or not the server is deferred or needed.
    for (turn, response) in recorded.turns[4..].iter().zip(&responses[4..]) {
        assert_eq!(turn.response.status_code, Some(424));
        assert_eq!(response["error"]["type"], "external_connector_error");
        assert_eq!(response["error"]["code"], "http_error");
        assert_eq!(response["error"]["param"], "tools");
        assert_eq!(
            response["error"]["message"],
            "Error retrieving tool list from MCP server: 'broken_docs'. Http status code: 424 (Failed Dependency)"
        );
    }
}

/// Every recorded gateway cassette, by `(group, mode)`.
const GATEWAY_RECORDINGS: [(&str, &str); 6] = [
    ("validation", "-nonstreaming"),
    ("contract", "-nonstreaming"),
    ("contract", "-streaming"),
    ("query", "-nonstreaming"),
    ("edges", "-nonstreaming"),
    ("websocket", ""),
];

/// `(arguments, loaded tools)` for each search in a response, in output order.
fn searches(response: &Value) -> Vec<(Value, Vec<Loaded>)> {
    // A failed response (the reference's 424 for an unreachable server) has no output.
    if !response["output"].is_array() {
        return Vec::new();
    }
    items(response, "tool_search_call")
        .into_iter()
        .zip(items(response, "tool_search_output"))
        .map(|(call, output)| (call["arguments"].clone(), loaded(output)))
        .collect()
}

#[test]
fn gateway_search_items_have_the_reference_shape_and_pairing() {
    let mut search_count = 0;
    for (group, mode) in GATEWAY_RECORDINGS {
        for turn in &provider_cassette(GATEWAY, group, mode).turns {
            let response = terminal(turn);
            let Some(output) = response["output"].as_array() else {
                continue;
            };
            for (index, item) in output.iter().enumerate() {
                if item["type"] != "tool_search_call" {
                    continue;
                }
                search_count += 1;
                assert_hosted(item);
                assert!(item["arguments"].is_object(), "{group}: {item}");
                let suffix = item["id"]
                    .as_str()
                    .and_then(|id| id.strip_prefix("tsc_"))
                    .expect("tsc_ id");
                // Its output follows at the next index and shares the id suffix.
                let result = &output[index + 1];
                assert_eq!(result["type"], "tool_search_output", "{group}");
                assert_hosted(result);
                assert_eq!(result["id"], format!("tso_{suffix}"), "{group}");
                for tool in result["tools"].as_array().expect("loaded tools") {
                    let is_mcp = tool["name"].as_str().is_some_and(|name| name.starts_with("mcp_"));
                    for member in tool["tools"].as_array().into_iter().flatten() {
                        assert_eq!(member["type"], "function");
                        assert_eq!(member["defer_loading"], true);
                        if is_mcp {
                            assert_eq!(member["output_schema"], Value::Null, "{member}");
                        }
                    }
                }
            }
        }
    }
    // How often the recorded model searches varies between recordings; the floor keeps the shape
    // checks above meaningful.
    assert!(
        search_count >= 50,
        "the gateway recordings searched {search_count} times"
    );
}

#[test]
fn gateway_streams_searches_with_only_added_and_done_events() {
    let streams: Vec<_> = provider_cassette(GATEWAY, "contract", "-streaming")
        .turns
        .into_iter()
        .chain(provider_cassette(GATEWAY, "websocket", "").turns)
        .collect();
    let mut checked = 0;
    for turn in &streams {
        let events = support::recorded_named_sse_events(turn);
        assert!(
            events
                .iter()
                .all(|event| !event["type"].as_str().unwrap_or_default().contains("tool_search")),
            "no dedicated tool_search event types"
        );
        let lifecycle: Vec<_> = events
            .iter()
            .filter(|event| {
                event["item"]["type"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("tool_search"))
            })
            .collect();
        for search in lifecycle.chunks(4) {
            let shape: Vec<_> = search
                .iter()
                .map(|event| {
                    (
                        event["type"].as_str(),
                        event["item"]["type"].as_str(),
                        event["item"]["status"].as_str(),
                    )
                })
                .collect();
            assert_eq!(
                shape,
                [
                    (
                        Some("response.output_item.added"),
                        Some("tool_search_call"),
                        Some("in_progress")
                    ),
                    (
                        Some("response.output_item.done"),
                        Some("tool_search_call"),
                        Some("completed")
                    ),
                    (
                        Some("response.output_item.added"),
                        Some("tool_search_output"),
                        Some("in_progress")
                    ),
                    (
                        Some("response.output_item.done"),
                        Some("tool_search_output"),
                        Some("completed")
                    ),
                ]
            );
            assert_eq!(search[0]["item"]["arguments"], json!({}));
            assert_eq!(
                search[2]["output_index"].as_u64(),
                search[0]["output_index"].as_u64().map(|index| index + 1)
            );
            assert!(search[2]["item"]["tools"].is_array(), "the output opens with its tools");
            checked += 1;
        }
        let sequence: Vec<_> = events
            .iter()
            .filter_map(|event| event["sequence_number"].as_u64())
            .collect();
        assert!(sequence.windows(2).all(|pair| pair[1] == pair[0] + 1), "{sequence:?}");
    }
    assert!(checked > 0);
}

#[test]
fn gateway_loads_the_same_tools_as_openai_for_the_same_arguments() {
    let mut compared = 0;
    for group in ["query", "edges"] {
        let openai = cassette(group, "-nonstreaming");
        let gateway = provider_cassette(GATEWAY, group, "-nonstreaming");
        assert_eq!(openai.turns.len(), gateway.turns.len(), "{group}: same legs");
        for (reference, recorded) in openai.turns.iter().zip(&gateway.turns) {
            let (reference, recorded) = (searches(&terminal(reference)), searches(&terminal(recorded)));
            for ((arguments, expected), (sent, actual)) in reference.iter().zip(&recorded) {
                if arguments == sent {
                    assert_eq!(actual, expected, "{group}: {arguments}");
                    compared += 1;
                }
            }
        }
    }
    // Qwen repeated OpenAI's exact arguments in 20 of 21 query legs and in the edge legs.
    assert!(compared >= 25, "compared {compared} identical searches");
}

#[test]
fn gateway_validation_matches_openai_except_the_intended_mcp_choice() {
    let openai = cassette("validation", "-nonstreaming");
    let gateway = provider_cassette(GATEWAY, "validation", "-nonstreaming");
    let statuses = |cassette: &support::Cassette| -> Vec<Option<u16>> {
        cassette.turns.iter().map(|turn| turn.response.status_code).collect()
    };
    assert_eq!(
        statuses(&openai),
        [Some(200), Some(400), Some(400), Some(400), Some(500), Some(200)]
    );
    // A deferred-server tool_choice is a clear 400 instead of the reference's 500.
    assert_eq!(
        statuses(&gateway),
        [Some(200), Some(400), Some(400), Some(400), Some(400), Some(200)]
    );
    let message = |index: usize| {
        terminal(&gateway.turns[index])["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    assert!(message(1).contains("tools.tool_search requires at least one deferred tool"));
    assert!(message(2).contains("defer_loading requires a tool_search declaration"));
    assert!(message(3).contains("before its definition is loaded"));
    // The gateway does not support an MCP tool_choice; it names the parameter instead of failing to parse.
    assert_eq!(
        terminal(&gateway.turns[4])["error"],
        json!({"code": "unsupported_value", "param": "tool_choice", "type": "invalid_request_error",
            "message": "MCP tool_choice is not supported; use `auto`, `required`, or a function `tool_choice` instead"})
    );
    // tool_choice=required with every tool deferred: the model must search first.
    let required = terminal(&gateway.turns[5]);
    assert_eq!(
        item_types(&required)[..3],
        ["mcp_list_tools", "tool_search_call", "tool_search_output"]
    );
}

#[test]
fn gateway_restores_loads_on_continuation() {
    let edges = provider_cassette(GATEWAY, "edges", "-nonstreaming");
    // repeat-continuation: loads from the earlier response are restored, not listed again.
    assert!(edges.turns[3].request.body.previous_response_id.is_some());
    assert!(items(&terminal(&edges.turns[3]), "mcp_list_tools").is_empty());
}

const LISTING_FAILED: &str =
    "Error retrieving tool list from MCP server: 'broken_docs'. Http status code: 424 (Failed Dependency)";

/// The `mcp-failures` group: unreachable, HTTP 500, not-MCP, and auth-required servers, deferred or not,
/// then a working server whose `allowed_tools` matches nothing.
#[test]
fn openai_fails_the_request_when_any_mcp_listing_fails() {
    let failing = 6;
    let json = cassette("mcp-failures", "-nonstreaming");
    assert_eq!(json.turns.len(), failing + 1);
    for turn in &json.turns[..failing] {
        assert_eq!(turn.response.status_code, Some(424));
        let error = &terminal(turn)["error"];
        assert_eq!(
            error,
            &json!({"code": "http_error", "message": LISTING_FAILED, "param": "tools", "type": "external_connector_error"})
        );
    }
    // An empty allowed set is not a failure: the listing is empty and the response completes.
    let empty = terminal(&json.turns[failing]);
    assert_eq!(empty["status"], "completed");
    assert_eq!(items(&empty, "mcp_list_tools")[0]["tools"], json!([]));

    let stream = cassette("mcp-failures", "-streaming");
    for turn in &stream.turns[..failing] {
        assert_eq!(turn.response.status_code, Some(200), "a stream starts, then fails");
        let events = support::recorded_named_sse_events(turn);
        let kinds: Vec<_> = events
            .iter()
            .map(|event| event["type"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            kinds,
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.mcp_list_tools.in_progress",
                "response.mcp_list_tools.failed",
                "error",
                "response.failed"
            ]
        );
        assert_eq!(events[5]["error"]["message"], LISTING_FAILED);
        assert_eq!(events[5]["error"]["param"], "tools");
        let failed = &events[6]["response"];
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["error"]["type"], "external_connector_error");
        assert_eq!(item_types(failed), ["mcp_list_tools"]);
        assert_eq!(failed["output"][0]["tools"], json!([]));
        assert!(failed["output"][0].get("error").is_none());
    }
    assert_eq!(terminal(&stream.turns[failing])["status"], "completed");
}

#[test]
fn gateway_handles_mcp_listing_failures_exactly_like_openai() {
    let failing = 6;
    for mode in ["-nonstreaming", "-streaming"] {
        let openai = cassette("mcp-failures", mode);
        let gateway = provider_cassette(GATEWAY, "mcp-failures", mode);
        for (reference, recorded) in openai.turns[..failing].iter().zip(&gateway.turns[..failing]) {
            assert_eq!(recorded.response.status_code, reference.response.status_code, "{mode}");
            if mode == "-nonstreaming" {
                assert_eq!(terminal(recorded)["error"], terminal(reference)["error"]);
                continue;
            }
            let (reference, recorded) = (
                support::recorded_named_sse_events(reference),
                support::recorded_named_sse_events(recorded),
            );
            let kinds = |events: &[Value]| -> Vec<String> {
                events
                    .iter()
                    .map(|event| event["type"].as_str().unwrap_or_default().to_owned())
                    .collect()
            };
            assert_eq!(kinds(&recorded), kinds(&reference));
            assert_eq!(recorded[5]["error"]["message"], reference[5]["error"]["message"]);
            assert_eq!(recorded[5]["error"]["param"], reference[5]["error"]["param"]);
            assert_eq!(recorded[6]["response"]["error"], reference[6]["response"]["error"]);
            assert_eq!(
                item_types(&recorded[6]["response"]),
                item_types(&reference[6]["response"])
            );
        }
    }
    // The edge legs with an unreachable server fail the same way.
    let edges = provider_cassette(GATEWAY, "edges", "-nonstreaming");
    for turn in &edges.turns[4..] {
        assert_eq!(turn.response.status_code, Some(424));
        assert_eq!(terminal(turn)["error"]["message"], LISTING_FAILED);
    }
    // An allowed_tools filter that matches nothing completes with an empty listing, as in the reference.
    for mode in ["-nonstreaming", "-streaming"] {
        let gateway = provider_cassette(GATEWAY, "mcp-failures", mode);
        let empty = terminal(&gateway.turns[failing]);
        assert_eq!(empty["status"], "completed", "{mode}");
        assert_eq!(items(&empty, "mcp_list_tools")[0]["tools"], json!([]), "{mode}");
    }
    let listing_events = |turn: &support::Turn| -> Vec<String> {
        support::recorded_named_sse_events(turn)
            .iter()
            .take(6)
            .map(|event| event["type"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    let (openai, gateway) = (
        cassette("mcp-failures", "-streaming"),
        provider_cassette(GATEWAY, "mcp-failures", "-streaming"),
    );
    assert_eq!(
        listing_events(&gateway.turns[failing]),
        listing_events(&openai.turns[failing])
    );
}
