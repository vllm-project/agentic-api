//! Assertions over the unchanged recordings, independent of live replay.

use std::collections::HashSet;

use super::{cassette, error_metadata, item_signature, terminal};
use serde_json::{Value, json};

const SCENARIOS: [&str; 5] = [
    "lifecycle",
    "pagination",
    "edge-cases",
    "conversation",
    "replayed-items",
];

fn items(turns: &[Value], turn: usize) -> Vec<Value> {
    terminal(&turns[turn - 1]["response"])["data"]
        .as_array()
        .unwrap()
        .clone()
}

fn comparable_items(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            let mut value = item_signature(item);
            value["id"] = item["id"].clone();
            value
        })
        .collect()
}

fn assert_pages(turns: &[Value], full_turn: usize, page_turns: &[usize], label: &str) {
    let full = items(turns, full_turn);
    for &turn in page_turns {
        let query = &turns[turn - 1]["request"]["query_params"];
        let body = terminal(&turns[turn - 1]["response"]);
        let mut ordered = full.clone();
        if query["order"].as_str().unwrap_or("desc") == "desc" {
            ordered.reverse();
        }
        let start = query["after"].as_str().filter(|id| !id.is_empty()).map_or(0, |id| {
            ordered
                .iter()
                .rposition(|item| item["id"] == id)
                .expect("cursor in full history")
                + 1
        });
        let limit = query["limit"]
            .as_str()
            .map_or(20, |value| value.parse::<usize>().unwrap());
        let expected: Vec<_> = ordered.iter().skip(start).take(limit).cloned().collect();
        assert_eq!(
            comparable_items(&items(turns, turn)),
            comparable_items(&expected),
            "{label} t{turn}: page content/order"
        );
        assert_eq!(
            body["has_more"],
            ordered.len() - start > limit,
            "{label} t{turn}: has_more"
        );
    }
}

fn assert_recording(scenario: &str, turns: &[Value], label: &str) {
    for (index, turn) in turns.iter().enumerate() {
        let body = terminal(&turn["response"]);
        if let Some(data) = body["data"].as_array() {
            for (field, expected) in [("first_id", data.first()), ("last_id", data.last())] {
                assert_eq!(
                    body[field],
                    expected.map_or(Value::Null, |item| item["id"].clone()),
                    "{label} t{}: {field}",
                    index + 1
                );
            }
        }
    }
    match scenario {
        "pagination" => assert_pages(turns, 3, &[4, 5, 6, 7, 8, 9, 10, 11, 12, 13], label),
        "lifecycle" => {
            assert_pages(turns, 4, &[5, 6], label);
            let before = items(turns, 4);
            assert_eq!(
                comparable_items(&items(turns, 11)),
                comparable_items(&before[before.len() - 1..]),
                "{label}: descendant input after deletion"
            );
            assert_eq!(
                terminal(&turns[6]["response"]),
                terminal(&turns[11]["response"]),
                "{label}: stored response changed after deleting parent"
            );
            let child = terminal(&turns[6]["response"]);
            let mut retained = items(turns, 11);
            retained.extend(child["output"].as_array().unwrap().iter().cloned());
            let continued = items(turns, 15);
            assert_eq!(continued.len(), retained.len() + 1, "{label}: continuation length");
            assert_eq!(
                comparable_items(&continued[..retained.len()]),
                comparable_items(&retained),
                "{label}: continued history must contain only the surviving child"
            );
            assert_eq!(
                continued.last().unwrap()["content"][0]["text"],
                turns[13]["request"]["body"]["input"][0]["content"],
                "{label}: continuation must retain the new user message"
            );
        }
        "conversation" => {
            for turn in [6, 12] {
                assert_eq!(
                    comparable_items(&items(turns, 4)),
                    comparable_items(&items(turns, turn)),
                    "{label}: first response input snapshot changed"
                );
            }
            for turn in [11, 14] {
                assert_eq!(
                    comparable_items(&items(turns, 8)),
                    comparable_items(&items(turns, turn)),
                    "{label}: child input snapshot changed"
                );
            }
            assert_pages(turns, 8, &[18], label);
            let deleted_id = items(turns, 5)[0]["id"].clone();
            let expected: Vec<_> = items(turns, 9)
                .into_iter()
                .filter(|item| item["id"] != deleted_id)
                .collect();
            assert_eq!(
                comparable_items(&items(turns, 15)),
                comparable_items(&expected),
                "{label}: conversation deletion removed the wrong items"
            );
        }
        "replayed-items" => {
            for turn in [4, 5, 7, 8, 10, 11, 14] {
                assert_eq!(
                    comparable_items(&items(turns, 2)),
                    comparable_items(&items(turns, turn)),
                    "{label}: reused ID or rejected request changed seed content"
                );
            }
        }
        "edge-cases" => {
            assert!(items(turns, 10).is_empty(), "{label}: limit=0 returns no items");
            assert_eq!(terminal(&turns[9]["response"])["has_more"], true);
        }
        _ => unreachable!(),
    }
}

#[test]
fn recorded_histories_and_pages_are_consistent() {
    for scenario in SCENARIOS {
        for suffix in ["", "-stream"] {
            for provider in ["openai", "gateway"] {
                let name = format!("{scenario}{suffix}");
                let turns = cassette(&name, provider).expect("required recording");
                assert_recording(scenario, &turns, &format!("{name}/{provider}"));
            }
        }
    }
}

fn generated_ids(turns: &[Value]) -> HashSet<String> {
    turns
        .iter()
        .filter(|turn| turn["request"]["method"] == "POST" && turn["request"]["path"] == "/v1/responses")
        .flat_map(|turn| {
            terminal(&turn["response"])["output"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|item| item["id"].as_str().map(str::to_owned))
        .collect()
}

#[test]
fn recorded_management_contract_matches_openai() {
    let mut differences = Vec::new();
    for scenario in SCENARIOS {
        for suffix in ["", "-stream"] {
            let name = format!("{scenario}{suffix}");
            let a = cassette(&name, "openai").expect("OpenAI recording");
            let b = cassette(&name, "gateway").expect("gateway recording");
            assert_eq!(a.len(), b.len(), "{name}: turn count");
            let a_generated = generated_ids(&a);
            let b_generated = generated_ids(&b);
            for (index, (left, right)) in a.iter().zip(&b).enumerate() {
                let location = format!("{name} t{}", index + 1);
                assert_eq!(
                    left["request"]["method"], right["request"]["method"],
                    "{location}: method"
                );
                assert_eq!(
                    left["response"]["status_code"], right["response"]["status_code"],
                    "{location}: HTTP status"
                );
                if left["response"]["status_code"] != 200 {
                    continue;
                }
                let l = terminal(&left["response"]);
                let r = terminal(&right["response"]);
                for field in ["object", "status", "deleted", "has_more"] {
                    assert_eq!(l[field], r[field], "{location}: {field}");
                }
                // Stateless creation is passed through on this branch; its upstream payload
                // may omit `store`. Storage behavior is checked by the subsequent CRUD calls.
                if left["request"]["body"]["store"] != false {
                    assert_eq!(l["store"], r["store"], "{location}: store");
                }
                // Different models can emit different numbers of reasoning/output items.
                // Each recording's complete pages (including those items) are checked above;
                // client-supplied items must still match across providers exactly.
                let client_items = |body: &Value, generated: &HashSet<String>| -> Vec<Value> {
                    body["data"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|item| !item["id"].as_str().is_some_and(|id| generated.contains(id)))
                        .map(item_signature)
                        .collect()
                };
                let lc = client_items(&l, &a_generated);
                let rc = client_items(&r, &b_generated);
                if lc != rc {
                    differences.push(format!("{location}: client items\nOpenAI: {lc:?}\ngateway: {rc:?}"));
                }
            }
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}

#[test]
fn recorded_error_metadata_matches_openai() {
    let mut differences = Vec::new();
    for scenario in SCENARIOS {
        for suffix in ["", "-stream"] {
            let name = format!("{scenario}{suffix}");
            let a = cassette(&name, "openai").expect("OpenAI recording");
            let b = cassette(&name, "gateway").expect("gateway recording");
            assert_eq!(a.len(), b.len());
            for (index, (left, right)) in a.iter().zip(&b).enumerate() {
                if left["response"]["status_code"].as_u64().unwrap() < 400 {
                    continue;
                }
                let expected = error_metadata(&terminal(&left["response"]));
                let actual = error_metadata(&terminal(&right["response"]));
                if expected != actual {
                    differences.push(format!("{name} t{}: OpenAI {expected}; gateway {actual}", index + 1));
                }
            }
        }
    }
    assert!(
        differences.is_empty(),
        "recordings need to be refreshed after fixing these errors:\n{}",
        differences.join("\n")
    );
}

#[test]
fn deferred_tool_status_defaults_do_not_hide_other_statuses() {
    for (kind, default) in [("function_call", "in_progress"), ("function_call_output", "completed")] {
        let absent = json!({"type": kind, "call_id": "call_1"});
        let mut recorded = absent.clone();
        recorded["status"] = json!(default);
        assert_eq!(item_signature(&absent), item_signature(&recorded));
        recorded["status"] = json!("incomplete");
        assert_ne!(item_signature(&absent), item_signature(&recorded));
    }
}

#[test]
fn error_metadata_comparison_detects_each_field() {
    let body = json!({"error":{"type":"invalid_request_error","code":"invalid_value","param":"limit"}});
    for field in ["type", "code", "param"] {
        let mut changed = body.clone();
        changed["error"][field] = Value::Null;
        assert_ne!(error_metadata(&body), error_metadata(&changed));
    }
}

#[test]
fn recorded_tool_execution_ids_and_statuses() {
    for provider in ["openai", "gateway"] {
        assert_tool_execution_ids_and_statuses(provider);
    }
}

fn assert_tool_execution_ids_and_statuses(provider: &str) {
    let turns = cassette("tool-items", provider).expect("tool-execution recording");
    assert_eq!(
        turns.iter().filter(|turn| turn["request"]["method"] == "POST").count(),
        24
    );
    let expected = [
        ("web_search_call", "ws_"),
        ("mcp_list_tools", "mcpl_"),
        ("mcp_call", "mcp_"),
        ("function_call", "fc_"),
        ("function_call_output", "fc_"),
        ("code_interpreter_call", "ci_"),
        ("shell_call", "sh_"),
        ("shell_call_output", "sho_"),
        ("custom_tool_call", "ctc_"),
        ("custom_tool_call_output", "ctco_"),
        ("tool_search_call", "tsc_"),
        ("tool_search_output", "tso_"),
        ("multi_agent_call", "mac_"),
        ("multi_agent_call_output", "maco_"),
        ("agent_message", "amsg_"),
    ];
    let mut seen = HashSet::new();
    let mut requests = turns.iter();
    while let Some(post) = requests.next() {
        let request = &post["request"]["body"];
        let created = terminal(&post["response"]);
        assert_eq!(post["request"]["method"], "POST");
        if request.get("previous_response_id").is_none() && request["multi_agent"]["enabled"] != true {
            assert!(!request["tools"].as_array().unwrap().is_empty());
        }
        assert_eq!(post["response"]["status_code"], 200, "{created}");
        assert_eq!(created["status"], "completed");
        let mut data = Vec::new();
        let mut after = Value::Null;
        loop {
            let listing = requests.next().expect("input-items page");
            let page = terminal(&listing["response"]);
            assert_eq!(listing["response"]["status_code"], 200, "{page}");
            assert_eq!(listing["request"]["method"], "GET");
            assert_eq!(listing["request"]["query_params"]["after"], after);
            assert_eq!(
                listing["request"]["path"],
                format!("/v1/responses/{}/input_items", created["id"].as_str().unwrap())
            );
            data.extend(page["data"].as_array().unwrap().iter().cloned());
            if page["has_more"] == false {
                break;
            }
            assert_ne!(page["last_id"], after);
            after = page["last_id"].clone();
            assert!(after.is_string());
        }
        for item in &data {
            let Some(&(kind, prefix)) = expected.iter().find(|(kind, _)| item["type"] == *kind) else {
                continue;
            };
            seen.insert(kind);
            assert!(item["id"].as_str().unwrap().starts_with(prefix), "{item}");
            if matches!(
                kind,
                "mcp_list_tools" | "multi_agent_call" | "multi_agent_call_output" | "agent_message"
            ) {
                assert!(item.get("status").is_none());
            } else {
                assert_eq!(item["status"], "completed", "{item}");
            }
        }
        if let Some(previous) = request["previous_response_id"].as_str() {
            let parent = turns
                .iter()
                .find(|turn| turn["response"]["body"]["id"] == previous)
                .unwrap();
            for output in parent["response"]["body"]["output"].as_array().unwrap() {
                if expected.iter().any(|(kind, _)| output["type"] == *kind) {
                    let listed = data.iter().find(|item| item["id"] == output["id"]).unwrap();
                    assert_eq!(listed["type"], output["type"]);
                    assert_eq!(listed["status"], output["status"]);
                    assert_eq!(listed["call_id"], output["call_id"]);
                }
            }
        }
    }
    assert_eq!(seen, expected.iter().map(|(kind, _)| *kind).collect());
}
