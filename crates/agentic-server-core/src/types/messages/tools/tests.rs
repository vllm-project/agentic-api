use super::*;
use serde_json::json;

// The pre-refactor wire shape is an independent compatibility oracle. Compare
// acceptance and serialized values, including nulls and opaque provider fields.
#[derive(Serialize, Deserialize)]
struct LegacyTool {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_schema: Option<Value>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    tool_type: Option<String>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

fn assert_compatible(value: &Value) {
    let legacy = serde_json::from_value::<LegacyTool>(value.clone());
    let typed = serde_json::from_value::<ToolParam>(value.clone());
    assert_eq!(typed.is_ok(), legacy.is_ok(), "acceptance changed for {value}");
    if let (Ok(legacy), Ok(typed)) = (legacy, typed) {
        assert_eq!(
            serde_json::to_value(typed).unwrap(),
            serde_json::to_value(legacy).unwrap(),
            "wire shape changed for {value}"
        );
    }
}

#[test]
fn supported_discriminators_select_distinct_variants() {
    let function: ToolParam = serde_json::from_value(json!({"name":"echo"})).unwrap();
    assert!(matches!(function, ToolParam::Function(_)));
    let search: ToolParam = serde_json::from_value(json!({"type":NATIVE_WEB_SEARCH_TYPE,"name":"web_search"})).unwrap();
    assert!(matches!(search, ToolParam::WebSearch(_)));
    let fetch: ToolParam = serde_json::from_value(json!({"type":NATIVE_WEB_FETCH_TYPE,"name":"web_fetch"})).unwrap();
    assert!(matches!(fetch, ToolParam::WebFetch(_)));
    // Names alone do not assign a native protocol kind.
    let named: ToolParam = serde_json::from_value(json!({"name":"web_fetch"})).unwrap();
    assert!(matches!(named, ToolParam::Function(_)));
}

#[test]
fn every_supported_native_version_is_classified_and_round_trips() {
    for (versions, name) in [
        (NATIVE_WEB_SEARCH_VERSIONS, "web_search"),
        (NATIVE_WEB_FETCH_VERSIONS, "web_fetch"),
    ] {
        for version in versions {
            let value = json!({"type":version.type_name,"name":name,
                "allowed_callers":["direct"],"allowed_domains":["example.com"],
                "user_location":{"type":"approximate","city":"Paris"},
                "future":{"nested":[null,false,42]}});
            let typed: ToolParam = serde_json::from_value(value.clone()).unwrap();
            let native = match &typed {
                ToolParam::WebSearch(native) if name == "web_search" => native,
                ToolParam::WebFetch(native) if name == "web_fetch" => native,
                other => panic!("{} classified as {other:?}", version.type_name),
            };
            assert_eq!(native.version(), *version);
            assert_eq!(typed.tool_type(), Some(version.type_name));
            assert_eq!(serde_json::to_value(typed).unwrap(), value);
            assert_compatible(&value);
        }
    }
}

#[test]
fn future_versions_and_provider_fields_round_trip_without_downgrading() {
    for kind in [
        "web_fetch_20990101",
        "web_search_20990101",
        "web_search_20260210",
        "web_search_20260318_extra",
        "web_fetch_20260310",
        "web_fetch_20260318_extra",
        "bash_20250124",
        "custom",
        "future_tool",
    ] {
        let value = json!({"type":kind,"name":"provider",
            "description":"kept","input_schema":{"oneOf":[{"type":"object"},{"type":"string"}]},
            "defer_loading":true,"cache_control":{"type":"ephemeral"},
            "future":{"nested":[null,false,42,{"type":"unknown"}]}});
        let typed: ToolParam = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(typed, ToolParam::Provider(_)), "{kind}");
        assert_eq!(typed.tool_type(), Some(kind));
        assert_eq!(serde_json::to_value(typed).unwrap(), value);
    }
}

#[test]
fn wire_acceptance_and_serialization_match_existing_contract() {
    for kind in [None, Some("future"), Some("")].into_iter().chain(
        NATIVE_WEB_SEARCH_VERSIONS
            .iter()
            .chain(NATIVE_WEB_FETCH_VERSIONS)
            .map(|version| Some(version.type_name)),
    ) {
        for name in ["echo", "web_search", "web_fetch", "", " "] {
            for schema in [
                None,
                Some(json!(null)),
                Some(json!(false)),
                Some(json!({"type":"object"})),
            ] {
                let mut value = json!({"name":name,"cache_control":{"type":"ephemeral"},"future":null});
                if let Some(kind) = kind {
                    value["type"] = json!(kind);
                }
                if let Some(schema) = schema {
                    value["input_schema"] = schema;
                }
                assert_compatible(&value);
            }
        }
    }
    assert_compatible(&json!({"name":"echo","type":null,"description":null,"input_schema":null}));
    // Native settings remain subject to the existing adapter's validation, not
    // a new deserialization policy that could alter the proxy/loop routing gate.
    for kind in NATIVE_WEB_SEARCH_VERSIONS
        .iter()
        .chain(NATIVE_WEB_FETCH_VERSIONS)
        .map(|version| version.type_name)
    {
        assert_compatible(&json!({"type":kind,"name":"wrong-name","max_uses":"bad",
            "allowed_domains":false,"user_location":{"type":"future"}}));
    }
}

#[test]
fn malformed_common_fields_cannot_fall_back_to_provider_tools() {
    for value in [
        json!(null),
        json!([]),
        json!(true),
        json!("tool"),
        json!({}),
        json!({"name":42}),
        json!({"name":null}),
        json!({"name":"echo","type":42}),
        json!({"name":"echo","description":false}),
        json!({"type":NATIVE_WEB_SEARCH_TYPE}),
        json!({"type":NATIVE_WEB_FETCH_TYPE,"name":42}),
        json!({"type":NATIVE_WEB_SEARCH_TYPE,"name":"web_search","description":42}),
        json!({"type":"future","name":"echo","description":{}}),
    ] {
        assert!(serde_json::from_value::<ToolParam>(value.clone()).is_err(), "{value}");
        assert_compatible(&value);
    }
}

#[test]
fn duplicate_known_fields_remain_invalid() {
    for body in [
        r#"{"name":"a","name":"b"}"#,
        r#"{"name":"a","type":"future","type":"web_fetch_20250910"}"#,
        r#"{"name":"a","description":"a","description":"b"}"#,
        r#"{"name":"a","input_schema":{},"input_schema":false}"#,
    ] {
        assert!(serde_json::from_str::<LegacyTool>(body).is_err());
        assert!(serde_json::from_str::<ToolParam>(body).is_err(), "{body}");
    }
}

#[test]
fn mcp_toolsets_have_typed_configuration_without_a_function_name() {
    let value = json!({"type":"mcp_toolset", "mcp_server_name":"example",
        "default_config":{"enabled":false, "defer_loading":true},
        "configs":{"echo":{"enabled":true, "defer_loading":false}},
        "cache_control":{"type":"ephemeral"}, "future":{"keep":true}});
    let tool: ToolParam = serde_json::from_value(value.clone()).unwrap();
    assert!(matches!(tool, ToolParam::McpToolset(_)));
    assert_eq!(tool.name(), None);
    assert_eq!(tool.tool_type(), Some("mcp_toolset"));
    assert_eq!(serde_json::to_value(tool).unwrap(), value);
}

#[test]
fn malformed_mcp_toolsets_cannot_fall_through_to_provider_tools() {
    for value in [
        json!({"type":"mcp_toolset", "name":"not_a_function"}),
        json!({"type":"mcp_toolset", "mcp_server_name":42}),
        json!({"type":"mcp_toolset", "mcp_server_name":"s", "default_config":{"enabled":"yes"}}),
        json!({"type":"mcp_toolset", "mcp_server_name":"s", "configs":{"echo":{"defer_loading":1}}}),
    ] {
        assert!(serde_json::from_value::<ToolParam>(value.clone()).is_err(), "{value}");
    }
}

#[test]
fn hosted_search_capabilities_require_an_exact_version_and_matching_name() {
    for (kind, name) in [
        ("tool_search_tool_regex_20251119", "tool_search_tool_regex"),
        ("tool_search_tool_bm25_20251119", "tool_search_tool_bm25"),
    ] {
        let tool: ToolParam = serde_json::from_value(json!({"type":kind,"name":name})).unwrap();
        assert!(tool.is_hosted_tool_search());
        let wrong: ToolParam = serde_json::from_value(json!({"type":kind,"name":"other"})).unwrap();
        assert!(!wrong.is_hosted_tool_search());
    }
    let unknown: ToolParam = serde_json::from_value(json!({
        "type":"tool_search_tool_regex_20251120","name":"tool_search_tool_regex"
    }))
    .unwrap();
    assert!(!unknown.is_hosted_tool_search());
    assert!(matches!(unknown, ToolParam::Provider(_)));
}

#[test]
fn mcp_toolset_preserves_an_opaque_name_extension() {
    let value = json!({"type":"mcp_toolset", "mcp_server_name":"s", "name":"provider_extension"});
    let tool: ToolParam = serde_json::from_value(value.clone()).unwrap();
    assert!(matches!(tool, ToolParam::McpToolset(_)));
    assert_eq!(serde_json::to_value(tool).unwrap(), value);
}
