use std::collections::HashMap;

use serde_json::{Value, json};

use super::super::ToolCatalog;
use super::HostedSearchArguments;
use crate::types::tools::{CodexNamespaceMember, CodexNamespaceToolParam, FunctionToolParam, ResponsesTool};

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

/// The catalog recorded in `tests/cassettes/hosted_tool_search/inputs/tools-query.json`, with the tools discovery
/// listed for the deferred MCP server.
fn recorded_catalog() -> ToolCatalog {
    let tiktoken = namespace(
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
    );
    let server: ResponsesTool = serde_json::from_value(json!({"type": "mcp", "server_label": "gitmcp_tiktoken",
        "server_url": "https://gitmcp.io/openai/tiktoken", "defer_loading": true,
        "server_description": "Documentation and code search for the openai/tiktoken tokenizer repository."}))
    .unwrap();
    let mut catalog = ToolCatalog::build(vec![
        ResponsesTool::Function(function("get_weather", "Get the current weather for a city")),
        ResponsesTool::Function(function(
            "get_exchange_rate",
            "Get the exchange rate between two currencies",
        )),
        ResponsesTool::Namespace(namespace(
            "travel",
            "Travel location tools",
            &[
                ("get_timezone", "Get the IANA time zone for a city"),
                ("get_coordinates", "Get latitude and longitude for a city"),
                ("calculate_distance", "Calculate the distance between two cities"),
                ("book_flight", "Reserve an airline seat between two airports"),
            ],
        )),
        server,
    ])
    .unwrap();
    catalog.mcp_members.insert("gitmcp_tiktoken".to_owned(), tiktoken.tools);
    catalog
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
    assert!(ToolCatalog::default().search_raw(r#"{"paths":["travel"]}"#).is_empty());
}
