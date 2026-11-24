use agentic_core::types::request_response::RequestPayload;
use serde_json::json;
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
    let chat = chat_request(request.to_upstream_request(true).unwrap(), "attempt_1", "test").unwrap();
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
    assert_eq!(chat.cache_salt.as_deref(), Some("tenant"));
}

#[test]
fn unsupported_requests_fail_before_engine_submission() {
    for fields in [
        json!({"include":["message.output_text.logprobs"]}),
        json!({"text":{"format":{"type":"json_object"}}}),
        json!({"reasoning":{"summary":"auto"}}),
        json!({"input":[{"role":"user","content":[{"type":"input_image","image_url":"https://example.test/a.png"}]}]}),
        json!({"model":"another-model"}),
    ] {
        let mut request = json!({"model":"test","input":"hello"});
        request
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let request: RequestPayload = serde_json::from_value(request).unwrap();
        assert!(matches!(
            chat_request(request.to_upstream_request(true).unwrap(), "attempt", "test"),
            Err(error) if matches!(*error, ExecutorError::InvalidRequest(_))
        ));
    }
}
