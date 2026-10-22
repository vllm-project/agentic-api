//! Replayed tool-search history: public search items become the model's private call and output pairs, and the
//! definitions they loaded are collected for this request.

use std::collections::{HashMap, HashSet, VecDeque};
use std::mem;

use serde::Serialize;
use serde_json::Value;

use crate::tool::codex::is_searched_function_call;
use crate::tool::handler::ToolError;
use crate::types::event::MessageStatus;
use crate::types::io::AgentAttribution;
use crate::types::io::{
    FunctionToolResultMessage, InOutItem, InputFunctionToolCall, InputItem, InputToolSearchCall, ResponsesInput,
    ToolCallOutput, ToolSearchOutputMessage,
};
use crate::types::tools::{FunctionToolParam, ResponsesTool, ToolSearchExecution, ToolSearchStatus};
use crate::utils::common::{serialize_to_string, serialize_to_value};

use super::definitions::load_definition;
use super::{DefinitionAccumulator, DefinitionViews, LoadedToolIdentity, TOOL_SEARCH_NAME};

#[derive(Serialize)]
struct CanonicalToolSearchOutput<'a> {
    tools: &'a [ModelVisibleLoadedTool<'a>],
}

/// Typed model-output projection, deliberately separate from the raw
/// credential-sensitive definition retained for equality and later execution.
#[derive(Serialize)]
#[serde(untagged)]
enum ModelVisibleLoadedTool<'a> {
    Function(ModelVisibleFunction<'a>),
    Namespace(ModelVisibleNamespace<'a>),
}

#[derive(Serialize)]
struct ModelVisibleFunction<'a> {
    #[serde(rename = "type")]
    type_: &'static str,
    #[serde(flatten)]
    definition: &'a FunctionToolParam,
}

#[derive(Serialize)]
struct ModelVisibleNamespace<'a> {
    #[serde(rename = "type")]
    type_: &'static str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
}

pub(super) fn prepare_history(
    input: &ResponsesInput,
    views: DefinitionViews<'_>,
    unqualified_call_positions: &mut HashMap<String, usize>,
) -> Result<ResponsesInput, ToolError> {
    let ResponsesInput::Items(items) = input else {
        return Ok(input.clone());
    };
    let mut private_items = Vec::with_capacity(items.len());
    let mut pairing = SearchPairing::default();
    let DefinitionViews {
        public_tools,
        definitions,
        definition_indexes,
        loaded_public_tools,
        withheld_function_names,
        mcp_loads,
    } = views;
    let mut definition_accumulator = DefinitionAccumulator {
        public_tools,
        definitions,
        definition_indexes,
        loaded_public_tools,
        withheld_function_names,
        mcp_loads,
        prior_unknown_namespace_calls: HashMap::new(),
        unqualified_call_positions: mem::take(unqualified_call_positions),
        current_history_position: None,
    };

    for (position, item) in items.iter().filter_map(InOutItem::as_input).enumerate() {
        definition_accumulator.current_history_position = Some(position);
        match item.as_ref() {
            InputItem::ToolSearchCall(call) => {
                private_items.push(prepare_search_call(call, &mut pairing)?);
            }
            InputItem::ToolSearchOutput(output) => {
                private_items.push(prepare_search_output(
                    output,
                    &mut pairing,
                    &mut definition_accumulator,
                )?);
            }
            InputItem::FunctionCall(call) => {
                let is_function = |name: &str| {
                    definition_accumulator
                        .definition_indexes
                        .get(name)
                        .is_some_and(|index| {
                            matches!(
                                definition_accumulator.definitions[*index].identity,
                                LoadedToolIdentity::Function(_)
                            )
                        })
                };
                let call = if is_searched_function_call(call, is_function) {
                    InputFunctionToolCall {
                        namespace: None,
                        ..call.clone()
                    }
                } else {
                    call.clone()
                };
                ensure_history_call_is_available(&call, &mut definition_accumulator)?;
                private_items.push(InputItem::FunctionCall(call));
            }
            InputItem::CompactionTrigger => {}
            InputItem::Message(_)
            | InputItem::CodeInterpreterCall(_)
            | InputItem::McpListTools(_)
            | InputItem::FunctionCallOutput(_)
            | InputItem::CustomToolCall(_)
            | InputItem::CustomToolCallOutput(_)
            | InputItem::ShellCall(_)
            | InputItem::ShellCallOutput(_)
            | InputItem::Reasoning(_)
            | InputItem::Compaction(_)
            | InputItem::MultiAgentCall(_)
            | InputItem::MultiAgentCallOutput(_)
            | InputItem::AgentMessage(_)
            | InputItem::WebSearchCall(_)
            | InputItem::McpCall(_) => private_items.push(item.into_owned()),
        }
    }

    if pairing.has_unresolved() {
        return Err(ToolError::Config(
            "unresolved tool_search_call requires a matching completed tool_search_output".to_owned(),
        ));
    }
    *unqualified_call_positions = definition_accumulator.unqualified_call_positions;
    Ok(ResponsesInput::from(private_items))
}

fn ensure_history_call_is_available(
    call: &InputFunctionToolCall,
    definitions: &mut DefinitionAccumulator<'_>,
) -> Result<(), ToolError> {
    if let Some(namespace) = call.namespace.as_deref() {
        let member = definitions
            .definition_indexes
            .get(namespace)
            .and_then(|index| definitions.definitions.get(*index))
            .filter(|record| matches!(record.identity, LoadedToolIdentity::Namespace(_)))
            .and_then(|record| record.namespace_members.as_ref())
            .and_then(|members| members.indexes.get(&call.name).map(|index| &members.ordered[*index]));
        match member {
            Some(member) if !member.loaded => return Err(withheld_function_history_call()),
            Some(_) => {}
            None => {
                definitions
                    .prior_unknown_namespace_calls
                    .entry(namespace.to_owned())
                    .or_default()
                    .insert(call.name.clone());
            }
        }
    } else {
        if definitions.withheld_function_names.contains(&call.name) {
            return Err(withheld_function_history_call());
        }
        let position = definitions
            .current_history_position
            .ok_or_else(|| ToolError::Config("tool-search history position is unavailable".to_owned()))?;
        definitions
            .unqualified_call_positions
            .entry(call.name.clone())
            .or_insert(position);
    }
    Ok(())
}

pub(super) fn withheld_function_history_call() -> ToolError {
    ToolError::Config("request history calls a function before its definition is loaded".to_owned())
}

/// Call/output pairing state while projecting public search history.
///
/// A client pair is matched by `call_id`. A hosted pair has no call id: its
/// output resolves the oldest unresolved hosted call, and the private call id
/// is the call's item id, so the projection is stable across requests.
#[derive(Default)]
struct SearchPairing {
    unresolved_calls: HashSet<String>,
    unresolved_hosted_calls: VecDeque<String>,
    completed_call_ids: HashSet<String>,
    item_ids: HashSet<String>,
}

impl SearchPairing {
    fn has_unresolved(&self) -> bool {
        !self.unresolved_calls.is_empty() || !self.unresolved_hosted_calls.is_empty()
    }
}

/// The private call id of a hosted search, derived from its public item id.
pub(crate) fn hosted_private_call_id(item_id: &str) -> String {
    item_id.to_owned()
}

/// The private function call the model sees for a public search call.
pub(crate) fn private_search_call(
    agent: Option<AgentAttribution>,
    item_id: &str,
    call_id: String,
    arguments: &Value,
) -> Result<InputItem, ToolError> {
    let canonical_arguments = serialize_to_string(arguments)
        .map_err(|_| ToolError::Config("tool_search_call arguments could not be canonicalized safely".to_owned()))?;
    Ok(InputItem::FunctionCall(InputFunctionToolCall {
        async_execution: false,
        agent,
        id: Some(item_id.to_owned()),
        call_id,
        name: TOOL_SEARCH_NAME.to_owned(),
        namespace: None,
        arguments: canonical_arguments,
        status: Some(MessageStatus::Completed),
    }))
}

/// The model-facing text of a hosted search that loaded nothing. It says why, so a model does
/// not keep repeating a filter that matches nothing or a path it has already loaded.
const HOSTED_EMPTY_SEARCH_OUTPUT: &str = r#"{"tools":[],"note":"No tools were loaded. `query` keeps only tools whose name or description contains it, and tools loaded earlier in this conversation are not returned again. Search the path again without `query`, or call a tool that is already loaded."}"#;

/// The model-facing text of a hosted search output, live or rebuilt from history.
pub(crate) fn hosted_search_output_text(tools: &[ResponsesTool]) -> Result<String, ToolError> {
    if tools.is_empty() {
        return Ok(HOSTED_EMPTY_SEARCH_OUTPUT.to_owned());
    }
    canonical_search_output(tools)
}

/// The model-facing text of a search output: functions with their schemas,
/// namespaces by name and description (members are loaded as tools).
pub(crate) fn canonical_search_output(tools: &[ResponsesTool]) -> Result<String, ToolError> {
    let projected_tools = model_visible_output_tools(tools)?;
    let canonical_value = serialize_to_value(&CanonicalToolSearchOutput {
        tools: &projected_tools,
    })
    .map_err(|_| ToolError::Config("tool_search_output could not be canonicalized safely".to_owned()))?;
    serialize_to_string(&canonical_value)
        .map_err(|_| ToolError::Config("tool_search_output could not be canonicalized safely".to_owned()))
}

fn prepare_search_call(call: &InputToolSearchCall, pairing: &mut SearchPairing) -> Result<InputItem, ToolError> {
    if call.id.trim().is_empty() {
        return Err(ToolError::Config("tool_search_call id must not be blank".to_owned()));
    }
    if !pairing.item_ids.insert(call.id.clone()) {
        return Err(ToolError::Config("duplicate tool_search_call item id".to_owned()));
    }
    let call_id = match (call.execution, call.call_id.as_deref()) {
        (ToolSearchExecution::Client, Some(call_id)) if !call_id.trim().is_empty() => call_id.to_owned(),
        (ToolSearchExecution::Server, None) => hosted_private_call_id(&call.id),
        (ToolSearchExecution::Client, _) => {
            return Err(ToolError::Config(
                "tool_search_call call_id must not be blank".to_owned(),
            ));
        }
        (ToolSearchExecution::Server, Some(_)) => {
            return Err(ToolError::Config(
                "a hosted tool_search_call must have a null call_id".to_owned(),
            ));
        }
    };
    if pairing.completed_call_ids.contains(&call_id) || !pairing.unresolved_calls.insert(call_id.clone()) {
        return Err(ToolError::Config("duplicate tool_search_call call_id".to_owned()));
    }
    if call.execution == ToolSearchExecution::Server {
        pairing.unresolved_calls.remove(&call_id);
        pairing.unresolved_hosted_calls.push_back(call_id.clone());
    }
    private_search_call(call.agent.clone(), &call.id, call_id, &call.arguments)
}

fn prepare_search_output(
    output: &ToolSearchOutputMessage,
    pairing: &mut SearchPairing,
    definition_accumulator: &mut DefinitionAccumulator<'_>,
) -> Result<InputItem, ToolError> {
    let call_id = match (output.execution, output.call_id.as_deref()) {
        (ToolSearchExecution::Client, Some(call_id)) if !call_id.trim().is_empty() => {
            if pairing.completed_call_ids.contains(call_id) {
                return Err(ToolError::Config("duplicate tool_search_output call_id".to_owned()));
            }
            if !pairing.unresolved_calls.remove(call_id) {
                return Err(ToolError::Config(
                    "tool_search_output has no matching unresolved call_id".to_owned(),
                ));
            }
            call_id.to_owned()
        }
        (ToolSearchExecution::Server, None) => pairing.unresolved_hosted_calls.pop_front().ok_or_else(|| {
            ToolError::Config("hosted tool_search_output has no preceding hosted tool_search_call".to_owned())
        })?,
        (ToolSearchExecution::Client, _) => {
            return Err(ToolError::Config(
                "tool_search_output call_id must not be blank".to_owned(),
            ));
        }
        (ToolSearchExecution::Server, Some(_)) => {
            return Err(ToolError::Config(
                "a hosted tool_search_output must have a null call_id".to_owned(),
            ));
        }
    };
    if output.status != ToolSearchStatus::Completed {
        return Err(ToolError::Config(
            "tool_search_output must be completed before it may load tool definitions".to_owned(),
        ));
    }
    for tool in &output.tools {
        load_definition(tool, definition_accumulator)?;
    }
    let canonical_output = match output.execution {
        ToolSearchExecution::Server => hosted_search_output_text(&output.tools)?,
        ToolSearchExecution::Client => canonical_search_output(&output.tools)?,
    };
    pairing.completed_call_ids.insert(call_id.clone());
    Ok(InputItem::FunctionCallOutput(FunctionToolResultMessage {
        call_id,
        output: ToolCallOutput::Text(canonical_output),
    }))
}

fn model_visible_output_tools(tools: &[ResponsesTool]) -> Result<Vec<ModelVisibleLoadedTool<'_>>, ToolError> {
    tools
        .iter()
        .map(|tool| match tool {
            ResponsesTool::Function(definition) => Ok(ModelVisibleLoadedTool::Function(ModelVisibleFunction {
                type_: "function",
                definition,
            })),
            ResponsesTool::Namespace(namespace) => Ok(ModelVisibleLoadedTool::Namespace(ModelVisibleNamespace {
                type_: "namespace",
                name: &namespace.name,
                description: namespace.description.as_deref(),
            })),
            ResponsesTool::ToolSearch(_)
            | ResponsesTool::Mcp(_)
            | ResponsesTool::WebSearch(_)
            | ResponsesTool::FileSearch(_)
            | ResponsesTool::CodeInterpreter(_)
            | ResponsesTool::Shell(_)
            | ResponsesTool::Custom(_)
            | ResponsesTool::Unknown => Err(ToolError::Config(
                "tool_search_output contains an unsupported model-output definition".to_owned(),
            )),
        })
        .collect()
}
