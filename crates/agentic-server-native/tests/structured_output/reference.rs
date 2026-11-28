//! Use the existing `OpenAI` WebSocket reference for the answer format, markers and tool continuation.
use serde_json::{Value, json};

use super::{array, output, text, value_str};

const OPENAI: &str = include_str!(
    "../../../agentic-server-core/tests/cassettes/multi_turn_steering/multi-turn-steering-openai-reference-edge-tools-settings-gpt-6-sol-websocket.yaml"
);

fn frames<'a>(sessions: &'a [Value], case: &str, direction: &'a str) -> impl Iterator<Item = Value> + 'a {
    let session = sessions
        .iter()
        .find(|s| s["handshake"]["probe"]["case"] == case)
        .unwrap();
    array(&session["frames"])
        .iter()
        .filter(move |f| f["direction"] == direction)
        .filter_map(|f| f["text"].as_str())
        .map(|s| serde_json::from_str(s).unwrap())
}

fn completed(sessions: &[Value], case: &str) -> Vec<Value> {
    frames(sessions, case, "server")
        .filter(|e| e["type"] == "response.completed")
        .map(|e| e["response"].clone())
        .collect()
}

fn format(response: &Value) -> Value {
    let mut format = response["text"]["format"].clone();
    // OpenAI serializes an unspecified description as null.
    if format["description"].is_null() {
        format.as_object_mut().unwrap().remove("description");
    }
    format
}

fn answer(response: &Value) -> Value {
    let answer: Value = serde_json::from_str(text(output(response))).unwrap();
    let keys: Vec<_> = answer.as_object().unwrap().keys().collect();
    let format = format(response);
    assert_eq!(
        keys,
        format["schema"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>()
    );
    answer
}

pub fn assert_contract(turns: &[Value], responses: &[Value]) {
    let [
        parent,
        stored,
        changed,
        cleared,
        called,
        explicit,
        default,
        structured,
        inputs,
    ] = responses
    else {
        panic!("expected both continuation chains and storage reads");
    };
    let reference: Value = serde_yaml::from_str(OPENAI).unwrap();
    let sessions = array(&reference["sessions"]);
    let automatic = completed(sessions, "settings-automatic");
    let [reference_answer] = automatic.as_slice() else {
        panic!("missing reference answer")
    };
    assert_eq!(format(parent), format(reference_answer));
    assert_eq!(answer(parent)["markers"], answer(reference_answer)["markers"]);
    assert_eq!(answer(parent)["answer"], "Ready");
    assert_eq!(stored["id"], parent["id"]);
    assert_eq!(stored["output"], parent["output"]);
    assert_eq!(format(stored), format(parent));
    assert_eq!(changed["previous_response_id"], parent["id"]);
    assert_eq!(format(changed)["name"], "phase");
    assert_eq!(answer(changed), json!({"phase":"DESIGN_COMPLETE"}));
    assert_eq!(cleared["previous_response_id"], changed["id"]);
    assert_eq!(format(cleared), json!({"type":"text"}));
    assert_eq!(text(output(cleared)), "DESIGN_COMPLETE");

    let items = array(&inputs["data"]);
    let [user, call, result] = items else {
        panic!("expected stored user, call, and result: {items:?}")
    };
    assert_eq!(user["role"], "user");
    assert_eq!(call, output(called));
    assert_eq!(result["type"], "function_call_output");
    assert_eq!(result["call_id"], call["call_id"]);
    for (case, request, child) in [
        ("settings-explicit", &turns[5]["request"]["body"], explicit),
        ("settings-explicit-defaults", &turns[6]["request"]["body"], default),
    ] {
        let turns = completed(sessions, case);
        let [ref_call, ref_child] = turns.as_slice() else {
            panic!("missing reference tool turns in {case}")
        };
        assert_eq!(format(called), format(ref_call));
        let ref_call = output(ref_call);
        for key in ["type", "name", "arguments"] {
            assert_eq!(call[key], ref_call[key]);
        }
        let ref_request = frames(sessions, case, "client")
            .find(|f| f["type"] == "response.create" && f["input"].is_array())
            .unwrap();
        assert_eq!(ref_request["input"][0]["call_id"], ref_call["call_id"]);
        assert_eq!(result["output"], ref_request["input"][0]["output"]);
        for setting in ["text", "tool_choice", "tools"] {
            assert_eq!(request.get(setting), ref_request.get(setting));
        }
        assert_eq!(child["previous_response_id"], called["id"]);
        assert_eq!(format(child), format(ref_child));
        let ref_message = array(&ref_child["output"])
            .iter()
            .find(|i| i["type"] == "message")
            .unwrap();
        let actual = text(output(child));
        let status = value_str(&result["output"]);
        assert!(actual.contains(status), "{case}: {actual}");
        let reference_text = text(ref_message).to_lowercase();
        for sentence in status.trim_end_matches('.').split(". ") {
            assert!(reference_text.contains(&sentence.to_lowercase()), "{case}: {sentence}");
        }
    }
    assert_eq!(structured["previous_response_id"], called["id"]);
    assert_eq!(format(structured), format(parent));
    assert_eq!(answer(structured)["answer"], result["output"]);
    assert_eq!(
        answer(structured)["markers"],
        json!(["Design is complete.", "Development has not started."])
    );
}
