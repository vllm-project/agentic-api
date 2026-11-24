use serde_json::json;

use super::{PriorCalls, ToolCatalog};
use crate::types::tools::ResponsesTool;

fn catalog(tools: serde_json::Value) -> ToolCatalog {
    ToolCatalog::build(serde_json::from_value::<Vec<ResponsesTool>>(tools).expect("tools parse"))
        .expect("catalog builds")
}

#[test]
fn a_member_name_that_repeats_a_top_level_function_is_rejected() {
    let catalog = catalog(json!([
        {"type": "function", "name": "agentic_ns__mcp__shell__run"},
        {"type": "namespace", "name": "mcp__shell", "tools": [{"type": "function", "name": "run"}]}
    ]));

    let err = catalog.validate_model_visible_names().unwrap_err();

    assert_eq!(
        err.to_string(),
        "invalid tool config: codex namespace member mcp__shell.run generates name agentic_ns__mcp__shell__run, which collides with a declared function tool"
    );
}

#[test]
fn two_members_that_generate_one_name_are_rejected() {
    let catalog = catalog(json!([
        {"type": "namespace", "name": "a__b", "tools": [{"type": "function", "name": "c"}]},
        {"type": "namespace", "name": "a", "tools": [{"type": "function", "name": "b__c"}]}
    ]));

    let err = catalog.validate_model_visible_names().unwrap_err();

    assert_eq!(
        err.to_string(),
        "invalid tool config: codex namespace member a.b__c collides with a__b.c at generated name agentic_ns__a__b__c"
    );
}

#[test]
fn a_loaded_definition_enters_the_same_name_check() {
    let mut catalog = catalog(json!([{"type": "function", "name": "agentic_ns__weather__now"}]));
    let namespace: ResponsesTool = serde_json::from_value(
        json!({"type": "namespace", "name": "weather", "tools": [{"type": "function", "name": "now"}]}),
    )
    .expect("namespace parses");

    catalog
        .load(&namespace, &PriorCalls::default())
        .expect("a new definition loads");

    assert!(catalog.validate_model_visible_names().is_err());
}

#[test]
fn a_returned_definition_must_match_the_held_one() {
    let mut catalog = catalog(json!([
        {"type": "function", "name": "weather", "description": "Weather", "defer_loading": true,
            "parameters": {"type": "object"}}
    ]));
    let changed: ResponsesTool = serde_json::from_value(json!(
        {"type": "function", "name": "weather", "description": "Changed", "defer_loading": true,
            "parameters": {"type": "object"}}
    ))
    .expect("function parses");

    let err = catalog.load(&changed, &PriorCalls::default()).unwrap_err();

    assert!(
        err.to_string()
            .contains("conflicts with its existing type, schema, description, or configuration")
    );
    assert!(catalog.withheld_function_names.contains("weather"));
}
