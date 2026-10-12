//! Connector acceptance checks with a deterministic MCP protocol fixture and mock inference.
use super::*;
use crate::executor::{
    ConversationHandler, ExecutionContext, MessagesUpstream, ResponseHandler, run_messages_loop, run_messages_stream,
};
use crate::storage::{ConversationStore, ResponseStore};
use crate::tool::mcp::messages::connector_tools;
use crate::tool::{GatewayExecutorRegistration, GatewayExecutors, McpClient, McpHandler, ToolRegistry, ToolType};
use futures::StreamExt;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::sync::Arc;

fn request(stream: bool) -> Value {
    json!({"model":"test", "max_tokens":128, "stream":stream,
        "mcp_servers":[{"type":"url", "name":"counter", "url":"https://mcp.example/mcp", "authorization_token":"connector-secret"}],
        "tools":[{"type":"tool_search_tool_regex_20251119", "name":"tool_search_tool_regex"},
            {"type":"mcp_toolset", "mcp_server_name":"counter", "default_config":{"enabled":false},
            "configs":{"echo":{"enabled":true}, "fail":{"enabled":true, "defer_loading":true}}}],
        "messages":[{"role":"user", "content":"use echo"}]})
}

const MCP_FIXTURE: &str = r"
import sys, json
for line in sys.stdin:
    req = json.loads(line)
    if 'id' not in req:
        continue
    method = req.get('method')
    if method == 'initialize':
        result = {'protocolVersion':'2025-06-18', 'capabilities':{'tools':{}}, 'serverInfo':{'name':'fixture', 'version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[{'name':name, 'description':name, 'inputSchema':{'type':'object'}} for name in ['echo','fail','disabled']]}
    elif method == 'tools/call':
        failed = req['params']['name'] == 'fail'
        result = {'content':[{'type':'text', 'text':'fixture error' if failed else 'fixture output: '+req['params']['arguments']['text']}], 'isError':failed}
    else:
        continue
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}), flush=True)
";

async fn prepare(stream: bool) -> (MessagesRequestContext, ToolRegistry) {
    prepare_fixture(request(stream)).await
}

async fn prepare_fixture(raw: Value) -> (MessagesRequestContext, ToolRegistry) {
    let mut ctx = MessagesRequestContext::from_value(raw).unwrap();
    let client = Arc::new(
        McpClient::connect_stdio(
            if cfg!(windows) { "python" } else { "python3" },
            &["-c".to_owned(), MCP_FIXTURE.to_owned()],
            None,
            None,
        )
        .await
        .unwrap(),
    );
    let handlers = McpHandler::discovered_tool_handlers("counter", client, None)
        .await
        .unwrap();
    let mut executors = GatewayExecutors::default();
    executors.insert(GatewayExecutorRegistration::Mcp {
        server_label: "counter".to_owned(),
        handlers,
    });
    let mut tools = connector_tools(ctx.typed.mcp_servers.as_deref().unwrap(), ctx.tools().unwrap()).unwrap();
    // Inject a configured fixture binding after testing the wire-to-typed conversion;
    // production request-declared connections retain the shared outbound host policy.
    for tool in &mut tools {
        if let crate::tool::ToolDeclaration::Mcp(param) = tool {
            assert_eq!(param.authorization.as_deref(), Some("connector-secret"));
            assert_eq!(param.require_approval.as_deref(), Some("never"));
            param.server_url = None;
            param.authorization = None;
        }
    }
    let registry = ToolRegistry::build_with_handlers(&mut tools, &mut executors)
        .await
        .unwrap();
    let mut map = GatewayToolMap::default();
    normalize_connector(&mut ctx.raw, &tools, &mut map).unwrap();
    ctx.gateway_tools = Some(map);
    (ctx, registry)
}

#[tokio::test]
async fn discovered_ownership_configuration_credentials_and_replay() {
    let (mut ctx, registry) = prepare(false).await;
    assert_eq!(registry.lookup("mcp__counter__echo").unwrap().tool_type, ToolType::Mcp);
    assert!(registry.lookup("mcp__counter__disabled").is_none());
    assert_eq!(ctx.raw["tools"].as_array().unwrap().len(), 3);
    let deferred = ctx.raw["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tool| tool["name"].as_str().unwrap().starts_with("mcp__"))
        .find(|t| t["name"] == "mcp__counter__fail")
        .unwrap();
    assert_eq!(deferred["defer_loading"], true);
    assert!(!ctx.upstream_body().unwrap().contains("connector-secret"));
    assert!(!format!("{ctx:?}").contains("connector-secret"));
    assert!(
        !format!(
            "{:?}",
            ParsedMessagesRequest::parse(&serde_json::to_vec(&request(false)).unwrap()).unwrap()
        )
        .contains("connector-secret")
    );
    let normalized_tools = ctx.raw["tools"].clone();
    ctx.raw = request(false);
    ctx.raw["messages"] = json!([{"role":"assistant", "content":[
        {"type":"mcp_tool_use", "id":"call", "server_name":"counter", "name":"echo", "input":{"text":"hello"}},
        {"type":"mcp_tool_result", "tool_use_id":"call", "is_error":false, "content":[{"type":"text", "text":"hello"}]},
        {"type":"text", "text":"done", "cache_control":{"type":"ephemeral"}}
    ]}]);
    // Replay lowering uses the same normalized tools and preserves extension fields.
    let mut tools = connector_tools(ctx.typed.mcp_servers.as_deref().unwrap(), ctx.tools().unwrap()).unwrap();
    if let crate::tool::ToolDeclaration::Mcp(param) = &mut tools[0] {
        param.discovered_tools = normalized_tools
            .as_array()
            .unwrap()
            .iter()
            .filter(|tool| tool["name"].as_str().unwrap().starts_with("mcp__"))
            .map(|tool| crate::types::tools::McpDiscoveredToolParam {
                server_label: "counter".to_owned(),
                tool_name: tool["name"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("mcp__counter__")
                    .to_owned(),
                internal_name: tool["name"].as_str().unwrap().to_owned(),
                tool: serde_json::from_value(json!({"name":"echo", "inputSchema":{"type":"object"}})).unwrap(),
            })
            .collect();
    }
    normalize_connector(&mut ctx.raw, &tools, ctx.gateway_tools.as_mut().unwrap()).unwrap();
    assert_eq!(ctx.raw["messages"][0]["content"][0]["type"], "tool_use");
    assert_eq!(ctx.raw["messages"][0]["content"][0]["name"], "mcp__counter__echo");
    assert_eq!(
        ctx.raw["messages"][2]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(ctx.raw["messages"][1]["role"], "user");
    assert_eq!(ctx.raw["messages"][1]["content"][0]["type"], "tool_result");
    assert_eq!(ctx.raw["messages"][1]["content"][0]["tool_use_id"], "call");
}

async fn inference_fixture(
    stream: bool,
    failed: bool,
    mixed: bool,
) -> (
    ExecutionContext,
    Arc<tokio::sync::Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let capture = Arc::clone(&requests);
    let app = axum::Router::new().route("/v1/messages", axum::routing::post(move |axum::Json(body):axum::Json<Value>| {
        let capture = Arc::clone(&capture);
        async move {
            let mut requests = capture.lock().await;
            let round = requests.len();
            requests.push(body);
            drop(requests);
            let mut content = if round == 0 { vec![json!({"type":"tool_use", "id":"call", "name":if failed {"mcp__counter__fail"} else {"mcp__counter__echo"}, "input":{"text":"hello"}})] }
                else { vec![json!({"type":"text", "text":"done"})] };
            if failed && round == 0 {
                content.splice(0..0, [
                    json!({"type":"server_tool_use", "id":"search", "name":"tool_search_tool_regex", "input":{"pattern":"fail"}}),
                    json!({"type":"tool_search_tool_result", "tool_use_id":"search", "content":{"type":"tool_search_tool_search_result", "tool_references":[{"type":"tool_reference", "tool_name":"mcp__counter__fail"}]}}),
                ]);
            }
            if mixed && round == 0 { content.push(json!({"type":"tool_use", "id":"client", "name":"client_echo", "input":{}})); }
            let stop = if round == 0 { "tool_use" } else { "end_turn" };
            if stream {
                let mut events = vec![json!({"type":"message_start", "message":{"id":"msg", "content":[], "usage":{"input_tokens":2}}})];
                for (index, block) in content.into_iter().enumerate() {
                    let mut start = block.clone();
                    let streamed_input = failed && matches!(block["type"].as_str(), Some("tool_use" | "server_tool_use"));
                    if streamed_input { start["input"] = json!({}); }
                    events.push(json!({"type":"content_block_start", "index":index, "content_block":start}));
                    if streamed_input {
                        let arguments = block["input"].to_string();
                        let (left, right) = arguments.split_at(arguments.len() / 2);
                        for partial in [left, right] {
                            events.push(json!({"type":"content_block_delta", "index":index, "delta":{"type":"input_json_delta", "partial_json":partial}}));
                        }
                    }
                    events.push(json!({"type":"content_block_stop", "index":index}));
                }
                events.push(json!({"type":"message_delta", "delta":{"stop_reason":stop}, "usage":{"output_tokens":3}}));
                events.push(json!({"type":"message_stop"}));
                let mut body = String::new();
                for event in &events { write!(body, "data: {event}\n\n").unwrap(); }
                axum::response::IntoResponse::into_response(([ (http::header::CONTENT_TYPE, "text/event-stream") ], body))
            } else {
                axum::response::IntoResponse::into_response(axum::Json(json!({"type":"message", "role":"assistant", "content":content, "stop_reason":stop, "usage":{"input_tokens":2,"output_tokens":3}})))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let exec = ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        Arc::new(reqwest::Client::new()),
        format!("http://{address}"),
    );
    (exec, requests, task)
}

#[tokio::test]
async fn mcp_calls_and_errors_use_the_same_loop_in_both_response_modes() {
    for stream in [false, true] {
        for failed in [false, true] {
            for mixed in [false, true] {
                let (ctx, registry) = prepare(stream).await;
                let (exec, requests, task) = inference_fixture(stream, failed, mixed).await;
                let upstream = MessagesUpstream::new(&exec.llm_base_url, None, http::HeaderMap::new());
                let content = if stream {
                    let response = run_messages_stream(ctx, Arc::new(registry), Arc::new(exec), upstream)
                        .await
                        .unwrap();
                    let frames = response.body.collect::<Vec<_>>().await.join("");
                    assert_eq!(frames.matches("event: message_start").count(), 1);
                    assert_eq!(frames.matches("event: message_stop").count(), 1);
                    let events = frames
                        .lines()
                        .filter_map(|line| line.strip_prefix("data: "))
                        .map(|data| serde_json::from_str::<Value>(data).unwrap())
                        .collect::<Vec<_>>();
                    let indices = events
                        .iter()
                        .filter(|event| event["type"] == "content_block_start")
                        .map(|event| event["index"].as_u64().unwrap())
                        .collect::<Vec<_>>();
                    assert_eq!(indices, (0..indices.len() as u64).collect::<Vec<_>>());
                    let terminal = events.iter().find(|event| event["type"] == "message_delta").unwrap();
                    assert_eq!(terminal["usage"]["output_tokens"], if mixed { 3 } else { 6 });
                    events
                        .iter()
                        .filter(|event| event["type"] == "content_block_start")
                        .map(|event| event["content_block"].clone())
                        .collect::<Vec<_>>()
                } else {
                    let response = run_messages_loop(ctx, &registry, &exec, &upstream).await.unwrap();
                    assert_eq!(response.body["usage"]["output_tokens"], if mixed { 3 } else { 6 });
                    response.body["content"].as_array().unwrap().clone()
                };
                let call = content.iter().find(|block| block["type"] == "mcp_tool_use").unwrap();
                assert_eq!(call["id"], "call");
                assert_eq!(call["server_name"], "counter");
                assert_eq!(call["name"], if failed { "fail" } else { "echo" });
                if mixed {
                    assert!(!content.iter().any(|block| block["type"] == "mcp_tool_result"));
                    assert_eq!(requests.lock().await.len(), 1);
                    task.abort();
                    let _ = task.await;
                    continue;
                }
                let result = content.iter().find(|block| block["type"] == "mcp_tool_result").unwrap();
                assert_eq!(result["tool_use_id"], "call");
                assert_eq!(result["is_error"], failed);
                assert_eq!(result["content"][0]["type"], "text");
                assert!(result["content"][0]["text"].as_str().unwrap().contains(if failed {
                    "fixture error"
                } else {
                    "fixture output: hello"
                }));
                let requests = requests.lock().await;
                assert_eq!(requests.len(), if mixed { 1 } else { 2 });
                if !mixed {
                    assert_eq!(requests[1]["messages"][2]["content"][0]["tool_use_id"], "call");
                    if failed {
                        assert_eq!(requests[1]["messages"][1]["content"][0]["type"], "server_tool_use");
                        assert_eq!(requests[1]["messages"][1]["content"][0]["input"]["pattern"], "fail");
                        assert_eq!(
                            requests[1]["messages"][1]["content"][1]["content"]["tool_references"][0]["tool_name"],
                            "mcp__counter__fail"
                        );
                    }
                }
                drop(requests);
                task.abort();
                let _ = task.await;
            }
        }
    }
}

#[tokio::test]
async fn disabled_tools_replay_without_becoming_executable() {
    let mut raw = request(false);
    raw["tools"][1]["configs"].as_object_mut().unwrap().remove("echo");
    raw["messages"] = json!([{"role":"assistant", "content":[
        {"type":"server_tool_use", "id":"search", "name":"tool_search_tool_regex", "input":{"pattern":"echo"}},
        {"type":"tool_search_tool_result", "tool_use_id":"search", "content":{"type":"tool_search_tool_search_result", "tool_references":[
            {"type":"tool_reference", "tool_name":"mcp__counter__echo"},
            {"type":"tool_reference", "tool_name":"mcp__counter__fail"}
        ]}},
        {"type":"mcp_tool_use", "id":"old", "server_name":"counter", "name":"echo", "input":{"text":"prior"}},
        {"type":"mcp_tool_result", "tool_use_id":"old", "content":[{"type":"text", "text":"prior output"}]}
    ]}]);
    let (ctx, registry) = prepare_fixture(raw.clone()).await;
    assert!(registry.lookup("mcp__counter__echo").is_none());
    assert!(
        !ctx.gateway_tools
            .as_ref()
            .unwrap()
            .is_gateway_owned("mcp__counter__echo")
    );
    assert_eq!(ctx.raw["messages"][0]["content"][2]["name"], "mcp__counter__echo");
    assert_eq!(
        ctx.raw["messages"][0]["content"][1]["content"]["tool_references"],
        json!([
            {"type":"tool_reference", "tool_name":"mcp__counter__fail"}
        ])
    );
    assert_eq!(ctx.raw["messages"][1]["content"][0]["type"], "tool_result");
    assert!(
        !ctx.raw["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "mcp__counter__echo")
    );

    raw.as_object_mut().unwrap().remove("mcp_servers");
    raw.as_object_mut().unwrap().remove("tools");
    let body = serde_json::to_vec(&raw).unwrap();
    let map = GatewayToolMap::default();
    let parsed = ParsedMessagesRequest::parse_for_gateway(&body, &map).unwrap().unwrap();
    let mut ctx = MessagesRequestContext::new(parsed).unwrap();
    let registry = ctx
        .prepare_registry(&map, &mut GatewayExecutors::default())
        .await
        .unwrap();
    assert!(registry.is_empty());
    assert_eq!(ctx.raw["messages"][0]["content"][2]["type"], "tool_use");
    assert_eq!(
        ctx.raw["messages"][0]["content"][1]["content"]["tool_references"],
        json!([])
    );
    raw.as_object_mut().unwrap().remove("max_tokens");
    let count = crate::executor::prepare_messages_count_tokens(
        &serde_json::to_vec(&raw).unwrap(),
        &map,
        &mut GatewayExecutors::default(),
    )
    .await
    .unwrap()
    .unwrap();
    let count: Value = serde_json::from_slice(&count).unwrap();
    assert_eq!(count["messages"][1]["content"][0]["tool_use_id"], "old");
}

#[tokio::test]
async fn routing_and_count_tokens_keep_connector_validation_local() {
    let map = GatewayToolMap::default();
    let count_body = json!({"model":"test", "messages":[], "mcp_servers":[], "tools":[], "extension":true});
    let normalized = crate::executor::prepare_messages_count_tokens(
        &serde_json::to_vec(&count_body).unwrap(),
        &map,
        &mut GatewayExecutors::default(),
    )
    .await
    .unwrap()
    .unwrap();
    let normalized: Value = serde_json::from_slice(&normalized).unwrap();
    assert!(normalized.get("mcp_servers").is_none());
    assert!(normalized.get("max_tokens").is_none());
    assert_eq!(normalized["extension"], true);
    let mut raw = request(false);
    assert!(
        ParsedMessagesRequest::parse_for_gateway(&serde_json::to_vec(&raw).unwrap(), &map)
            .unwrap()
            .is_some()
    );
    raw["tool_choice"] = json!({"type":"tool"});
    assert!(ParsedMessagesRequest::parse_for_gateway(&serde_json::to_vec(&raw).unwrap(), &map).is_err());
    raw = request(false);
    raw["mcp_servers"] = json!([{"type":"url", "url":"https://mcp.example", "name":"unused"}]);
    assert!(
        crate::executor::prepare_messages_count_tokens(
            &serde_json::to_vec(&raw).unwrap(),
            &map,
            &mut GatewayExecutors::default()
        )
        .await
        .is_err()
    );
    let mut ctx = MessagesRequestContext::from_value(request(false)).unwrap();
    let error = ctx
        .prepare_registry(&map, &mut GatewayExecutors::default())
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("connector-secret"));
    assert!(error.to_string().contains("no valid request-declared configuration"));
}

#[tokio::test]
async fn discovery_failure_stops_messages_and_count_tokens_before_inference() {
    let mut raw = request(false);
    raw["mcp_servers"][0]["url"] = json!("https://127.0.0.1:1/mcp");
    let mut ctx = MessagesRequestContext::from_value(raw.clone()).unwrap();
    let map = GatewayToolMap::default();
    let error = ctx
        .prepare_registry(&map, &mut GatewayExecutors::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("MCP connector discovery failed"));
    assert!(!error.to_string().contains("connector-secret"));
    let error = crate::executor::prepare_messages_count_tokens(
        &serde_json::to_vec(&raw).unwrap(),
        &map,
        &mut GatewayExecutors::default(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("MCP connector discovery failed"));
    assert!(!error.to_string().contains("connector-secret"));
}

#[test]
fn parallel_calls_project_before_results_and_replay_in_order() {
    let mut map = GatewayToolMap::default();
    map.insert_mcp("mcp__s__echo".to_owned(), "s".to_owned(), "echo".to_owned());
    assert!(
        map.public_mcp_call(&json!({"type":"server_tool_use", "id":"search", "name":"mcp__s__echo", "input":{}}))
            .is_none()
    );
    let calls = vec![
        json!({"type":"tool_use", "id":"a", "name":"mcp__s__echo", "input":{}}),
        json!({"type":"tool_use", "id":"b", "name":"mcp__s__echo", "input":{}}),
    ];
    let results = vec![
        GatewayToolResult::new("a", "first".to_owned(), false),
        GatewayToolResult::new("b", "second".to_owned(), false),
    ];
    let projected = crate::types::messages::tool_seam::project_mcp_round(&calls, &results, &map, true).unwrap();
    assert_eq!(projected[0]["id"], "a");
    assert_eq!(projected[1]["id"], "b");
    assert_eq!(projected[2]["tool_use_id"], "a");
    assert_eq!(projected[3]["tool_use_id"], "b");
    let mut raw = json!({"mcp_servers":[], "tools":[], "messages":[{"role":"assistant", "content":projected}]});
    normalize_connector(&mut raw, &[], &mut map).unwrap();
    assert_eq!(raw["messages"].as_array().unwrap().len(), 2);
    assert_eq!(raw["messages"][0]["content"].as_array().unwrap().len(), 2);
    assert_eq!(raw["messages"][1]["role"], "user");
    assert_eq!(raw["messages"][1]["content"][1]["tool_use_id"], "b");
}

#[test]
fn historical_names_reuse_shared_collision_handling_without_execution_grants() {
    let mut map = GatewayToolMap::default();
    map.insert_mcp("mcp__a_b__echo".to_owned(), "a_b".to_owned(), "echo".to_owned());
    let mut raw = json!({"messages":[{"role":"assistant", "content":[
        {"type":"mcp_tool_use", "id":"a", "server_name":"a.b", "name":"echo", "input":{}},
        {"type":"mcp_tool_use", "id":"b", "server_name":"a_b", "name":"echo", "input":{}},
        {"type":"mcp_tool_use", "id":"c", "server_name":"a.b", "name":"echo", "input":{}}
    ]}]});
    normalize_connector(&mut raw, &[], &mut map).unwrap();
    let blocks = raw["messages"][0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["name"], blocks[2]["name"]);
    assert_ne!(blocks[0]["name"], blocks[1]["name"]);
    assert_eq!(blocks[1]["name"], "mcp__a_b__echo");
    assert!(!map.is_gateway_owned(blocks[0]["name"].as_str().unwrap()));
}

#[tokio::test]
async fn connector_cache_boundaries_preserve_tool_order() {
    let mut raw = request(false);
    raw["tools"] = json!([
        {"name":"before", "input_schema":{"type":"object"}},
        {"type":"mcp_toolset", "mcp_server_name":"counter", "cache_control":{"type":"ephemeral","ttl":"1h"}},
        {"name":"after", "input_schema":{"type":"object"}, "cache_control":{"type":"ephemeral"}}
    ]);
    let (ctx, _) = prepare_fixture(raw).await;
    let tools = ctx.raw["tools"].as_array().unwrap();
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "before",
            "mcp__counter__echo",
            "mcp__counter__fail",
            "mcp__counter__disabled",
            "after"
        ]
    );
    assert!(tools[1].get("cache_control").is_none());
    assert!(tools[2].get("cache_control").is_none());
    assert_eq!(tools[3]["cache_control"], json!({"type":"ephemeral","ttl":"1h"}));
    assert_eq!(tools[4]["cache_control"], json!({"type":"ephemeral"}));
}

#[test]
fn empty_toolset_cache_boundary_is_rejected_instead_of_lost() {
    let mut raw = json!({"mcp_servers":[], "tools":[
        {"name":"client", "input_schema":{"type":"object"}},
        {"type":"mcp_toolset", "mcp_server_name":"s", "cache_control":{"type":"ephemeral"}}
    ]});
    let declarations = connector_tools(
        &[serde_json::from_value(json!({"type":"url", "name":"s", "url":"https://example.com/mcp"})).unwrap()],
        &[serde_json::from_value(raw["tools"][1].clone()).unwrap()],
    )
    .unwrap();
    let error = normalize_connector(&mut raw, &declarations, &mut GatewayToolMap::default()).unwrap_err();
    assert!(error.to_string().contains("empty MCP toolset"));
    raw["tools"] = json!([{ "type":"mcp_toolset", "mcp_server_name":"s" }]);
    normalize_connector(&mut raw, &declarations, &mut GatewayToolMap::default()).unwrap();
    assert_eq!(raw["tools"], json!([]));
}
