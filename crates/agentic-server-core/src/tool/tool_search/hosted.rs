//! Hosted (`execution: "server"`) tool search: a pure lookup over the
//! response's catalog of deferred tools.
//!
//! The behavior mirrors the recorded Responses API reference
//! (`tests/hosted_tool_search_characterization_test.rs`): each requested path
//! names a deferred function or namespace (an MCP server is the namespace
//! `mcp_<server_label>`), and an optional `query` keeps only the functions whose
//! name or description contains the whole query, ignoring case.

use std::collections::{HashMap, HashSet};
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Map, Value};

use super::{TOOL_SEARCH_NAME, ToolSearchHandler, hosted_search_output_text, private_search_call, public_item_id};
use crate::tool::handler::{GatewayExecutor, GatewayToolEventPlan, ToolError, ToolHandler, ToolOutput};
use crate::tool::registry::ToolType;
use crate::types::io::output::{FunctionToolCall, GatewayCallStatus};
use crate::types::io::{FunctionTool, FunctionToolResultMessage, InputItem, OutputItem, ToolSearchCall};
use crate::types::io::{InputFunctionToolCall, InputToolSearchCall, ToolCallOutput, ToolSearchOutputMessage};
use crate::types::tools::{
    CodexNamespaceMember, CodexNamespaceToolParam, FunctionToolParam, ResponsesTool, ToolSearchExecution,
    ToolSearchStatus, ToolSearchToolParam,
};
use crate::utils::common::deserialize_from_str;

/// Arguments the model sends to the hosted `tool_search` function.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub(crate) struct HostedSearchArguments {
    pub(crate) paths: Vec<String>,
    #[serde(default)]
    pub(crate) query: Option<String>,
}

impl HostedSearchArguments {
    /// Parses the model's JSON arguments. Invalid arguments load nothing.
    pub(crate) fn parse(arguments: &str) -> Option<Self> {
        deserialize_from_str(arguments).ok()
    }
}

/// One searchable surface, with only the members a search may still load.
#[derive(Debug, Clone)]
pub(crate) enum HostedCatalogEntry {
    Function(FunctionToolParam),
    Namespace(CodexNamespaceToolParam),
}

impl HostedCatalogEntry {
    fn name(&self) -> &str {
        match self {
            Self::Function(function) => function.name.as_str(),
            Self::Namespace(namespace) => &namespace.name,
        }
    }
}

/// The deferred tools a hosted search can load during one response.
///
/// Tools loaded by earlier responses are left out when the catalog is built,
/// so a repeated search in a later response returns only what is new, while a
/// repeated search within this response returns the same tools again.
#[derive(Debug, Clone, Default)]
pub(crate) struct HostedCatalog {
    entries: Vec<HostedCatalogEntry>,
    indexes: HashMap<String, usize>,
}

impl HostedCatalog {
    pub(crate) fn new(entries: Vec<HostedCatalogEntry>) -> Self {
        let mut indexes = HashMap::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            indexes.entry(entry.name().to_owned()).or_insert(index);
        }
        Self { entries, indexes }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The public tools one search loads, in path order. Unknown and repeated
    /// paths load nothing; a namespace with no matching member is dropped.
    pub(crate) fn search(&self, arguments: &HostedSearchArguments) -> Vec<ResponsesTool> {
        let query = arguments
            .query
            .as_deref()
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase);
        let mut seen = HashSet::with_capacity(arguments.paths.len());
        arguments
            .paths
            .iter()
            .filter(|path| seen.insert(path.as_str()))
            .filter_map(|path| self.indexes.get(path).map(|index| &self.entries[*index]))
            .filter_map(|entry| match entry {
                HostedCatalogEntry::Function(function) => {
                    matches(query.as_deref(), function).then(|| ResponsesTool::Function(function.clone()))
                }
                HostedCatalogEntry::Namespace(namespace) => {
                    let tools: Vec<_> = namespace
                        .tools
                        .iter()
                        .filter(|member| match member {
                            CodexNamespaceMember::Function(function) => matches(query.as_deref(), function),
                            CodexNamespaceMember::Unknown => false,
                        })
                        .cloned()
                        .collect();
                    (!tools.is_empty()).then(|| {
                        ResponsesTool::Namespace(CodexNamespaceToolParam {
                            tools,
                            ..namespace.clone()
                        })
                    })
                }
            })
            .collect()
    }
}

impl HostedCatalog {
    /// Runs one search from the model's raw JSON arguments.
    pub(crate) fn search_raw(&self, arguments: &str) -> Vec<ResponsesTool> {
        HostedSearchArguments::parse(arguments).map_or_else(Vec::new, |arguments| self.search(&arguments))
    }
}

/// Runs hosted searches for one response. The search is a pure lookup, so the
/// public `tool_search_output` is already known when the call is planned.
#[derive(Debug, Default)]
pub(crate) struct HostedToolSearchExecutor;

impl ToolHandler for HostedToolSearchExecutor {
    type ToolParams = ToolSearchToolParam;

    fn tool_type(&self) -> ToolType {
        ToolType::HostedToolSearch
    }

    fn validate(&self, params: &ToolSearchToolParam) -> Result<(), ToolError> {
        ToolSearchHandler.validate(params)
    }

    fn normalize(&self, params: &ToolSearchToolParam) -> Vec<FunctionTool> {
        ToolSearchHandler.normalize(params)
    }
}

impl GatewayExecutor for HostedToolSearchExecutor {
    type ExecutionParams = Arc<HostedCatalog>;

    fn execute(
        &self,
        call_id: &str,
        _tool_name: &str,
        arguments: &str,
        params: &Self::ExecutionParams,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        let output = model_output(&params.search_raw(arguments));
        Box::pin(ready(Ok(ToolOutput::success(call_id, output))))
    }

    fn supports_parallel_execution(&self) -> bool {
        true
    }

    fn plan_gateway_events(&self, call: &FunctionToolCall, params: &Self::ExecutionParams) -> GatewayToolEventPlan {
        GatewayToolEventPlan::new(Some(OutputItem::ToolSearchCall(public_call(
            call,
            ToolSearchStatus::InProgress,
        ))))
        .with_follow_up(OutputItem::ToolSearchOutput(public_output_item(
            call,
            params.search_raw(&call.arguments),
        )))
    }

    fn public_output(
        &self,
        call: &FunctionToolCall,
        _output: &ToolOutput,
        _status: GatewayCallStatus,
        _params: &Self::ExecutionParams,
    ) -> Option<OutputItem> {
        Some(OutputItem::ToolSearchCall(public_call(
            call,
            ToolSearchStatus::Completed,
        )))
    }

    /// The model sees the same call and output a continuation rebuilds from
    /// the public pair, so later prompts share this response's prefix.
    fn model_input(
        &self,
        call: &FunctionToolCall,
        output: ToolOutput,
        follow_up: Option<&OutputItem>,
    ) -> Vec<InputItem> {
        let public = public_call(call, ToolSearchStatus::Completed);
        let call_id = super::hosted_private_call_id(&public.id);
        let tools = match follow_up {
            Some(OutputItem::ToolSearchOutput(item)) => model_output(&item.tools),
            _ => output.output,
        };
        let call_item = private_search_call(call.agent.clone(), &public.id, call_id.clone(), &public.arguments)
            .unwrap_or_else(|_| {
                InputItem::FunctionCall(InputFunctionToolCall {
                    async_execution: false,
                    agent: call.agent.clone(),
                    id: Some(public.id.clone()),
                    call_id: call_id.clone(),
                    name: TOOL_SEARCH_NAME.to_owned(),
                    namespace: None,
                    arguments: call.arguments.clone(),
                    status: None,
                })
            });
        vec![
            call_item,
            InputItem::FunctionCallOutput(FunctionToolResultMessage {
                call_id,
                output: ToolCallOutput::Text(tools),
            }),
        ]
    }

    /// History keeps the public pair; continuation rebuilds the model's view from it.
    fn history_input(&self, call: &FunctionToolCall, follow_up: Option<&OutputItem>) -> Option<Vec<InputItem>> {
        let public = public_call(call, ToolSearchStatus::Completed);
        let Some(OutputItem::ToolSearchOutput(output)) = follow_up else {
            return None;
        };
        let call = InputToolSearchCall::try_from(&public).ok()?;
        Some(vec![
            InputItem::ToolSearchCall(call),
            InputItem::ToolSearchOutput(output.clone()),
        ])
    }
}

/// Model-facing text of a hosted output. The projection serializes known
/// types; an empty result stands in if that ever fails.
fn model_output(tools: &[ResponsesTool]) -> String {
    hosted_search_output_text(tools).unwrap_or_else(|_| r#"{"tools":[]}"#.to_owned())
}

fn public_call(call: &FunctionToolCall, status: ToolSearchStatus) -> ToolSearchCall {
    let arguments = match status {
        ToolSearchStatus::Completed => {
            deserialize_from_str::<Value>(&call.arguments).unwrap_or_else(|_| Value::Object(Map::new()))
        }
        // Arguments arrive whole on `output_item.done`, as the Responses API streams them.
        ToolSearchStatus::InProgress | ToolSearchStatus::Incomplete => Value::Object(Map::new()),
    };
    ToolSearchCall {
        agent: call.agent.clone(),
        id: public_item_id(&call.id),
        call_id: None,
        execution: ToolSearchExecution::Server,
        arguments,
        status,
    }
}

fn public_output_item(call: &FunctionToolCall, tools: Vec<ResponsesTool>) -> ToolSearchOutputMessage {
    let call_item_id = public_item_id(&call.id);
    ToolSearchOutputMessage {
        agent: call.agent.clone(),
        id: Some(format!("tso_{}", call_item_id.trim_start_matches("tsc_"))),
        call_id: None,
        execution: ToolSearchExecution::Server,
        status: ToolSearchStatus::Completed,
        tools,
    }
}

/// Whether `function` matches a lowercased query: the whole query must appear
/// in its name or description. No query matches everything.
fn matches(query: Option<&str>, function: &FunctionToolParam) -> bool {
    let Some(query) = query else {
        return true;
    };
    function.name.as_str().to_lowercase().contains(query)
        || function
            .description
            .as_deref()
            .is_some_and(|description| description.to_lowercase().contains(query))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn function(name: &str, description: &str) -> FunctionToolParam {
        serde_json::from_value(json!({
            "name": name,
            "description": description,
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
            "strict": true,
            "defer_loading": true
        }))
        .unwrap()
    }

    fn namespace(name: &str, description: &str, members: &[(&str, &str)]) -> CodexNamespaceToolParam {
        CodexNamespaceToolParam {
            name: name.to_owned(),
            description: Some(description.to_owned()),
            tools: members
                .iter()
                .map(|(name, description)| CodexNamespaceMember::Function(function(name, description)))
                .collect(),
            unsupported_async: None,
            extra: HashMap::new(),
        }
    }

    /// The catalog recorded in `tests/cassettes/hosted_tool_search/inputs/tools-query.json`.
    fn recorded_catalog() -> HostedCatalog {
        HostedCatalog::new(vec![
            HostedCatalogEntry::Function(function("get_weather", "Get the current weather for a city")),
            HostedCatalogEntry::Function(function(
                "get_exchange_rate",
                "Get the exchange rate between two currencies",
            )),
            HostedCatalogEntry::Namespace(namespace(
                "travel",
                "Travel location tools",
                &[
                    ("get_timezone", "Get the IANA time zone for a city"),
                    ("get_coordinates", "Get latitude and longitude for a city"),
                    ("calculate_distance", "Calculate the distance between two cities"),
                    ("book_flight", "Reserve an airline seat between two airports"),
                ],
            )),
            HostedCatalogEntry::Namespace(namespace(
                "mcp_gitmcp_tiktoken",
                "Documentation and code search for the openai/tiktoken tokenizer repository.",
                &[
                    (
                        "fetch_tiktoken_documentation",
                        "Fetch entire documentation file from GitHub repository: openai/tiktoken. Useful for general questions. Always call this tool first if asked about openai/tiktoken.",
                    ),
                    (
                        "search_tiktoken_documentation",
                        "Semantically search within the fetched documentation from GitHub repository: openai/tiktoken. Useful for specific queries.",
                    ),
                    (
                        "search_tiktoken_code",
                        "Search for code within the GitHub repository: \"openai/tiktoken\" using the GitHub Search API (exact match). Returns matching files for you to query further if relevant.",
                    ),
                ],
            )),
        ])
    }

    /// `(name, member names)` for each loaded tool.
    fn loaded(arguments: Value) -> Vec<(String, Vec<String>)> {
        let arguments: HostedSearchArguments = serde_json::from_value(arguments).unwrap();
        recorded_catalog()
            .search(&arguments)
            .into_iter()
            .map(|tool| match tool {
                ResponsesTool::Function(function) => (function.name.as_str().to_owned(), Vec::new()),
                ResponsesTool::Namespace(namespace) => (
                    namespace.name,
                    namespace
                        .tools
                        .into_iter()
                        .filter_map(|member| match member {
                            CodexNamespaceMember::Function(function) => Some(function.name.as_str().to_owned()),
                            CodexNamespaceMember::Unknown => None,
                        })
                        .collect(),
                ),
                _ => panic!("hosted search loads only functions and namespaces"),
            })
            .collect()
    }

    fn travel(members: &[&str]) -> Vec<(String, Vec<String>)> {
        vec![(
            "travel".to_owned(),
            members.iter().map(|member| (*member).to_owned()).collect(),
        )]
    }

    #[test]
    fn query_matches_the_recorded_reference_cases() {
        let all = ["get_timezone", "get_coordinates", "calculate_distance", "book_flight"];
        let weather = vec![("get_weather".to_owned(), Vec::new())];
        let cases = [
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
            (json!({"paths": ["travel"], "query": "airplane ticket"}), Vec::new()),
            (json!({"paths": ["travel"], "query": "timezone distance"}), Vec::new()),
            (
                json!({"paths": ["travel"], "query": "GET_TIMEZONE"}),
                travel(&["get_timezone"]),
            ),
            (
                json!({"paths": ["travel"], "query": "chocolate cake recipe"}),
                Vec::new(),
            ),
            (json!({"paths": ["travel"], "query": ""}), travel(&all)),
            (json!({"paths": ["get_weather"], "query": "weather"}), weather.clone()),
            (json!({"paths": ["get_weather"], "query": "CURRENT"}), weather.clone()),
            (json!({"paths": ["get_weather"], "query": "timezone"}), Vec::new()),
            (
                json!({"paths": ["get_weather"], "query": "get_exchange_rate"}),
                Vec::new(),
            ),
            (
                json!({"paths": ["mcp_gitmcp_tiktoken"], "query": "code"}),
                vec![(
                    "mcp_gitmcp_tiktoken".to_owned(),
                    vec!["search_tiktoken_code".to_owned()],
                )],
            ),
            (
                json!({"paths": ["travel"], "query": "coord"}),
                travel(&["get_coordinates"]),
            ),
            (
                json!({"paths": ["travel"], "query": "city"}),
                travel(&["get_timezone", "get_coordinates"]),
            ),
            (json!({"paths": ["travel"], "query": "latitude city"}), Vec::new()),
            (json!({"paths": ["travel"], "query": "get timezone"}), Vec::new()),
            (
                json!({"paths": ["travel"], "query": "time zone"}),
                travel(&["get_timezone"]),
            ),
            (json!({"paths": ["travel"], "query": "destination_airport"}), Vec::new()),
            (json!({"paths": ["no_such_tool"]}), Vec::new()),
            (json!({"paths": [], "query": "timezone"}), Vec::new()),
            (
                json!({"paths": ["get_weather", "travel"], "query": "get_timezone"}),
                travel(&["get_timezone"]),
            ),
            (
                json!({"paths": ["get_weather", "travel"]}),
                [weather.clone(), travel(&all)].concat(),
            ),
        ];
        for (arguments, expected) in cases {
            assert_eq!(loaded(arguments.clone()), expected, "{arguments}");
        }
    }

    #[test]
    fn repeated_paths_load_once_and_invalid_arguments_load_nothing() {
        assert_eq!(
            loaded(json!({"paths": ["travel", "travel"], "query": "timezone"})),
            travel(&["get_timezone"])
        );
        for arguments in [r#"{"query":"timezone"}"#, r#"{"paths":"travel"}"#, "not json", "[]"] {
            assert_eq!(HostedSearchArguments::parse(arguments), None, "{arguments}");
        }
        assert!(HostedCatalog::default().is_empty());
    }
}
