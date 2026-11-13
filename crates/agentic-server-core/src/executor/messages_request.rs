//! Request preparation shared by the native Anthropic Messages tool loops.
//!
//! The gateway executes two native Anthropic server tools, web search and web
//! fetch, in every version listed in [`NATIVE_WEB_SEARCH_VERSIONS`] and
//! [`NATIVE_WEB_FETCH_VERSIONS`]. The versions after the basic ones add dynamic
//! filtering, which calls the tool from code execution by default; the gateway
//! does not run code execution, so it runs those versions as the basic one when
//! the declaration permits direct calls, and rejects them otherwise.
//!
//! This adapter judges what is Anthropic-specific about a declaration (tool
//! version, `citations`, cache settings, `allowed_callers`, `max_uses`), reads
//! the shared parameters into the tool's typed form, and hands validation of
//! those parameters and the upstream function schema to the tool's own
//! `ToolHandler`. The result is written back as the ordinary function-tool
//! shape vLLM accepts, and `max_uses` becomes the request-wide budget the loops
//! enforce before dispatching a call.

use serde_json::{Value, json};

use crate::executor::{ExecutorError, ExecutorResult};
use crate::tool::ToolHandler;
use crate::tool::declaration::web_search_config;
use crate::tool::domain_policy::{EXCLUSIVE_LISTS_RULE, EntryPaths, validate_domain_entry};
use crate::tool::web_fetch::{self, WebFetchErrorCode, WebFetchHandler};
use crate::tool::web_search::WebSearchHandler;
use crate::types::io::FunctionTool;
use crate::types::messages::GatewayToolResult;
use crate::types::messages::request::ToolParam;
use crate::types::messages::tool_seam::{
    NATIVE_WEB_FETCH_VERSIONS, NATIVE_WEB_SEARCH_VERSIONS, NativeToolVersion, WEB_FETCH_EXECUTOR, WEB_SEARCH_EXECUTOR,
    is_native_web_fetch_type, tool_result_block,
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

/// The listed version a declaration of `tool_name` names, or a refusal that
/// lists the supported ones.
fn declared_version(
    tool_name: &str,
    versions: &[NativeToolVersion],
    tool_type: &str,
) -> ExecutorResult<NativeToolVersion> {
    NativeToolVersion::find(versions, Some(tool_type)).ok_or_else(|| {
        let supported: Vec<&str> = versions.iter().map(|version| version.type_name).collect();
        invalid(format!(
            "unsupported {tool_name} tool type '{tool_type}'; supported versions are {}",
            supported.join(", ")
        ))
    })
}

/// `use_cache` or `response_inclusion` on a version that does not define it is
/// refused, not ignored.
fn undefined_setting(version: NativeToolVersion, setting: &str) -> ExecutorError {
    invalid(format!("{} does not define {setting}", version.type_name))
}

/// Each entry must parse under the domain policy's entry rule, so a malformed
/// entry gets Anthropic's 400 instead of matching nothing. For `web_search`
/// that is a host name or address, optionally followed by a path, with `*` only
/// in the path (`*.example.com` and `ex*.com` are refused); `web_fetch` entries
/// stay host names, as its handler requires.
fn validate_domain_list(tool: &Value, tool_name: &str, field: &str, paths: EntryPaths) -> ExecutorResult<()> {
    let Some(value) = tool.get(field) else {
        return Ok(());
    };
    let shape_error = || invalid(format!("{tool_name} {field} must be an array of non-empty strings"));
    let domains = value.as_array().ok_or_else(shape_error)?;
    for domain in domains {
        let domain = domain.as_str().ok_or_else(shape_error)?;
        if domain.trim().is_empty() {
            return Err(shape_error());
        }
        validate_domain_entry(domain, paths)
            .map_err(|reason| invalid(format!("{tool_name} {field} entry {domain:?} {reason}")))?;
    }
    Ok(())
}

/// `allowed_domains` and `blocked_domains` must each be well formed and are
/// mutually exclusive, as Anthropic documents for every server tool. Each
/// entry is parsed with the tool layer's entry parser (`domain_policy`), the
/// same one the result filter is built from.
fn validate_domain_list_shape(tool: &Value, tool_name: &str, paths: EntryPaths) -> ExecutorResult<()> {
    validate_domain_list(tool, tool_name, "allowed_domains", paths)?;
    validate_domain_list(tool, tool_name, "blocked_domains", paths)?;
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

/// The gateway calls a server tool only directly, never from code execution,
/// so a declaration must permit direct calls. The basic versions do unless
/// `allowed_callers` says otherwise; a dynamic-filtering version's default
/// caller is code execution, so its declaration has to name `"direct"` itself.
/// A list that also names a code-execution caller is accepted: the gateway only
/// ever makes the direct calls it permits.
fn validate_allowed_callers(tool: &Value, tool_name: &str, version: NativeToolVersion) -> ExecutorResult<()> {
    let permits_direct = match tool.get("allowed_callers") {
        None => !version.dynamic_filtering,
        Some(value) => {
            let callers = value
                .as_array()
                .filter(|callers| callers.iter().all(Value::is_string))
                .ok_or_else(|| invalid(format!("{tool_name} allowed_callers must be an array of strings")))?;
            callers.iter().any(|caller| caller.as_str() == Some("direct"))
        }
    };
    if permits_direct {
        return Ok(());
    }
    let reason = if version.dynamic_filtering {
        "dynamic filtering calls the tool from code execution, which this gateway does not run"
    } else {
        "this gateway calls the tool only directly, never from code execution"
    };
    Err(invalid(format!(
        "{} allowed_callers must include \"direct\": {reason}",
        version.type_name
    )))
}

/// `use_cache` lets a fetch return cached content; no web search version
/// defines it. The gateway keeps no cache of fetched pages, so either value is
/// honoured on a version that defines it.
fn validate_use_cache(tool: &Value, tool_name: &str, version: NativeToolVersion) -> ExecutorResult<()> {
    let Some(value) = tool.get("use_cache") else {
        return Ok(());
    };
    if !version.use_cache {
        return Err(undefined_setting(version, "use_cache"));
    }
    if value.is_boolean() {
        Ok(())
    } else {
        Err(invalid(format!("{tool_name} use_cache must be a boolean")))
    }
}

/// `response_inclusion` decides whether results that a completed
/// code-execution call consumed appear in the response. The gateway never calls
/// the tool from code execution, so the setting has nothing to act on, and
/// either documented value is accepted on a version that defines it.
fn validate_response_inclusion(tool: &Value, tool_name: &str, version: NativeToolVersion) -> ExecutorResult<()> {
    let Some(value) = tool.get("response_inclusion") else {
        return Ok(());
    };
    if !version.response_inclusion {
        return Err(undefined_setting(version, "response_inclusion"));
    }
    match value.as_str() {
        Some("full" | "excluded") => Ok(()),
        _ => Err(invalid(format!(
            "{tool_name} response_inclusion must be \"full\" or \"excluded\""
        ))),
    }
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

/// A native `web_search_*` declaration, if `tool` is one: the version and the
/// Anthropic settings are judged here, the shared parameters go through
/// [`WebSearchHandler`], and `max_uses` folds into the request-wide budget.
/// Every supported version is rewritten into the same function tool.
fn normalize_web_search_declaration(
    tool: &Value,
    max_uses: &mut Option<usize>,
) -> ExecutorResult<Option<FunctionTool>> {
    let Some(tool_type) = tool
        .get("type")
        .and_then(Value::as_str)
        .filter(|tool_type| tool_type.starts_with("web_search_"))
    else {
        return Ok(None);
    };
    if tool.get("name").and_then(Value::as_str) != Some(WEB_SEARCH_EXECUTOR) {
        return Ok(None);
    }
    let version = declared_version(WEB_SEARCH_EXECUTOR, NATIVE_WEB_SEARCH_VERSIONS, tool_type)?;
    validate_domain_list_shape(tool, WEB_SEARCH_EXECUTOR, EntryPaths::Allowed)?;
    validate_user_location(tool)?;
    validate_allowed_callers(tool, WEB_SEARCH_EXECUTOR, version)?;
    validate_use_cache(tool, WEB_SEARCH_EXECUTOR, version)?;
    validate_response_inclusion(tool, WEB_SEARCH_EXECUTOR, version)?;
    *max_uses = fold_max_uses(*max_uses, tool, WEB_SEARCH_EXECUTOR)?;
    let declaration: ToolParam = serde_json::from_value(tool.clone())
        .map_err(|error| invalid(format!("web_search declaration is not a tool: {error}")))?;
    let params = web_search_config(&declaration);
    handler_function_tool(&WebSearchHandler::spec_only(), WEB_SEARCH_EXECUTOR, &params).map(Some)
}

/// A native `web_fetch_*` declaration, if `tool` is one. Settings the gateway
/// cannot honour are rejected rather than ignored. The shared parameters are
/// read once into [`WebFetchToolParam`] and validated by [`WebFetchHandler`];
/// every supported version is rewritten into the same function tool.
fn normalize_web_fetch_declaration(tool: &Value, max_uses: &mut Option<usize>) -> ExecutorResult<Option<FunctionTool>> {
    let Some(tool_type) = tool
        .get("type")
        .and_then(Value::as_str)
        .filter(|tool_type| is_native_web_fetch_type(Some(tool_type)))
    else {
        return Ok(None);
    };
    let version = declared_version(WEB_FETCH_EXECUTOR, NATIVE_WEB_FETCH_VERSIONS, tool_type)?;
    if tool.get("name").and_then(Value::as_str) != Some(WEB_FETCH_EXECUTOR) {
        return Err(invalid(format!(
            "{tool_type} declarations must be named {WEB_FETCH_EXECUTOR}"
        )));
    }
    validate_domain_list_shape(tool, WEB_FETCH_EXECUTOR, EntryPaths::Refused)?;
    validate_allowed_callers(tool, WEB_FETCH_EXECUTOR, version)?;
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
    validate_use_cache(tool, WEB_FETCH_EXECUTOR, version)?;
    validate_response_inclusion(tool, WEB_FETCH_EXECUTOR, version)?;
    *max_uses = fold_max_uses(*max_uses, tool, WEB_FETCH_EXECUTOR)?;
    let params = WebFetchToolParam::from_declaration(|field| tool.get(field))
        .map_err(|reason| invalid(format!("web_fetch {reason}")))?;
    handler_function_tool(&WebFetchHandler::spec_only(), WEB_FETCH_EXECUTOR, &params).map(Some)
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
    rewrite_native_server_tools(request).map(|(_, rewritten)| rewritten)
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
    rewrite_native_server_tools(request).map(|(budgets, _)| budgets)
}

/// The rewrite behind both entry points: the request-wide budgets, and whether
/// any declaration was rewritten, decided by the rewrite itself so the two can
/// never disagree about which declarations are native.
fn rewrite_native_server_tools(request: &mut Value) -> ExecutorResult<(ServerToolBudgets, bool)> {
    let mut searches = None;
    let mut fetches = None;
    let mut rewritten = false;
    if let Some(tools) = request.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools {
            let function = match normalize_web_search_declaration(tool, &mut searches)? {
                Some(function) => Some(function),
                None => normalize_web_fetch_declaration(tool, &mut fetches)?,
            };
            if let Some(function) = function {
                *tool = anthropic_function_tool(&function);
                rewritten = true;
            }
        }
    }
    let budgets = ServerToolBudgets {
        searches: UseBudget::limited(searches),
        fetches: UseBudget::limited(fetches),
    };
    Ok((budgets, rewritten))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::messages::tool_seam::{NATIVE_WEB_FETCH_TYPE, NATIVE_WEB_SEARCH_TYPE};
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
    fn native_web_search_domain_entries_follow_anthropics_rule() {
        let search = |field: &str, entry: &str| {
            json!({"model": "m", "max_tokens": 8, "messages": [], "tools": [
                {"type": "web_search_20250305", "name": "web_search", field: [entry]}
            ]})
        };
        for entry in [
            "example.com/blog",
            "example.com/*",
            "example.com/*/articles",
            "docs.example.com",
        ] {
            for field in ["allowed_domains", "blocked_domains"] {
                assert!(
                    normalize_native_server_tools(&mut search(field, entry)).is_ok(),
                    "{field} {entry}"
                );
            }
        }
        for (entry, reason) in [
            (
                "*.example.com",
                "must not use a wildcard (*) in the host, only in the path after it",
            ),
            (
                "ex*.com",
                "must not use a wildcard (*) in the host, only in the path after it",
            ),
            (
                "https://example.com/blog",
                "must be a host name, optionally followed by a path, without a scheme",
            ),
            ("example.com/*/a/*", "may use the wildcard (*) at most once"),
            ("example.com:8080/blog", "is not a host name"),
            ("/blog", "is not a host name"),
        ] {
            assert_eq!(
                invalid_request(&mut search("allowed_domains", entry)),
                format!("web_search allowed_domains entry {entry:?} {reason}")
            );
        }
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
                json!({"type": "web_fetch_20991231", "name": "web_fetch"}),
                "unsupported web_fetch tool type 'web_fetch_20991231'; supported versions are web_fetch_20250910, \
                 web_fetch_20260209, web_fetch_20260309, web_fetch_20260318",
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
                "web_fetch allowed_domains entry \"https://example.com\" must be a host name without a scheme or path",
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
            (
                json!({"use_cache": false}),
                "web_fetch_20250910 does not define use_cache",
            ),
            (
                json!({"response_inclusion": "excluded"}),
                "web_fetch_20250910 does not define response_inclusion",
            ),
            (
                json!({"allowed_callers": ["code_execution_20260120"]}),
                "web_fetch_20250910 allowed_callers must include \"direct\"",
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

    /// Every version the gateway executes and what it defines, written out
    /// from Anthropic's documentation of each tool independently of the
    /// production lists: (type, dynamic filtering, `use_cache`,
    /// `response_inclusion`).
    const DOCUMENTED: [(&str, bool, bool, bool); 7] = [
        ("web_search_20250305", false, false, false),
        ("web_search_20260209", true, false, false),
        ("web_search_20260318", true, false, true),
        ("web_fetch_20250910", false, false, false),
        ("web_fetch_20260209", true, false, false),
        ("web_fetch_20260309", true, true, false),
        ("web_fetch_20260318", true, true, true),
    ];

    fn tool_name(tool_type: &str) -> &'static str {
        if tool_type.starts_with("web_search_") {
            WEB_SEARCH_EXECUTOR
        } else {
            WEB_FETCH_EXECUTOR
        }
    }

    /// One native declaration of `tool_type`, named after its tool, with
    /// `extra` settings.
    fn native_request(tool_type: &str, extra: &Value) -> Value {
        let mut tool = json!({"type": tool_type, "name": tool_name(tool_type)});
        for (key, value) in extra.as_object().expect("object") {
            tool[key] = value.clone();
        }
        json!({"model": "m", "max_tokens": 8, "messages": [], "tools": [tool]})
    }

    /// `extra` on top of `allowed_callers: ["direct"]`, which lets every
    /// version past the caller check.
    fn direct(extra: &Value) -> Value {
        let mut settings = json!({"allowed_callers": ["direct"]});
        for (key, value) in extra.as_object().expect("object") {
            settings[key] = value.clone();
        }
        settings
    }

    #[test]
    fn the_version_lists_match_the_documented_versions() {
        let listed: Vec<(&str, bool, bool, bool)> = NATIVE_WEB_SEARCH_VERSIONS
            .iter()
            .chain(NATIVE_WEB_FETCH_VERSIONS)
            .map(|version| {
                (
                    version.type_name,
                    version.dynamic_filtering,
                    version.use_cache,
                    version.response_inclusion,
                )
            })
            .collect();
        assert_eq!(listed, DOCUMENTED);
        assert_eq!(NATIVE_WEB_SEARCH_VERSIONS[0].type_name, NATIVE_WEB_SEARCH_TYPE);
        assert_eq!(NATIVE_WEB_FETCH_VERSIONS[0].type_name, NATIVE_WEB_FETCH_TYPE);
    }

    #[test]
    fn every_supported_version_is_rewritten_into_its_basic_function_tool() {
        let search =
            anthropic_function_tool(&WebSearchHandler::spec_only().normalize(&WebSearchToolParam::default())[0]);
        let fetch = anthropic_function_tool(&WebFetchHandler::spec_only().normalize(&WebFetchToolParam::default())[0]);
        for (tool_type, ..) in DOCUMENTED {
            let is_search = tool_name(tool_type) == WEB_SEARCH_EXECUTOR;
            let mut request = native_request(
                tool_type,
                &direct(&json!({"max_uses": 2, "allowed_domains": ["example.com"]})),
            );
            let mut budgets = normalize_native_server_tools(&mut request).expect(tool_type);
            let expected = if is_search { &search } else { &fetch };
            assert_eq!(
                &request["tools"][0], expected,
                "{tool_type} reaches the upstream as its basic function tool"
            );
            let (used, untouched) = if is_search {
                (&mut budgets.searches, &mut budgets.fetches)
            } else {
                (&mut budgets.fetches, &mut budgets.searches)
            };
            assert!(used.admit(2), "{tool_type}");
            assert!(!used.admit(1), "{tool_type}: max_uses caps the request-wide total");
            assert!(untouched.admit(50), "{tool_type}: the other tool's budget is unlimited");
        }
    }

    #[test]
    fn every_version_runs_only_when_its_callers_include_direct() {
        for (tool_type, dynamic_filtering, ..) in DOCUMENTED {
            let reason = if dynamic_filtering {
                "dynamic filtering calls the tool from code execution, which this gateway does not run"
            } else {
                "this gateway calls the tool only directly, never from code execution"
            };
            let expected = format!("{tool_type} allowed_callers must include \"direct\": {reason}");
            let mut refused = vec![
                json!({"allowed_callers": ["code_execution_20260120"]}),
                json!({"allowed_callers": []}),
            ];
            if dynamic_filtering {
                refused.push(json!({}));
            } else {
                let mut request = native_request(tool_type, &json!({}));
                assert!(
                    normalize_native_server_tools(&mut request).is_ok(),
                    "{tool_type} is called directly by default"
                );
            }
            for extra in refused {
                let message = invalid_request(&mut native_request(tool_type, &extra));
                assert_eq!(message, expected, "{tool_type} {extra}");
            }
            for callers in [
                json!(["direct"]),
                json!(["direct", "code_execution_20260120"]),
                json!(["code_execution_20260120", "direct"]),
            ] {
                let mut request = native_request(tool_type, &json!({"allowed_callers": callers}));
                assert!(
                    normalize_native_server_tools(&mut request).is_ok(),
                    "{tool_type} {callers}"
                );
            }
            for malformed in [json!(["direct", 1]), json!("direct")] {
                let message = invalid_request(&mut native_request(tool_type, &json!({"allowed_callers": malformed})));
                assert_eq!(
                    message,
                    format!("{} allowed_callers must be an array of strings", tool_name(tool_type))
                );
            }
        }
    }

    #[test]
    fn use_cache_is_accepted_only_where_its_version_defines_it() {
        for (tool_type, _, use_cache, _) in DOCUMENTED {
            for value in [json!(true), json!(false)] {
                let mut request = native_request(tool_type, &direct(&json!({"use_cache": value})));
                if use_cache {
                    assert!(
                        normalize_native_server_tools(&mut request).is_ok(),
                        "{tool_type} use_cache {value}"
                    );
                } else {
                    assert_eq!(
                        invalid_request(&mut request),
                        format!("{tool_type} does not define use_cache")
                    );
                }
            }
            if use_cache {
                for malformed in [json!("false"), json!(null), json!(1)] {
                    let mut request = native_request(tool_type, &direct(&json!({"use_cache": malformed})));
                    assert_eq!(
                        invalid_request(&mut request),
                        "web_fetch use_cache must be a boolean",
                        "{tool_type} {malformed}"
                    );
                }
            }
        }
    }

    #[test]
    fn response_inclusion_is_accepted_only_where_its_version_defines_it() {
        for (tool_type, _, _, response_inclusion) in DOCUMENTED {
            for value in ["full", "excluded"] {
                let mut request = native_request(tool_type, &direct(&json!({"response_inclusion": value})));
                if response_inclusion {
                    assert!(
                        normalize_native_server_tools(&mut request).is_ok(),
                        "{tool_type} response_inclusion {value}"
                    );
                } else {
                    assert_eq!(
                        invalid_request(&mut request),
                        format!("{tool_type} does not define response_inclusion")
                    );
                }
            }
            if response_inclusion {
                for malformed in [json!("partial"), json!(null), json!(1), json!(true), json!({})] {
                    let mut request = native_request(tool_type, &direct(&json!({"response_inclusion": malformed})));
                    assert_eq!(
                        invalid_request(&mut request),
                        format!(
                            "{} response_inclusion must be \"full\" or \"excluded\"",
                            tool_name(tool_type)
                        ),
                        "{tool_type} {malformed}"
                    );
                }
            }
        }
    }

    /// The settings every version shares are judged on a later version exactly
    /// as on the basic one, so no version can skip a check.
    #[test]
    fn shared_settings_are_judged_the_same_on_every_version() {
        let shared = [
            json!({"max_uses": 0}),
            json!({"max_uses": "3"}),
            json!({"allowed_domains": "example.com"}),
            json!({"blocked_domains": [""]}),
            json!({"allowed_domains": ["a.com"], "blocked_domains": ["b.com"]}),
        ];
        let search_only = [
            json!({"user_location": {"type": "exact", "country": "CA"}}),
            json!({"user_location": {"type": "approximate"}}),
        ];
        let fetch_only = [
            json!({"citations": {"enabled": true}}),
            json!({"max_content_tokens": 0}),
            json!({"allowed_domains": ["*.example.com"]}),
        ];
        for (basic, only) in [
            (NATIVE_WEB_SEARCH_TYPE, &search_only[..]),
            (NATIVE_WEB_FETCH_TYPE, &fetch_only[..]),
        ] {
            for extra in shared.iter().chain(only) {
                let settings = direct(extra);
                let expected = invalid_request(&mut native_request(basic, &settings));
                let versions = DOCUMENTED
                    .iter()
                    .map(|(tool_type, ..)| *tool_type)
                    .filter(|tool_type| tool_name(tool_type) == tool_name(basic));
                for tool_type in versions {
                    let message = invalid_request(&mut native_request(tool_type, &settings));
                    assert_eq!(message, expected, "{tool_type} {extra}");
                }
            }
        }
    }

    #[test]
    fn unsupported_versions_are_refused_with_the_supported_ones() {
        let settings = direct(&json!({}));
        assert_eq!(
            invalid_request(&mut native_request("web_search_20991231", &settings)),
            "unsupported web_search tool type 'web_search_20991231'; supported versions are web_search_20250305, \
             web_search_20260209, web_search_20260318"
        );
        // A stamp between two supported versions is not a version either, nor
        // is a listed version with anything appended.
        for tool_type in [
            "web_search_20260210",
            "web_fetch_20260310",
            "web_search_",
            "web_fetch_",
            "web_search_20260318_beta",
            "web_fetch_20250910x",
            "web_search_20250305 ",
        ] {
            let tool = tool_name(tool_type);
            let supported: Vec<&str> = DOCUMENTED
                .iter()
                .map(|(listed, ..)| *listed)
                .filter(|listed| tool_name(listed) == tool)
                .collect();
            assert_eq!(
                invalid_request(&mut native_request(tool_type, &settings)),
                format!(
                    "unsupported {tool} tool type '{tool_type}'; supported versions are {}",
                    supported.join(", ")
                )
            );
        }
    }

    /// A later web fetch version is held to the same name as the basic one.
    #[test]
    fn every_web_fetch_version_must_be_named_web_fetch() {
        let versions = DOCUMENTED
            .iter()
            .map(|(tool_type, ..)| *tool_type)
            .filter(|tool_type| tool_name(tool_type) == WEB_FETCH_EXECUTOR);
        for tool_type in versions {
            let mut request = native_request(tool_type, &direct(&json!({})));
            request["tools"][0]["name"] = json!("fetch");
            assert_eq!(
                invalid_request(&mut request),
                format!("{tool_type} declarations must be named web_fetch")
            );
        }
    }

    /// `count_tokens` forwards the rewritten body only when normalization
    /// reports a change, so every version must report one.
    #[test]
    fn upstream_normalization_reports_the_rewrite_of_every_version() {
        for (tool_type, ..) in DOCUMENTED {
            let mut request = native_request(tool_type, &direct(&json!({})));
            assert!(
                normalize_native_server_tools_for_upstream(&mut request).unwrap(),
                "{tool_type}"
            );
            assert!(request["tools"][0].get("type").is_none(), "{tool_type}");
            assert!(request["tools"][0].get("allowed_callers").is_none(), "{tool_type}");
            assert!(
                !normalize_native_server_tools_for_upstream(&mut request).unwrap(),
                "{tool_type}: the rewritten body has nothing left to rewrite"
            );
        }
    }
}
