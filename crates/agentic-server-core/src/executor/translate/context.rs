use crate::events::WireEvent;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::tool::custom::CustomToolMap;
use crate::tool::file_search::handler::FileSearchCitations;
use crate::tool::{NamespaceMap, ToolType};
use crate::types::event::ResponseStatus;
use crate::types::io::MultiAgentAction;
use crate::types::io::ToolChoice;
use crate::types::io::{FunctionToolCall, OutputItem, OutputMessage, ReasoningOutput};
use crate::types::request_response::{RequestPayload, ResponsePayload, StandardResponseFields};
use crate::types::tools::ResponsesTool;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

/// An owned snapshot of the effective tool classification and availability for one round.
#[derive(Default)]
pub(in crate::executor) struct TranslationContext {
    file_search_citations: Option<FileSearchCitations>,
    tool_types: HashMap<String, ToolType>,
    gateway_owned_names: HashSet<String>,
    async_tool_names: HashSet<String>,
    withheld_function_names: HashSet<String>,
    /// Deferred top-level functions behind hosted search. The Responses API
    /// reports their calls with `namespace` set to the function's own name.
    self_namespaced_functions: HashSet<String>,
    tool_search_active: bool,
    collaboration_enabled: bool,
    namespace_map: Option<NamespaceMap>,
    custom_tool_map: Option<CustomToolMap>,
    response_tools: Option<Vec<ResponsesTool>>,
    response_tool_choice: Option<ToolChoice>,
    response_fields: StandardResponseFields,
}

/// Who executes a model call, from the round's tool classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CallOwner {
    Client(ClientTool),
    Gateway,
}

/// The tool types a client executes; every other type is gateway-owned
/// (see [`ToolType::is_gateway_owned`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClientTool {
    Function,
    CodexNamespace,
    Custom,
    Shell,
    ToolSearch,
}

/// The changes that turn a model function call into its public form.
struct FunctionCallRewrite<'a> {
    /// A client function call gets the public `fc_` id.
    public_id: bool,
    /// A call to an async tool carries `async: true`.
    async_call: bool,
    /// A shortened namespace member's public namespace and name.
    member: Option<(&'a str, &'a str)>,
    /// A deferred top-level function behind hosted search names itself as its namespace.
    self_namespace: bool,
}

impl std::fmt::Debug for TranslationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranslationContext")
            .field("tool_count", &self.tool_types.len())
            .field("withheld_count", &self.withheld_function_names.len())
            .field("tool_search_active", &self.tool_search_active)
            .finish_non_exhaustive()
    }
}

impl TranslationContext {
    pub(in crate::executor) fn with_collaboration(mut self, enabled: bool) -> Self {
        self.collaboration_enabled = enabled;
        self
    }

    pub(super) fn is_collaboration(&self, name: &str) -> bool {
        self.collaboration_enabled && MultiAgentAction::from_tool_name(name).is_some()
    }
    pub(in crate::executor) fn new(
        tool_types: HashMap<String, ToolType>,
        withheld_function_names: HashSet<String>,
        tool_search_active: bool,
    ) -> Self {
        Self {
            tool_types,
            withheld_function_names,
            tool_search_active,
            ..Self::default()
        }
    }

    pub(in crate::executor) fn with_self_namespaced_functions(mut self, names: HashSet<String>) -> Self {
        self.self_namespaced_functions = names;
        self
    }

    /// Resolved ownership copied from the registry, including opt-in gateway shell execution.
    pub(in crate::executor) fn with_gateway_owned_names(mut self, names: HashSet<String>) -> Self {
        self.gateway_owned_names = names;
        self
    }

    pub(super) fn is_gateway_owned(&self, name: &str) -> bool {
        self.gateway_owned_names.contains(name)
    }

    /// Model-visible names of async client tools, copied from the registry.
    pub(in crate::executor) fn with_async_tool_names(mut self, names: HashSet<String>) -> Self {
        self.async_tool_names = names;
        self
    }

    pub(super) fn is_async_tool(&self, name: &str) -> bool {
        self.async_tool_names.contains(name)
    }

    /// Owned public mappings; construction performs no registry lookups.
    pub(in crate::executor) fn with_response_metadata(
        mut self,
        namespace_map: Option<NamespaceMap>,
        custom_tool_map: Option<CustomToolMap>,
        response_tools: Option<Vec<ResponsesTool>>,
        response_tool_choice: Option<ToolChoice>,
    ) -> Self {
        self.namespace_map = namespace_map;
        self.custom_tool_map = custom_tool_map;
        self.response_tools = super::tool_search::public_response_tools(response_tools);
        self.response_tool_choice = response_tool_choice;
        self
    }

    pub(in crate::executor) fn with_file_search_context(mut self, input: &crate::types::io::ResponsesInput) -> Self {
        // Validate only the gateway's model-facing search results. Native upstream
        // citations have their own evidence and must survive unrelated execution.
        self.file_search_citations = self
            .is_gateway_owned("file_search")
            .then(|| FileSearchCitations::from_input(input));
        self
    }

    pub(in crate::executor) fn with_request_fields(mut self, request: &RequestPayload) -> Self {
        self.response_fields.apply_request(request);
        self
    }

    pub(super) fn restore_stream_event_wire(&self, wire: &mut WireEvent) -> ExecutorResult<()> {
        if let Some(citations) = &self.file_search_citations {
            citations.restore_wire(wire);
        }
        super::tool_search::restore_response_tools(wire, self.response_tools.as_deref())?;
        // Report the selector the client submitted on every lifecycle snapshot, including
        // terminal ones that omit it, instead of letting a gateway default stand in for it.
        if self.response_tools.is_some()
            && let Some(choice) = self.response_tool_choice.as_ref()
            && let Some(response) = wire.rest.get_mut("response").and_then(serde_json::Value::as_object_mut)
        {
            response.insert("tool_choice".to_owned(), serde_json::to_value(choice)?);
        }
        super::custom::CustomTranslator::restore_response_wire(wire, self.custom_tool_map.as_ref());
        if let Some(response) = wire.rest.get_mut("response").and_then(Value::as_object_mut) {
            if let Some((map, choice)) = self.namespace_map.as_ref().zip(response.get_mut("tool_choice")) {
                super::namespace::restore_tool_choice(choice, map);
            }
            complete_response_fields(response, &self.response_fields)?;
            if let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) {
                for item in output {
                    self.normalize_wire_item(item);
                }
            }
        }
        if let Some(item) = wire.rest.get_mut("item") {
            self.normalize_wire_item(item);
        }
        if let Some(part) = wire.rest.get_mut("part") {
            normalize_content_part(part);
        }
        Ok(())
    }

    /// The wire counterpart of [`Self::normalize_output_item`]: the same public call rewrite, plus the
    /// shape fixes only upstream JSON needs. Provider fields the typed item does not model are kept.
    pub(super) fn normalize_wire_item(&self, item: &mut Value) {
        let Some(item) = item.as_object_mut() else {
            return;
        };
        normalize_item_shape(item);
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        let Some(model_name) = item.get("name").and_then(Value::as_str) else {
            return;
        };
        let has_namespace = item.get("namespace").is_some_and(|namespace| !namespace.is_null());
        let rewrite = self.function_call_rewrite(model_name, has_namespace);
        let self_namespace = rewrite.self_namespace.then(|| Value::String(model_name.to_owned()));
        if rewrite.public_id
            && let Some(Value::String(id)) = item.get_mut("id")
        {
            FunctionToolCall::normalize_public_id(id);
        }
        if rewrite.async_call {
            item.insert("async".to_owned(), Value::Bool(true));
        }
        if let Some((namespace, member)) = rewrite.member {
            log_namespace_restore(
                item.get("name").and_then(Value::as_str).unwrap_or_default(),
                namespace,
                member,
            );
            item.insert("namespace".to_owned(), Value::String(namespace.to_owned()));
            item.insert("name".to_owned(), Value::String(member.to_owned()));
        } else if let Some(namespace) = self_namespace {
            item.insert("namespace".to_owned(), namespace);
        }
    }

    /// The public form of one output item: message and reasoning ids, and the call rewrite.
    pub(super) fn normalize_output_item(&self, item: &mut OutputItem) {
        match item {
            OutputItem::Message(message) => OutputMessage::normalize_public_id(&mut message.id),
            OutputItem::Reasoning(reasoning) => ReasoningOutput::normalize_public_id(&mut reasoning.id),
            OutputItem::FunctionCall(call) => {
                let rewrite = self.function_call_rewrite(&call.name, call.namespace.is_some());
                if rewrite.public_id {
                    FunctionToolCall::normalize_public_id(&mut call.id);
                }
                call.async_execution |= rewrite.async_call;
                if let Some((namespace, member)) = rewrite.member {
                    log_namespace_restore(&call.name, namespace, member);
                    call.namespace = Some(namespace.to_owned());
                    member.clone_into(&mut call.name);
                } else if rewrite.self_namespace {
                    call.namespace = Some(call.name.clone());
                }
            }
            OutputItem::MultiAgentCall(_)
            | OutputItem::MultiAgentCallOutput(_)
            | OutputItem::AgentMessage(_)
            | OutputItem::CodeInterpreterCall(_)
            | OutputItem::FileSearchCall(_)
            | OutputItem::ToolSearchCall(_)
            | OutputItem::ToolSearchOutput(_)
            | OutputItem::CustomToolCall(_)
            | OutputItem::ShellCall(_)
            | OutputItem::WebSearchCall(_)
            | OutputItem::McpCall(_)
            | OutputItem::McpListTools(_)
            | OutputItem::Compaction(_)
            | OutputItem::Unknown => {}
        }
    }

    /// How a function call's public form differs from the model's, decided once on its
    /// model-visible name for both the typed output and the wire.
    fn function_call_rewrite(&self, model_name: &str, has_namespace: bool) -> FunctionCallRewrite<'_> {
        let member = self
            .namespace_map
            .as_ref()
            .filter(|_| !has_namespace)
            .and_then(|map| map.public_member(model_name));
        FunctionCallRewrite {
            public_id: self.is_client_function(model_name),
            async_call: self.is_async_tool(model_name),
            member,
            self_namespace: !has_namespace && member.is_none() && self.self_namespaced_functions.contains(model_name),
        }
    }

    pub(super) fn restore_response_metadata(&self, payload: &mut ResponsePayload) {
        if let Some(tools) = &self.response_tools {
            payload.tools = Some(tools.clone());
            payload.tool_choice = Some(self.response_tool_choice.clone().unwrap_or_default());
        }
    }

    pub(in crate::executor) fn tool_type(&self, name: &str) -> ToolType {
        if self.tool_search_active && name == crate::tool::tool_search::TOOL_SEARCH_NAME {
            ToolType::ToolSearch
        } else {
            self.tool_types.get(name).copied().unwrap_or(ToolType::Function)
        }
    }

    /// Whether `name` is the client-executed search the client must resolve,
    /// as opposed to a hosted search the gateway runs.
    pub(super) fn is_client_tool_search(&self, name: &str) -> bool {
        matches!(self.call_owner(name), CallOwner::Client(ClientTool::ToolSearch))
    }

    pub(super) fn is_withheld_function(&self, name: &str) -> bool {
        self.withheld_function_names.contains(name)
    }

    fn is_client_function(&self, name: &str) -> bool {
        matches!(
            self.call_owner(name),
            CallOwner::Client(ClientTool::Function | ClientTool::CodexNamespace)
        )
    }

    /// Who executes a call to `name`: the gateway for collaboration actions, gateway tool types and
    /// tools this request runs itself; otherwise the client, with the call's tool type.
    pub(super) fn call_owner(&self, name: &str) -> CallOwner {
        if self.is_collaboration(name) || self.is_gateway_owned(name) {
            return CallOwner::Gateway;
        }
        CallOwner::Client(match self.tool_type(name) {
            ToolType::Function => ClientTool::Function,
            ToolType::CodexNamespace => ClientTool::CodexNamespace,
            ToolType::Custom => ClientTool::Custom,
            ToolType::Shell => ClientTool::Shell,
            ToolType::ToolSearch => ClientTool::ToolSearch,
            ToolType::Mcp
            | ToolType::WebSearch
            | ToolType::WebFetch
            | ToolType::FileSearch
            | ToolType::CodeInterpreter => {
                return CallOwner::Gateway;
            }
        })
    }

    pub(super) fn tool_search_is_active(&self) -> bool {
        self.tool_search_active
    }

    pub(super) fn withheld_function_names(&self) -> &HashSet<String> {
        &self.withheld_function_names
    }

    pub(in crate::executor) fn normalize_response_output(
        &self,
        output: &mut Vec<OutputItem>,
        status: ResponseStatus,
        unfinished_stream_item_ids: &HashSet<String>,
    ) -> ExecutorResult<()> {
        super::tool_search::normalize_response_output(self, output, status, unfinished_stream_item_ids)?;
        if let Some(citations) = &self.file_search_citations {
            for item in output.iter_mut() {
                citations.restore_output(item);
            }
        }
        for item in output.iter_mut() {
            self.normalize_output_item(item);
        }
        Ok(())
    }
}

/// Sets the public `async` flag on a call to an async tool, for the function and custom
/// translators' item events; final output and snapshots take it from the call rewrite. Only
/// function and custom calls are client calls that can be async.
pub(super) fn mark_async_call(item: &mut OutputItem) -> ExecutorResult<()> {
    match item {
        OutputItem::FunctionCall(call) => call.async_execution = true,
        OutputItem::CustomToolCall(call) => call.async_execution = true,
        OutputItem::MultiAgentCall(_)
        | OutputItem::MultiAgentCallOutput(_)
        | OutputItem::AgentMessage(_)
        | OutputItem::Message(_)
        | OutputItem::CodeInterpreterCall(_)
        | OutputItem::FileSearchCall(_)
        | OutputItem::ToolSearchCall(_)
        | OutputItem::ToolSearchOutput(_)
        | OutputItem::ShellCall(_)
        | OutputItem::WebSearchCall(_)
        | OutputItem::McpCall(_)
        | OutputItem::McpListTools(_)
        | OutputItem::Reasoning(_)
        | OutputItem::Compaction(_)
        | OutputItem::Unknown => {
            return Err(ExecutorError::StreamError(
                "an async tool call translated to an item that is not a function or custom call".to_owned(),
            ));
        }
    }
    Ok(())
}

fn complete_response_fields(response: &mut Map<String, Value>, fields: &StandardResponseFields) -> ExecutorResult<()> {
    let Value::Object(standard) = serde_json::to_value(fields)? else {
        unreachable!("standard response fields serialize as an object");
    };
    for (key, value) in standard {
        if key == "completed_at" {
            response.entry(key).or_insert(value);
        } else {
            response.insert(key, value);
        }
    }
    let required_defaults = [
        ("error", Value::Null),
        ("incomplete_details", Value::Null),
        ("previous_response_id", Value::Null),
        ("instructions", Value::Null),
        ("tools", json!([])),
        ("tool_choice", json!("auto")),
        ("usage", Value::Null),
        ("max_tool_calls", Value::Null),
        ("service_tier", json!("default")),
    ];
    for (key, default) in required_defaults {
        if !response.contains_key(key) {
            response.insert(key.to_owned(), default);
        }
    }
    Ok(())
}

/// Shape fixes upstream items need on the wire: empty reasoning lists, no null optional fields,
/// public message and reasoning ids, and output-text `logprobs`.
fn normalize_item_shape(item: &mut Map<String, Value>) {
    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
        if let Some(Value::String(id)) = item.get_mut("id") {
            ReasoningOutput::normalize_public_id(id);
        }
        for field in ["content", "summary"] {
            if item.get(field).is_some_and(Value::is_null) {
                item.insert(field.to_owned(), json!([]));
            }
        }
        if item.get("encrypted_content").is_some_and(Value::is_null) {
            item.remove("encrypted_content");
        }
    }
    if item.get("type").and_then(Value::as_str) == Some("message") {
        if let Some(Value::String(id)) = item.get_mut("id") {
            OutputMessage::normalize_public_id(id);
        }
        if item.get("phase").is_some_and(Value::is_null) {
            item.remove("phase");
        }
    }
    if let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) {
        for part in content {
            normalize_content_part(part);
        }
    }
}

fn log_namespace_restore(model_name: &str, namespace: &str, member: &str) {
    tracing::debug!(
        upstream_name = %model_name,
        namespace = %namespace,
        member = %member,
        "restored upstream namespace function call"
    );
}

fn normalize_content_part(part: &mut Value) {
    let Some(part) = part.as_object_mut() else {
        return;
    };
    if part.get("type").and_then(Value::as_str) == Some("output_text")
        && part.get("logprobs").is_none_or(Value::is_null)
    {
        part.insert("logprobs".to_owned(), json!([]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_lifecycle_has_required_standard_fields() {
        let mut wire = WireEvent::new("response.created");
        wire.rest.insert(
            "response".to_owned(),
            json!({
                "id": "resp_1", "object": "response", "created_at": 1,
                "model": "test", "status": "in_progress", "output": [],
                "text": null, "top_logprobs": null, "parallel_tool_calls": true,
                "temperature": 0.9
            }),
        );
        TranslationContext::default()
            .with_request_fields(&RequestPayload {
                store: false,
                ..RequestPayload::default()
            })
            .restore_stream_event_wire(&mut wire)
            .expect("normalize response event");
        let response = &wire.rest["response"];
        assert_eq!(response["text"]["format"]["type"], "text");
        assert_eq!(response["top_logprobs"], 0);
        assert_eq!(response["store"], false);
        assert_eq!(response["parallel_tool_calls"], false);
        assert_eq!(response["temperature"], 1.0);
        assert_eq!(response["completed_at"], Value::Null);
        assert_eq!(response["tools"], json!([]));
    }

    #[test]
    fn snapshot_output_follows_the_typed_public_call_rules() {
        use crate::tool::codex::model_visible_namespace_member_name;
        use crate::tool::{CodexNamespaceHandler, responses_declarations};

        // A namespace member whose public name is also a gateway tool, declared async.
        let tools = responses_declarations(
            &serde_json::from_value::<Vec<ResponsesTool>>(json!([{
                "type": "namespace", "name": "travel",
                "tools": [{"type": "function", "name": "web_search"}]
            }]))
            .unwrap(),
        );
        let map = CodexNamespaceHandler.build_namespace_map(Some(&tools)).unwrap();
        let model_name = model_visible_namespace_member_name("travel", "web_search");
        let context = TranslationContext::new(
            HashMap::from([
                (model_name.clone(), ToolType::CodexNamespace),
                ("web_search".to_owned(), ToolType::WebSearch),
            ]),
            HashSet::new(),
            false,
        )
        .with_gateway_owned_names(HashSet::from(["web_search".to_owned()]))
        .with_async_tool_names(HashSet::from([model_name.clone()]))
        .with_response_metadata(map, None, None, None);
        let call = json!({
            "type": "function_call", "id": "call_upstream", "call_id": "call_1",
            "name": model_name, "arguments": "{}", "status": "completed"
        });

        let mut typed = vec![serde_json::from_value::<OutputItem>(call.clone()).unwrap()];
        context
            .normalize_response_output(&mut typed, ResponseStatus::Completed, &HashSet::new())
            .unwrap();
        let mut wire = WireEvent::new("response.in_progress");
        wire.rest.insert(
            "response".to_owned(),
            json!({"id": "resp_1", "status": "in_progress", "output": [call]}),
        );
        context.restore_stream_event_wire(&mut wire).unwrap();

        let typed = serde_json::to_value(&typed[0]).unwrap();
        let snapshot = &wire.rest["response"]["output"][0];
        for field in ["id", "namespace", "name", "async"] {
            assert_eq!(
                snapshot[field], typed[field],
                "{field} differs between the snapshot and the output"
            );
        }
        assert_eq!(snapshot["async"], true);
    }

    #[test]
    fn upstream_reasoning_and_content_parts_use_public_event_shapes() {
        let context = TranslationContext::default();
        let mut reasoning = WireEvent::new("response.reasoning_text.delta");
        context
            .restore_stream_event_wire(&mut reasoning)
            .expect("normalize reasoning event");
        assert_eq!(reasoning.event_type.as_deref(), Some("response.reasoning_text.delta"));

        let mut reasoning_part = WireEvent::new("response.reasoning_part.added");
        context
            .restore_stream_event_wire(&mut reasoning_part)
            .expect("preserve reasoning part event");
        assert_eq!(
            reasoning_part.event_type.as_deref(),
            Some("response.reasoning_part.added")
        );

        let mut item = WireEvent::new("response.output_item.added");
        item.rest.insert(
            "item".to_owned(),
            json!({
                "type": "reasoning", "id": "rs_1", "content": null,
                "summary": null, "encrypted_content": null
            }),
        );
        context.restore_stream_event_wire(&mut item).expect("normalize item");
        assert_eq!(item.rest["item"]["content"], json!([]));
        assert_eq!(item.rest["item"]["summary"], json!([]));
        assert!(item.rest["item"].get("encrypted_content").is_none());

        let mut part = WireEvent::new("response.content_part.done");
        part.rest.insert(
            "part".to_owned(),
            json!({
                "type": "output_text", "text": "ok", "annotations": [], "logprobs": null
            }),
        );
        context.restore_stream_event_wire(&mut part).expect("normalize part");
        assert_eq!(part.rest["part"]["logprobs"], json!([]));
        part.rest["part"].as_object_mut().unwrap().remove("logprobs");
        context
            .restore_stream_event_wire(&mut part)
            .expect("default missing logprobs");
        assert_eq!(part.rest["part"]["logprobs"], json!([]));
        for logprobs in [
            json!([]),
            json!([{"token": "ok", "bytes": [111, 107], "logprob": -0.1, "top_logprobs": []}]),
        ] {
            part.rest["part"]["logprobs"] = logprobs.clone();
            context.restore_stream_event_wire(&mut part).expect("preserve logprobs");
            assert_eq!(part.rest["part"]["logprobs"], logprobs);
        }
    }
}
