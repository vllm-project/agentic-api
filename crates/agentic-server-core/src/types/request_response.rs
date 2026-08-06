use std::borrow::Cow;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::io::{
    FunctionTool, InputItem, InputMessage, InputMessageContent, OutputItem, ResponseUsage, ResponsesInput, ToolChoice,
};
use super::tools::{CustomToolParam, ResponsesTool};
use crate::tool::{CodexNamespaceHandler, ToolError};
use crate::utils::common::serialize_to_string;

#[derive(Debug, Clone, Serialize)]
pub struct RequestPayload {
    pub model: String,
    pub input: ResponsesInput,
    pub instructions: Option<String>,
    pub previous_response_id: Option<String>,
    #[serde(rename = "conversation")]
    pub conversation_id: Option<String>,
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default = "default_true")]
    pub store: bool,
    pub include: Option<Vec<String>>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_output_tokens: Option<u32>,
    pub truncation: Option<String>,
    pub metadata: Option<Value>,
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_salt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_management: Option<Vec<ContextManagement>>,
}

#[derive(Debug, Deserialize)]
struct RequestPayloadWire {
    model: String,
    input: ResponsesInput,
    instructions: Option<String>,
    previous_response_id: Option<String>,
    conversation: Option<String>,
    #[serde(rename = "conversation_id")]
    legacy_conversation_id: Option<String>,
    tools: Option<Vec<ResponsesTool>>,
    #[serde(default)]
    tool_choice: Option<ToolChoice>,
    #[serde(default)]
    stream: bool,
    #[serde(default = "default_true")]
    store: bool,
    include: Option<Vec<String>>,
    temperature: Option<f64>,
    top_p: Option<f64>,
    max_output_tokens: Option<u32>,
    truncation: Option<String>,
    metadata: Option<Value>,
    parallel_tool_calls: Option<bool>,
    #[serde(default)]
    cache_salt: Option<String>,
    #[serde(default)]
    context_management: Option<Vec<ContextManagement>>,
}

impl<'de> Deserialize<'de> for RequestPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = RequestPayloadWire::deserialize(deserializer)?;
        let conversation_id = match (wire.conversation, wire.legacy_conversation_id) {
            (Some(conversation), Some(legacy_conversation)) if conversation != legacy_conversation => {
                return Err(serde::de::Error::custom(
                    "conversation and conversation_id must reference the same conversation",
                ));
            }
            (Some(conversation), _) => Some(conversation),
            (_, Some(legacy_conversation)) => Some(legacy_conversation),
            (None, None) => None,
        };

        Ok(Self {
            model: wire.model,
            input: wire.input,
            instructions: wire.instructions,
            previous_response_id: wire.previous_response_id,
            conversation_id,
            tools: wire.tools,
            tool_choice: wire.tool_choice,
            stream: wire.stream,
            store: wire.store,
            include: wire.include,
            temperature: wire.temperature,
            top_p: wire.top_p,
            max_output_tokens: wire.max_output_tokens,
            truncation: wire.truncation,
            metadata: wire.metadata,
            parallel_tool_calls: wire.parallel_tool_calls,
            cache_salt: wire.cache_salt,
            context_management: wire.context_management,
        })
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct UpstreamRequest<'a> {
    pub model: &'a str,
    pub input: Cow<'a, ResponsesInput>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<&'a str>,
    /// Tools forwarded to vLLM. Namespace members are flattened to ordinary
    /// function declarations; native custom declarations retain their freeform
    /// wire shape.
    /// Skipped when empty so vLLM does not receive an empty array.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<UpstreamTool>>,
    #[serde(skip_serializing_if = "is_absent_or_default_tool_choice")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<&'a Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    pub cache_salt: Option<&'a str>,
}

/// A tool declaration supported by the upstream Responses endpoint.
///
/// Function-like gateway declarations are normalized to [`FunctionTool`],
/// while freeform custom declarations retain their native Responses shape.
/// Keeping these as distinct variants prevents unrelated request tool types
/// from entering the upstream tool list.
#[derive(Debug, Clone)]
pub enum UpstreamTool {
    Function(FunctionTool),
    Custom(CustomToolParam),
}

impl Serialize for UpstreamTool {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Function(tool) => tool.serialize(serializer),
            Self::Custom(declaration) => {
                #[derive(Serialize)]
                struct NativeCustomTool<'a> {
                    #[serde(rename = "type")]
                    type_: &'static str,
                    #[serde(flatten)]
                    declaration: &'a CustomToolParam,
                }

                NativeCustomTool {
                    type_: "custom",
                    declaration,
                }
                .serialize(serializer)
            }
        }
    }
}

// serde's `skip_serializing_if` requires a `&Option<T>` receiver, so the
// idiomatic `Option<&T>` clippy suggests does not apply here.
#[allow(clippy::ref_option)]
fn is_absent_or_default_tool_choice(choice: &Option<ToolChoice>) -> bool {
    choice.as_ref().is_none_or(|choice| matches!(choice, ToolChoice::Auto))
}

impl RequestPayload {
    /// Construct an `UpstreamRequest` suitable for forwarding to vLLM.
    ///
    /// Codex `namespace` tools' members are first renamed to their flat,
    /// model-visible names via [`CodexNamespaceHandler::resolve_namespace_members`].
    /// Namespace and gateway tools are then normalized to function declarations.
    /// Native custom tools are forwarded unchanged because their calls are not
    /// function calls. `tool_choice` is resolved the same way via
    /// [`CodexNamespaceHandler::resolve_tool_choice`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Config`] when a Codex namespace member's generated
    /// flat name collides with a top-level function tool or another namespace
    /// member.
    pub fn to_upstream_request(&self, stream: bool) -> Result<UpstreamRequest<'_>, ToolError> {
        let has_built_in_tool = self.declares_built_in_tool();
        if has_built_in_tool && self.parallel_tool_calls == Some(true) {
            return Err(ToolError::Config(
                "parallel_tool_calls must be false when using built-in tools".into(),
            ));
        }
        let parallel_tool_calls = if has_built_in_tool {
            Some(false)
        } else {
            self.parallel_tool_calls
        };

        let renamed_tools = self
            .tools
            .as_deref()
            .map(|tools| CodexNamespaceHandler.resolve_namespace_members(tools))
            .transpose()?;
        let tools: Option<Vec<UpstreamTool>> =
            renamed_tools.map(|tools| tools.into_iter().flat_map(upstream_tools).collect());
        let tools = tools.filter(|tools| !tools.is_empty());
        let namespace_map = CodexNamespaceHandler.build_namespace_map(self.tools.as_deref())?;
        let tool_choice = CodexNamespaceHandler.resolve_tool_choice(namespace_map.as_ref(), self.tool_choice.as_ref());
        Ok(UpstreamRequest {
            model: &self.model,
            input: self.input.model_input(),
            stream,
            instructions: self.instructions.as_deref(),
            tools,
            tool_choice: Some(tool_choice),
            include: self.include.as_ref(),
            temperature: self.temperature,
            top_p: self.top_p,
            max_output_tokens: self.max_output_tokens,
            truncation: self.truncation.as_deref(),
            metadata: self.metadata.as_ref(),
            parallel_tool_calls,
            cache_salt: self.cache_salt.as_deref(),
        })
    }

    fn declares_built_in_tool(&self) -> bool {
        self.tools
            .as_deref()
            .is_some_and(|tools| tools.iter().any(ResponsesTool::is_gateway_owned))
    }
}

/// Server-side context management configuration for a Responses request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextManagement {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_threshold: Option<u64>,
}

/// Request body accepted by `POST /v1/responses/compact`.
#[derive(Debug, Clone, Deserialize)]
pub struct CompactRequest {
    pub model: String,
    #[serde(default)]
    pub input: Option<ResponsesInput>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub previous_response_id: Option<String>,
    /// Compatibility fields sent by current SDK and Codex clients.
    #[serde(flatten)]
    pub compatibility: HashMap<String, Value>,
}

/// Result returned by `POST /v1/responses/compact`.
#[derive(Debug, Clone, Serialize)]
pub struct CompactedResponse {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub output: Vec<InputItem>,
    pub usage: ResponseUsage,
}

fn upstream_tools(tool: ResponsesTool) -> Vec<UpstreamTool> {
    match tool {
        ResponsesTool::Custom(declaration) => {
            tracing::debug!(
                name = %declaration.name,
                has_format = declaration.format.is_some(),
                "forwarding native custom tool declaration upstream"
            );
            vec![UpstreamTool::Custom(declaration)]
        }
        function_like => function_like
            .to_function_tools()
            .into_iter()
            .map(UpstreamTool::Function)
            .collect(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncompleteDetails {
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsePayload {
    pub id: String,
    pub object: String,
    pub created_at: i64,
    pub model: String,
    pub status: String,
    #[serde(default)]
    pub output: Vec<OutputItem>,
    pub usage: Option<ResponseUsage>,
    pub incomplete_details: Option<IncompleteDetails>,
    pub error: Option<Value>,
    pub previous_response_id: Option<String>,
    #[serde(
        rename = "conversation",
        alias = "conversation_id",
        serialize_with = "serialize_conversation_reference",
        deserialize_with = "deserialize_conversation_reference"
    )]
    pub conversation_id: Option<String>,
    pub instructions: Option<String>,
    pub temperature: Option<f64>,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    #[serde(default)]
    pub tools: Vec<ResponsesTool>,
    pub top_p: Option<f64>,
    pub truncation: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ConversationReference {
    Id(String),
    Object { id: String },
}

#[allow(clippy::ref_option)]
fn serialize_conversation_reference<S>(value: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match value {
        Some(id) => serde_json::json!({"id": id}).serialize(serializer),
        None => serializer.serialize_none(),
    }
}

fn deserialize_conversation_reference<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<ConversationReference>::deserialize(deserializer).map(|reference| {
        reference.map(|reference| match reference {
            ConversationReference::Id(id) | ConversationReference::Object { id } => id,
        })
    })
}

impl ResponsePayload {
    #[must_use]
    pub fn as_created_response_chunk(&self) -> String {
        let mut response = self.clone();
        "in_progress".clone_into(&mut response.status);
        let event = json!({
            "type": "response.created",
            "response": response,
        });
        let json_str = serialize_to_string(&event).unwrap_or_else(|_| String::new());
        format!("data: {json_str}\n\n")
    }

    #[must_use]
    pub fn as_responses_chunk(&self) -> String {
        let json_str = serialize_to_string(self).unwrap_or_else(|_| String::new());
        format!("data: {json_str}\n\n")
    }

    #[must_use]
    pub fn as_terminal_response_chunk(&self) -> String {
        let event = json!({
            "type": self.terminal_event_type(),
            "response": self,
        });
        let json_str = serialize_to_string(&event).unwrap_or_else(|_| String::new());
        format!("data: {json_str}\n\n")
    }

    pub(crate) fn terminal_event_type(&self) -> &'static str {
        match self.status.as_str() {
            "incomplete" => "response.incomplete",
            "failed" | "error" => "response.failed",
            "in_progress" => "response.in_progress",
            _ => "response.completed",
        }
    }
}

impl From<&ResponsesInput> for Vec<InputItem> {
    fn from(input: &ResponsesInput) -> Self {
        match input {
            ResponsesInput::Text(text) => vec![InputItem::Message(InputMessage {
                id: None,
                role: "user".into(),
                status: None,
                content: InputMessageContent::Text(text.clone()),
            })],
            ResponsesInput::Items(items) => items.iter().filter(|item| !item.is_unknown()).cloned().collect(),
        }
    }
}

impl From<ResponsesInput> for Vec<InputItem> {
    fn from(input: ResponsesInput) -> Self {
        match input {
            ResponsesInput::Text(text) => vec![InputItem::Message(InputMessage {
                id: None,
                role: "user".into(),
                status: None,
                content: InputMessageContent::Text(text),
            })],
            ResponsesInput::Items(items) => items.into_iter().filter(|item| !item.is_unknown()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_wire_field_uses_standard_name_and_accepts_legacy_alias() {
        let standard: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "input": [],
            "conversation": "conv_standard"
        }))
        .expect("standard conversation field");
        let legacy: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "input": [],
            "conversation_id": "conv_legacy"
        }))
        .expect("legacy conversation field");
        let matching_aliases: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "input": [],
            "conversation": "conv_same",
            "conversation_id": "conv_same"
        }))
        .expect("matching conversation aliases");
        let conflicting_aliases = serde_json::from_value::<RequestPayload>(serde_json::json!({
            "model": "test-model",
            "input": [],
            "conversation": "conv_one",
            "conversation_id": "conv_two"
        }));

        assert_eq!(standard.conversation_id.as_deref(), Some("conv_standard"));
        assert_eq!(legacy.conversation_id.as_deref(), Some("conv_legacy"));
        assert_eq!(matching_aliases.conversation_id.as_deref(), Some("conv_same"));
        assert!(conflicting_aliases.is_err());
        let serialized = serde_json::to_value(standard).expect("serialize standard conversation field");
        assert_eq!(serialized["conversation"], "conv_standard");
        assert!(serialized.get("conversation_id").is_none());
    }

    #[test]
    fn response_conversation_serializes_as_standard_reference_and_accepts_legacy_shapes() {
        let mut payload: ResponsePayload = serde_json::from_value(serde_json::json!({
            "id": "resp_test",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [],
            "usage": null,
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation": {"id": "conv_standard"},
            "instructions": null
        }))
        .expect("standard response conversation reference");
        assert_eq!(payload.conversation_id.as_deref(), Some("conv_standard"));

        payload.conversation_id = Some("conv_serialized".to_owned());
        let serialized = serde_json::to_value(payload).expect("serialize response payload");
        assert_eq!(serialized["conversation"]["id"], "conv_serialized");
        assert!(serialized.get("conversation_id").is_none());

        let legacy: ResponsePayload = serde_json::from_value(serde_json::json!({
            "id": "resp_legacy",
            "object": "response",
            "created_at": 0,
            "model": "test-model",
            "status": "completed",
            "output": [],
            "usage": null,
            "incomplete_details": null,
            "error": null,
            "previous_response_id": null,
            "conversation_id": "conv_legacy",
            "instructions": null
        }))
        .expect("legacy response conversation reference");
        assert_eq!(legacy.conversation_id.as_deref(), Some("conv_legacy"));
    }

    #[test]
    fn compact_request_accepts_codex_compatibility_fields() {
        let request: CompactRequest = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "input": [{"role": "user", "content": "hello"}],
            "tools": [],
            "parallel_tool_calls": true,
            "reasoning": {"effort": "medium"},
            "text": {"verbosity": "low"}
        }))
        .expect("compact request should parse");

        assert_eq!(request.model, "test-model");
        assert!(request.input.is_some());
        assert_eq!(request.compatibility.len(), 4);
    }

    #[test]
    fn request_payload_forwards_cache_salt_upstream() {
        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "input": "hello",
            "cache_salt": "tenant-a"
        }))
        .expect("request should deserialize");

        let upstream = serde_json::to_value(payload.to_upstream_request(false).expect("request should normalize"))
            .expect("upstream request should serialize");

        assert_eq!(upstream["cache_salt"], "tenant-a");
    }

    #[test]
    fn request_payload_uses_option_tool_choice_for_missing_vs_explicit() {
        let absent: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test",
            "input": "hi"
        }))
        .unwrap();
        assert_eq!(absent.tool_choice, None);

        let explicit: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test",
            "input": "hi",
            "tool_choice": "none"
        }))
        .unwrap();
        assert_eq!(explicit.tool_choice, Some(ToolChoice::None));
    }

    #[test]
    fn to_upstream_request_carries_instructions_forward() {
        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test",
            "instructions": "rules",
            "input": "hi"
        }))
        .unwrap();

        assert_eq!(payload.instructions.as_deref(), Some("rules"));
        assert!(matches!(&payload.input, ResponsesInput::Text(text) if text == "hi"));

        let upstream = payload.to_upstream_request(false).expect("valid upstream request");
        let value = serde_json::to_value(upstream).unwrap();
        assert_eq!(value["instructions"], "rules");
        assert_eq!(value["input"], "hi");
    }

    #[test]
    fn to_upstream_request_preserves_parallel_tool_calls() {
        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test",
            "input": "hi",
            "parallel_tool_calls": false
        }))
        .unwrap();

        let upstream = payload.to_upstream_request(false).expect("valid upstream request");
        let value = serde_json::to_value(upstream).unwrap();
        assert_eq!(value["parallel_tool_calls"], false);
    }

    #[test]
    fn to_upstream_request_allows_parallel_tool_calls_for_client_function_tools() {
        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test",
            "input": "hi",
            "parallel_tool_calls": true,
            "tools": [{"type": "function", "name": "get_weather"}]
        }))
        .unwrap();

        let upstream = payload
            .to_upstream_request(false)
            .expect("function tools allow parallel calls");
        let value = serde_json::to_value(upstream).unwrap();
        assert_eq!(value["parallel_tool_calls"], true);
    }

    #[test]
    fn to_upstream_request_validates_parallel_tool_calls_for_mixed_tools() {
        for built_in_tool in builtin_tool_declarations() {
            for (parallel_tool_calls, should_reject) in [(false, false), (true, true)] {
                let payload: RequestPayload = serde_json::from_value(serde_json::json!({
                    "model": "test",
                    "input": "hi",
                    "parallel_tool_calls": parallel_tool_calls,
                    "tools": [
                        {"type": "function", "name": "get_weather"},
                        built_in_tool.clone()
                    ]
                }))
                .unwrap();

                let result = payload.to_upstream_request(false);
                if should_reject {
                    let err = result.expect_err("built-in tools should reject parallel tool calls");
                    assert!(err.to_string().contains("parallel_tool_calls must be false"));
                } else {
                    let value =
                        serde_json::to_value(result.expect("mixed built-in and function tools allow serial calls"))
                            .unwrap();
                    assert_eq!(value["parallel_tool_calls"], false);
                }
            }
        }
    }

    #[test]
    fn to_upstream_request_sets_serial_tool_calls_for_builtin_tools() {
        for tool in builtin_tool_declarations() {
            let payload: RequestPayload = serde_json::from_value(serde_json::json!({
                "model": "test",
                "input": "hi",
                "tools": [tool]
            }))
            .unwrap();

            let upstream = payload
                .to_upstream_request(false)
                .expect("built-in tools default to serial tool calls");
            let value = serde_json::to_value(upstream).unwrap();
            assert_eq!(value["parallel_tool_calls"], false);
        }
    }

    #[test]
    fn to_upstream_request_rejects_parallel_tool_calls_for_builtin_tools() {
        for tool in builtin_tool_declarations() {
            let payload: RequestPayload = serde_json::from_value(serde_json::json!({
                "model": "test",
                "input": "hi",
                "parallel_tool_calls": true,
                "tools": [tool]
            }))
            .unwrap();

            let Err(err) = payload.to_upstream_request(false) else {
                panic!("built-in tools should reject parallel_tool_calls=true");
            };

            assert!(err.to_string().contains("parallel_tool_calls must be false"));
        }
    }

    #[test]
    fn to_upstream_request_allows_builtin_tools_with_serial_tool_calls() {
        for tool in builtin_tool_declarations() {
            let payload: RequestPayload = serde_json::from_value(serde_json::json!({
                "model": "test",
                "input": "hi",
                "parallel_tool_calls": false,
                "tools": [tool]
            }))
            .unwrap();

            let upstream = payload
                .to_upstream_request(false)
                .expect("serial built-in tool request is valid");
            let value = serde_json::to_value(upstream).unwrap();
            assert_eq!(value["parallel_tool_calls"], false);
        }
    }

    fn builtin_tool_declarations() -> Vec<Value> {
        vec![
            serde_json::json!({
                "type": "mcp",
                "server_label": "repo",
                "server_url": "http://localhost:9001/mcp"
            }),
            serde_json::json!({"type": "web_search_preview"}),
            serde_json::json!({"type": "file_search", "vector_store_ids": ["vs_abc"]}),
            serde_json::json!({"type": "code_interpreter"}),
        ]
    }

    #[test]
    fn to_upstream_request_flattens_namespace_and_skips_unknown_tools() {
        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test",
            "input": "hi",
            "tools": [
                {
                    "type": "namespace",
                    "name": "mcp__shell",
                    "tools": [
                        {"type": "function", "name": "run", "parameters": {"type": "object"}},
                        {"type": "future_member", "opaque": true}
                    ]
                },
                {"type": "future_tool", "opaque": true}
            ]
        }))
        .unwrap();

        let tools = payload.tools.as_ref().expect("tools should preserve explicit presence");
        assert_eq!(tools.len(), 2);
        let ResponsesTool::Namespace(namespace) = &tools[0] else {
            panic!("expected namespace tool");
        };
        assert_eq!(namespace.tools.len(), 2);

        let upstream = payload.to_upstream_request(false).expect("valid upstream request");
        let value = serde_json::to_value(upstream).unwrap();
        assert_eq!(value["tools"].as_array().expect("upstream tools").len(), 1);
        assert_eq!(value["tools"][0]["name"], "agentic_ns__mcp__shell__run");
    }

    #[test]
    fn to_upstream_request_rejects_namespace_collisions() {
        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test",
            "input": "hi",
            "tools": [
                {"type": "function", "name": "agentic_ns__mcp__shell__run"},
                {
                    "type": "namespace",
                    "name": "mcp__shell",
                    "tools": [{"type": "function", "name": "run"}]
                }
            ]
        }))
        .unwrap();

        let Err(err) = payload.to_upstream_request(false) else {
            panic!("colliding namespace member should be rejected");
        };

        assert!(err.to_string().contains("collides with a declared function tool"));
    }

    #[test]
    fn to_upstream_request_serializes_mixed_function_and_native_custom_tools() {
        let payload: RequestPayload = serde_json::from_value(serde_json::json!({
            "model": "test",
            "input": "hi",
            "tool_choice": {
                "type": "custom",
                "name": "apply_patch"
            },
            "tools": [
                {
                    "type": "function",
                    "name": "read_file",
                    "description": "Read a file.",
                    "parameters": {"type": "object"}
                },
                {
                    "type": "custom",
                    "name": "apply_patch",
                    "description": "Apply a patch.",
                    "format": {
                        "type": "grammar",
                        "syntax": "lark",
                        "definition": "start: patch"
                    },
                    "x-provider-field": {"mode": "strict"}
                }
            ]
        }))
        .unwrap();

        let request = payload.to_upstream_request(false).unwrap();
        let tools = request.tools.as_ref().expect("mixed upstream tools");
        assert!(matches!(tools[0], UpstreamTool::Function(_)));
        assert!(matches!(tools[1], UpstreamTool::Custom(_)));

        let upstream = serde_json::to_value(request).unwrap();
        assert_eq!(upstream["tools"][0]["type"], "function");
        assert_eq!(upstream["tools"][0]["name"], "read_file");
        assert_eq!(upstream["tools"][1]["type"], "custom");
        assert_eq!(upstream["tools"][1]["name"], "apply_patch");
        assert_eq!(upstream["tools"][1]["description"], "Apply a patch.");
        assert_eq!(upstream["tools"][1]["format"]["type"], "grammar");
        assert_eq!(upstream["tools"][1]["format"]["syntax"], "lark");
        assert_eq!(upstream["tools"][1]["format"]["definition"], "start: patch");
        assert_eq!(upstream["tools"][1]["x-provider-field"]["mode"], "strict");
        assert_eq!(upstream["tool_choice"]["type"], "custom");
        assert_eq!(upstream["tool_choice"]["name"], "apply_patch");
    }

    #[test]
    fn responses_input_discards_unknown_items_when_converted_for_storage() {
        let input: ResponsesInput = serde_json::from_value(serde_json::json!([
            {"type": "message", "role": "user", "content": "hi"},
            {"type": "future_item", "payload": {"a": 1}}
        ]))
        .unwrap();

        let items = Vec::<InputItem>::from(&input);
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], InputItem::Message(_)));
    }

    #[test]
    fn response_payload_terminal_chunk_uses_status_specific_event_type() {
        let mut payload = ResponsePayload {
            id: "resp_test".to_string(),
            object: "response".to_string(),
            created_at: 0,
            model: "test-model".to_string(),
            status: "completed".to_string(),
            output: Vec::new(),
            usage: None,
            incomplete_details: None,
            error: None,
            previous_response_id: None,
            conversation_id: None,
            instructions: None,
            temperature: None,
            tool_choice: ToolChoice::Auto,
            tools: Vec::new(),
            top_p: None,
            truncation: None,
            metadata: None,
        };

        for (status, expected_type) in [
            ("completed", "response.completed"),
            ("incomplete", "response.incomplete"),
            ("failed", "response.failed"),
            ("error", "response.failed"),
            ("in_progress", "response.in_progress"),
        ] {
            payload.status = status.to_string();
            let chunk = payload.as_terminal_response_chunk();
            let data = chunk.trim().strip_prefix("data: ").unwrap();
            let event: Value = serde_json::from_str(data).unwrap();
            assert_eq!(event["type"], expected_type);
            assert_eq!(event["response"]["status"], status);
        }
    }

    #[test]
    fn response_payload_created_chunk_uses_in_progress_status() {
        let payload = ResponsePayload {
            id: "resp_test".to_string(),
            object: "response".to_string(),
            created_at: 0,
            model: "test-model".to_string(),
            status: "completed".to_string(),
            output: Vec::new(),
            usage: None,
            incomplete_details: None,
            error: None,
            previous_response_id: None,
            conversation_id: None,
            instructions: None,
            temperature: None,
            tool_choice: ToolChoice::Auto,
            tools: Vec::new(),
            top_p: None,
            truncation: None,
            metadata: None,
        };

        let chunk = payload.as_created_response_chunk();
        let data = chunk.trim().strip_prefix("data: ").unwrap();
        let event: Value = serde_json::from_str(data).unwrap();
        assert_eq!(event["type"], "response.created");
        assert_eq!(event["response"]["id"], "resp_test");
        assert_eq!(event["response"]["status"], "in_progress");
    }
}
