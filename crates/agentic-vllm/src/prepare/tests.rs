use agentic_core::types::request_response::RequestPayload;
use serde_json::json;
use std::collections::HashMap;
use vllm_chat::{AssistantContentBlock, ChatMessage};

use super::*;

#[test]
fn request_projection_preserves_reasoning_tools_and_sampling() {
    let request: RequestPayload = serde_json::from_value(json!({
        "model":"test", "instructions":"be helpful", "temperature":0.0, "top_p":0.9,
        "max_output_tokens":42, "parallel_tool_calls":true, "cache_salt":"tenant",
        "reasoning":{"effort":"high"},
        "tools":[{"type":"custom","name":"echo"}], "tool_choice":{"type":"custom","name":"echo"},
        "input":[
            {"role":"user","content":"hi"},
            {"type":"reasoning","id":"rs_1","content":[{"type":"reasoning_text","text":"think"}],"summary":[]},
            {"type":"custom_tool_call","id":"ct_1","call_id":"call_1","name":"echo","input":"hi"},
            {"type":"custom_tool_call_output","call_id":"call_1","output":"hi"}
        ],
    }))
    .unwrap();
    let chat = chat_request(
        request.to_upstream_request(true, &HashMap::default()).unwrap(),
        "attempt_1",
        "test",
        StructuredOutputBackend::Xgrammar,
    )
    .unwrap();
    assert_eq!(chat.request_id, "attempt_1");
    assert_eq!(chat.messages.len(), 4);
    assert!(matches!(&chat.messages[0], ChatMessage::System { .. }));
    let ChatMessage::Assistant { content } = &chat.messages[2] else {
        panic!("assistant history")
    };
    assert!(matches!(&content[0], AssistantContentBlock::Reasoning { text } if text == "think"));
    let AssistantContentBlock::ToolCall(call) = &content[1] else {
        panic!("tool call")
    };
    assert_eq!(call.id, "call_1");
    assert_eq!(call.arguments, r#"{"input":"hi"}"#);
    assert!(matches!(&chat.messages[3], ChatMessage::ToolResponse { tool_call_id, .. } if tool_call_id == "call_1"));
    assert_eq!(chat.tool_context.effective_tools[0].name, "echo");
    assert!(matches!(&chat.tool_context.tool_choice, ChatToolChoice::Function { name } if name == "echo"));
    assert_eq!(chat.sampling_params.temperature, Some(0.0));
    assert_eq!(chat.sampling_params.max_tokens, Some(42));
    assert!(chat.sampling_params.structured_outputs.is_none());
    assert_eq!(chat.cache_salt.as_deref(), Some("tenant"));
}

fn prepare_request(fields: Value) -> Result<ChatRequest> {
    let Value::Object(mut fields) = fields else {
        panic!("expected request fields")
    };
    fields.entry("model").or_insert_with(|| json!("test"));
    fields.entry("input").or_insert_with(|| json!("Answer in JSON."));
    let request: RequestPayload = serde_json::from_value(Value::Object(fields)).unwrap();
    chat_request(
        request.to_upstream_request(true, &HashMap::default()).unwrap(),
        "attempt",
        "test",
        StructuredOutputBackend::Xgrammar,
    )
}

#[test]
fn json_object_selects_constrained_decoding_and_plain_text_does_not() {
    let plain = prepare_request(json!({"text":{"format":{"type":"text"}}})).unwrap();
    assert!(plain.sampling_params.structured_outputs.is_none());
    let object = prepare_request(json!({"text":{"format":{"type":"json_object"}}})).unwrap();
    assert_eq!(
        object.sampling_params.structured_outputs.unwrap().constraint,
        StructuredOutputConstraint::JsonObject
    );
}

#[test]
fn json_schema_preserves_property_order_with_each_strict_setting() {
    let schema = json!({
        "type": "object",
        "properties": {"zeta": {"type": "string"}, "alpha": {"type": "integer"}},
        "required": ["zeta", "alpha"],
        "additionalProperties": false
    });
    for strict in [None, Some(false), Some(true)] {
        let mut format = json!({"type":"json_schema","name":"answer","description":"Answer fields","schema":schema});
        if let Some(strict) = strict {
            format["strict"] = json!(strict);
        }
        let chat = prepare_request(json!({"text":{"format":format}})).unwrap();
        let StructuredOutputConstraint::Json(actual) = chat.sampling_params.structured_outputs.unwrap().constraint
        else {
            panic!("expected the schema constraint");
        };
        assert_eq!(actual, schema);
        assert_eq!(
            actual["properties"].as_object().unwrap().keys().collect::<Vec<_>>(),
            ["zeta", "alpha"]
        );
    }
}

#[test]
fn a_stored_json_answer_replays_as_assistant_text_without_constraining_the_next_turn() {
    let previous = r#"{"color":"green"}"#;
    let chat = prepare_request(json!({
        "input":[
            {"role":"assistant","content":[{"type":"output_text","text":previous,"annotations":[],"logprobs":[]}]},
            {"role":"user","content":"What color was it?"}
        ]
    }))
    .unwrap();
    assert!(chat.sampling_params.structured_outputs.is_none());
    let ChatMessage::Assistant { content } = &chat.messages[0] else {
        panic!("expected the prior assistant message");
    };
    assert!(matches!(content.as_slice(), [AssistantContentBlock::Text { text }] if text == previous));
}

#[test]
fn schema_validation_errors_refer_to_the_public_format() {
    let mut nested = json!({"type":"string"});
    for _ in 0..65 {
        nested = json!({"type":"array","items":nested});
    }
    for (schema, message) in [
        (nested, "nested too deeply"),
        (json!({"type":"unknown_kind"}), "unknown_kind"),
        (json!({"type":"integer","multipleOf":2}), "not supported by xgrammar"),
        (json!({"$schema":"https://example.test/unknown-draft"}), "unknown-draft"),
    ] {
        let error = prepare_request(json!({"text":{"format":{"type":"json_schema","name":"answer","schema":schema}}}))
            .unwrap_err();
        assert_eq!(error.http_status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(&error.into_response_body()).unwrap();
        assert_eq!(body["error"]["param"], "text.format");
        assert!(body["error"]["message"].as_str().unwrap().contains(message), "{body}");
    }
}

#[test]
fn json_object_uses_the_configured_decoder_capabilities() {
    let config: ResponseTextConfig = serde_json::from_value(json!({"format":{"type":"json_object"}})).unwrap();
    let error = structured_outputs(Some(&config), StructuredOutputBackend::Outlines).unwrap_err();
    assert_eq!(error.http_status(), StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(&error.into_response_body()).unwrap();
    assert_eq!(body["error"]["param"], "text.format");
    assert!(body["error"]["message"].as_str().unwrap().contains("Outlines"));
}

#[test]
fn unsupported_requests_fail_before_engine_submission() {
    for fields in [
        json!({"top_logprobs":1}),
        json!({"service_tier":"priority"}),
        json!({"prompt_cache_options":{}}),
        json!({"unsupported_option":true}),
        json!({"include":["message.output_text.logprobs"]}),
        json!({"text":{"verbosity":"high","format":{"type":"json_object"}}}),
        json!({"text":{"future_option":true,"format":{"type":"json_object"}}}),
        json!({"text":{"format":{"type":"text","future_option":true}}}),
        json!({"text":{"format":{"type":"json_object","future_option":true}}}),
        json!({"text":{"format":{"type":"json_schema","name":"answer","schema":{},"future_option":true}}}),
        json!({"reasoning":{"summary":"auto"}}),
        json!({"input":[{"role":"user","content":[{"type":"input_image","image_url":"https://example.test/a.png"}]}]}),
        json!({"input":[{"role":"assistant","content":[{"type":"output_text","text":"hi","unknown":[]}]}]}),
        json!({"input":[{"role":"assistant","content":[{"type":"output_text","text":"hi","annotations":"bad"}]}]}),
        json!({"input":[{"role":"assistant","content":[{"type":"output_text","text":"hi","annotations":[{"type":"url_citation"}]}]}]}),
        json!({"input":[{"role":"assistant","content":[{"type":"input_text","text":"hi","logprobs":[]}]}]}),
        json!({"model":"another-model"}),
    ] {
        assert!(matches!(
            prepare_request(fields),
            Err(error) if matches!(*error, ExecutorError::InvalidRequest(_))
        ));
    }
}
