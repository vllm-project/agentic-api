// Integration tests for the OpenAPI spec and Swagger UI endpoints.
#![cfg(feature = "openapi")]

mod common;

use agentic_core::types::RequestPayload;
use agentic_server::app::{AppState, ServerConfig, build_router};
use common::{spawn_mock_llm, test_config, test_state};
use tokio::net::TcpListener;

async fn spawn_gateway_with_docs(state: AppState) -> (String, tokio::task::JoinHandle<()>) {
    let config = ServerConfig {
        enable_openapi_docs: true,
        ..ServerConfig::from_env()
    };
    let router = build_router(state, &config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), handle)
}

async fn fetch_spec() -> serde_json::Value {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let (gw_url, _h2) = spawn_gateway_with_docs(test_state(&test_config(&llm_url))).await;

    let resp = reqwest::get(format!("{gw_url}/openapi.json")).await.unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

#[tokio::test]
async fn prompt_cache_options_schema_exposes_the_closed_mode_ttl_contract() {
    let spec = fetch_spec().await;
    let schemas = &spec["components"]["schemas"];
    assert_eq!(
        schemas["PromptCacheMode"]["enum"],
        serde_json::json!(["implicit", "explicit"])
    );
    assert_eq!(schemas["PromptCacheTtl"]["enum"], serde_json::json!(["30m"]));
    let options = &schemas["PromptCacheOptions"];
    assert_eq!(options["additionalProperties"], false);
    let properties = options["properties"].as_object().unwrap();
    assert_eq!(properties.len(), 2);
    assert!(properties.contains_key("mode"));
    assert!(properties.contains_key("ttl"));
    for name in ["RequestPayload", "CompactRequest", "ResponsePayload"] {
        assert!(!schemas[name]["properties"]["prompt_cache_options"].is_null());
    }
}

#[tokio::test]
async fn responses_capability_parameter_schemas_match_typed_decoding() {
    let spec = fetch_spec().await;
    let schema = serde_json::json!({
        "$ref":"#/components/schemas/RequestPayload", "components":spec["components"]
    });
    let validator = jsonschema::validator_for(&schema).expect("Responses request schema");
    for fields in [
        serde_json::json!({}),
        serde_json::json!({"top_logprobs":null,"reasoning":{"effort":null},"service_tier":null}),
        serde_json::json!({"top_logprobs":-1,"reasoning":{"effort":"none"},"service_tier":"fast"}),
        serde_json::json!({"top_logprobs":21,"reasoning":{"effort":"minimal"},"service_tier":"ultrafast"}),
        serde_json::json!({"reasoning":{"effort":"max"},"service_tier":"scale"}),
    ] {
        let mut wire = serde_json::json!({"model":"test-model","input":"hi"});
        wire.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        assert!(validator.is_valid(&wire), "schema rejected {wire}");
        assert!(serde_json::from_value::<RequestPayload>(wire).is_ok());
    }
    for fields in [
        serde_json::json!({"top_logprobs":"3"}),
        serde_json::json!({"top_logprobs":1.5}),
        serde_json::json!({"reasoning":{"effort":"hgih"}}),
        serde_json::json!({"service_tier":"custom"}),
    ] {
        let mut wire = serde_json::json!({"model":"test-model","input":"hi"});
        wire.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        assert!(!validator.is_valid(&wire), "schema accepted {wire}");
        assert!(serde_json::from_value::<RequestPayload>(wire).is_err());
    }
    let compact = serde_json::json!({
        "$ref":"#/components/schemas/CompactRequest", "components":spec["components"]
    });
    let validator = jsonschema::validator_for(&compact).expect("compact request schema");
    assert!(validator.is_valid(&serde_json::json!({"model":"test-model","service_tier":"flex"})));
    assert!(!validator.is_valid(&serde_json::json!({"model":"test-model","service_tier":"custom"})));
}

#[tokio::test]
async fn openapi_json_returns_valid_spec() {
    let body = fetch_spec().await;

    assert!(
        body["openapi"].as_str().unwrap().starts_with("3."),
        "must be OpenAPI 3.x"
    );
    assert_eq!(body["info"]["title"], "vLLM Agentic API");
    assert_eq!(
        body["components"]["schemas"]["RequestPayload"]["additionalProperties"],
        true
    );

    let paths = body["paths"].as_object().expect("paths must be an object");
    for expected in [
        "/health",
        "/ready",
        "/v1/models",
        "/v1/responses",
        "/v1/responses/compact",
        "/v1/responses/{response_id}",
        "/v1/conversations",
        "/v1/messages",
        "/v1/messages/count_tokens",
    ] {
        assert!(paths.contains_key(expected), "missing path: {expected}");
    }

    let schemas = body["components"]["schemas"]
        .as_object()
        .expect("schemas must be an object");
    assert!(!schemas.is_empty(), "schemas must not be empty");
    for expected in ["RequestPayload", "ResponsePayload", "MessagesRequest"] {
        assert!(schemas.contains_key(expected), "missing schema: {expected}");
    }

    let security_schemes = body["components"]["securitySchemes"]
        .as_object()
        .expect("securitySchemes must be an object");
    assert!(
        security_schemes.contains_key("bearer_auth"),
        "bearer_auth scheme missing"
    );
}

#[tokio::test]
async fn streaming_endpoints_declare_dual_media_types() {
    let body = fetch_spec().await;

    for path in ["/v1/responses", "/v1/messages"] {
        let response_200 = &body["paths"][path]["post"]["responses"]["200"]["content"];
        let media_types: Vec<&str> = response_200
            .as_object()
            .unwrap_or_else(|| panic!("{path} 200 must have content"))
            .keys()
            .map(String::as_str)
            .collect();
        assert!(
            media_types.contains(&"application/json"),
            "{path} 200 missing application/json, got: {media_types:?}"
        );
        assert!(
            media_types.contains(&"text/event-stream"),
            "{path} 200 missing text/event-stream, got: {media_types:?}"
        );
    }
}

#[tokio::test]
async fn messages_tool_choice_schema_matches_the_typed_boundary() {
    let spec = fetch_spec().await;
    let schema = serde_json::json!({"components":spec["components"], "$ref":"#/components/schemas/MessagesToolChoice"});
    let validator = jsonschema::validator_for(&schema).unwrap();
    for choice in [
        serde_json::json!({"type":"auto"}),
        serde_json::json!({"type":"any", "name":"extension", "disable_parallel_tool_use":false}),
        serde_json::json!({"type":"tool", "name":"client_echo", "disable_parallel_tool_use":true, "extension":[1,2]}),
        serde_json::json!({"type":"none"}),
    ] {
        assert!(validator.is_valid(&choice), "{choice}");
    }
    for choice in [
        serde_json::json!([]),
        serde_json::json!(false),
        serde_json::json!({"type":"future"}),
        serde_json::json!({"type":"tool"}),
        serde_json::json!({"type":"tool", "name":""}),
        serde_json::json!({"type":"tool", "name":42}),
        serde_json::json!({"type":"auto", "disable_parallel_tool_use":null}),
    ] {
        assert!(!validator.is_valid(&choice), "{choice}");
    }
}

#[tokio::test]
async fn error_envelopes_match_api_style() {
    let body = fetch_spec().await;

    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        let err_400 = &body["paths"][path]["post"]["responses"]["400"];
        let err_ref = err_400["content"]["application/json"]["schema"]["$ref"]
            .as_str()
            .unwrap_or_else(|| panic!("{path} 400 must reference a schema"));
        assert!(
            err_ref.contains("AnthropicErrorResponse"),
            "{path} 400 should use AnthropicErrorResponse, got: {err_ref}"
        );
    }

    for path in ["/v1/responses", "/v1/responses/compact"] {
        let err_400 = &body["paths"][path]["post"]["responses"]["400"];
        let err_ref = err_400["content"]["application/json"]["schema"]["$ref"]
            .as_str()
            .unwrap_or_else(|| panic!("{path} 400 must reference a schema"));
        assert!(
            err_ref.contains("ApiErrorResponse"),
            "{path} 400 should use ApiErrorResponse, got: {err_ref}"
        );
    }
}

#[tokio::test]
async fn input_file_wire_content_matches_the_published_schema() {
    let spec = fetch_spec().await;
    let schema = serde_json::json!({
        "$ref": "#/components/schemas/InputContent",
        "components": spec["components"],
    });
    let validator = jsonschema::validator_for(&schema).expect("valid input content schema");
    let file = serde_json::json!({
        "type": "input_file",
        "file_id": "file_document",
        "file_url": "https://example.invalid/document.pdf",
        "file_data": "data:application/pdf;base64,JVBERi0=",
        "filename": "document.pdf",
        "detail": "auto",
    });
    assert!(
        validator.is_valid(&file),
        "the schema must describe the preserved file wire format"
    );
    for field in ["file_id", "file_url", "file_data", "filename", "detail"] {
        let mut invalid = file.clone();
        invalid[field] = serde_json::json!(123);
        assert!(!validator.is_valid(&invalid), "{field} must remain a string");
    }
}

#[tokio::test]
async fn tagged_enum_discriminators_present() {
    let body = fetch_spec().await;
    let schemas = body["components"]["schemas"]
        .as_object()
        .expect("schemas must be an object");

    let responses_tool = &schemas["ResponsesTool"];
    let one_of = responses_tool["oneOf"].as_array().expect("ResponsesTool must be oneOf");
    let type_values: Vec<&str> = one_of
        .iter()
        .filter_map(|variant| variant["allOf"][0]["properties"]["type"]["enum"][0].as_str())
        .collect();
    for expected in ["function", "mcp", "web_search_preview", "custom"] {
        assert!(
            type_values.contains(&expected),
            "ResponsesTool missing discriminator value '{expected}', got: {type_values:?}"
        );
    }

    let input_content = &schemas["InputContent"];
    let one_of = input_content["oneOf"].as_array().expect("InputContent must be oneOf");
    let type_values: Vec<&str> = one_of
        .iter()
        .filter_map(|variant| variant["properties"]["type"]["enum"][0].as_str())
        .collect();
    for expected in ["input_text", "input_image", "output_text", "reasoning_text"] {
        assert!(
            type_values.contains(&expected),
            "InputContent missing discriminator value '{expected}', got: {type_values:?}"
        );
    }

    let input_item = &schemas["InputItem"];
    let one_of = input_item["oneOf"].as_array().expect("InputItem must be oneOf");
    let type_values: Vec<&str> = one_of
        .iter()
        .filter_map(|variant| {
            variant["properties"]["type"]["enum"][0]
                .as_str()
                .or_else(|| variant["allOf"][0]["properties"]["type"]["enum"][0].as_str())
        })
        .collect();
    for expected in [
        "compaction_trigger",
        "message",
        "web_search_call",
        "mcp_call",
        "computer_call",
        "computer_call_output",
    ] {
        assert!(
            type_values.contains(&expected),
            "InputItem missing '{expected}', got: {type_values:?}"
        );
    }
    assert!(schemas.get("ConversationItem").is_none());
    assert_eq!(schemas["InOutItem"]["$ref"], "#/components/schemas/InputItem");
    assert_eq!(
        schemas["ResponsesInput"]["oneOf"][1]["items"]["$ref"],
        "#/components/schemas/InOutItem"
    );
    let output_types = schemas["OutputItem"]["oneOf"].as_array().unwrap();
    for kind in ["web_search_call", "mcp_call"] {
        assert!(
            output_types
                .iter()
                .any(|variant| variant["allOf"][0]["properties"]["type"]["enum"][0] == kind)
        );
    }
}

#[tokio::test]
async fn swagger_ui_returns_html() {
    let (llm_url, _h1) = spawn_mock_llm().await;
    let (gw_url, _h2) = spawn_gateway_with_docs(test_state(&test_config(&llm_url))).await;

    let resp = reqwest::get(format!("{gw_url}/swagger-ui/")).await.unwrap();
    assert_eq!(resp.status(), 200);

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/html"),
        "swagger-ui should return HTML, got: {content_type}"
    );
}

#[tokio::test]
async fn collaboration_input_schemas_allow_omitted_ids_without_weakening_output() {
    let spec = fetch_spec().await;
    let schemas = &spec["components"]["schemas"];
    for name in ["MultiAgentCall", "MultiAgentCallOutput", "AgentMessage"] {
        let has_required_id = |name: &str| {
            schemas[name]["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "id")
        };
        assert!(has_required_id(name), "{name} output requires id");
        assert!(
            !has_required_id(&format!("Input{name}")),
            "{name} input allows omitted id"
        );
    }
}

#[tokio::test]
async fn prompt_cache_retention_schema_matches_request_validation() {
    let spec = fetch_spec().await;
    let schemas = &spec["components"]["schemas"];
    assert_eq!(
        schemas["PromptCacheRetention"]["enum"],
        serde_json::json!(["in_memory", "24h"])
    );
    for name in ["RequestPayload", "CompactRequest"] {
        let property = &schemas[name]["properties"]["prompt_cache_retention"];
        assert!(!property.is_null(), "{name} must expose retention");
        let alternatives = property["oneOf"].as_array().expect("nullable retention alternatives");
        assert!(alternatives.iter().any(|schema| schema["type"] == "null"));
        assert!(
            alternatives
                .iter()
                .any(|schema| schema["enum"] == serde_json::json!(["in_memory", "24h"])
                    || schema["$ref"] == "#/components/schemas/PromptCacheRetention")
        );
        assert!(
            !schemas[name]["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "prompt_cache_retention")
        );
    }
}

#[tokio::test]
async fn computer_wire_schemas_match_typed_decoding() {
    let spec = fetch_spec().await;
    let validator = jsonschema::validator_for(&spec).expect("valid OpenAPI schema");
    let schemas = spec["components"]["schemas"].as_object().unwrap();
    let request_schema = serde_json::json!({
        "$ref": "#/components/schemas/RequestPayload",
        "components": { "schemas": schemas }
    });
    let request_validator = jsonschema::validator_for(&request_schema).expect("RequestPayload schema");

    let valid = serde_json::json!({
        "model": "test-model",
        "input": [
            {
                "type": "computer_call",
                "id": "cu_1",
                "call_id": "call_1",
                "actions": [
                    {"type": "click", "button": "left", "x": 1, "y": 2, "keys": null},
                    {"type": "double_click", "x": 3, "y": 4},
                    {"type": "drag", "path": [{"x": 1, "y": 2}]},
                    {"type": "keypress", "keys": ["ctrl", "l"]},
                    {"type": "move", "x": 5, "y": 6, "keys": ["shift"]},
                    {"type": "scroll", "x": 0, "y": 0, "scroll_x": 0, "scroll_y": -100},
                    {"type": "type", "text": "hi"},
                    {"type": "wait"},
                    {"type": "screenshot"},
                    {"type": "zoom", "factor": 2}
                ],
                "pending_safety_checks": [{"id": "sc_1", "code": null, "message": null}],
                "status": "completed"
            },
            {
                "type": "computer_call_output",
                "call_id": "call_1",
                "output": {"type": "computer_screenshot", "file_id": "file_1"}
            }
        ]
    });
    if let Err(err) = request_validator.validate(&valid) {
        panic!("valid computer history rejected: {err} at {}", err.instance_path);
    }
    drop(validator);

    let invalid_screenshot = serde_json::json!({
        "model": "test-model",
        "input": [{
            "type": "computer_call_output",
            "call_id": "call_1",
            "output": {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
        }]
    });
    assert!(
        !request_validator.is_valid(&invalid_screenshot),
        "input_image is not a valid computer_call_output.output"
    );

    let missing_call_id = serde_json::json!({
        "model": "test-model",
        "input": [{"type": "computer_call", "actions": [{"type": "wait"}]}]
    });
    assert!(
        !request_validator.is_valid(&missing_call_id),
        "computer_call requires call_id"
    );

    for (label, action) in [
        ("click without fields", serde_json::json!({"type": "click"})),
        (
            "click with an unknown button",
            serde_json::json!({"type": "click", "button": "nope", "x": 1, "y": 2}),
        ),
    ] {
        let malformed = serde_json::json!({
            "model": "test-model",
            "input": [{"type": "computer_call", "call_id": "call_1", "actions": [action]}]
        });
        assert!(
            !request_validator.is_valid(&malformed),
            "{label} must fail schema validation"
        );
    }

    let zoom = serde_json::json!({
        "model": "test-model",
        "input": [{
            "type": "computer_call",
            "call_id": "call_1",
            "actions": [{"type": "zoom", "factor": 2}]
        }]
    });
    if let Err(err) = request_validator.validate(&zoom) {
        panic!("unmodeled zoom action rejected: {err} at {}", err.instance_path);
    }
}
