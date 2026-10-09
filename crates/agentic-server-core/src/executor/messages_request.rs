//! Request preparation shared by the native Anthropic Messages tool loops.
//!
//! The gateway executes two native Anthropic server tools: `web_search_20250305`
//! and `web_fetch_20250910`. This adapter judges what is Anthropic-specific
//! about a declaration (tool version, `citations`, cache settings,
//! `allowed_callers`, `max_uses`), reads the shared parameters into the tool's
//! typed form, and hands validation of those parameters and the upstream
//! function schema to the tool's own `ToolHandler`. The result is written back
//! as the ordinary function-tool shape vLLM accepts, and `max_uses` becomes
//! the request-wide budget the loops enforce before dispatching a call.

use serde_json::{Value, json};

use crate::executor::{ExecutorError, ExecutorResult};
use crate::tool::ToolHandler;
use crate::tool::declaration::web_search_config;
use crate::tool::domain_policy::EXCLUSIVE_LISTS_RULE;
use crate::tool::web_fetch::{self, WebFetchErrorCode, WebFetchHandler};
use crate::tool::web_search::WebSearchHandler;
use crate::types::io::FunctionTool;
use crate::types::messages::GatewayToolResult;
use crate::types::messages::request::ToolParam;
use crate::types::messages::tool_seam::{
    NATIVE_WEB_FETCH_TYPE, NATIVE_WEB_SEARCH_TYPE, WEB_FETCH_EXECUTOR, WEB_SEARCH_EXECUTOR, is_native_web_fetch_type,
    tool_result_block,
};
use crate::types::tools::WebFetchToolParam;

/// One request-wide `max_uses` budget, counted in the tool's unit of use.
#[derive(Debug, Default)]
pub(super) struct UseBudget {
    remaining: Option<usize>,
}

impl UseBudget {
    const fn limited(max_uses: Option<usize>) -> Self {
        Self { remaining: max_uses }
    }

    /// Admit one call that would perform `uses` uses. The call runs only when
    /// the remaining budget covers all of them; a refused call leaves the
    /// budget untouched, so a later call that fits may still run.
    pub(super) fn admit(&mut self, uses: usize) -> bool {
        match &mut self.remaining {
            Some(remaining) if uses > *remaining => false,
            Some(remaining) => {
                *remaining -= uses;
                true
            }
            None => true,
        }
    }
}

/// The native server-tool budgets of one request: searches for `web_search`,
/// fetches for `web_fetch`.
#[derive(Debug, Default)]
pub(super) struct ServerToolBudgets {
    pub(super) searches: UseBudget,
    pub(super) fetches: UseBudget,
}

fn invalid(message: impl Into<String>) -> ExecutorError {
    ExecutorError::InvalidRequest(message.into())
}

fn validate_domain_list(tool: &Value, tool_name: &str, field: &str) -> ExecutorResult<()> {
    let Some(value) = tool.get(field) else {
        return Ok(());
    };
    let valid = value.as_array().is_some_and(|domains| {
        domains.iter().all(|domain| {
            domain.as_str().is_some_and(|domain| {
                let domain = domain.trim();
                !domain.is_empty()
                    && !domain.contains("://")
                    && !domain.starts_with('/')
                    && !domain.chars().any(char::is_whitespace)
            })
        })
    });
    if valid {
        Ok(())
    } else {
        Err(invalid(format!(
            "{tool_name} {field} must be an array of non-empty strings"
        )))
    }
}

/// `allowed_domains` and `blocked_domains` must each be well formed and are
/// mutually exclusive, as Anthropic documents for every server tool. What an
/// entry must look like to match anything is the tool layer's rule
/// (`domain_policy`), applied where the handler validates its declaration.
fn validate_domain_list_shape(tool: &Value, tool_name: &str) -> ExecutorResult<()> {
    validate_domain_list(tool, tool_name, "allowed_domains")?;
    validate_domain_list(tool, tool_name, "blocked_domains")?;
    let has_entries = |field: &str| {
        tool.get(field)
            .and_then(Value::as_array)
            .is_some_and(|domains| !domains.is_empty())
    };
    if has_entries("allowed_domains") && has_entries("blocked_domains") {
        return Err(invalid(format!("{tool_name} {EXCLUSIVE_LISTS_RULE}")));
    }
    Ok(())
}

fn validate_allowed_callers(tool: &Value, tool_name: &str) -> ExecutorResult<()> {
    let Some(value) = tool.get("allowed_callers") else {
        return Ok(());
    };
    let Some(callers) = value.as_array() else {
        return Err(invalid(format!(
            "{tool_name} allowed_callers must be an array of strings"
        )));
    };
    if !callers.iter().all(Value::is_string) {
        return Err(invalid(format!(
            "{tool_name} allowed_callers must be an array of strings"
        )));
    }
    if !callers.iter().any(|caller| caller.as_str() == Some("direct")) {
        return Err(invalid(format!(
            "{tool_name} allowed_callers must permit direct invocation"
        )));
    }
    Ok(())
}

fn validate_user_location(tool: &Value) -> ExecutorResult<()> {
    let Some(value) = tool.get("user_location") else {
        return Ok(());
    };
    let Some(location) = value.as_object() else {
        return Err(invalid("web_search user_location must be an object"));
    };
    if location.get("type").and_then(Value::as_str) != Some("approximate") {
        return Err(invalid("web_search user_location.type must be approximate"));
    }
    let fields = ["city", "region", "country", "timezone"];
    let mut has_location = false;
    for field in fields {
        if let Some(value) = location.get(field) {
            let valid = value.as_str().is_some_and(|value| !value.trim().is_empty());
            if !valid {
                return Err(invalid(format!(
                    "web_search user_location.{field} must be a non-empty string"
                )));
            }
            has_location = true;
        }
    }
    if !has_location {
        return Err(invalid(
            "web_search user_location must include city, region, country, or timezone",
        ));
    }
    Ok(())
}

/// A positive integer field that must fit the gateway's counters.
fn positive_integer(tool: &Value, tool_name: &str, field: &str) -> ExecutorResult<Option<usize>> {
    let Some(value) = tool.get(field) else {
        return Ok(None);
    };
    value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .map(Some)
        .ok_or_else(|| invalid(format!("{tool_name} {field} must be a positive integer")))
}

/// Fold one declaration's `max_uses` into the request-wide minimum.
fn fold_max_uses(current: Option<usize>, tool: &Value, tool_name: &str) -> ExecutorResult<Option<usize>> {
    let Some(parsed) = positive_integer(tool, tool_name, "max_uses")? else {
        return Ok(current);
    };
    Ok(Some(current.map_or(parsed, |current| current.min(parsed))))
}

/// The handler's model-visible function tool for a validated declaration.
fn handler_function_tool<H: ToolHandler>(
    handler: &H,
    tool_name: &str,
    params: &H::ToolParams,
) -> ExecutorResult<FunctionTool> {
    handler.validate(params).map_err(|error| invalid(error.to_string()))?;
    handler
        .normalize(params)
        .into_iter()
        .next()
        .ok_or_else(|| invalid(format!("{tool_name} declaration produced no function tool")))
}

/// A native `web_search_20250305` declaration, if `tool` is one: the Anthropic
/// settings are judged here, the shared parameters go through
/// [`WebSearchHandler`], and `max_uses` folds into the request-wide budget.
fn normalize_web_search_declaration(
    tool: &Value,
    max_uses: &mut Option<usize>,
) -> ExecutorResult<Option<FunctionTool>> {
    let tool_type = tool.get("type").and_then(Value::as_str);
    let is_web_search = tool.get("name").and_then(Value::as_str) == Some(WEB_SEARCH_EXECUTOR);
    if is_web_search
        && tool_type
            .is_some_and(|tool_type| tool_type.starts_with("web_search_") && tool_type != NATIVE_WEB_SEARCH_TYPE)
    {
        return Err(invalid(format!(
            "unsupported web_search tool type '{}'",
            tool_type.unwrap_or_default()
        )));
    }
    if tool_type != Some(NATIVE_WEB_SEARCH_TYPE) || !is_web_search {
        return Ok(None);
    }
    validate_domain_list_shape(tool, WEB_SEARCH_EXECUTOR)?;
    validate_user_location(tool)?;
    validate_allowed_callers(tool, WEB_SEARCH_EXECUTOR)?;
    *max_uses = fold_max_uses(*max_uses, tool, WEB_SEARCH_EXECUTOR)?;
    let declaration: ToolParam = serde_json::from_value(tool.clone())
        .map_err(|error| invalid(format!("web_search declaration is not a tool: {error}")))?;
    let params = web_search_config(&declaration);
    handler_function_tool(&WebSearchHandler::spec_only(), WEB_SEARCH_EXECUTOR, &params).map(Some)
}

/// A native `web_fetch_*` declaration, if `tool` is one. Only
/// `web_fetch_20250910` is honoured; settings the gateway cannot honour are
/// rejected rather than ignored. The shared parameters are read once into
/// [`WebFetchToolParam`] and validated by [`WebFetchHandler`].
fn normalize_web_fetch_declaration(tool: &Value, max_uses: &mut Option<usize>) -> ExecutorResult<Option<FunctionTool>> {
    let tool_type = tool.get("type").and_then(Value::as_str);
    if !is_native_web_fetch_type(tool_type) {
        return Ok(None);
    }
    if tool_type != Some(NATIVE_WEB_FETCH_TYPE) {
        return Err(invalid(format!(
            "unsupported web_fetch tool type '{}'; only {NATIVE_WEB_FETCH_TYPE} is supported",
            tool_type.unwrap_or_default()
        )));
    }
    if tool.get("name").and_then(Value::as_str) != Some(WEB_FETCH_EXECUTOR) {
        return Err(invalid(format!(
            "{NATIVE_WEB_FETCH_TYPE} declarations must be named {WEB_FETCH_EXECUTOR}"
        )));
    }
    validate_domain_list_shape(tool, WEB_FETCH_EXECUTOR)?;
    validate_allowed_callers(tool, WEB_FETCH_EXECUTOR)?;
    if let Some(citations) = tool.get("citations") {
        match citations.get("enabled").and_then(Value::as_bool) {
            Some(false) => {}
            Some(true) => return Err(invalid("web_fetch citations are not supported")),
            None => {
                return Err(invalid(
                    "web_fetch citations must be an object with a boolean enabled field",
                ));
            }
        }
    }
    for unsupported in ["use_cache", "response_inclusion"] {
        if tool.get(unsupported).is_some() {
            return Err(invalid(format!(
                "web_fetch {unsupported} requires a later web_fetch tool version"
            )));
        }
    }
    *max_uses = fold_max_uses(*max_uses, tool, WEB_FETCH_EXECUTOR)?;
    let params = WebFetchToolParam::from_declaration(|field| tool.get(field))
        .map_err(|reason| invalid(format!("web_fetch {reason}")))?;
    handler_function_tool(&WebFetchHandler::spec_only(), WEB_FETCH_EXECUTOR, &params).map(Some)
}

fn declares_native_web_search(request: &Value) -> bool {
    request.get("tools").and_then(Value::as_array).is_some_and(|tools| {
        tools.iter().any(|tool| {
            tool.get("type").and_then(Value::as_str) == Some(NATIVE_WEB_SEARCH_TYPE)
                && tool.get("name").and_then(Value::as_str) == Some(WEB_SEARCH_EXECUTOR)
        })
    })
}

/// Whether the request declares a native web-fetch server tool of any version.
#[must_use]
pub fn declares_native_web_fetch(request: &Value) -> bool {
    request.get("tools").and_then(Value::as_array).is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| is_native_web_fetch_type(tool.get("type").and_then(Value::as_str)))
    })
}

/// Normalize the native server-tool declarations for an upstream endpoint that
/// validates ordinary function-tool schemas, returning whether the body changed.
///
/// # Errors
/// Returns [`ExecutorError::InvalidRequest`] for unsupported or invalid native
/// declarations.
pub fn normalize_native_server_tools_for_upstream(request: &mut Value) -> ExecutorResult<bool> {
    let had_native = declares_native_web_search(request) || declares_native_web_fetch(request);
    normalize_native_server_tools(request).map(|_| had_native)
}

/// Kept for callers that predate `web_fetch`; behaves exactly like
/// [`normalize_native_server_tools_for_upstream`].
///
/// # Errors
/// Returns [`ExecutorError::InvalidRequest`] for unsupported or invalid native
/// declarations.
pub fn normalize_native_web_search_for_upstream(request: &mut Value) -> ExecutorResult<bool> {
    normalize_native_server_tools_for_upstream(request)
}

pub(super) fn web_search_budget_exhausted_result(tool_use_id: &str) -> GatewayToolResult {
    tool_result_block(
        tool_use_id,
        "web_search max_uses exceeded; search was not run".to_owned(),
        true,
    )
}

/// The error `tool_result` for a `web_fetch` call refused before dispatch, in
/// the documented `web_fetch_tool_result_error` shape.
pub(super) fn web_fetch_refused_result(tool_use_id: &str, code: WebFetchErrorCode, message: &str) -> GatewayToolResult {
    tool_result_block(tool_use_id, web_fetch::failure_output(code, message), true)
}

pub(super) fn web_fetch_budget_exhausted_result(tool_use_id: &str) -> GatewayToolResult {
    web_fetch_refused_result(
        tool_use_id,
        WebFetchErrorCode::MaxUsesExceeded,
        "web_fetch max_uses exceeded; page was not fetched",
    )
}

/// The Anthropic function-tool declaration vLLM accepts for a gateway tool.
fn anthropic_function_tool(function: &FunctionTool) -> Value {
    json!({
        "name": function.name,
        "description": function.description,
        "input_schema": function.parameters,
    })
}

/// Translate Claude's native server-tool declarations into the ordinary
/// Anthropic function-tool shape accepted by vLLM. The gateway executes the
/// resulting `tool_use` internally, so this is an upstream-only representation
/// change; the client request itself remains Anthropic-native.
pub(super) fn normalize_native_server_tools(request: &mut Value) -> ExecutorResult<ServerToolBudgets> {
    let mut searches = None;
    let mut fetches = None;
    if let Some(tools) = request.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            let function = match normalize_web_search_declaration(tool, &mut searches)? {
                Some(function) => Some(function),
                None => normalize_web_fetch_declaration(tool, &mut fetches)?,
            };
            if let Some(function) = function {
                *tool = anthropic_function_tool(&function);
            }
        }
    }
    Ok(ServerToolBudgets {
        searches: UseBudget::limited(searches),
        fetches: UseBudget::limited(fetches),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tools::WebSearchToolParam;

    fn fetch_request(extra: &Value) -> Value {
        let mut tool = json!({"type": "web_fetch_20250910", "name": "web_fetch"});
        for (key, value) in extra.as_object().expect("object") {
            tool[key] = value.clone();
        }
        json!({"model": "m", "max_tokens": 8, "messages": [], "tools": [tool]})
    }

    fn invalid_request(request: &mut Value) -> String {
        match normalize_native_server_tools(request) {
            Err(ExecutorError::InvalidRequest(message)) => message,
            other => panic!("expected an invalid request, got {other:?}"),
        }
    }

    #[test]
    fn native_web_fetch_is_rewritten_into_the_function_tool_with_its_budget() {
        let mut request = fetch_request(&json!({"max_uses": 3, "allowed_domains": ["example.com"],
            "max_content_tokens": 5000, "citations": {"enabled": false}, "allowed_callers": ["direct"]}));
        assert!(declares_native_web_fetch(&request));
        let mut budgets = normalize_native_server_tools(&mut request).unwrap();
        assert_eq!(request["tools"][0]["name"], "web_fetch");
        assert!(request["tools"][0].get("type").is_none());
        assert_eq!(request["tools"][0]["input_schema"]["required"], json!(["url"]));
        assert_eq!(
            request["tools"][0],
            anthropic_function_tool(&WebFetchHandler::spec_only().normalize(&WebFetchToolParam::default())[0]),
            "the upstream declaration is the handler's own normalization"
        );
        assert!(budgets.fetches.admit(3));
        assert!(!budgets.fetches.admit(1), "max_uses caps the request-wide total");
        assert!(
            budgets.searches.admit(50),
            "no web_search declaration leaves searches unlimited"
        );
    }

    #[test]
    fn native_web_search_is_rewritten_through_its_handler() {
        let mut request = json!({"model": "m", "max_tokens": 8, "messages": [], "tools": [
            {"type": "web_search_20250305", "name": "web_search", "max_uses": 2, "allowed_domains": ["example.com"]}
        ]});
        normalize_native_server_tools(&mut request).unwrap();
        let expected =
            anthropic_function_tool(&WebSearchHandler::spec_only().normalize(&WebSearchToolParam::default())[0]);
        assert_eq!(request["tools"][0], expected);
    }

    #[test]
    fn upstream_normalization_reports_whether_it_changed_the_body() {
        let mut request = fetch_request(&json!({}));
        assert!(normalize_native_server_tools_for_upstream(&mut request).unwrap());
        assert!(!normalize_native_server_tools_for_upstream(&mut request).unwrap());
        let mut plain = json!({"model": "m", "max_tokens": 8, "messages": [],
            "tools": [{"name": "web_fetch", "input_schema": {"type": "object"}}]});
        assert!(!declares_native_web_fetch(&plain));
        assert!(!normalize_native_server_tools_for_upstream(&mut plain).unwrap());
        assert!(plain["tools"][0].get("type").is_none());
    }

    #[test]
    fn unsupported_web_fetch_declarations_are_rejected() {
        let cases = [
            (
                json!({"type": "web_fetch_20260318", "name": "web_fetch"}),
                "unsupported web_fetch tool type",
            ),
            (
                json!({"type": "web_fetch_20250910", "name": "fetch"}),
                "must be named web_fetch",
            ),
            (json!({"max_uses": 0}), "web_fetch max_uses must be a positive integer"),
            (
                json!({"max_uses": "3"}),
                "web_fetch max_uses must be a positive integer",
            ),
            (
                json!({"max_content_tokens": 0}),
                "web_fetch max_content_tokens must be a positive integer",
            ),
            (
                json!({"max_content_tokens": 5_000_000_000_u64}),
                "web_fetch max_content_tokens must be a positive integer that fits 32 bits",
            ),
            (
                json!({"allowed_domains": ["https://example.com"]}),
                "web_fetch allowed_domains must be",
            ),
            (json!({"blocked_domains": [""]}), "web_fetch blocked_domains must be"),
            (
                json!({"allowed_domains": ["."]}),
                "web_fetch allowed_domains entry \".\" is not a host name",
            ),
            (
                json!({"blocked_domains": ["example.com/blog"]}),
                "web_fetch blocked_domains entry \"example.com/blog\" must be a host name without a scheme or path",
            ),
            (
                json!({"allowed_domains": ["*.example.com"]}),
                "web_fetch allowed_domains entry \"*.example.com\" is not a host name",
            ),
            (
                json!({"allowed_domains": ["a.com"], "blocked_domains": ["b.com"]}),
                "cannot be used together",
            ),
            (
                json!({"citations": {"enabled": true}}),
                "web_fetch citations are not supported",
            ),
            (json!({"citations": "yes"}), "web_fetch citations must be an object"),
            (json!({"use_cache": false}), "web_fetch use_cache requires a later"),
            (
                json!({"response_inclusion": "excluded"}),
                "web_fetch response_inclusion requires a later",
            ),
            (
                json!({"allowed_callers": ["code_execution_20260120"]}),
                "must permit direct invocation",
            ),
        ];
        for (extra, expected) in cases {
            let mut request = fetch_request(&extra);
            if let Some(tool) = extra.get("type") {
                request["tools"][0]["type"] = tool.clone();
            }
            let message = invalid_request(&mut request);
            assert!(message.contains(expected), "{extra}: {message}");
        }
    }

    #[test]
    fn host_name_entries_are_accepted_after_normalization() {
        let mut request = fetch_request(&json!({"allowed_domains": ["Example.COM.", "93.184.216.34"]}));
        assert!(normalize_native_server_tools(&mut request).is_ok());
    }

    #[test]
    fn both_server_tools_normalize_in_one_request_with_separate_budgets() {
        let mut request = json!({"model": "m", "max_tokens": 8, "messages": [], "tools": [
            {"type": "web_search_20250305", "name": "web_search", "max_uses": 1},
            {"type": "web_fetch_20250910", "name": "web_fetch", "max_uses": 2},
            {"name": "echo", "input_schema": {"type": "object"}}
        ]});
        let mut budgets = normalize_native_server_tools(&mut request).unwrap();
        let names: Vec<&str> = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["web_search", "web_fetch", "echo"]);
        assert!(
            request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool.get("type").is_none())
        );
        assert!(budgets.searches.admit(1));
        assert!(!budgets.searches.admit(1));
        assert!(
            budgets.fetches.admit(2),
            "the fetch budget is independent of the search budget"
        );
        assert!(!budgets.fetches.admit(1));
    }

    #[test]
    fn refusal_results_carry_documented_codes() {
        let exhausted = serde_json::to_value(web_fetch_budget_exhausted_result("t1")).unwrap();
        assert_eq!(exhausted["tool_use_id"], "t1");
        assert_eq!(exhausted["is_error"], true);
        let content: Value = serde_json::from_str(exhausted["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["type"], "web_fetch_tool_result_error");
        assert_eq!(content["error_code"], "max_uses_exceeded");

        let refused = serde_json::to_value(web_fetch_refused_result(
            "t2",
            WebFetchErrorCode::UrlNotInPriorContext,
            "nope",
        ))
        .unwrap();
        let content: Value = serde_json::from_str(refused["content"].as_str().unwrap()).unwrap();
        assert_eq!(content["error_code"], "url_not_in_prior_context");
        assert_eq!(content["message"], "nope");
    }
}
