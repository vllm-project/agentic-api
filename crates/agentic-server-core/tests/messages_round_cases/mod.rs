//! Synthetic rounds shared by the JSON and SSE behavior checks, not recordings.

use std::fmt::Write as _;

use serde_json::{Value, json};

pub struct Case {
    pub label: &'static str,
    pub content: Vec<Value>,
    pub stop: Option<&'static str>,
    pub choice: Option<Value>,
    pub searches: usize,
    pub visible_stop: Option<&'static str>,
    pub client: bool,
}

pub fn cases() -> Vec<Case> {
    let named = || Some(json!({"type":"tool", "name":"web_search", "disable_parallel_tool_use":true}));
    let rows = [
        ("text only", false, false, Some("end_turn"), None, 0, Some("end_turn")),
        ("search", true, false, Some("tool_use"), None, 1, Some("end_turn")),
        (
            "automatic end_turn",
            true,
            false,
            Some("end_turn"),
            None,
            0,
            Some("end_turn"),
        ),
        (
            "named search",
            true,
            false,
            Some("end_turn"),
            named(),
            1,
            Some("end_turn"),
        ),
        (
            "unmatched named tool",
            true,
            false,
            Some("end_turn"),
            Some(json!({"type":"tool", "name":"get_weather"})),
            0,
            Some("end_turn"),
        ),
        (
            "any is not named",
            true,
            false,
            Some("end_turn"),
            Some(json!({"type":"any"})),
            0,
            Some("end_turn"),
        ),
        ("client", false, true, Some("tool_use"), None, 0, Some("tool_use")),
        (
            "client end_turn",
            false,
            true,
            Some("end_turn"),
            None,
            0,
            Some("tool_use"),
        ),
        ("mixed", true, true, Some("tool_use"), None, 0, Some("tool_use")),
        (
            "mixed named",
            true,
            true,
            Some("end_turn"),
            named(),
            0,
            Some("tool_use"),
        ),
        (
            "truncated search",
            true,
            false,
            Some("max_tokens"),
            named(),
            0,
            Some("max_tokens"),
        ),
        (
            "truncated mixed",
            true,
            true,
            Some("max_tokens"),
            None,
            0,
            Some("max_tokens"),
        ),
        (
            "unknown stop",
            true,
            false,
            Some("future_stop"),
            named(),
            0,
            Some("future_stop"),
        ),
        ("missing stop", true, false, None, named(), 0, None),
    ];
    rows.into_iter().map(Case::from_spec).collect()
}

type Scenario = (
    &'static str,
    bool,
    bool,
    Option<&'static str>,
    Option<Value>,
    usize,
    Option<&'static str>,
);

impl Case {
    fn from_spec((label, gateway, client, stop, choice, searches, visible_stop): Scenario) -> Self {
        let mut content = vec![
            json!({"type":"thinking", "thinking":"keep reasoning", "signature":"sig"}),
            json!({"type":"text", "text":"preface"}),
            json!({"type":"provider_extension", "opaque":{"keep":[1,true,null]}}),
        ];
        if gateway {
            content.push(json!({"type":"tool_use", "id":"g1", "name":"web_search",
        "input":{"query":"rust \"你好\"\nrelease"}, "provider_extra":true}));
        }
        if client {
            content.push(json!({"type":"tool_use", "id":"c1", "name":"get_weather", "input":{"city":"Paris"}}));
        }
        Case {
            label,
            content,
            stop,
            choice,
            searches,
            visible_stop,
            client,
        }
    }

    pub fn request(&self, stream: bool) -> Value {
        let mut request = json!({"model":"test", "max_tokens":128, "stream":stream,
            "messages":[{"role":"user", "content":"Search Rust."}],
            "tools":[{"name":"web_search", "input_schema":{"type":"object"}},
                     {"name":"get_weather", "input_schema":{"type":"object"}}]});
        if let Some(choice) = &self.choice {
            request["tool_choice"] = choice.clone();
        }
        request
    }

    pub fn message(&self) -> Value {
        json!({"id":"m", "type":"message", "role":"assistant", "model":"test",
            "content":self.content, "stop_reason":self.stop, "usage":{"input_tokens":5,"output_tokens":3}})
    }
}

pub fn final_message() -> Value {
    json!({"id":"final", "type":"message", "role":"assistant", "model":"test",
        "content":[{"type":"text","text":"done"}], "stop_reason":"end_turn",
        "usage":{"input_tokens":7,"output_tokens":2}})
}

// Only the SSE integration target uses this half of the shared scenarios.
#[allow(dead_code)]
pub fn stream(message: &Value) -> String {
    let mut events = vec![json!({"type":"message_start", "message":{
        "id":message["id"], "type":"message", "role":"assistant", "model":"test", "content":[],
        "usage":{"input_tokens":message["usage"]["input_tokens"], "output_tokens":0}}})];
    for (index, block) in message["content"].as_array().unwrap().iter().enumerate() {
        let mut start = block.clone();
        if block["type"] == "tool_use" {
            start["input"] = json!({});
        }
        events.push(json!({"type":"content_block_start", "index":index, "content_block":start}));
        if block["type"] == "tool_use" {
            let input: Vec<char> = block["input"].to_string().chars().collect();
            for fragment in input.chunks(3) {
                events.push(json!({"type":"content_block_delta", "index":index,
                    "delta":{"type":"input_json_delta", "partial_json":fragment.iter().collect::<String>()}}));
            }
        }
        events.push(json!({"type":"content_block_stop", "index":index}));
    }
    events.push(
        json!({"type":"message_delta", "delta":{"stop_reason":message["stop_reason"]},
        "usage":{"output_tokens":message["usage"]["output_tokens"]}}),
    );
    events.push(json!({"type":"message_stop"}));
    let mut wire = String::new();
    for event in events {
        write!(wire, "event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()).unwrap();
    }
    wire
}
