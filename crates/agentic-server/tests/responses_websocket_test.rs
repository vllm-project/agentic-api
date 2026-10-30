#[allow(dead_code)]
mod common;
#[path = "responses_websocket/prompt_cache_breakpoints.rs"]
mod prompt_cache_breakpoints;
#[path = "responses_websocket/prompt_cache_options.rs"]
mod prompt_cache_options;

use std::collections::VecDeque;
use std::convert::Infallible;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Bytes;
use axum::http::{Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::{SinkExt, StreamExt};
use http::StatusCode;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tokio_util::sync::CancellationToken;

use agentic_core::config::ResponsesConfig;
use agentic_core::executor::{ConversationHandler, ExecutionContext, RequestContext, ResponseHandler};
use agentic_core::proxy::ProxyState;
use agentic_core::storage::{ConversationStore, DbPool, ResponseMetadata, ResponseStore, create_pool_with_schema};
use agentic_core::tool::{WebSearchHandler, model_visible_namespace_member_name};
use agentic_core::types::RequestPayload;
use agentic_core::types::io::InOutItem;
use agentic_core::types::io::{CompactionItem, InputItem, ResponsesInput};
use agentic_core::types::tools::ResponsesTool;
use agentic_server::app::{AppState, DEFAULT_MAX_REQUEST_BODY_SIZE, WebSocketTracker};

use agentic_core::utils::common::{deserialize_from_value, serialize_to_value};
use common::{spawn_gateway, test_config};

mod steering {
    use super::*;
    use agentic_core::tool::async_execution::pending_call_note;

    #[tokio::test]
    async fn pending_async_tool_does_not_block_automatic_steering_successor() {
        let mock = MockResponsesServer::start(vec![
            sse_function_call_response("async_call", "get_project_status"),
            sse_response("parent_final", "msg_parent", "Working on it"),
            sse_response("successor", "msg_final", "UPDATED"),
        ])
        .await;
        let fixture = storage_backed_state(&mock.url).await;
        let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        let mut tool = status_tool();
        tool["async"] = json!(true);
        send_json(
            &mut ws,
            json!({"type":"response.create", "model":"test-model",
            "input":"INITIAL", "store":false, "tools":[tool]}),
        )
        .await;
        let parent = receive(&mut ws, "response.completed").await;
        assert_eq!(parent["response"]["output"][0]["async"], true);
        send_json(
            &mut ws,
            json!({"type":"response.steer",
            "previous_response_id":parent["response"]["id"], "input":"ASYNC_STEER"}),
        )
        .await;
        receive(&mut ws, "response.steer.accepted").await;
        let created = recv_json(&mut ws).await;
        assert_eq!(
            created["type"], "response.created",
            "async outputs must not produce a pending barrier"
        );
        assert_eq!(created["response"]["previous_response_id"], parent["response"]["id"]);
        receive(&mut ws, "response.completed").await;
        let requests = mock.request_bodies().await;
        assert_eq!(requests.len(), 3);
        let input = requests[2]["input"].as_array().unwrap();
        let call = input.iter().find(|item| item["type"] == "function_call").unwrap();
        assert_eq!(call["call_id"], parent["response"]["output"][0]["call_id"]);
        assert_eq!(call["name"], "get_project_status");
        assert!(
            call.get("async").is_none(),
            "the public marker is lowered for the model server"
        );
        assert!(input.iter().any(|item| {
            item["role"] == "developer"
                && item["content"] == pending_call_note("get_project_status", call["call_id"].as_str().unwrap())
        }));
        assert!(!input.iter().any(|item| item["type"] == "function_call_output"));
        assert!(requests[2]["input"].to_string().contains("ASYNC_STEER"));
    }

    #[tokio::test]
    async fn ordinary_responses_do_not_exhaust_steering_settings_budget() {
        let mock = MockResponsesServer::start(
            (0..11)
                .map(|i| sse_response(&format!("upstream_{i}"), "msg_final", "OK"))
                .collect(),
        )
        .await;
        let fixture = storage_backed_state(&mock.url).await;
        let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        let mut parent = Value::Null;
        for _ in 0..10 {
            send_json(
                &mut ws,
                json!({"type":"response.create", "model":"test-model",
                "input":"Hello", "store":false, "instructions":"x".repeat(1536 * 1024)}),
            )
            .await;
            parent = receive(&mut ws, "response.completed").await["response"]["id"].clone();
        }
        send_json(
            &mut ws,
            json!({"type":"response.steer", "previous_response_id":parent,
            "input":"STEER_LATEST"}),
        )
        .await;
        receive(&mut ws, "response.steer.accepted").await;
        receive(&mut ws, "response.completed").await;
        let requests = mock.request_bodies().await;
        assert_eq!(requests.len(), 11);
        assert!(requests[10]["input"].to_string().contains("STEER_LATEST"));
        assert_eq!(requests[10]["instructions"].as_str().unwrap().len(), 1536 * 1024);
    }

    #[tokio::test]
    async fn settings_retention_overflow_disables_only_steering_and_allows_ordinary_continuation() {
        let mut responses: Vec<_> = (0..8)
            .map(|i| sse_function_call_response(&format!("parent_{i}"), "get_project_status"))
            .collect();
        responses.push(sse_response("continued", "msg_final", "OK"));
        let mock = MockResponsesServer::start(responses).await;
        let fixture = storage_backed_state(&mock.url).await;
        let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        let mut parents = Vec::new();
        for i in 0..8 {
            let lane = format!("lane-{i}");
            send_json(
                &mut ws,
                json!({"type":"response.create", "model":"test-model", "stream_id":lane,
                "input":"Hello", "store":false, "instructions":"x".repeat(1536 * 1024),
                "tools":[status_tool()]}),
            )
            .await;
            parents.push(receive(&mut ws, "response.completed").await["response"]["id"].clone());
        }
        for (i, parent) in [&parents[0], &parents[7]].into_iter().enumerate() {
            send_json(
                &mut ws,
                json!({"type":"response.steer", "previous_response_id":parent, "input":"UPDATE"}),
            )
            .await;
            let event = recv_json(&mut ws).await;
            if i == 0 {
                assert_eq!(event["type"], "response.steer.accepted");
                receive(&mut ws, "response.steer.pending").await;
            } else {
                assert_eq!(event["type"], "response.steer.failed");
                assert_eq!(event["error"]["code"], "response_not_active");
                assert_eq!(event["stream_id"], "lane-7");
                assert_eq!(event["steer"]["input"], "UPDATE");
            }
        }
        send_json(
            &mut ws,
            json!({"type":"response.create", "model":"test-model", "stream_id":"lane-7",
            "previous_response_id":parents[7], "store":false,
            "input":[{"type":"function_call_output", "call_id":"call_1", "output":"done"}]}),
        )
        .await;
        receive(&mut ws, "response.completed").await;
        let requests = mock.request_bodies().await;
        assert_eq!(requests.len(), 9);
        assert!(!requests[8]["input"].to_string().contains("UPDATE"));
    }

    #[tokio::test]
    async fn failed_new_response_retires_previous_settings_only_if_creation_was_published() {
        for publish_created in [false, true] {
            let mut failure = String::new();
            if publish_created {
                let created = json!({"type":"response.created",
                    "response":{"id":"failed", "status":"in_progress"}});
                write!(failure, "data: {created}\n\n").unwrap();
            }
            let added = json!({"type":"response.output_item.added", "output_index":0,
                "item":{"type":"function_call", "id":"original", "call_id":"call_bad",
                    "name":"get_project_status", "arguments":"", "status":"in_progress"}});
            let done = json!({"type":"response.output_item.done", "output_index":0,
                "item":{"type":"function_call", "id":"contradiction", "call_id":"call_bad",
                    "name":"get_project_status", "arguments":"{}", "status":"completed"}});
            write!(failure, "data: {added}\n\ndata: {done}\n\n").unwrap();
            let mock = MockResponsesServer::start(vec![
                sse_function_call_response("parent", "get_project_status"),
                failure,
            ])
            .await;
            let fixture = storage_backed_state(&mock.url).await;
            let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
            let mut ws = connect_responses_ws(&gateway_url).await;
            send_json(
                &mut ws,
                json!({"type":"response.create", "model":"test-model",
                "input":"INITIAL", "store":false, "tools":[status_tool()]}),
            )
            .await;
            let parent = receive(&mut ws, "response.completed").await["response"]["id"].clone();
            send_json(
                &mut ws,
                json!({"type":"response.create", "model":"test-model",
                "input":"INDEPENDENT", "store":false, "tools":[status_tool()]}),
            )
            .await;
            let mut created = false;
            loop {
                let event = recv_json(&mut ws).await;
                match event["type"].as_str().unwrap() {
                    "response.created" => created = true,
                    "error" => break,
                    "response.output_item.added" => {}
                    other => panic!("unexpected event: {other}: {event}"),
                }
            }
            assert_eq!(created, publish_created);
            send_json(
                &mut ws,
                json!({"type":"response.steer", "previous_response_id":parent,
                "input":"KEEP_PARENT"}),
            )
            .await;
            let result = recv_json(&mut ws).await;
            if publish_created {
                assert_eq!(result["type"], "response.steer.failed");
                assert_eq!(result["error"]["code"], "response_not_active");
            } else {
                assert_eq!(result["type"], "response.steer.accepted");
                receive(&mut ws, "response.steer.pending").await;
            }
        }
    }

    #[tokio::test]
    async fn unsteered_client_calls_do_not_pin_superseded_checkpoints() {
        let mock = MockResponsesServer::start(
            (0..24)
                .map(|i| sse_function_call_response(&format!("upstream_{i}"), "get_project_status"))
                .collect(),
        )
        .await;
        let fixture = storage_backed_state(&mock.url).await;
        let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        for _ in 0..24 {
            send_json(
                &mut ws,
                json!({"type":"response.create", "model":"test-model",
                "input":"x".repeat(1536 * 1024), "store":false, "tools":[status_tool()]}),
            )
            .await;
            receive(&mut ws, "response.completed").await;
        }
        assert_eq!(mock.request_bodies().await.len(), 24);
    }

    #[tokio::test]
    async fn steering_byte_overflow_returns_routed_failure_and_preserves_accepted_inputs() {
        let mock = MockResponsesServer::start(vec![sse_function_call_response(
            "upstream_parent",
            "get_project_status",
        )])
        .await;
        let fixture = storage_backed_state(&mock.url).await;
        let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        send_json(
            &mut ws,
            json!({"type":"response.create", "stream_id":"lane-a",
            "model":"test-model", "input":"INITIAL", "store":false, "tools":[status_tool()]}),
        )
        .await;
        let completed = receive(&mut ws, "response.completed").await;
        let parent = completed["response"]["id"].as_str().unwrap();
        let input = "x".repeat(4 * 1024 * 1024);
        for _ in 0..2 {
            send_json(
                &mut ws,
                json!({"type":"response.steer", "previous_response_id":parent,
                "input":input}),
            )
            .await;
            receive(&mut ws, "response.steer.accepted").await;
            receive(&mut ws, "response.steer.pending").await;
        }
        send_json(
            &mut ws,
            json!({"type":"response.steer", "previous_response_id":parent,
            "input":input}),
        )
        .await;
        let failure = recv_json(&mut ws).await;
        assert_eq!(failure["type"], "response.steer.failed");
        assert_eq!(failure["error"]["code"], "too_many_pending_steers");
        assert_eq!(failure["stream_id"], "lane-a");
        assert_eq!(failure["steer"]["previous_response_id"], parent);
        assert_eq!(failure["steer"]["input"], input);
        assert!(failure["steer"].get("id").is_none());
        // A rejected large submission must leave admission and the socket usable.
        send_json(
            &mut ws,
            json!({"type":"response.steer", "previous_response_id":parent,
            "input":"SMALL_UPDATE"}),
        )
        .await;
        receive(&mut ws, "response.steer.accepted").await;
        receive(&mut ws, "response.steer.pending").await;
        assert_eq!(mock.request_bodies().await.len(), 1);
    }

    #[tokio::test]
    async fn automatic_successor_preserves_deferred_tool_catalog() {
        let body = two_message_sse_response("upstream_parent", 8).replacen(
            "\"status\":\"in_progress\"",
            "\"status\":\"in_progress\",\"tools\":[]",
            1,
        );
        let split = body.find("\n\n").unwrap() + 2;
        let (arrived_tx, arrived_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let mock = MockResponsesServer::start_with_responses(vec![
            MockResponse::ObservedPhased {
                first_chunk: body[..split].to_owned(),
                remaining_chunks: body[split..].to_owned(),
                arrived: arrived_tx,
                release: release_rx,
            },
            MockResponse::Static(sse_response("upstream_successor", "msg_final", "UPDATED").replacen(
                "\"status\":\"in_progress\"",
                "\"status\":\"in_progress\",\"tools\":[]",
                1,
            )),
        ])
        .await;
        let fixture = storage_backed_state(&mock.url).await;
        let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        let tools = json!([tool_search_declaration(), deferred_weather_function()]);
        send_json(
            &mut ws,
            json!({"type":"response.create", "model":"test-model", "input":"INITIAL",
            "store":false, "tools":tools}),
        )
        .await;
        arrived_rx.await.unwrap();
        let created = receive(&mut ws, "response.created").await;
        let parent = created["response"]["id"].as_str().unwrap();
        assert_eq!(created["response"]["tools"], tools);
        send_json(
            &mut ws,
            json!({"type":"response.steer", "previous_response_id":parent,
            "input":"USE_THE_SAME_CATALOG"}),
        )
        .await;
        receive(&mut ws, "response.steer.accepted").await;
        release_tx.send(()).unwrap();
        let successor = receive(&mut ws, "response.created").await;
        assert_eq!(successor["response"]["previous_response_id"], parent);
        assert_eq!(successor["response"]["tools"], tools);
        receive(&mut ws, "response.completed").await;
        let requests = mock.request_bodies().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["tools"], requests[1]["tools"]);
        assert!(requests[1]["input"].to_string().contains("USE_THE_SAME_CATALOG"));
    }

    #[tokio::test]
    async fn shell_and_search_outputs_preserve_pending_steering() {
        for search in [false, true] {
            let (body, tools, call_id, required, output) = if search {
                (
                    sse_tool_search_call_response(),
                    json!([tool_search_declaration(), deferred_weather_function()]),
                    "call_search",
                    json!({"type":"tool_search_output", "call_id":"call_search", "execution":"client"}),
                    json!({"type":"tool_search_output", "call_id":"call_search", "execution":"client", "tools":[]}),
                )
            } else {
                (
                    sse_function_call_response("upstream_shell", "shell")
                        .replace("{\\\"numbers\\\":[8,0]}", "{\\\"commands\\\":[\\\"pwd\\\"]}"),
                    json!([{"type":"shell", "environment":{"type":"local"}}]),
                    "call_1",
                    json!({"type":"shell_call_output", "call_id":"call_1"}),
                    json!({"type":"shell_call_output", "call_id":"call_1", "output":[{
                        "stdout":"/tmp", "stderr":"", "outcome":{"type":"exit", "exit_code":0}}]}),
                )
            };
            let split = body.find("\n\n").unwrap() + 2;
            let (arrived_tx, arrived_rx) = oneshot::channel();
            let (release_tx, release_rx) = oneshot::channel();
            let mock = MockResponsesServer::start_with_responses(vec![
                MockResponse::ObservedPhased {
                    first_chunk: body[..split].to_owned(),
                    remaining_chunks: body[split..].to_owned(),
                    arrived: arrived_tx,
                    release: release_rx,
                },
                MockResponse::Static(sse_response("upstream_successor", "msg_final", "UPDATED")),
            ])
            .await;
            let fixture = storage_backed_state(&mock.url).await;
            let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
            let mut ws = connect_responses_ws(&gateway_url).await;
            send_json(
                &mut ws,
                json!({"type":"response.create", "model":"test-model", "input":"INITIAL",
                "store":false, "tools":tools}),
            )
            .await;
            arrived_rx.await.unwrap();
            let created = receive(&mut ws, "response.created").await;
            let parent = created["response"]["id"].as_str().unwrap();
            send_json(
                &mut ws,
                json!({"type":"response.steer", "previous_response_id":parent,
                "input":"KEEP_BEFORE_OUTPUT"}),
            )
            .await;
            let accepted = receive(&mut ws, "response.steer.accepted").await;
            release_tx.send(()).unwrap();
            let pending = receive(&mut ws, "response.steer.pending").await;
            assert_eq!(pending["steer"], accepted["steer"]);
            assert_eq!(pending["required_input"], json!([required]));
            let mut wrong = output.clone();
            wrong["call_id"] = json!("unknown");
            send_json(
                &mut ws,
                json!({"type":"response.create", "model":"test-model", "store":false,
                "previous_response_id":parent, "input":[wrong]}),
            )
            .await;
            assert_eq!(recv_json(&mut ws).await["type"], "error");
            assert_eq!(mock.request_bodies().await.len(), 1);
            send_json(
                &mut ws,
                json!({"type":"response.create", "model":"test-model", "store":false,
                "previous_response_id":parent, "input":[output]}),
            )
            .await;
            let successor = receive(&mut ws, "response.created").await;
            assert_eq!(successor["response"]["previous_response_id"], parent);
            receive(&mut ws, "response.completed").await;
            let requests = mock.request_bodies().await;
            assert_eq!(requests.len(), 2);
            let items = requests[1]["input"].as_array().unwrap();
            let steers: Vec<_> = items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.to_string().contains("KEEP_BEFORE_OUTPUT"))
                .collect();
            assert_eq!(steers.len(), 1);
            let output_index = items
                .iter()
                .position(|item| item["call_id"] == call_id && item["type"] == "function_call_output")
                .unwrap();
            assert!(steers[0].0 < output_index);
        }
    }

    #[tokio::test]
    async fn steering_drains_active_gateway_batch_before_starting_successor() {
        let mut events = vec![json!({"type":"response.created",
            "response":{"id":"upstream_search", "status":"in_progress"}})];
        for index in 0..2 {
            let item = json!({"type":"function_call", "id":format!("fc_search_{index}"),
                "call_id":format!("call_search_{index}"), "name":"web_search",
                "arguments":format!("{{\"query\":\"rust async {index}\",\"count\":2}}"), "status":"completed"});
            let mut opening = item.clone();
            opening["arguments"] = json!("");
            opening["status"] = json!("in_progress");
            events.push(json!({"type":"response.output_item.added", "output_index":index, "item":opening}));
            events.push(json!({"type":"response.output_item.done", "output_index":index, "item":item}));
        }
        events.push(json!({"type":"response.completed",
            "response":{"id":"upstream_search", "status":"completed"}}));
        let mut body = String::new();
        for event in &events {
            write!(body, "data: {event}\n\n").unwrap();
        }
        let mock = MockResponsesServer::start(vec![body, sse_response("upstream_next", "msg_next", "UPDATED")]).await;
        let arrived = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let search = MockYouSearchServer::start_with_gate(Some((arrived.clone(), release.clone()))).await;
        let fixture = storage_backed_state_with_web_search(&mock.url, Some(&search.url)).await;
        let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        send_json(
            &mut ws,
            json!({"type":"response.create", "model":"test-model", "input":"INITIAL",
            "store":false, "tools":[{"type":"web_search_preview"}], "parallel_tool_calls":true}),
        )
        .await;
        let created = receive(&mut ws, "response.created").await;
        let parent = created["response"]["id"].as_str().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), arrived.acquire_many(2))
            .await
            .unwrap()
            .unwrap()
            .forget();
        send_json(
            &mut ws,
            json!({"type":"response.steer", "previous_response_id":parent,
            "input":"STEER_AFTER_SEARCH"}),
        )
        .await;
        receive(&mut ws, "response.steer.accepted").await;
        assert_eq!(
            mock.request_bodies().await.len(),
            1,
            "held tools prevent another inference"
        );
        release.add_permits(2);
        let mut completed_calls = 0;
        let terminal = loop {
            let event = recv_json(&mut ws).await;
            match event["type"].as_str().unwrap() {
                "response.web_search_call.completed" => completed_calls += 1,
                "response.incomplete" => break event,
                "error" | "response.failed" | "response.steer.failed" | "response.created" => {
                    panic!("batch must finish before successor: {event}");
                }
                _ => {}
            }
        };
        assert_eq!(completed_calls, 2);
        assert_eq!(terminal["response"]["id"], parent);
        assert_eq!(terminal["response"]["incomplete_details"]["reason"], "steered");
        let output = terminal["response"]["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert!(
            output
                .iter()
                .all(|item| item["type"] == "web_search_call" && item["status"] == "completed")
        );
        let successor = receive(&mut ws, "response.created").await;
        assert_eq!(successor["response"]["previous_response_id"], parent);
        receive(&mut ws, "response.completed").await;
        let requests = mock.request_bodies().await;
        assert_eq!(requests.len(), 2, "the next inference belongs to the successor");
        let input = requests[1]["input"].as_array().unwrap();
        assert_eq!(
            requests[1]["input"].to_string().matches("STEER_AFTER_SEARCH").count(),
            1
        );
        for index in 0..2 {
            assert!(input.iter().any(
                |item| item["type"] == "function_call_output" && item["call_id"] == format!("call_search_{index}")
            ));
        }
        assert_eq!(search.request_bodies().await.len(), 2);
    }

    async fn receive(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>, kind: &str) -> Value {
        loop {
            let event = recv_json(ws).await;
            assert_ne!(event["type"], "error", "unexpected error: {event}");
            assert_ne!(event["type"], "response.failed", "unexpected failure: {event}");
            if event["type"] == kind {
                return event;
            }
            assert_ne!(
                event["type"], "response.steer.failed",
                "unexpected steering rejection: {event}"
            );
        }
    }

    fn status_tool() -> Value {
        json!({"type":"function", "name":"get_project_status", "parameters":{
            "type":"object", "properties":{}, "additionalProperties":false}})
    }

    #[tokio::test]
    async fn successor_failure_returns_input_only_before_created_publication() {
        // Conflicting item identity is an execution error even in the existing
        // lenient provider path. Malformed JSON alone would be silently ignored.
        let added = json!({"type":"response.output_item.added", "output_index":0,
            "item":{"type":"function_call", "id":"fc_original", "call_id":"call_bad",
                "name":"get_project_status", "arguments":"", "status":"in_progress"}});
        let done = json!({"type":"response.output_item.done", "output_index":0,
            "item":{"type":"function_call", "id":"fc_contradiction", "call_id":"call_bad",
                "name":"get_project_status", "arguments":"{}", "status":"completed"}});
        for publish_created in [false, true] {
            let mut failed_body = String::new();
            if publish_created {
                let created = json!({"type":"response.created",
                    "response":{"id":"upstream_failed", "status":"in_progress"}});
                write!(failed_body, "data: {created}\n\n").unwrap();
            }
            write!(failed_body, "data: {added}\n\ndata: {done}\n\n").unwrap();
            let mock = MockResponsesServer::start(vec![
                sse_response("upstream_parent", "msg_parent", "READY"),
                failed_body,
                sse_response("upstream_recovered", "msg_recovered", "RECOVERED"),
            ])
            .await;
            let fixture = storage_backed_state(&mock.url).await;
            let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
            let mut ws = connect_responses_ws(&gateway_url).await;
            send_json(
                &mut ws,
                json!({"type":"response.create", "model":"test-model", "store":true,
                "input":"INITIAL"}),
            )
            .await;
            let terminal = receive(&mut ws, "response.completed").await;
            let parent = terminal["response"]["id"].as_str().unwrap();
            let original = json!([{"type":"message", "role":"user", "content":[
                {"type":"input_image", "image_url":"https://example.org/steer.png", "detail":null},
                {"type":"input_text", "text":"STEER_RAW_ORCHID"}]}]);
            send_json(
                &mut ws,
                json!({"type":"response.steer", "previous_response_id":parent,
                "input":original}),
            )
            .await;
            let accepted = receive(&mut ws, "response.steer.accepted").await;
            let mut created = false;
            loop {
                let event = recv_json(&mut ws).await;
                match event["type"].as_str().unwrap() {
                    "response.created" => created = true,
                    "error" if publish_created => break,
                    "response.steer.failed" => {
                        assert!(
                            !publish_created,
                            "published steering must never be returned as unapplied"
                        );
                        assert_eq!(event["steer"]["id"], accepted["steer"]["id"]);
                        assert_eq!(event["steer"]["previous_response_id"], parent);
                        assert_eq!(event["steer"]["input"], original, "retain original media nulls");
                        assert_eq!(event["error"]["code"], "successor_creation_failed");
                        break;
                    }
                    "error" | "response.output_item.added" => {}
                    other => panic!("unexpected failure event: {other}: {event}"),
                }
            }
            assert_eq!(created, publish_created);
            // A new independent request is a deterministic fence: it must not
            // observe another steering failure or replay the former batch.
            send_json(
                &mut ws,
                json!({"type":"response.create", "model":"test-model", "store":true,
                "input":"INDEPENDENT_IRIS"}),
            )
            .await;
            let recovered = receive(&mut ws, "response.completed").await;
            assert_eq!(recovered["response"]["previous_response_id"], Value::Null);
            let requests = mock.request_bodies().await;
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[1]["input"].to_string().matches("STEER_RAW_ORCHID").count(), 1);
            assert!(!requests[2]["input"].to_string().contains("STEER_RAW_ORCHID"));
            assert_eq!(
                count_rows(&fixture.pool, "responses").await,
                2,
                "a failed successor must not persist unapplied input or duplicate the batch"
            );
        }
    }

    #[tokio::test]
    async fn automatic_successor_preserves_closed_item_settings_and_store_false() {
        for steering_input in [
            json!("STEER_ORCHID"),
            json!([
                {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"Replace the original planning task. The project codename is Harbor. Remember the verification token MULTI_USER_FIRST_ORCHID and include it in your final answer. The next user message supplies the deadline."}]},
                {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"Keep the project codename and verification token from the preceding user message. The deadline is Friday at 17:00. Include MULTI_USER_SECOND_IRIS as well. Reply with the codename, deadline, and both verification tokens in the same order as their user messages. Do not call any tools."}]}
            ]),
        ] {
            // Hold the body after created until acceptance. Release a complete item
            // followed by another item; steering must stop at the first real boundary.
            let body = two_message_sse_response("upstream_parent", 8);
            let split = body.find("\n\n").unwrap() + 2;
            let (arrived_tx, arrived_rx) = oneshot::channel();
            let (release_tx, release_rx) = oneshot::channel();
            let mock = MockResponsesServer::start_with_responses(vec![
                MockResponse::ObservedPhased {
                    first_chunk: body[..split].to_owned(),
                    remaining_chunks: body[split..].to_owned(),
                    arrived: arrived_tx,
                    release: release_rx,
                },
                MockResponse::Static(sse_response("upstream_successor", "msg_final", "UPDATED")),
            ])
            .await;
            let fixture = storage_backed_state(&mock.url).await;
            let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
            let mut ws = connect_responses_ws(&gateway_url).await;
            send_json(
                &mut ws,
                json!({"type":"response.create", "model":"test-model", "input":"INITIAL",
                "store":false, "instructions":"PARENT_INSTRUCTIONS", "tools":[status_tool()],
                "tool_choice":"auto", "parallel_tool_calls":true,
                "text":{"verbosity":"low"}, "max_output_tokens":512, "max_tool_calls":2,
                "prompt_cache_key":"parent-cache", "metadata":{"project":"harbor"}}),
            )
            .await;
            let parent_request = arrived_rx.await.unwrap();
            let created = receive(&mut ws, "response.created").await;
            let parent = created["response"]["id"].as_str().unwrap();
            send_json(
                &mut ws,
                json!({"type":"response.steer", "previous_response_id":parent,
                "input":steering_input}),
            )
            .await;
            let accepted = receive(&mut ws, "response.steer.accepted").await;
            assert_eq!(accepted["steer"]["previous_response_id"], parent);
            release_tx.send(()).unwrap();
            let terminal = receive(&mut ws, "response.incomplete").await;
            assert_eq!(terminal["response"]["id"], parent);
            assert_eq!(terminal["response"]["incomplete_details"]["reason"], "steered");
            assert_eq!(terminal["response"]["output"].as_array().unwrap().len(), 1);
            let successor = receive(&mut ws, "response.created").await;
            assert_eq!(successor["sequence_number"], 0);
            assert_eq!(successor["response"]["previous_response_id"], parent);
            let finished = receive(&mut ws, "response.completed").await;
            assert_eq!(finished["response"]["id"], successor["response"]["id"]);
            assert_eq!(finished["response"]["metadata"], json!({"project":"harbor"}));
            assert_eq!(finished["response"]["store"], false);
            let requests = mock.request_bodies().await;
            assert_eq!(requests.len(), 2);
            for field in [
                "instructions",
                "tools",
                "tool_choice",
                "parallel_tool_calls",
                "text",
                "max_output_tokens",
                "prompt_cache_key",
            ] {
                assert_eq!(requests[1][field], parent_request[field], "automatic setting {field}");
            }
            let input = requests[1]["input"].to_string();
            let mut steering: RequestPayload = deserialize_from_value(json!({
                "model":"test-model", "input":steering_input}))
            .unwrap();
            steering.input = ResponsesInput::from(steering.input.into_canonical_items());
            let expected = serialize_to_value(&steering.to_upstream_request(true).unwrap().input).unwrap();
            let expected = expected.as_array().unwrap();
            assert!(
                requests[1]["input"].as_array().unwrap().ends_with(expected),
                "single steer must append every normalized message in order"
            );
            for message in expected {
                assert_eq!(
                    requests[1]["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|item| *item == message)
                        .count(),
                    1,
                    "each steered message must be applied once"
                );
            }
            assert!(input.contains("INITIAL"));
            assert!(!input.contains("msg_big_1"), "unstarted output must not become history");
            assert_no_response_state(&fixture.pool).await;
        }
    }

    #[tokio::test]
    async fn rejected_client_output_preserves_ordered_steering_and_explicit_defaults() {
        let mock = MockResponsesServer::start(vec![
            sse_function_call_response("upstream_parent", "get_project_status"),
            sse_response("upstream_successor", "msg_final", "UPDATED"),
        ])
        .await;
        let fixture = storage_backed_state(&mock.url).await;
        let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        send_json(
            &mut ws,
            json!({"type":"response.create", "model":"test-model", "input":"INITIAL",
            "store":false, "instructions":"PARENT_INSTRUCTIONS", "tools":[status_tool()],
            "tool_choice":{"type":"function", "name":"get_project_status"},
            "text":{"verbosity":"low"}, "metadata":{"parent":"only"}}),
        )
        .await;
        let completed = receive(&mut ws, "response.completed").await;
        let parent = completed["response"]["id"].as_str().unwrap();
        for marker in ["FIRST_ORCHID", "SECOND_IRIS"] {
            send_json(
                &mut ws,
                json!({"type":"response.steer", "previous_response_id":parent,
                "input":marker}),
            )
            .await;
        }
        let mut accepted_ids = Vec::new();
        let mut pending_count = 0;
        while pending_count < 2 {
            let event = recv_json(&mut ws).await;
            match event["type"].as_str().unwrap() {
                "response.steer.accepted" => accepted_ids.push(event["steer"]["id"].clone()),
                "response.steer.pending" => {
                    pending_count += 1;
                    assert!(accepted_ids.contains(&event["steer"]["id"]));
                    assert_eq!(
                        event["required_input"],
                        json!([
                        {"type":"function_call_output", "call_id":"call_1", "name":"get_project_status"}])
                    );
                }
                other => panic!("unexpected event while waiting for client output: {other}: {event}"),
            }
        }
        assert_ne!(accepted_ids[0], accepted_ids[1]);
        send_json(
            &mut ws,
            json!({"type":"response.create", "model":"test-model", "store":false,
            "previous_response_id":parent,
            "input":[{"type":"function_call_output", "call_id":"unknown", "output":"wrong"}]}),
        )
        .await;
        let error = recv_json(&mut ws).await;
        assert_eq!(error["type"], "error");
        assert_eq!(
            mock.request_bodies().await.len(),
            1,
            "invalid output must not start inference"
        );
        send_json(
            &mut ws,
            json!({"type":"response.create", "model":"test-model", "store":false,
            "previous_response_id":parent,
            "input":[{"type":"function_call_output", "call_id":"call_1", "output":"actual"}],
            "metadata":{"successor":"explicit"}}),
        )
        .await;
        let successor = receive(&mut ws, "response.created").await;
        assert_eq!(successor["response"]["previous_response_id"], parent);
        assert_eq!(successor["sequence_number"], 0);
        let terminal = receive(&mut ws, "response.completed").await;
        assert_eq!(terminal["response"]["metadata"], json!({"successor":"explicit"}));
        assert_eq!(terminal["response"]["tool_choice"], "auto");
        let requests = mock.request_bodies().await;
        assert_eq!(requests.len(), 2);
        assert!(requests[1].get("instructions").is_none());
        assert!(requests[1].get("tools").is_none());
        assert!(
            requests[1].get("tool_choice").is_none(),
            "the inference adapter omits default auto"
        );
        assert!(requests[1].get("text").is_none());
        let input = requests[1]["input"].to_string();
        assert_eq!(input.matches("FIRST_ORCHID").count(), 1);
        assert_eq!(input.matches("SECOND_IRIS").count(), 1);
        assert!(input.find("FIRST_ORCHID").unwrap() < input.find("SECOND_IRIS").unwrap());
        assert!(input.find("SECOND_IRIS").unwrap() < input.find("actual").unwrap());
        assert!(!input.contains("wrong"));
        assert_no_response_state(&fixture.pool).await;
    }

    #[tokio::test]
    async fn another_lane_cannot_consume_pending_steering_and_superseded_parent_is_rejected() {
        let mock = MockResponsesServer::start(vec![
            sse_function_call_response("upstream_parent", "get_project_status"),
            sse_response("upstream_branch", "msg_branch", "BRANCH"),
            sse_response("upstream_successor", "msg_final", "UPDATED"),
        ])
        .await;
        let fixture = storage_backed_state(&mock.url).await;
        let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        send_json(
            &mut ws,
            json!({"type":"response.create", "model":"test-model", "input":"INITIAL",
            "stream_id":"lane-a", "store":true, "tools":[status_tool()],
            "tool_choice":{"type":"function", "name":"get_project_status"}}),
        )
        .await;
        let completed = receive(&mut ws, "response.completed").await;
        let parent = completed["response"]["id"].as_str().unwrap();
        send_json(
            &mut ws,
            json!({"type":"response.steer", "previous_response_id":parent,
            "input":"LANE_A_ORCHID"}),
        )
        .await;
        let accepted = receive(&mut ws, "response.steer.accepted").await;
        assert_eq!(accepted["stream_id"], "lane-a");
        assert_eq!(receive(&mut ws, "response.steer.pending").await["stream_id"], "lane-a");
        for lane in ["lane-b", "lane-a"] {
            send_json(
                &mut ws,
                json!({"type":"response.create", "model":"test-model", "store":true,
                "stream_id":lane, "previous_response_id":parent, "tools":[],
                "input":[{"type":"function_call_output", "call_id":"call_1", "output":"actual"}]}),
            )
            .await;
            let terminal = receive(&mut ws, "response.completed").await;
            assert_eq!(terminal["stream_id"], lane);
        }
        let requests = mock.request_bodies().await;
        assert_eq!(requests.len(), 3);
        assert!(
            requests[1].get("tool_choice").is_none(),
            "explicit tools reset the inherited forced choice"
        );
        assert!(requests[2].get("tool_choice").is_none());
        assert!(!requests[1]["input"].to_string().contains("LANE_A_ORCHID"));
        assert_eq!(requests[2]["input"].to_string().matches("LANE_A_ORCHID").count(), 1);
        send_json(
            &mut ws,
            json!({"type":"response.steer", "previous_response_id":parent,
            "input":"DO_NOT_RETARGET"}),
        )
        .await;
        let failed = recv_json(&mut ws).await;
        assert_eq!(failed["type"], "response.steer.failed");
        assert_eq!(failed["error"]["code"], "response_not_active");
        assert_eq!(failed["steer"]["input"], "DO_NOT_RETARGET");
        assert_eq!(failed["stream_id"], "lane-a");
        assert!(failed["steer"].get("id").is_none());
    }

    #[tokio::test]
    async fn invalid_input_returns_original_shape_without_closing_or_evicting_parent() {
        let mock = MockResponsesServer::start(vec![
            sse_response("upstream_parent", "msg_first", "READY"),
            sse_response("upstream_next", "msg_next", "CONTINUED"),
        ])
        .await;
        let (gateway_url, _gateway) = spawn_gateway(persistence_disabled_state(&mock.url)).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        send_json(
            &mut ws,
            json!({"type":"response.create", "model":"test-model", "store":false,
            "input":"INITIAL"}),
        )
        .await;
        let terminal = receive(&mut ws, "response.completed").await;
        let parent = terminal["response"]["id"].as_str().unwrap();
        for input in [
            json!([{"role":"assistant", "content":"RAW_ASSISTANT", "unknown":null}]),
            json!([{"role":"user", "content":"RAW_METADATA", "id":null}]),
            json!([]),
        ] {
            send_json(
                &mut ws,
                json!({"type":"response.steer", "previous_response_id":parent,
                "input":input}),
            )
            .await;
            let failed = recv_json(&mut ws).await;
            assert_eq!(failed["type"], "response.steer.failed");
            assert_eq!(failed["error"]["code"], "invalid_input");
            assert_eq!(failed["steer"]["input"], input);
            assert!(failed["steer"].get("id").is_none());
        }
        send_json(
            &mut ws,
            json!({"type":"response.create", "model":"test-model", "store":false,
            "previous_response_id":parent, "input":"ORDINARY_CONTINUATION"}),
        )
        .await;
        assert_eq!(
            receive(&mut ws, "response.completed").await["response"]["previous_response_id"],
            parent
        );
        assert_eq!(mock.request_bodies().await.len(), 2);
    }

    #[tokio::test]
    async fn reconnect_discards_pending_steering_but_retains_stored_tool_continuation() {
        let mock = MockResponsesServer::start(vec![
            sse_function_call_response("upstream_parent", "get_project_status"),
            sse_response("upstream_resumed", "msg_resumed", "RESUMED"),
        ])
        .await;
        let fixture = storage_backed_state(&mock.url).await;
        let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        send_json(
            &mut ws,
            json!({"type":"response.create", "model":"test-model", "store":true,
            "input":"INITIAL", "tools":[status_tool()],
            "tool_choice":{"type":"function", "name":"get_project_status"}}),
        )
        .await;
        let completed = receive(&mut ws, "response.completed").await;
        let parent = completed["response"]["id"].as_str().unwrap();
        send_json(
            &mut ws,
            json!({"type":"response.steer", "previous_response_id":parent,
            "input":"UNAPPLIED_ORCHID"}),
        )
        .await;
        receive(&mut ws, "response.steer.accepted").await;
        receive(&mut ws, "response.steer.pending").await;
        ws.close(None).await.unwrap();
        recv_close_or_end(&mut ws).await;
        let mut resumed = connect_responses_ws(&gateway_url).await;
        send_json(
            &mut resumed,
            json!({"type":"response.steer", "previous_response_id":parent,
            "input":"UNKNOWN_TARGET_IRIS"}),
        )
        .await;
        let failed = recv_json(&mut resumed).await;
        assert_eq!(failed["type"], "response.steer.failed");
        assert_eq!(failed["error"]["code"], "response_not_found");
        send_json(
            &mut resumed,
            json!({"type":"response.create", "model":"test-model", "store":true,
            "previous_response_id":parent, "tools":[],
            "input":[{"type":"function_call_output", "call_id":"call_1", "output":"actual"}]}),
        )
        .await;
        let terminal = receive(&mut resumed, "response.completed").await;
        assert_eq!(terminal["response"]["previous_response_id"], parent);
        let requests = mock.request_bodies().await;
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1].get("tool_choice").is_none(),
            "reconnect uses explicit tools and default choice"
        );
        let resumed_input = requests[1]["input"].to_string();
        assert!(resumed_input.contains("INITIAL"));
        assert!(resumed_input.contains("actual"));
        assert!(!resumed_input.contains("UNAPPLIED_ORCHID"));
        assert!(!resumed_input.contains("UNKNOWN_TARGET_IRIS"));
    }
}

struct MockResponsesServer {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    handle: tokio::task::JoinHandle<()>,
}

struct MockYouSearchServer {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockYouSearchServer {
    async fn start() -> Self {
        Self::start_with_gate(None).await
    }

    async fn start_with_gate(gate: Option<(Arc<tokio::sync::Semaphore>, Arc<tokio::sync::Semaphore>)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let route_requests = Arc::clone(&requests);

        let app = Router::new().route(
            "/v1/search",
            get(move |uri: Uri| {
                let requests = Arc::clone(&route_requests);
                let gate = gate.clone();
                async move {
                    let body = query_params_as_json(&uri);
                    requests.lock().await.push(body);
                    if let Some((arrived, release)) = gate {
                        arrived.add_permits(1);
                        release.acquire().await.unwrap().forget();
                    }
                    axum::Json(json!({
                        "results": {
                            "web": [{
                                "title": "Rust async guide",
                                "url": "https://example.com/rust-async",
                                "snippet": "Async Rust reference"
                            }],
                            "news": []
                        },
                        "metadata": {"provider": "mock-you"}
                    }))
                    .into_response()
                }
            }),
        );

        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url: format!("http://{addr}"),
            requests,
            handle,
        }
    }

    async fn request_bodies(&self) -> Vec<Value> {
        self.requests.lock().await.clone()
    }
}

fn query_params_as_json(uri: &Uri) -> Value {
    let mut params = serde_json::Map::new();
    for (key, value) in url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
        params.insert(key.into_owned(), Value::String(value.into_owned()));
    }
    Value::Object(params)
}

impl Drop for MockYouSearchServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

enum MockResponse {
    Static(String),
    Gated {
        response: String,
        arrived: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
    ObservedGated {
        response: String,
        arrived: oneshot::Sender<Value>,
        release: oneshot::Receiver<()>,
    },
    ObservedPhased {
        first_chunk: String,
        remaining_chunks: String,
        arrived: oneshot::Sender<Value>,
        release: oneshot::Receiver<()>,
    },
    Hanging {
        first_chunk: String,
        drop_tx: oneshot::Sender<()>,
    },
}

struct HangingSse {
    first_chunk: Option<Bytes>,
    drop_tx: Option<oneshot::Sender<()>>,
}

impl HangingSse {
    fn new(first_chunk: String, drop_tx: oneshot::Sender<()>) -> Self {
        Self {
            first_chunk: Some(Bytes::from(first_chunk)),
            drop_tx: Some(drop_tx),
        }
    }
}

impl futures::Stream for HangingSse {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(first_chunk) = self.first_chunk.take() {
            Poll::Ready(Some(Ok(first_chunk)))
        } else {
            Poll::Pending
        }
    }
}

impl Drop for HangingSse {
    fn drop(&mut self) {
        if let Some(drop_tx) = self.drop_tx.take() {
            let _ = drop_tx.send(());
        }
    }
}

impl MockResponsesServer {
    async fn start(responses: Vec<String>) -> Self {
        Self::start_with_responses(responses.into_iter().map(MockResponse::Static).collect()).await
    }

    async fn start_hanging(first_chunk: String) -> (Self, oneshot::Receiver<()>) {
        let (drop_tx, drop_rx) = oneshot::channel();
        let server = Self::start_with_responses(vec![MockResponse::Hanging { first_chunk, drop_tx }]).await;
        (server, drop_rx)
    }

    async fn start_gated(response: String) -> (Self, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (arrived_tx, arrived_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let server = Self::start_with_responses(vec![MockResponse::Gated {
            response,
            arrived: arrived_tx,
            release: release_rx,
        }])
        .await;
        (server, arrived_rx, release_tx)
    }

    async fn start_with_responses(responses: Vec<MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let route_queue = Arc::clone(&queue);
        let route_requests = Arc::clone(&requests);

        let app = Router::new().route(
            "/v1/responses",
            post(move |body: Bytes| {
                let queue = Arc::clone(&route_queue);
                let requests = Arc::clone(&route_requests);
                async move {
                    let body = serde_json::from_slice::<Value>(&body).expect("request body should be JSON");
                    requests.lock().await.push(body.clone());
                    let response = queue.lock().await.pop_front().expect("mock response queue exhausted");
                    let body = match response {
                        MockResponse::Static(response) => axum::body::Body::from(response),
                        MockResponse::Gated {
                            response,
                            arrived,
                            release,
                        } => {
                            let _ = arrived.send(());
                            let _ = release.await;
                            axum::body::Body::from(response)
                        }
                        MockResponse::ObservedGated {
                            response,
                            arrived,
                            release,
                        } => {
                            let _ = arrived.send(body);
                            let _ = release.await;
                            axum::body::Body::from(response)
                        }
                        MockResponse::ObservedPhased {
                            first_chunk,
                            remaining_chunks,
                            arrived,
                            release,
                        } => {
                            let _ = arrived.send(body);
                            let first =
                                futures::stream::once(async move { Ok::<_, Infallible>(Bytes::from(first_chunk)) });
                            let remaining = futures::stream::once(async move {
                                let _ = release.await;
                                Ok::<_, Infallible>(Bytes::from(remaining_chunks))
                            });
                            axum::body::Body::from_stream(first.chain(remaining))
                        }
                        MockResponse::Hanging { first_chunk, drop_tx } => {
                            axum::body::Body::from_stream(HangingSse::new(first_chunk, drop_tx))
                        }
                    };
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
                        .body(body)
                        .unwrap()
                        .into_response()
                }
            }),
        );

        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url: format!("http://{addr}"),
            requests,
            handle,
        }
    }

    async fn request_bodies(&self) -> Vec<Value> {
        self.requests.lock().await.clone()
    }
}

impl Drop for MockResponsesServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct TestDb {
    path: PathBuf,
}

impl TestDb {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("agentic_ws_test_{}.db", uuid::Uuid::now_v7()));
        Self { path }
    }

    fn url(&self) -> String {
        format!("sqlite://{}", self.path.display())
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("db-shm"));
        let _ = std::fs::remove_file(self.path.with_extension("db-wal"));
    }
}

struct StorageBackedState {
    state: AppState,
    pool: Arc<DbPool>,
    _db: TestDb,
}

async fn storage_backed_state(llm_url: &str) -> StorageBackedState {
    storage_backed_state_with(llm_url, None, None).await
}

async fn storage_backed_state_with_responses_config(llm_url: &str, responses: ResponsesConfig) -> StorageBackedState {
    storage_backed_state_with(llm_url, None, Some(responses)).await
}

fn persistence_disabled_state(llm_url: &str) -> AppState {
    let config = test_config(llm_url);
    let client = Arc::new(reqwest::Client::new());
    let exec_ctx = Arc::new(ExecutionContext::new(
        ConversationHandler::new(ConversationStore::disabled()),
        ResponseHandler::new(ResponseStore::disabled()),
        client,
        config.llm_api_base.clone(),
    ));
    let proxy_state = ProxyState::new(config.clone()).expect("proxy state");
    AppState {
        proxy_state,
        exec_ctx,
        llm_readiness_client: agentic_core::readiness::llm_readiness_client().expect("readiness client"),
        readiness_tracker: agentic_server::app::ReadinessTracker::default(),
        shutdown_token: CancellationToken::new(),
        websocket_tracker: WebSocketTracker::default(),
        llm_api_base: config.llm_api_base,
        skip_llm_ready_check: config.skip_llm_ready_check,
        openai_api_key: config.openai_api_key,
        model_capabilities: std::sync::Arc::default(),
        max_request_body_size: DEFAULT_MAX_REQUEST_BODY_SIZE,
    }
}

async fn storage_backed_state_with_web_search(llm_url: &str, web_search_base_url: Option<&str>) -> StorageBackedState {
    storage_backed_state_with(llm_url, web_search_base_url, None).await
}

async fn storage_backed_state_with(
    llm_url: &str,
    web_search_base_url: Option<&str>,
    responses: Option<ResponsesConfig>,
) -> StorageBackedState {
    let db = TestDb::new();
    let db_url = db.url();
    let pool = create_pool_with_schema(Some(&db_url)).await.unwrap();
    let config = test_config(llm_url);
    let client = Arc::new(reqwest::Client::new());
    let mut exec_ctx = ExecutionContext::new(
        ConversationHandler::new(ConversationStore::new(Arc::clone(&pool))),
        ResponseHandler::new(ResponseStore::new(Arc::clone(&pool))),
        Arc::clone(&client),
        config.llm_api_base.clone(),
    );
    if let Some(base_url) = web_search_base_url {
        exec_ctx = exec_ctx.with_gateway_executor(Arc::new(WebSearchHandler::with_api_key(
            client,
            "test-you-key".to_owned(),
            base_url,
        )));
    }
    if let Some(responses) = responses {
        exec_ctx = exec_ctx.with_responses_config(responses);
    }
    let exec_ctx = Arc::new(exec_ctx);
    let proxy_state = ProxyState::new(config.clone()).expect("proxy state");

    let state = AppState {
        proxy_state,
        exec_ctx,
        llm_readiness_client: agentic_core::readiness::llm_readiness_client().expect("readiness client"),
        readiness_tracker: agentic_server::app::ReadinessTracker::default(),
        shutdown_token: CancellationToken::new(),
        websocket_tracker: WebSocketTracker::default(),
        llm_api_base: config.llm_api_base,
        skip_llm_ready_check: config.skip_llm_ready_check,
        openai_api_key: config.openai_api_key,
        model_capabilities: std::sync::Arc::default(),
        max_request_body_size: DEFAULT_MAX_REQUEST_BODY_SIZE,
    };
    StorageBackedState { state, pool, _db: db }
}

/// Deliberately tiny ceiling used to exercise the limit without large allocations.
const SMALL_REQUEST_SIZE_LIMIT: std::num::NonZeroUsize = std::num::NonZeroUsize::new(500).expect("nonzero");

const COMPETING_RESPONSE_ID: &str = "resp_competing";
const CONFLICT_MESSAGE: &str = "conversation changed while the response was being generated; retry the request";

async fn create_conversation(gateway_url: &str) -> String {
    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/conversations"))
        .json(&json!({"store": true}))
        .send()
        .await
        .expect("conversation request");
    assert_eq!(response.status(), StatusCode::OK);
    response.json::<Value>().await.expect("conversation response JSON")["id"]
        .as_str()
        .expect("conversation ID")
        .to_owned()
}

fn competing_turn_items() -> Vec<InOutItem> {
    Vec::<InputItem>::from(&ResponsesInput::Text("competing turn".to_owned()))
        .into_iter()
        .map(InOutItem::Input)
        .collect()
}

async fn persist_competing_turn(pool: &Arc<DbPool>, conversation_id: &str) {
    ConversationStore::new(Arc::clone(pool))
        .persist(
            conversation_id,
            COMPETING_RESPONSE_ID,
            None,
            competing_turn_items(),
            &ResponseMetadata {
                multi_agent_tree: None,
                model: "competing-model".to_owned(),
                ..ResponseMetadata::default()
            },
        )
        .await
        .expect("competing turn should persist");
}

async fn assert_conflicting_websocket_turn_not_persisted(
    pool: &Arc<DbPool>,
    conversation_id: &str,
    stale_response_id: &str,
) {
    let conversation_store = ConversationStore::new(Arc::clone(pool));
    assert_eq!(
        conversation_store
            .rehydrate(conversation_id)
            .await
            .expect("conversation history"),
        competing_turn_items()
    );
    let response_store = ResponseStore::new(Arc::clone(pool));
    let competing = response_store
        .get(COMPETING_RESPONSE_ID)
        .await
        .expect("competing response should remain");
    assert_eq!(competing.conversation_id.as_deref(), Some(conversation_id));
    let error = response_store
        .get(stale_response_id)
        .await
        .expect_err("rejected response must not be persisted");
    assert!(error.is_not_found(), "expected missing response, got {error}");
    let item_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items")
        .fetch_one(pool.as_ref())
        .await
        .expect("stored item count");
    assert_eq!(item_count, 1, "rejected turn must not leave orphaned items");
}

fn ws_url(gateway_url: &str) -> String {
    format!("{}/v1/responses", gateway_url.replacen("http://", "ws://", 1))
}

async fn connect_responses_ws(url: &str) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
    let (ws, response) = connect_async(ws_url(url)).await.expect("websocket handshake");
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    ws
}

async fn recv_json(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Value {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for websocket message")
            .expect("websocket should yield a message")
            .expect("websocket message should be ok");
        match message {
            Message::Text(text) => return serde_json::from_str(&text).expect("message should be JSON"),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(frame) => panic!("websocket closed before JSON event: {frame:?}"),
            Message::Binary(_) => panic!("unexpected binary websocket message"),
        }
    }
}

async fn recv_until_completed(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Vec<Value> {
    let mut events = Vec::new();
    loop {
        let event = recv_json(ws).await;
        let is_done = matches!(
            event.get("type").and_then(Value::as_str),
            Some("response.completed" | "response.failed" | "response.incomplete" | "error")
        );
        events.push(event);
        if is_done {
            return events;
        }
    }
}

async fn wait_for_request_count(mock: &MockResponsesServer, count: usize) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if mock.request_bodies().await.len() >= count {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for mock request"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

async fn recv_close_or_end(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) {
    let message = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
        .await
        .expect("timed out waiting for websocket close");
    match message {
        None | Some(Ok(Message::Close(_)) | Err(_)) => {}
        Some(Ok(message)) => panic!("expected websocket close, got {message:?}"),
    }
}

async fn recv_clean_close(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) {
    let message = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
        .await
        .expect("timed out waiting for clean websocket close");
    match message {
        Some(Ok(Message::Close(_))) => ws.flush().await.expect("failed to acknowledge websocket close"),
        None => panic!("websocket ended without a close frame"),
        Some(Err(error)) => panic!("websocket close failed: {error}"),
        Some(Ok(message)) => panic!("expected websocket close, got {message:?}"),
    }
}

async fn send_ping_and_wait_for_pong(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>, payload: Bytes) {
    ws.send(Message::Ping(payload.clone())).await.unwrap();
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .expect("timed out waiting for websocket pong")
            .expect("websocket should yield a message")
            .expect("websocket message should be ok");
        match message {
            Message::Pong(actual) => {
                assert_eq!(actual, payload);
                break;
            }
            Message::Ping(_) | Message::Frame(_) => {}
            Message::Text(text) => panic!("unexpected text before pong: {text}"),
            Message::Close(frame) => panic!("websocket closed before pong: {frame:?}"),
            Message::Binary(_) => panic!("unexpected binary websocket message"),
        }
    }
}

async fn send_json(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>, value: Value) {
    ws.send(Message::Text(value.to_string().into())).await.unwrap();
}

fn sse_response(response_id: &str, message_id: &str, text: &str) -> String {
    let created = json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": {"id": response_id, "status": "in_progress"}
    });
    let added = json!({
        "type": "response.output_item.added",
        "sequence_number": 1,
        "output_index": 0,
        "item": {"id": message_id, "type": "message"}
    });
    let delta = json!({
        "type": "response.output_text.delta",
        "sequence_number": 2,
        "item_id": message_id,
        "output_index": 0,
        "content_index": 0,
        "delta": text
    });
    let completed = json!({
        "type": "response.completed",
        "sequence_number": 3,
        "response": {"id": response_id, "status": "completed", "usage": null}
    });
    format!("data: {created}\n\ndata: {added}\n\ndata: {delta}\n\ndata: {completed}\n\ndata: [DONE]\n\n")
}

fn sse_response_with_service_tier(response_id: &str, message_id: &str, text: &str, service_tier: &str) -> String {
    sse_response(response_id, message_id, text).replace(
        "\"status\":\"completed\",\"usage\":null",
        &format!("\"status\":\"completed\",\"service_tier\":\"{service_tier}\",\"usage\":null"),
    )
}

#[tokio::test]
async fn websocket_forwards_provider_extensions_without_event_metadata() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_upstream", "msg_upstream", "OK")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    send_json(
        &mut ws,
        json!({
            "type": "response.create", "model": "test-model", "input": "hello",
            "store": false, "generate": true, "stream_id": "provider-test",
            "chat_template_kwargs": {"enable_thinking": false}, "top_k": 20
        }),
    )
    .await;
    recv_until_completed(&mut ws).await;
    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["chat_template_kwargs"], json!({"enable_thinking": false}));
    assert_eq!(requests[0]["top_k"], 20);
    assert_eq!(requests[0]["store"], false);
    for field in ["type", "generate", "stream_id"] {
        assert!(requests[0].get(field).is_none(), "{field} is not a provider extension");
    }
}

#[tokio::test]
async fn websocket_preserves_actual_service_tier_without_inheriting_request_tier() {
    let mock = MockResponsesServer::start(vec![
        sse_response_with_service_tier("resp_upstream_1", "msg_upstream_1", "first", "default"),
        sse_response_with_service_tier("resp_upstream_2", "msg_upstream_2", "second", "flex"),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create", "model": "test-model", "input": "first",
            "service_tier": "priority", "store": false, "stream": true
        }),
    )
    .await;
    let first = recv_until_completed(&mut ws).await;
    let first_response = &first.last().expect("first terminal event")["response"];
    assert_eq!(first_response["service_tier"], "default");
    let first_response_id = first_response["id"].as_str().expect("first response id");

    send_json(
        &mut ws,
        json!({
            "type": "response.create", "model": "test-model", "input": "second",
            "previous_response_id": first_response_id, "store": false, "stream": true
        }),
    )
    .await;
    let second = recv_until_completed(&mut ws).await;
    assert_eq!(
        second.last().expect("second terminal event")["response"]["service_tier"],
        "flex"
    );

    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["service_tier"], "priority");
    assert!(requests[1].get("service_tier").is_none());
}

#[tokio::test]
async fn websocket_echoes_max_tool_calls_without_inheriting_it_and_rejects_invalid_values() {
    let mock = MockResponsesServer::start(vec![
        sse_response("resp_upstream_1", "msg_upstream_1", "first"),
        sse_response("resp_upstream_2", "msg_upstream_2", "second"),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({"type": "response.create", "model": "test-model", "input": "bad", "max_tool_calls": 0}),
    )
    .await;
    let rejected = recv_json(&mut ws).await;
    assert_eq!(rejected["type"], "error");
    assert_eq!(rejected["status"], 400);
    assert_eq!(rejected["error"]["code"], "integer_below_min_value");
    assert_eq!(rejected["error"]["param"], "max_tool_calls");

    send_json(
        &mut ws,
        json!({"type": "response.create", "model": "test-model", "input": "first", "max_tool_calls": 4}),
    )
    .await;
    let first = recv_until_completed(&mut ws).await;
    assert_eq!(first[0]["type"], "response.created");
    assert_eq!(first[0]["response"]["max_tool_calls"], 4);
    let first_response = &first.last().expect("first terminal event")["response"];
    assert_eq!(first_response["max_tool_calls"], 4);
    let first_response_id = first_response["id"].as_str().expect("first response id");

    send_json(
        &mut ws,
        json!({
            "type": "response.create", "model": "test-model", "input": "second",
            "previous_response_id": first_response_id
        }),
    )
    .await;
    let second = recv_until_completed(&mut ws).await;
    assert_eq!(
        second.last().expect("second terminal event")["response"]["max_tool_calls"],
        Value::Null
    );
    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 2, "the invalid request never reaches the upstream");
    assert!(requests.iter().all(|request| request.get("max_tool_calls").is_none()));
}

#[tokio::test]
async fn websocket_rejects_unsupported_parameters_and_keeps_the_connection() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_upstream_1", "msg_upstream_1", "first")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    for (param, value, code, message) in [
        (
            "background",
            json!(true),
            "unsupported_value",
            "background mode is not supported; omit `background` or set it to false",
        ),
        (
            "prompt",
            json!({"id": "pmpt_1"}),
            "unsupported_parameter",
            "reusable prompt templates are not supported; send `instructions` and `input` instead of `prompt`",
        ),
    ] {
        let mut request = json!({"type": "response.create", "model": "test-model", "input": "bad"});
        request[param] = value;
        send_json(&mut ws, request).await;
        assert_eq!(
            recv_json(&mut ws).await,
            json!({
                "type": "error",
                "status": 400,
                "error": {"message": message, "type": "invalid_request_error", "code": code, "param": param}
            })
        );
    }

    send_json(
        &mut ws,
        json!({"type": "response.create", "model": "test-model", "input": "first", "background": false}),
    )
    .await;
    let events = recv_until_completed(&mut ws).await;
    assert_eq!(events.last().expect("terminal event")["type"], "response.completed");
    assert_eq!(
        mock.request_bodies().await.len(),
        1,
        "only the supported request reaches the upstream"
    );
}

/// Two message items whose combined text is larger than any single delta, so
/// the terminal `response.completed` snapshot is the largest event of the stream.
fn two_message_sse_response(response_id: &str, text_bytes: usize) -> String {
    let text = "x".repeat(text_bytes);
    let mut events = vec![json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": {"id": response_id, "status": "in_progress"}
    })];
    for (index, message_id) in ["msg_big_0", "msg_big_1"].iter().enumerate() {
        events.push(json!({
            "type": "response.output_item.added",
            "output_index": index,
            "item": {"id": message_id, "type": "message", "role": "assistant", "status": "in_progress", "content": []}
        }));
        events.push(json!({
            "type": "response.output_text.delta",
            "item_id": message_id,
            "output_index": index,
            "content_index": 0,
            "delta": text
        }));
        events.push(json!({
            "type": "response.output_item.done",
            "output_index": index,
            "item": {
                "id": message_id, "type": "message", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": text, "annotations": []}]
            }
        }));
    }
    events.push(json!({
        "type": "response.completed",
        "response": {"id": response_id, "status": "completed", "usage": null}
    }));
    let mut body = String::new();
    for (sequence_number, mut event) in events.into_iter().enumerate() {
        event["sequence_number"] = json!(sequence_number);
        write!(body, "data: {event}\n\n").expect("writing to a String cannot fail");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

async fn count_rows(pool: &DbPool, table: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

fn phased_sse_response(response_id: &str, message_id: &str, text: &str) -> (String, String) {
    let response = sse_response(response_id, message_id, text);
    let split_at = response.find("\n\n").expect("SSE response has a first event") + 2;
    let (first_chunk, remaining_chunks) = response.split_at(split_at);
    (first_chunk.to_owned(), remaining_chunks.to_owned())
}

fn sse_failed_response() -> String {
    let created = json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": {"id": "resp_failed_upstream", "status": "in_progress"}
    });
    let failed = json!({
        "type": "response.failed",
        "sequence_number": 1,
        "response": {
            "id": "resp_failed_upstream",
            "status": "failed",
            "error": {
                "code": "tool_catalog_too_large",
                "message": "Too many tools"
            },
            "incomplete_details": {
                "reason": "upstream_error"
            },
            "usage": null
        }
    });
    format!("data: {created}\n\ndata: {failed}\n\ndata: [DONE]\n\n")
}

fn sse_function_call_response(response_id: &str, call_name: &str) -> String {
    let created = json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": {"id": response_id, "status": "in_progress"}
    });
    let added = json!({
        "type": "response.output_item.added",
        "sequence_number": 1,
        "output_index": 0,
        "item": {
            "id": "fc_upstream_1",
            "type": "function_call",
            "status": "in_progress",
            "name": call_name,
            "call_id": "call_1",
            "arguments": ""
        }
    });
    let done = json!({
        "type": "response.output_item.done",
        "sequence_number": 2,
        "output_index": 0,
        "item": {
            "id": "fc_upstream_1",
            "type": "function_call",
            "status": "completed",
            "name": call_name,
            "call_id": "call_1",
            "arguments": "{\"numbers\":[8,0]}"
        }
    });
    let completed = json!({
        "type": "response.completed",
        "sequence_number": 3,
        "response": {"id": response_id, "status": "completed", "usage": null}
    });
    format!("data: {created}\n\ndata: {added}\n\ndata: {done}\n\ndata: {completed}\n\ndata: [DONE]\n\n")
}

fn sse_tool_search_call_response() -> String {
    let created = json!({
        "type": "response.created",
        "response": {
            "id": "resp_search", "status": "in_progress",
            "tools": [{"type": "function", "name": "tool_search", "parameters": {"type": "object"}}]
        }
    });
    let in_progress = json!({
        "type": "response.in_progress",
        "response": {
            "id": "resp_search", "status": "in_progress",
            "tools": [{"type": "function", "name": "tool_search", "parameters": {"type": "object"}}]
        }
    });
    let added = json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "fc_search", "type": "function_call", "status": "in_progress",
            "name": "tool_search", "call_id": "call_search", "arguments": ""}
    });
    let delta = json!({
        "type": "response.function_call_arguments.delta", "output_index": 0,
        "item_id": "fc_search", "delta": "{\"query\":\"weather\"}"
    });
    let arguments_done = json!({
        "type": "response.function_call_arguments.done", "output_index": 0,
        "item_id": "fc_search", "name": "tool_search", "arguments": "{\"query\":\"weather\"}"
    });
    let done = json!({
        "type": "response.output_item.done", "output_index": 0,
        "item": {"id": "fc_search", "type": "function_call", "status": "completed",
            "name": "tool_search", "call_id": "call_search", "arguments": "{\"query\":\"weather\"}"}
    });
    let completed = json!({
        "type": "response.completed", "response": {"id": "resp_search", "status": "completed", "usage": null}
    });
    format!(
        "data: {created}\n\ndata: {in_progress}\n\ndata: {added}\n\ndata: {delta}\n\ndata: {arguments_done}\n\ndata: {done}\n\ndata: {completed}\n\ndata: [DONE]\n\n"
    )
}

fn sse_weather_function_call_response() -> String {
    let created = json!({
        "type": "response.created", "response": {"id": "resp_weather", "status": "in_progress"}
    });
    let added = json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"id": "fc_weather", "type": "function_call", "status": "in_progress",
            "name": "get_weather", "call_id": "call_weather", "arguments": ""}
    });
    let done = json!({
        "type": "response.output_item.done", "output_index": 0,
        "item": {"id": "fc_weather", "type": "function_call", "status": "completed",
            "name": "get_weather", "call_id": "call_weather", "arguments": "{\"city\":\"Paris\"}"}
    });
    let completed = json!({
        "type": "response.completed", "response": {"id": "resp_weather", "status": "completed", "usage": null}
    });
    format!("data: {created}\n\ndata: {added}\n\ndata: {done}\n\ndata: {completed}\n\ndata: [DONE]\n\n")
}

fn tool_search_declaration() -> Value {
    json!({
        "type": "tool_search",
        "execution": "client",
        "description": "Search the client catalog",
        "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}
    })
}

fn deferred_weather_function() -> Value {
    json!({
        "type": "function",
        "name": "get_weather",
        "description": "Get weather",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
        "defer_loading": true
    })
}

fn sse_custom_tool_call_response() -> String {
    let created = json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": {"id": "resp_custom", "status": "in_progress"}
    });
    let added = json!({
        "type": "response.output_item.added",
        "sequence_number": 1,
        "output_index": 0,
        "item": {
            "id": "fc_upstream_1",
            "type": "function_call",
            "status": "in_progress",
            "name": "apply_patch",
            "call_id": "call_custom_1",
            "arguments": ""
        }
    });
    let delta = json!({
        "type": "response.function_call_arguments.delta",
        "sequence_number": 2,
        "output_index": 0,
        "item_id": "fc_upstream_1",
        "delta": "{\"input\":\"*** Begin Patch\\n*** End Patch\"}"
    });
    let input_done = json!({
        "type": "response.function_call_arguments.done",
        "sequence_number": 3,
        "output_index": 0,
        "item_id": "fc_upstream_1",
        "name": "apply_patch",
        "arguments": "{\"input\":\"*** Begin Patch\\n*** End Patch\"}"
    });
    let item_done = json!({
        "type": "response.output_item.done",
        "sequence_number": 4,
        "output_index": 0,
        "item": {
            "id": "fc_upstream_1",
            "type": "function_call",
            "status": "completed",
            "name": "apply_patch",
            "call_id": "call_custom_1",
            "arguments": "{\"input\":\"*** Begin Patch\\n*** End Patch\"}"
        }
    });
    let completed = json!({
        "type": "response.completed",
        "sequence_number": 5,
        "response": {"id": "resp_custom", "status": "completed", "usage": null}
    });
    format!(
        "data: {created}\n\ndata: {added}\n\ndata: {delta}\n\ndata: {input_done}\n\ndata: {item_done}\n\ndata: {completed}\n\ndata: [DONE]\n\n"
    )
}

fn web_search_function_call_sse_response() -> String {
    let created = json!({
        "type": "response.created",
        "sequence_number": 0,
        "response": {"id": "resp_tool_call", "status": "in_progress", "usage": null}
    });
    let added = json!({
        "type": "response.output_item.added",
        "sequence_number": 1,
        "output_index": 0,
        "item": {
            "id": "fc_search",
            "type": "function_call",
            "call_id": "call_search",
            "name": "web_search",
            "arguments": "",
            "status": "in_progress"
        }
    });
    let done = json!({
        "type": "response.function_call_arguments.done",
        "sequence_number": 2,
        "item_id": "fc_search",
        "output_index": 0,
        "call_id": "call_search",
        "name": "web_search",
        "arguments": "{\"query\":\"rust async\",\"count\":2}"
    });
    let completed = json!({
        "type": "response.completed",
        "sequence_number": 3,
        "response": {"id": "resp_tool_call", "status": "completed", "usage": null}
    });
    format!("data: {created}\n\ndata: {added}\n\ndata: {done}\n\ndata: {completed}\n\ndata: [DONE]\n\n")
}

#[tokio::test]
async fn websocket_generate_false_rejects_oversized_events_before_persistence() {
    assert_oversized_local_completion_rejected(false).await;
}

#[tokio::test]
async fn websocket_generate_false_checks_completed_event_before_persistence_or_delivery() {
    assert_oversized_local_completion_rejected(true).await;
}

/// Event ceiling used by the oversized local-completion tests. Pinned so the
/// fill computation does not track the configured default.
const LOCAL_COMPLETION_EVENT_LIMIT: usize = 1024 * 1024;

async fn assert_oversized_local_completion_rejected(only_completion_oversized: bool) {
    let fixture = storage_backed_state_with_responses_config(
        "http://127.0.0.1:9",
        ResponsesConfig {
            max_retained_bytes: LOCAL_COMPLETION_EVENT_LIMIT / 2,
            max_stream_event_bytes: LOCAL_COMPLETION_EVENT_LIMIT,
            ..ResponsesConfig::default()
        },
    )
    .await;
    let (gateway_url, gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    let mut payload = json!({
        "type": "response.create", "model": "test-model", "input": [],
        "generate": false, "stream_id": "oversized-local", "instructions": ""
    });
    let instruction_bytes = if only_completion_oversized {
        send_json(&mut ws, payload.clone()).await;
        let baseline = recv_until_completed(&mut ws).await;
        assert_eq!(baseline.len(), 2);
        let created_bytes = baseline[0].to_string().len();
        assert!(baseline[1].to_string().len() > created_bytes);
        // Fill the created event exactly to the wire limit. The completed event
        // is larger because it includes usage, so both must be checked up front.
        LOCAL_COMPLETION_EVENT_LIMIT - created_bytes
    } else {
        2 * LOCAL_COMPLETION_EVENT_LIMIT
    };
    payload["instructions"] = json!("x".repeat(instruction_bytes));
    payload["input"] = json!("must not be stored");
    send_json(&mut ws, payload).await;
    let event = recv_json(&mut ws).await;
    assert_eq!(
        event["type"], "error",
        "no lifecycle event may be emitted for an oversized local response"
    );
    assert_eq!(event["stream_id"], "oversized-local");
    assert!(event["error"]["message"].as_str().unwrap().contains("exceeded"));
    assert!(event.to_string().len() <= LOCAL_COMPLETION_EVENT_LIMIT);
    let responses = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM responses")
        .fetch_one(fixture.pool.as_ref())
        .await
        .unwrap();
    assert_eq!(
        responses,
        i64::from(only_completion_oversized),
        "rejected response must not be persisted"
    );
    let items = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items")
        .fetch_one(fixture.pool.as_ref())
        .await
        .unwrap();
    assert_eq!(items, 0, "rejected response must not leave orphaned items");
    gateway.abort();
}

#[tokio::test]
async fn test_websocket_generate_false_prewarm_retains_context_without_inference() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_upstream_1", "msg_upstream_1", "READY")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "instructions": "Follow the warmup rules.",
            "input": [{"type": "message", "role": "user", "content": "warmup prefix"}],
            "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a patch."
            }],
            "generate": false,
            "store": false,
            "stream": true
        }),
    )
    .await;

    let prewarm = recv_until_completed(&mut ws).await;
    assert_eq!(
        prewarm
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["response.created", "response.completed"]
    );
    let prewarm_response = &prewarm.last().unwrap()["response"];
    let prewarm_response_id = prewarm_response["id"].as_str().unwrap().to_owned();
    assert_eq!(prewarm_response["status"], "completed");
    assert_eq!(prewarm_response["output"], json!([]));
    assert!(mock.request_bodies().await.is_empty());

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "instructions": "Follow the warmup rules.",
            "previous_response_id": prewarm_response_id,
            "input": [{"type": "message", "role": "user", "content": "first turn"}],
            "store": false,
            "stream": true
        }),
    )
    .await;

    let turn = recv_until_completed(&mut ws).await;
    assert_eq!(
        turn.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "READY"
    );

    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["instructions"], "Follow the warmup rules.");
    assert_eq!(requests[0]["input"][0]["content"], "warmup prefix");
    assert_eq!(requests[0]["input"][1]["content"], "first turn");
    assert_eq!(requests[0]["tools"][0]["type"], "function");
    assert_eq!(requests[0]["tools"][0]["name"], "apply_patch");
    assert_eq!(
        requests[0]["tools"][0]["parameters"]["properties"]["input"]["type"],
        "string"
    );
    assert!(requests[0].get("generate").is_none());
}

#[tokio::test]
async fn test_websocket_generate_false_prewarm_redacts_mcp_runtime_credentials() {
    let mock = MockResponsesServer::start(vec![]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [],
            "tools": [{
                "type": "mcp",
                "server_label": "counter",
                "server_url": "https://mcp.example.com/mcp",
                "headers": {"X-API-Key": "secret"},
                "authorization": "bearer-secret",
                "require_approval": "never"
            }],
            "generate": false,
            "store": true,
            "stream": true
        }),
    )
    .await;

    let prewarm = recv_until_completed(&mut ws).await;
    let response_id = prewarm.last().unwrap()["response"]["id"]
        .as_str()
        .expect("prewarm response id")
        .to_owned();
    assert!(mock.request_bodies().await.is_empty());

    let request = serde_json::from_value::<RequestPayload>(json!({
        "model": "test-model",
        "input": [],
        "previous_response_id": response_id
    }))
    .expect("lookup request");
    let lookup_ctx = RequestContext {
        multi_agent_tree: None,
        original_request: request.clone(),
        enriched_request: request,
        new_input_items: vec![],
        response_id: "resp_lookup".to_owned(),
        conversation_id: None,
        conversation_version: None,
        continuation: None,
    };
    let stored = fixture
        .state
        .exec_ctx
        .resp_handler
        .get(&lookup_ctx)
        .await
        .expect("stored prewarm response");
    let tools = stored.metadata.effective_tools.expect("persisted tools");
    let ResponsesTool::Mcp(tool) = &tools[0] else {
        panic!("expected MCP tool");
    };

    assert!(tool.headers.is_none());
    assert!(tool.authorization.is_none());
}

#[tokio::test]
async fn test_websocket_generate_false_rejects_mismatched_search_output_without_persistence() {
    let mock = MockResponsesServer::start(vec![]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [
                {
                    "type": "tool_search_call",
                    "id": "tsc_1",
                    "call_id": "call_search_1",
                    "arguments": {"query": "weather"}
                },
                {
                    "type": "tool_search_output",
                    "call_id": "call_mismatch",
                    "tools": [{
                        "type": "mcp",
                        "server_label": "private",
                        "server_url": "https://mcp.example.test/mcp",
                        "headers": {"Authorization": "Bearer must-not-persist"}
                    }]
                }
            ],
            "tools": [],
            "generate": false,
            "store": false,
            "stream": true
        }),
    )
    .await;

    let events = recv_until_completed(&mut ws).await;
    let error = events.last().expect("terminal tool-search error");
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], StatusCode::BAD_REQUEST.as_u16());
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert!(mock.request_bodies().await.is_empty());

    let response_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM responses")
        .fetch_one(fixture.pool.as_ref())
        .await
        .expect("response count");
    let item_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items")
        .fetch_one(fixture.pool.as_ref())
        .await
        .expect("item count");
    assert_eq!(response_count, 0, "fail-closed request must not persist a response");
    assert_eq!(item_count, 0, "fail-closed request must not persist tool-search items");
}

fn assert_public_ws_search_lifecycle(first: &[Value]) -> String {
    let expected_public_tools = json!([tool_search_declaration(), deferred_weather_function()]);
    let response_tool_envelopes = first
        .iter()
        .filter_map(|event| event.get("response"))
        .filter_map(|response| response.get("tools"))
        .collect::<Vec<_>>();
    assert!(!response_tool_envelopes.is_empty());
    assert!(
        response_tool_envelopes
            .iter()
            .all(|tools| *tools == &expected_public_tools)
    );
    assert!(first.iter().all(|event| {
        event["response"]["tools"].as_array().is_none_or(|tools| {
            tools
                .iter()
                .all(|tool| !(tool["type"] == "function" && tool["name"] == "tool_search"))
        })
    }));
    assert_eq!(
        first
            .iter()
            .map(|event| event["sequence_number"].as_u64())
            .collect::<Vec<_>>(),
        (0..u64::try_from(first.len()).unwrap()).map(Some).collect::<Vec<_>>()
    );
    assert!(first.iter().all(|event| {
        !matches!(
            event["type"].as_str(),
            Some("response.function_call_arguments.delta" | "response.function_call_arguments.done" | "error")
        )
    }));
    let lifecycle = first
        .iter()
        .filter(|event| {
            matches!(
                event["type"].as_str(),
                Some("response.output_item.added" | "response.output_item.done")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(lifecycle.len(), 2);
    assert_eq!(lifecycle[0]["item"]["type"], "tool_search_call");
    assert_eq!(lifecycle[1]["item"]["type"], "tool_search_call");
    assert_eq!(lifecycle[0]["item"]["id"], lifecycle[1]["item"]["id"]);
    assert_eq!(lifecycle[0]["output_index"], lifecycle[1]["output_index"]);
    let first_terminal = first.last().expect("first terminal");
    assert_eq!(first_terminal["response"]["output"][0], lifecycle[1]["item"]);
    first_terminal["response"]["id"].as_str().unwrap().to_owned()
}

async fn assert_ws_search_persistence(requests: &[Value], pool: &DbPool) {
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["tools"].as_array().map(Vec::len), Some(1));
    assert_eq!(requests[0]["tools"][0]["name"], "tool_search");
    assert!(
        requests[1]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "get_weather")
    );
    assert!(requests[1]["input"].as_array().unwrap().iter().any(|item| {
        item["type"] == "function_call" && item["name"] == "tool_search" && item["call_id"] == "call_search"
    }));
    assert!(
        requests[2]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| { item["type"] == "function_call_output" && item["call_id"] == "call_weather" })
    );
    let stored_items = sqlx::query_scalar::<_, String>("SELECT data FROM items ORDER BY created_at, seq")
        .fetch_all(pool)
        .await
        .expect("stored public items");
    assert!(stored_items.iter().any(|item| item.contains("tool_search_call")));
    assert!(stored_items.iter().any(|item| item.contains("tool_search_output")));
    assert!(
        stored_items
            .iter()
            .all(|item| !item.contains("\"name\":\"tool_search\""))
    );
}

#[tokio::test]
async fn test_websocket_tool_search_streaming_continuation_is_public_and_persisted() {
    let mock = MockResponsesServer::start(vec![
        sse_tool_search_call_response(),
        sse_weather_function_call_response(),
        sse_response("resp_final", "msg_final", "PARIS_WEATHER_OK"),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "stream_id": "tool-search",
            "model": "test-model",
            "input": "find weather",
            "tools": [tool_search_declaration(), deferred_weather_function()],
            "parallel_tool_calls": false,
            "store": true,
            "stream": true
        }),
    )
    .await;
    let first = recv_until_completed(&mut ws).await;
    let first_response_id = assert_public_ws_search_lifecycle(&first);

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "stream_id": "tool-search",
            "model": "test-model",
            "previous_response_id": first_response_id,
            "input": [{
                "type": "tool_search_output",
                "call_id": "call_search",
                "tools": [deferred_weather_function()]
            }],
            "store": true,
            "stream": true
        }),
    )
    .await;
    let second = recv_until_completed(&mut ws).await;
    let second_terminal = second.last().expect("second terminal");
    assert_eq!(second_terminal["response"]["output"][0]["type"], "function_call");
    assert_eq!(second_terminal["response"]["output"][0]["name"], "get_weather");
    let second_response_id = second_terminal["response"]["id"].as_str().unwrap().to_owned();

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "stream_id": "tool-search",
            "model": "test-model",
            "previous_response_id": second_response_id,
            "input": [{
                "type": "function_call_output",
                "call_id": "call_weather",
                "output": "sunny"
            }],
            "store": true,
            "stream": true
        }),
    )
    .await;
    let third = recv_until_completed(&mut ws).await;
    assert_eq!(
        third.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "PARIS_WEATHER_OK"
    );

    for events in [&first, &second, &third] {
        assert!(events.iter().all(|event| event["stream_id"] == "tool-search"));
        assert_eq!(
            events
                .iter()
                .map(|event| event["sequence_number"].as_u64())
                .collect::<Vec<_>>(),
            (0..u64::try_from(events.len()).unwrap()).map(Some).collect::<Vec<_>>()
        );
    }

    let requests = mock.request_bodies().await;
    assert!(requests.iter().all(|request| request.get("stream_id").is_none()));
    assert_ws_search_persistence(&requests, fixture.pool.as_ref()).await;
}

#[tokio::test]
async fn test_websocket_generate_false_persists_valid_tool_search_state_for_reuse() {
    let mock = MockResponsesServer::start(vec![sse_weather_function_call_response()]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [
                {
                    "type": "tool_search_call", "id": "tsc_prewarm", "call_id": "call_prewarm",
                    "arguments": {"query": "weather"}
                },
                {
                    "type": "tool_search_output", "call_id": "call_prewarm",
                    "tools": [deferred_weather_function()]
                }
            ],
            "tools": [tool_search_declaration(), deferred_weather_function()],
            "parallel_tool_calls": false,
            "generate": false,
            "store": false,
            "stream": true
        }),
    )
    .await;
    let prewarm = recv_until_completed(&mut ws).await;
    assert!(
        mock.request_bodies().await.is_empty(),
        "generate:false must not run inference"
    );
    let response_id = prewarm.last().unwrap()["response"]["id"].as_str().unwrap().to_owned();

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "previous_response_id": response_id,
            "input": "call the loaded weather tool",
            "store": true,
            "stream": true
        }),
    )
    .await;
    let response = recv_until_completed(&mut ws).await;
    assert_eq!(response.last().unwrap()["response"]["output"][0]["name"], "get_weather");

    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "get_weather")
    );
    assert!(
        requests[0]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output")
    );
}

#[tokio::test]
async fn websocket_empty_prewarm_replaces_compacted_conversation_tool_search_state() {
    let mock = MockResponsesServer::start(vec![sse_tool_search_call_response()]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let conversation_id = create_conversation(&gateway_url).await;
    let conversation_store = ConversationStore::new(Arc::clone(&fixture.pool));
    conversation_store
        .persist(
            &conversation_id,
            "resp_before_prewarm",
            None,
            vec![InOutItem::Input(InputItem::Compaction(CompactionItem {
                agent: None,
                id: Some("cmp_before_prewarm".to_owned()),
                encrypted_content: "Existing compacted conversation state".to_owned(),
            }))],
            &ResponseMetadata::default(),
        )
        .await
        .expect("seed compacted conversation state");
    let mut ws = connect_responses_ws(&gateway_url).await;

    for store in [false, true] {
        send_json(
            &mut ws,
            json!({
                "type": "response.create",
                "model": "test-model",
                "conversation_id": conversation_id,
                "input": [],
                "tools": [tool_search_declaration(), deferred_weather_function()],
                "parallel_tool_calls": false,
                "generate": false,
                "store": store,
                "stream": true
            }),
        )
        .await;
        let prewarm = recv_until_completed(&mut ws).await;
        let prewarm_response_id = prewarm.last().unwrap()["response"]["id"]
            .as_str()
            .expect("prewarm response ID")
            .to_owned();
        assert!(
            mock.request_bodies().await.is_empty(),
            "generate:false must not run inference"
        );
        let item_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items WHERE conversation_id = $1")
            .bind(&conversation_id)
            .fetch_one(fixture.pool.as_ref())
            .await
            .expect("conversation item count");
        assert_eq!(item_count, 1, "an empty prewarm must not advance item history");
        let prewarm_checkpoint: String =
            sqlx::query_scalar("SELECT latest_response_id FROM conversations WHERE id = $1")
                .bind(&conversation_id)
                .fetch_one(fixture.pool.as_ref())
                .await
                .expect("prewarm conversation checkpoint");
        assert_eq!(
            prewarm_checkpoint,
            if store {
                prewarm_response_id.as_str()
            } else {
                "resp_before_prewarm"
            }
        );
    }

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "conversation_id": conversation_id,
            "input": "find the weather tool",
            "store": true,
            "stream": true
        }),
    )
    .await;
    let response = recv_until_completed(&mut ws).await;
    assert_eq!(
        response.last().unwrap()["response"]["output"][0]["type"],
        "tool_search_call"
    );

    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["type"] == "function" && tool["name"] == "tool_search")
    );
    assert!(
        requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["name"] != "get_weather"),
        "deferred definitions remain withheld until the client returns them"
    );
}

/// Regression for PR #304 review: the WebSocket transport enforced a fixed
/// 1 MiB event ceiling while the executor validated the terminal frame against
/// the configured 16 MiB. Two 600,000-byte messages streamed every delta, then
/// the socket rejected `response.completed` after the response was persisted.
/// With one configured limit, the aligned transport delivers the full payload.
#[tokio::test]
async fn websocket_delivers_a_terminal_event_larger_than_the_old_fixed_socket_limit() {
    const TEXT_BYTES: usize = 600_000;
    let mock = MockResponsesServer::start(vec![two_message_sse_response("resp_upstream_big", TEXT_BYTES)]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create", "model": "test-model", "stream_id": "big",
            "input": [{"type": "message", "role": "user", "content": "write a lot"}],
            "store": true, "stream": true
        }),
    )
    .await;

    let events = recv_until_completed(&mut ws).await;
    let delivered_text = events
        .iter()
        .filter(|event| event["type"] == "response.output_text.delta")
        .map(|event| event["delta"].as_str().unwrap().len())
        .sum::<usize>();
    assert_eq!(delivered_text, 2 * TEXT_BYTES);
    let terminal = events.last().unwrap();
    assert_eq!(terminal["type"], "response.completed", "{terminal}");
    assert_eq!(terminal["stream_id"], "big");
    assert!(
        terminal.to_string().len() > 1024 * 1024,
        "the terminal event exceeds the old fixed limit"
    );
    assert_eq!(terminal["response"]["output"].as_array().unwrap().len(), 2);
    for item in terminal["response"]["output"].as_array().unwrap() {
        assert_eq!(item["content"][0]["text"].as_str().unwrap().len(), TEXT_BYTES);
    }
    assert!(events.iter().all(|event| event["stream_id"] == "big"));
    assert_eq!(count_rows(&fixture.pool, "responses").await, 1);
}

/// The same stream against a transport whose event ceiling is below the response
/// it must deliver. `ResponsesConfig::validate` rejects this pairing at startup;
/// the executor must still refuse the terminal frame before persisting or
/// publishing a checkpoint, so a client that received every delta but no
/// `response.completed` cannot later find the response stored.
#[tokio::test]
async fn websocket_undeliverable_terminal_event_is_rejected_before_persistence() {
    const TEXT_BYTES: usize = 600_000;
    let mock = MockResponsesServer::start(vec![two_message_sse_response("resp_upstream_big", TEXT_BYTES)]).await;
    let fixture = storage_backed_state_with_responses_config(
        &mock.url,
        ResponsesConfig {
            max_retained_bytes: 4 * 1024 * 1024,
            max_stream_event_bytes: 1024 * 1024,
            ..ResponsesConfig::default()
        },
    )
    .await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create", "model": "test-model", "stream_id": "big",
            "input": [{"type": "message", "role": "user", "content": "write a lot"}],
            "store": true, "stream": true
        }),
    )
    .await;

    let events = recv_until_completed(&mut ws).await;
    let delivered_text = events
        .iter()
        .filter(|event| event["type"] == "response.output_text.delta")
        .map(|event| event["delta"].as_str().unwrap().len())
        .sum::<usize>();
    assert_eq!(
        delivered_text,
        2 * TEXT_BYTES,
        "every delta fits the event limit and is delivered"
    );
    let terminal = events.last().unwrap();
    assert_eq!(terminal["type"], "error", "{terminal}");
    assert_eq!(terminal["stream_id"], "big");
    let message = terminal["error"]["message"].as_str().unwrap();
    assert!(message.contains("stream event exceeded"), "{message}");
    assert!(
        events.iter().all(|event| event["type"] != "response.completed"),
        "an undeliverable terminal event is never announced as completed"
    );
    assert!(events.iter().all(|event| event.to_string().len() <= 1024 * 1024));
    assert_eq!(
        count_rows(&fixture.pool, "responses").await,
        0,
        "rejected response must not be persisted"
    );
    assert_eq!(
        count_rows(&fixture.pool, "items").await,
        0,
        "rejected response must not leave items"
    );
}

#[tokio::test]
async fn test_websocket_first_turn_forwards_incremental_events_and_final_payload() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_upstream_1", "msg_upstream_1", "HELLO")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "reasoning": {"effort": "high"},
            "text": {"format": {"type": "json_object"}, "verbosity": "high"},
            "store": true,
            "stream": true
        }),
    )
    .await;

    let events = recv_until_completed(&mut ws).await;
    let event_types = events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        event_types,
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.completed"
        ]
    );
    assert_ne!(events[0]["response"]["id"], "resp_upstream_1");
    assert_eq!(events[2]["delta"], "HELLO");
    let response = &events.last().unwrap()["response"];
    assert_ne!(response["id"], "resp_upstream_1");
    assert_eq!(response["status"], "completed");
    assert_eq!(response["output"][0]["content"][0]["text"], "HELLO");
    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["stream"], true);
    assert_eq!(requests[0]["input"][0]["content"], "hi");
    assert_eq!(requests[0]["reasoning"], json!({"effort": "high"}));
    assert_eq!(
        requests[0]["text"],
        json!({"format": {"type": "json_object"}, "verbosity": "high"})
    );
    assert!(requests[0].get("type").is_none());
}

#[tokio::test]
async fn websocket_distinct_stream_ids_run_concurrently_and_tag_every_event() {
    // Arrange: hold both upstream responses so the second request can arrive
    // only if the WebSocket handler starts distinct lanes concurrently.
    let (first_arrived_tx, first_arrived_rx) = oneshot::channel();
    let (first_release_tx, first_release_rx) = oneshot::channel();
    let (second_arrived_tx, second_arrived_rx) = oneshot::channel();
    let (second_release_tx, second_release_rx) = oneshot::channel();
    let (first_chunk, first_remaining) = phased_sse_response("resp_upstream_first", "msg_upstream_first", "FIRST");
    let (second_chunk, second_remaining) = phased_sse_response("resp_upstream_second", "msg_upstream_second", "SECOND");
    let mock = MockResponsesServer::start_with_responses(vec![
        MockResponse::Static(sse_response("resp_upstream_parent", "msg_upstream_parent", "PARENT")),
        MockResponse::ObservedPhased {
            first_chunk,
            remaining_chunks: first_remaining,
            arrived: first_arrived_tx,
            release: first_release_rx,
        },
        MockResponse::ObservedPhased {
            first_chunk: second_chunk,
            remaining_chunks: second_remaining,
            arrived: second_arrived_tx,
            release: second_release_rx,
        },
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": "remember the parent context",
            "store": true
        }),
    )
    .await;
    let parent = recv_until_completed(&mut ws).await;
    let parent_response_id = parent
        .last()
        .and_then(|event| event["response"]["id"].as_str())
        .expect("parent response id")
        .to_owned();

    // Act: fork the stored parent into two independent lanes.
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "stream_id": "lane-a",
            "model": "test-model",
            "previous_response_id": &parent_response_id,
            "input": "request-a",
            "metadata": {"test_lane": "lane-a"}
        }),
    )
    .await;
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "stream_id": "lane-b",
            "model": "test-model",
            "previous_response_id": &parent_response_id,
            "input": "request-b",
            "metadata": {"test_lane": "lane-b"}
        }),
    )
    .await;

    let first_request = tokio::time::timeout(std::time::Duration::from_secs(2), first_arrived_rx)
        .await
        .expect("first lane did not reach upstream")
        .expect("first arrival sender dropped");
    let second_request = tokio::time::timeout(std::time::Duration::from_secs(2), second_arrived_rx)
        .await
        .expect("distinct WebSocket lane was serialized behind the active lane")
        .expect("second arrival sender dropped");
    assert!(first_request.get("stream_id").is_none());
    assert!(second_request.get("stream_id").is_none());

    let first_stream_id = first_request["metadata"]["test_lane"]
        .as_str()
        .expect("first request test lane");
    let second_stream_id = second_request["metadata"]["test_lane"]
        .as_str()
        .expect("second request test lane");
    assert_ne!(first_stream_id, second_stream_id);

    // Both lanes publish an initial event before either completes. Releasing
    // the first lane then proves its remaining events can follow an event from
    // the still-active second lane: first, second, first.
    let created_events = [recv_json(&mut ws).await, recv_json(&mut ws).await];
    assert!(created_events.iter().all(|event| event["type"] == "response.created"));
    assert!(created_events.iter().any(|event| event["stream_id"] == first_stream_id));
    assert!(
        created_events
            .iter()
            .any(|event| event["stream_id"] == second_stream_id)
    );

    first_release_tx.send(()).expect("release first upstream response");
    let first_events = recv_until_completed(&mut ws).await;
    assert_eq!(first_events.len(), 3);
    assert!(first_events.iter().all(|event| event["stream_id"] == first_stream_id));

    second_release_tx.send(()).expect("release second upstream response");
    let second_events = recv_until_completed(&mut ws).await;
    assert_eq!(second_events.len(), 3);
    assert!(second_events.iter().all(|event| event["stream_id"] == second_stream_id));
}

#[tokio::test]
async fn websocket_requests_with_the_same_stream_id_run_fifo() {
    let (first_arrived_tx, first_arrived_rx) = oneshot::channel();
    let (first_release_tx, first_release_rx) = oneshot::channel();
    let (second_arrived_tx, mut second_arrived_rx) = oneshot::channel();
    let (second_release_tx, second_release_rx) = oneshot::channel();
    let mock = MockResponsesServer::start_with_responses(vec![
        MockResponse::ObservedGated {
            response: sse_response("resp_upstream_first", "msg_upstream_first", "FIRST"),
            arrived: first_arrived_tx,
            release: first_release_rx,
        },
        MockResponse::ObservedGated {
            response: sse_response("resp_upstream_second", "msg_upstream_second", "SECOND"),
            arrived: second_arrived_tx,
            release: second_release_rx,
        },
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    for input in ["first", "second"] {
        send_json(
            &mut ws,
            json!({
                "type": "response.create",
                "stream_id": "ordered-lane",
                "model": "test-model",
                "input": input
            }),
        )
        .await;
    }

    tokio::time::timeout(std::time::Duration::from_secs(2), first_arrived_rx)
        .await
        .expect("timed out waiting for first request to reach upstream")
        .expect("first arrival sender dropped");
    send_ping_and_wait_for_pong(&mut ws, Bytes::from_static(b"same-lane-request-received")).await;
    assert_eq!(
        mock.request_bodies().await.len(),
        1,
        "second request on the same lane started before the first completed"
    );

    first_release_tx.send(()).expect("release first response");
    let first_events = recv_until_completed(&mut ws).await;
    assert!(first_events.iter().all(|event| event["stream_id"] == "ordered-lane"));

    tokio::time::timeout(std::time::Duration::from_secs(2), &mut second_arrived_rx)
        .await
        .expect("second request did not start after first completed")
        .expect("second arrival sender dropped");
    second_release_tx.send(()).expect("release second response");
    let second_events = recv_until_completed(&mut ws).await;
    assert!(second_events.iter().all(|event| event["stream_id"] == "ordered-lane"));
}

async fn assert_validation_error_waits_for_active_lane(stream_id: Option<&str>) {
    let (mock, arrived, release) =
        MockResponsesServer::start_gated(sse_response("resp_upstream_first", "msg_upstream_first", "FIRST")).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    let mut valid = json!({
        "type": "response.create",
        "model": "test-model",
        "input": "first"
    });
    let mut invalid = json!({
        "type": "response.create",
        "input": "missing model"
    });
    if let Some(stream_id) = stream_id {
        valid["stream_id"] = json!(stream_id);
        invalid["stream_id"] = json!(stream_id);
    }

    send_json(&mut ws, valid).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), arrived)
        .await
        .expect("timed out waiting for first request to reach upstream")
        .expect("first arrival sender dropped");
    send_json(&mut ws, invalid).await;
    send_ping_and_wait_for_pong(&mut ws, Bytes::from_static(b"validation-error-queued")).await;

    release.send(()).expect("release first response");
    let events = recv_until_completed(&mut ws).await;
    assert_eq!(events.last().expect("terminal event")["type"], "response.completed");

    let error = recv_json(&mut ws).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], StatusCode::BAD_REQUEST.as_u16());
    if let Some(stream_id) = stream_id {
        assert_eq!(error["stream_id"], stream_id);
    } else {
        assert!(error.get("stream_id").is_none());
    }
}

#[tokio::test]
async fn websocket_validation_errors_wait_for_their_named_lane() {
    assert_validation_error_waits_for_active_lane(Some("ordered-lane")).await;
}

#[tokio::test]
async fn websocket_validation_errors_wait_for_the_default_lane() {
    assert_validation_error_waits_for_active_lane(None).await;
}

#[tokio::test]
async fn websocket_invalid_stream_ids_return_400_and_leave_connection_usable() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_valid", "msg_valid", "done")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    for stream_id in [json!(""), json!("x".repeat(257)), json!(42)] {
        send_json(
            &mut ws,
            json!({
                "type": "response.create",
                "stream_id": stream_id,
                "model": "test-model",
                "input": "invalid stream ID"
            }),
        )
        .await;
        let error = recv_json(&mut ws).await;
        assert_eq!(error["type"], "error");
        assert_eq!(error["status"], StatusCode::BAD_REQUEST.as_u16());
        assert!(error.get("stream_id").is_none());
        assert!(mock.request_bodies().await.is_empty());
    }

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "stream_id": "x",
            "model": "test-model",
            "input": "valid request"
        }),
    )
    .await;
    let events = recv_until_completed(&mut ws).await;
    assert!(events.iter().all(|event| event["stream_id"] == "x"));
    assert_eq!(mock.request_bodies().await.len(), 1);
}

#[tokio::test]
async fn websocket_rejects_more_than_64_outstanding_requests() {
    let first_chunk = format!(
        "data: {}\n\n",
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {"id": "resp_upstream_hanging", "status": "in_progress"}
        })
    );
    let (mock, upstream_dropped) = MockResponsesServer::start_hanging(first_chunk).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    for index in 0..64 {
        send_json(
            &mut ws,
            json!({
                "type": "response.create",
                "stream_id": "capacity-lane",
                "model": "test-model",
                "input": format!("request {index}")
            }),
        )
        .await;
    }
    wait_for_request_count(&mock, 1).await;
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "stream_id": "capacity-lane",
            "model": "test-model",
            "input": "request 65"
        }),
    )
    .await;

    let overload = loop {
        let event = recv_json(&mut ws).await;
        if event["type"] == "error" {
            break event;
        }
    };
    assert_eq!(overload["status"], StatusCode::TOO_MANY_REQUESTS.as_u16());
    assert_eq!(overload["error"]["code"], "rate_limit_exceeded");
    assert_eq!(overload["stream_id"], "capacity-lane");
    assert_eq!(mock.request_bodies().await.len(), 1);

    ws.close(None).await.expect("close websocket");
    tokio::time::timeout(std::time::Duration::from_secs(2), upstream_dropped)
        .await
        .expect("timed out waiting for upstream stream cancellation")
        .expect("upstream drop sender should notify");
}

#[tokio::test]
async fn websocket_rejects_a_65th_active_stream_lane() {
    let mut responses = Vec::new();
    let mut upstream_drops = Vec::new();
    for index in 0..64 {
        let first_chunk = format!(
            "data: {}\n\n",
            json!({
                "type": "response.created",
                "sequence_number": 0,
                "response": {"id": format!("resp_active_{index}"), "status": "in_progress"}
            })
        );
        let (drop_tx, drop_rx) = oneshot::channel();
        responses.push(MockResponse::Hanging { first_chunk, drop_tx });
        upstream_drops.push(drop_rx);
    }
    let mock = MockResponsesServer::start_with_responses(responses).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    for index in 0..64 {
        send_json(
            &mut ws,
            json!({
                "type": "response.create",
                "stream_id": format!("active-{index}"),
                "model": "test-model",
                "input": format!("request {index}")
            }),
        )
        .await;
    }
    wait_for_request_count(&mock, 64).await;
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "stream_id": "active-64",
            "model": "test-model",
            "input": "request 65"
        }),
    )
    .await;

    let overload = loop {
        let event = recv_json(&mut ws).await;
        if event["type"] == "error" {
            break event;
        }
    };
    assert_eq!(overload["status"], StatusCode::TOO_MANY_REQUESTS.as_u16());
    assert_eq!(overload["stream_id"], "active-64");
    assert_eq!(mock.request_bodies().await.len(), 64);

    ws.close(None).await.expect("close websocket");
    for upstream_dropped in upstream_drops {
        tokio::time::timeout(std::time::Duration::from_secs(2), upstream_dropped)
            .await
            .expect("timed out waiting for active upstream stream cancellation")
            .expect("upstream drop sender should notify");
    }
}

#[tokio::test]
async fn websocket_rejects_requests_over_the_aggregate_input_budget() {
    let (mock, arrived, release) =
        MockResponsesServer::start_gated(sse_response("resp_large", "msg_large", "done")).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "stream_id": "large-a",
            "model": "test-model",
            "input": "x".repeat(512 * 1024)
        }),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(10), arrived)
        .await
        .expect("timed out waiting for large request to reach upstream")
        .expect("arrival sender dropped");

    for index in 1..23 {
        send_json(
            &mut ws,
            json!({
                "type": "response.create",
                "stream_id": "large-a",
                "model": "test-model",
                "input": "q".repeat(512 * 1024),
                "metadata": {"queued_index": index}
            }),
        )
        .await;
    }
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "stream_id": "large-b",
            "model": "test-model",
            "input": "y".repeat(512 * 1024)
        }),
    )
    .await;
    let overload = recv_json(&mut ws).await;
    assert_eq!(overload["type"], "error");
    assert_eq!(overload["status"], StatusCode::TOO_MANY_REQUESTS.as_u16());
    assert_eq!(overload["error"]["code"], "rate_limit_exceeded");
    assert_eq!(overload["stream_id"], "large-b");
    assert_eq!(mock.request_bodies().await.len(), 1);

    release.send(()).expect("release large response");
    let events = recv_until_completed(&mut ws).await;
    assert!(events.iter().all(|event| event["stream_id"] == "large-a"));
}

#[tokio::test]
async fn websocket_reclaims_completed_request_capacity_before_admission() {
    let (first_arrived_tx, first_arrived_rx) = oneshot::channel();
    let (first_release_tx, first_release_rx) = oneshot::channel();
    let (second_drop_tx, second_drop_rx) = oneshot::channel();
    let second_chunk = format!(
        "data: {}\n\n",
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {"id": "resp_capacity_second", "status": "in_progress"}
        })
    );
    let mock = MockResponsesServer::start_with_responses(vec![
        MockResponse::Gated {
            response: sse_response("resp_capacity_first", "msg_capacity_first", "first"),
            arrived: first_arrived_tx,
            release: first_release_rx,
        },
        MockResponse::Hanging {
            first_chunk: second_chunk,
            drop_tx: second_drop_tx,
        },
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    for index in 0..64 {
        send_json(
            &mut ws,
            json!({
                "type": "response.create",
                "stream_id": "capacity-lane",
                "model": "test-model",
                "input": format!("request {index}")
            }),
        )
        .await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), first_arrived_rx)
        .await
        .expect("timed out waiting for first request to reach upstream")
        .expect("first arrival sender dropped");
    first_release_tx.send(()).expect("release first response");
    let first_events = recv_until_completed(&mut ws).await;
    assert_eq!(
        first_events.last().expect("terminal event")["type"],
        "response.completed"
    );

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "stream_id": "capacity-lane",
            "model": "test-model",
            "input": "request after completed capacity is reclaimed"
        }),
    )
    .await;

    let second_created = recv_json(&mut ws).await;
    assert_eq!(second_created["type"], "response.created");
    assert_eq!(second_created["stream_id"], "capacity-lane");
    send_ping_and_wait_for_pong(&mut ws, Bytes::from_static(b"capacity-reclaimed")).await;

    ws.close(None).await.expect("close websocket");
    tokio::time::timeout(std::time::Duration::from_secs(2), second_drop_rx)
        .await
        .expect("timed out waiting for upstream stream cancellation")
        .expect("upstream drop sender should notify");
}

#[tokio::test]
async fn websocket_conversation_conflict_ends_request_without_persisting_stale_turn() {
    // Arrange
    let (mock, arrived, release) =
        MockResponsesServer::start_gated(sse_response("resp_upstream_stale_ws", "msg_upstream_stale_ws", "STALE"))
            .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let conversation_id = create_conversation(&gateway_url).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    // Act
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "stale turn"}],
            "conversation_id": conversation_id,
            "store": true,
            "stream": true
        }),
    )
    .await;
    arrived.await.expect("upstream request should arrive after rehydration");
    persist_competing_turn(&fixture.pool, &conversation_id).await;
    release.send(()).expect("release gated WebSocket response");
    let events = recv_until_completed(&mut ws).await;

    // Assert
    let stale_response_id = events
        .iter()
        .find(|event| event["type"] == "response.created")
        .and_then(|event| event["response"]["id"].as_str())
        .expect("gateway response ID from response.created")
        .to_owned();
    let error = events.last().expect("terminal conflict error");
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], StatusCode::BAD_REQUEST.as_u16());
    assert_eq!(
        error["error"],
        json!({
            "message": CONFLICT_MESSAGE,
            "type": "invalid_request_error",
            "code": "conversation_locked",
            "param": "conversation"
        })
    );
    assert!(events.iter().all(|event| event["type"] != "response.completed"));

    // A local response queued after the error is an ordering barrier: it can only
    // begin after the conflicted executor stream has ended.
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [],
            "generate": false,
            "store": false,
            "stream": true
        }),
    )
    .await;
    let barrier_created = recv_json(&mut ws).await;
    assert_eq!(barrier_created["type"], "response.created");
    let barrier_response_id = barrier_created["response"]["id"]
        .as_str()
        .expect("barrier response ID")
        .to_owned();
    let barrier_completed = recv_json(&mut ws).await;
    assert_eq!(barrier_completed["type"], "response.completed");
    assert_eq!(barrier_completed["response"]["id"], barrier_response_id);
    assert_ne!(barrier_response_id, stale_response_id);

    assert_conflicting_websocket_turn_not_persisted(&fixture.pool, &conversation_id, &stale_response_id).await;
}

#[tokio::test]
async fn websocket_generate_false_conversation_conflict_rejects_stale_local_completion() {
    // Arrange
    let mock = MockResponsesServer::start(vec![]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (rehydrated, release) = fixture.state.websocket_tracker.install_local_completion_test_barrier();
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let conversation_id = create_conversation(&gateway_url).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    // Act: the one-shot barrier deterministically pauses local completion after
    // rehydration and before persistence, without relying on timing sleeps.
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "stale local turn"}],
            "conversation_id": conversation_id,
            "generate": false,
            "store": true,
            "stream": true
        }),
    )
    .await;
    rehydrated
        .await
        .expect("local completion should pause after rehydration");
    persist_competing_turn(&fixture.pool, &conversation_id).await;
    release
        .send(())
        .expect("local completion should remain paused before persistence");
    let error = recv_json(&mut ws).await;

    // Assert
    assert_eq!(
        error,
        json!({
            "type": "error",
            "status": StatusCode::BAD_REQUEST.as_u16(),
            "error": {
                "message": CONFLICT_MESSAGE,
                "type": "invalid_request_error",
                "code": "conversation_locked",
                "param": "conversation"
            }
        })
    );

    // A local response queued after the error is an ordering barrier. Its first
    // event proves the stale turn emitted no response.completed after the error.
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [],
            "generate": false,
            "store": false,
            "stream": true
        }),
    )
    .await;
    let barrier_created = recv_json(&mut ws).await;
    assert_eq!(barrier_created["type"], "response.created");
    let barrier_response_id = barrier_created["response"]["id"]
        .as_str()
        .expect("barrier response ID")
        .to_owned();
    let barrier_completed = recv_json(&mut ws).await;
    assert_eq!(barrier_completed["type"], "response.completed");
    assert_eq!(barrier_completed["response"]["id"], barrier_response_id);

    assert!(mock.request_bodies().await.is_empty());
    let conversation_store = ConversationStore::new(Arc::clone(&fixture.pool));
    assert_eq!(
        conversation_store
            .rehydrate(&conversation_id)
            .await
            .expect("conversation history"),
        competing_turn_items()
    );
    let response_ids = sqlx::query_scalar::<_, String>("SELECT id FROM responses WHERE id != $1 ORDER BY id")
        .bind(&barrier_response_id)
        .fetch_all(fixture.pool.as_ref())
        .await
        .expect("stored response IDs");
    assert_eq!(response_ids, vec![COMPETING_RESPONSE_ID]);
    let item_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items")
        .fetch_one(fixture.pool.as_ref())
        .await
        .expect("stored item count");
    assert_eq!(item_count, 1, "rejected turn must not leave orphaned items");
}

#[tokio::test]
async fn test_websocket_streaming_persistence_error_uses_standard_envelope() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_upstream_1", "msg_upstream_1", "HELLO")]).await;
    let (gateway_url, _gateway) = spawn_gateway(persistence_disabled_state(&mock.url)).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "store": true,
            "stream": true
        }),
    )
    .await;

    let events = recv_until_completed(&mut ws).await;
    let error = events.last().expect("persistence error event");
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], 500);
    assert_eq!(error["error"]["message"], "failed to persist response");
    assert_eq!(error["error"]["type"], "server_error");
    assert_eq!(error["error"]["code"], "server_error");
    assert!(events.iter().all(|event| event["type"] != "response.completed"));
}

#[tokio::test]
async fn test_websocket_preserves_upstream_failure_details() {
    let mock = MockResponsesServer::start(vec![sse_failed_response()]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": "fail",
            "store": true,
            "stream": true
        }),
    )
    .await;

    let events = recv_until_completed(&mut ws).await;
    let failed = events.last().unwrap();
    assert_eq!(failed["type"], "response.failed");
    assert_eq!(failed["response"]["status"], "error");
    assert_eq!(failed["response"]["error"]["code"], "tool_catalog_too_large");
    assert_eq!(failed["response"]["error"]["message"], "Too many tools");
    assert_eq!(failed["response"]["incomplete_details"]["reason"], "upstream_error");
}

#[tokio::test]
async fn test_websocket_generate_false_is_local_and_reusable() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_upstream_1", "msg_upstream_1", "HELLO")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "stream_id": "warmup-lane",
            "model": "test-model",
            "input": [],
            "generate": false,
            "store": false,
            "stream": true
        }),
    )
    .await;

    let warmup = recv_until_completed(&mut ws).await;
    assert_eq!(warmup.len(), 2);
    assert_eq!(warmup[0]["type"], "response.created");
    assert_eq!(warmup[1]["type"], "response.completed");
    assert_eq!(warmup[0]["response"]["id"], warmup[1]["response"]["id"]);
    assert_eq!(warmup[1]["response"]["output"], json!([]));
    assert_eq!(warmup[1]["response"]["usage"]["total_tokens"], 0);
    assert!(warmup.iter().all(|event| event["stream_id"] == "warmup-lane"));
    assert!(mock.request_bodies().await.is_empty());

    let warmup_id = warmup[1]["response"]["id"].as_str().unwrap();
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "previous_response_id": warmup_id,
            "input": [{"type": "message", "role": "user", "content": "hello"}],
            "store": false,
            "stream": true
        }),
    )
    .await;

    let response = recv_until_completed(&mut ws).await;
    assert_eq!(response.last().unwrap()["response"]["previous_response_id"], warmup_id);
    assert!(response.iter().all(|event| event.get("stream_id").is_none()));
    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["input"].as_array().unwrap().len(), 1);
    assert_eq!(requests[0]["input"][0]["role"], "user");
    assert_eq!(requests[0]["input"][0]["content"], "hello");
}

#[cfg(not(feature = "embedded-code-interpreter"))]
#[tokio::test]
async fn websocket_generate_false_rejects_code_interpreter_before_rehydration_or_inference() {
    let mock = MockResponsesServer::start(vec![]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "previous_response_id": "resp_missing",
            "input": [],
            "tools": [{"type": "code_interpreter", "container": {"type": "auto"}}],
            "generate": false,
            "store": false,
            "stream": true
        }),
    )
    .await;

    let error = recv_json(&mut ws).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], StatusCode::BAD_REQUEST.as_u16());
    assert_eq!(error["error"]["type"], "invalid_request_error");
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("code_interpreter is disabled"))
    );
    assert!(
        mock.request_bodies().await.is_empty(),
        "local completion must not contact upstream inference"
    );

    let response_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM responses")
        .fetch_one(fixture.pool.as_ref())
        .await
        .expect("response count");
    assert_eq!(
        response_count, 0,
        "rejected local completion must not persist a response"
    );
}

#[tokio::test]
async fn websocket_generate_false_emits_conversation_as_object() {
    let mock = MockResponsesServer::start(vec![]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let conversation_id = create_conversation(&gateway_url).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "conversation": conversation_id,
            "input": [{"type": "message", "role": "user", "content": "warmup"}],
            "generate": false,
            "store": true,
            "stream": true
        }),
    )
    .await;

    let events = recv_until_completed(&mut ws).await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["type"], "response.created");
    assert_eq!(events[1]["type"], "response.completed");

    // Both events should have conversation as an object with id field
    for event in &events {
        assert!(
            event["response"]["conversation"].is_object(),
            "conversation should be object, got: {}",
            event["response"]["conversation"]
        );
        assert_eq!(
            event["response"]["conversation"]["id"], conversation_id,
            "conversation.id should match"
        );
    }

    assert_eq!(events[1]["response"]["status"], "completed");
    assert_eq!(events[1]["response"]["output"], json!([]));
    assert!(
        mock.request_bodies().await.is_empty(),
        "generate:false must not contact upstream"
    );
}

#[cfg(not(feature = "embedded-code-interpreter"))]
#[tokio::test]
async fn websocket_generate_false_rejects_rehydrated_code_interpreter_before_persistence() {
    let mock = MockResponsesServer::start(vec![]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let parent_metadata = ResponseMetadata {
        effective_tools: Some(vec![
            serde_json::from_value(json!({
                "type": "code_interpreter",
                "container": {"type": "auto"}
            }))
            .expect("code interpreter declaration"),
        ]),
        ..ResponseMetadata::default()
    };
    ResponseStore::new(Arc::clone(&fixture.pool))
        .persist("resp_parent", None, Vec::new(), &parent_metadata)
        .await
        .expect("persist legacy parent response");

    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "previous_response_id": "resp_parent",
            "input": [],
            "generate": false,
            "store": true,
            "stream": true
        }),
    )
    .await;

    let error = recv_json(&mut ws).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], StatusCode::BAD_REQUEST.as_u16());
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("code_interpreter is disabled"))
    );
    assert!(mock.request_bodies().await.is_empty());

    let response_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM responses")
        .fetch_one(fixture.pool.as_ref())
        .await
        .expect("response count");
    assert_eq!(
        response_count, 1,
        "rejected continuation must not persist a child response"
    );
}

#[tokio::test]
async fn test_websocket_empty_input_without_generate_reaches_upstream() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_upstream_1", "msg_upstream_1", "HELLO")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [],
            "store": false,
            "stream": true
        }),
    )
    .await;

    let response = recv_until_completed(&mut ws).await;
    assert_eq!(
        response.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "HELLO"
    );
    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["input"], json!([]));
}

#[tokio::test]
async fn test_websocket_restores_namespace_tool_call_events() {
    let mock = MockResponsesServer::start(vec![sse_function_call_response(
        "resp_upstream_1",
        "agentic_ns__mcp__agentic_fixture__add_numbers",
    )])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "use the tool"}],
            "tools": [
                {
                    "type": "namespace",
                    "name": "mcp__agentic_fixture",
                    "tools": [
                        {
                            "type": "function",
                            "name": "add_numbers",
                            "parameters": {"type": "object"}
                        }
                    ]
                }
            ],
            "store": true,
            "stream": true
        }),
    )
    .await;

    let events = recv_until_completed(&mut ws).await;
    let added = events
        .iter()
        .find(|event| event["type"] == "response.output_item.added")
        .unwrap();
    let done = events
        .iter()
        .find(|event| event["type"] == "response.output_item.done")
        .unwrap();
    assert_eq!(added["item"]["namespace"], "mcp__agentic_fixture");
    assert_eq!(added["item"]["name"], "add_numbers");
    assert_eq!(done["item"]["namespace"], "mcp__agentic_fixture");
    assert_eq!(done["item"]["name"], "add_numbers");

    let completed = events.last().unwrap();
    assert_eq!(completed["type"], "response.completed");
    let response = &completed["response"];
    assert_eq!(response["output"][0]["namespace"], "mcp__agentic_fixture");
    assert_eq!(response["output"][0]["name"], "add_numbers");

    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["tools"][0]["type"], "function");
    assert_eq!(
        requests[0]["tools"][0]["name"],
        "agentic_ns__mcp__agentic_fixture__add_numbers"
    );
}

#[tokio::test]
async fn test_websocket_bounds_and_restores_long_namespace_tool_name() {
    let namespace = "mcp__codex_apps__github";
    let member = "_remove_reaction_from_pr_review_comment";
    let upstream_name = model_visible_namespace_member_name(namespace, member);
    let mock = MockResponsesServer::start(vec![sse_function_call_response(
        "resp_upstream_long_namespace",
        &upstream_name,
    )])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": "use the long namespace tool",
            "tools": [{
                "type": "namespace",
                "name": namespace,
                "tools": [{
                    "type": "function",
                    "name": member,
                    "parameters": {"type": "object"}
                }]
            }],
            "store": true,
            "stream": true
        }),
    )
    .await;

    let events = recv_until_completed(&mut ws).await;
    let added = events
        .iter()
        .find(|event| event["type"] == "response.output_item.added")
        .unwrap();
    let done = events
        .iter()
        .find(|event| event["type"] == "response.output_item.done")
        .unwrap();
    assert_eq!(added["item"]["namespace"], namespace);
    assert_eq!(added["item"]["name"], member);
    assert_eq!(done["item"]["namespace"], namespace);
    assert_eq!(done["item"]["name"], member);

    let completed = events.last().unwrap();
    assert_eq!(completed["response"]["output"][0]["namespace"], namespace);
    assert_eq!(completed["response"]["output"][0]["name"], member);

    let requests = mock.request_bodies().await;
    let forwarded_name = requests[0]["tools"][0]["name"].as_str().unwrap();
    assert_eq!(forwarded_name, upstream_name);
    assert_eq!(forwarded_name.chars().count(), 64);
}

#[tokio::test]
async fn test_websocket_custom_tool_round_trip_and_continuation() {
    let mock = MockResponsesServer::start(vec![
        sse_custom_tool_call_response(),
        sse_response("resp_after_custom", "msg_after_custom", "CUSTOM TOOL COMPLETE"),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "apply the patch"}],
            "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a patch."
            }],
            "store": true,
            "stream": true
        }),
    )
    .await;

    let first_events = recv_until_completed(&mut ws).await;
    let event_types = first_events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect::<Vec<_>>();
    assert!(event_types.contains(&"response.custom_tool_call_input.delta"));
    assert!(event_types.contains(&"response.custom_tool_call_input.done"));
    let first_completed = first_events.last().unwrap();
    assert_eq!(first_completed["response"]["output"][0]["type"], "custom_tool_call");
    assert_eq!(
        first_completed["response"]["output"][0]["input"],
        "*** Begin Patch\n*** End Patch"
    );
    let previous_response_id = first_completed["response"]["id"].as_str().unwrap();

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "previous_response_id": previous_response_id,
            "input": [{
                "type": "custom_tool_call_output",
                "call_id": "call_custom_1",
                "output": "Done!"
            }],
            "store": true,
            "stream": true
        }),
    )
    .await;

    let second_events = recv_until_completed(&mut ws).await;
    assert_eq!(
        second_events.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "CUSTOM TOOL COMPLETE"
    );

    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["tools"][0]["type"], "function");
    assert_eq!(
        requests[0]["tools"][0]["parameters"]["properties"]["input"]["type"],
        "string"
    );
    let continuation = requests[1]["input"].as_array().unwrap();
    assert!(continuation.iter().any(|item| {
        item["type"] == "function_call"
            && item["call_id"] == "call_custom_1"
            && item["arguments"] == "{\"input\":\"*** Begin Patch\\n*** End Patch\"}"
    }));
    assert!(continuation.iter().any(|item| {
        item["type"] == "function_call_output" && item["call_id"] == "call_custom_1" && item["output"] == "Done!"
    }));
    assert_eq!(requests[1]["tools"][0]["type"], "function");
}

#[tokio::test]
async fn test_websocket_executes_web_search_gateway_tool() {
    let mock_llm = MockResponsesServer::start(vec![
        web_search_function_call_sse_response(),
        sse_response("resp_final", "msg_final", "Use async carefully."),
    ])
    .await;
    let mock_you = MockYouSearchServer::start().await;
    let fixture = storage_backed_state_with_web_search(&mock_llm.url, Some(&mock_you.url)).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "search rust async"}],
            "tools": [{"type": "web_search_preview"}],
            "store": true,
            "stream": true
        }),
    )
    .await;

    let events = recv_until_completed(&mut ws).await;
    let event_types = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect::<Vec<_>>();

    assert!(event_types.contains(&"response.web_search_call.in_progress"));
    assert!(event_types.contains(&"response.web_search_call.searching"));
    assert!(event_types.contains(&"response.web_search_call.completed"));
    assert!(
        !events
            .iter()
            .any(|event| event["item"]["type"] == "function_call" && event["item"]["name"] == "web_search"),
        "internal gateway function_call events should not be forwarded"
    );
    assert_eq!(
        events.last().unwrap()["response"]["output"][1]["content"][0]["text"],
        "Use async carefully."
    );
    assert_eq!(mock_you.request_bodies().await[0]["query"], "rust async");

    let llm_requests = mock_llm.request_bodies().await;
    assert_eq!(llm_requests.len(), 2);
    assert_eq!(llm_requests[0]["tools"][0]["name"], "web_search");
    assert!(
        llm_requests[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == "call_search")
    );
}

#[tokio::test]
async fn test_websocket_preserves_plain_function_tool_call_events() {
    let mock = MockResponsesServer::start(vec![sse_function_call_response("resp_upstream_1", "get_weather")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "use the tool"}],
            "tools": [
                {
                    "type": "function",
                    "name": "get_weather",
                    "parameters": {"type": "object"}
                }
            ],
            "store": true,
            "stream": true
        }),
    )
    .await;

    let events = recv_until_completed(&mut ws).await;
    let added = events
        .iter()
        .find(|event| event["type"] == "response.output_item.added")
        .unwrap();
    let done = events
        .iter()
        .find(|event| event["type"] == "response.output_item.done")
        .unwrap();
    assert!(added["item"].get("namespace").is_none());
    assert_eq!(added["item"]["name"], "get_weather");
    assert!(done["item"].get("namespace").is_none());
    assert_eq!(done["item"]["name"], "get_weather");

    let completed = events.last().unwrap();
    assert_eq!(completed["type"], "response.completed");
    let response = &completed["response"];
    assert!(response["output"][0].get("namespace").is_none());
    assert_eq!(response["output"][0]["name"], "get_weather");

    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["tools"][0]["type"], "function");
    assert_eq!(requests[0]["tools"][0]["name"], "get_weather");
}

#[tokio::test]
async fn test_websocket_continuation_rehydrates_previous_response() {
    let mock = MockResponsesServer::start(vec![
        sse_response("resp_upstream_1", "msg_upstream_1", "HELLO"),
        sse_response("resp_upstream_2", "msg_upstream_2", "WORLD"),
        sse_response("resp_upstream_3", "msg_upstream_3", "AGAIN"),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "text": {"verbosity": "low"},
            "prompt_cache_key": "workspace-a",
            "prompt_cache_retention": "in_memory",
            "store": true,
            "stream": true
        }),
    )
    .await;
    let first = recv_until_completed(&mut ws).await;
    let first_completed = first.last().unwrap();
    let previous_response_id = first_completed["response"]["id"].as_str().unwrap();

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "previous_response_id": previous_response_id,
            "input": [{"type": "message", "role": "user", "content": "continue"}],
            "text": {"verbosity": "high"},
            "store": true,
            "stream": true
        }),
    )
    .await;
    let second = recv_until_completed(&mut ws).await;
    let completed = second.last().unwrap();
    let event_types = second
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        event_types,
        vec![
            "response.created",
            "response.output_item.added",
            "response.output_text.delta",
            "response.completed"
        ]
    );
    assert_eq!(second[2]["delta"], "WORLD");
    assert_eq!(completed["type"], "response.completed");
    let response = &completed["response"];
    assert_eq!(response["output"][0]["content"][0]["text"], "WORLD");
    assert_eq!(response["previous_response_id"], previous_response_id);

    let second_response_id = response["id"].as_str().unwrap();
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "previous_response_id": second_response_id,
            "input": [{"type": "message", "role": "user", "content": "again"}],
            "prompt_cache_key": "workspace-b",
            "prompt_cache_retention": "24h",
            "store": true,
            "stream": true
        }),
    )
    .await;
    let third = recv_until_completed(&mut ws).await;
    assert_eq!(
        third.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "AGAIN"
    );

    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0]["text"], json!({"verbosity": "low"}));
    assert_eq!(requests[1]["text"], json!({"verbosity": "high"}));
    assert!(requests[2].get("text").is_none());
    assert_eq!(requests[0]["prompt_cache_key"], "workspace-a");
    assert_eq!(requests[0]["prompt_cache_retention"], "in_memory");
    assert!(requests[1].get("prompt_cache_retention").is_none());
    assert_eq!(requests[2]["prompt_cache_retention"], "24h");
    assert!(requests[1].get("prompt_cache_key").is_none());
    assert_eq!(requests[2]["prompt_cache_key"], "workspace-b");
    assert!(requests[1].get("previous_response_id").is_none());
    assert_eq!(requests[1]["input"][0]["content"], "hi");
    assert_eq!(requests[1]["input"][1]["role"], "assistant");
    assert_eq!(requests[1]["input"][1]["content"][0]["text"], "HELLO");
    assert_eq!(requests[1]["input"][2]["content"], "continue");
}

#[tokio::test]
async fn test_websocket_unknown_previous_response_returns_error_event() {
    let mock = MockResponsesServer::start(vec![]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "stream_id": "missing-response-lane",
            "model": "test-model",
            "previous_response_id": "resp_missing",
            "input": [{"type": "message", "role": "user", "content": "continue"}],
            "store": true,
            "stream": true
        }),
    )
    .await;

    let error = recv_json(&mut ws).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], StatusCode::BAD_REQUEST.as_u16());
    assert_eq!(error["error"]["code"], "previous_response_not_found");
    assert_eq!(error["stream_id"], "missing-response-lane");
    assert!(mock.request_bodies().await.is_empty());
}

#[tokio::test]
async fn test_websocket_rejects_binary_json_without_upstream_request() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_upstream_1", "msg_upstream_1", "HELLO")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    ws.send(Message::Binary(
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "store": true,
            "stream": true
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let error = recv_json(&mut ws).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], StatusCode::BAD_REQUEST.as_u16());
    assert_eq!(error["error"]["code"], "invalid_request_error");
    assert!(mock.request_bodies().await.is_empty());
}

/// Over-limit messages are rejected by the transport before any JSON is parsed,
/// so the connection closes instead of carrying a JSON error event.
#[tokio::test]
async fn test_websocket_rejects_messages_larger_than_the_configured_request_size_limit() {
    let mock = MockResponsesServer::start(vec![]).await;
    let mut state = storage_backed_state(&mock.url).await;
    state.state.max_request_body_size = SMALL_REQUEST_SIZE_LIMIT;
    let (gateway_url, _gateway) = spawn_gateway(state.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    if ws
        .send(Message::Text("x".repeat(SMALL_REQUEST_SIZE_LIMIT.get() + 1).into()))
        .await
        .is_ok()
    {
        let message = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .expect("timed out waiting for websocket close/error")
            .expect("websocket should yield a close or error");
        assert!(message.is_err() || matches!(message, Ok(Message::Close(_))));
    }
    assert!(mock.request_bodies().await.is_empty());
}

/// A raised ceiling admits a request that the small limit above would have closed.
#[tokio::test]
async fn test_websocket_accepts_messages_within_a_raised_request_size_limit() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_upstream_1", "msg_upstream_1", "HELLO")]).await;
    let mut state = storage_backed_state(&mock.url).await;
    state.state.max_request_body_size = std::num::NonZeroUsize::new(64 * 1024).expect("nonzero");
    let (gateway_url, _gateway) = spawn_gateway(state.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    let request = json!({
        "type": "response.create",
        "model": "test-model",
        "input": [{"type": "message", "role": "user", "content": "x".repeat(4 * 1024)}],
        "store": true,
        "stream": true
    });
    assert!(
        request.to_string().len() > SMALL_REQUEST_SIZE_LIMIT.get(),
        "the payload must exceed the small limit to prove the ceiling is what changed"
    );
    send_json(&mut ws, request).await;

    let events = recv_until_completed(&mut ws).await;
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    assert_eq!(mock.request_bodies().await.len(), 1);
}

#[tokio::test]
async fn test_websocket_ping_returns_pong_without_upstream_request() {
    let mock = MockResponsesServer::start(vec![]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_ping_and_wait_for_pong(&mut ws, Bytes::from_static(b"ping")).await;

    assert!(mock.request_bodies().await.is_empty());
}

#[tokio::test]
async fn test_websocket_shutdown_token_closes_idle_connection() {
    let mock = MockResponsesServer::start(vec![]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let shutdown_token = fixture.state.shutdown_token.clone();
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    shutdown_token.cancel();

    recv_close_or_end(&mut ws).await;
    assert!(mock.request_bodies().await.is_empty());
}

#[tokio::test]
async fn test_websocket_shutdown_drains_active_response_before_closing() {
    let (mock, arrived, release) =
        MockResponsesServer::start_gated(sse_response("resp_upstream_shutdown", "msg_upstream_shutdown", "DONE")).await;
    let fixture = storage_backed_state(&mock.url).await;
    let shutdown_token = fixture.state.shutdown_token.clone();
    let websocket_tracker = fixture.state.websocket_tracker.clone();
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "store": true,
            "stream": true
        }),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(2), arrived)
        .await
        .expect("timed out waiting for shutdown request to reach upstream")
        .expect("arrival sender dropped");

    shutdown_token.cancel();
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": "must not start during shutdown",
            "store": true,
            "stream": true
        }),
    )
    .await;
    let barrier = Bytes::from_static(b"shutdown-request-received");
    send_ping_and_wait_for_pong(&mut ws, barrier).await;
    release.send(()).unwrap();

    let events = recv_until_completed(&mut ws).await;
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    recv_clean_close(&mut ws).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), websocket_tracker.wait_until_idle())
        .await
        .expect("server did not receive the websocket close acknowledgement");
    assert_eq!(mock.request_bodies().await.len(), 1);
}

#[tokio::test]
async fn websocket_shutdown_drains_all_active_lanes_and_discards_queued_work() {
    let (first_arrived_tx, first_arrived_rx) = oneshot::channel();
    let (first_release_tx, first_release_rx) = oneshot::channel();
    let (second_arrived_tx, second_arrived_rx) = oneshot::channel();
    let (second_release_tx, second_release_rx) = oneshot::channel();
    let mock = MockResponsesServer::start_with_responses(vec![
        MockResponse::Gated {
            response: sse_response("resp_shutdown_a", "msg_shutdown_a", "A"),
            arrived: first_arrived_tx,
            release: first_release_rx,
        },
        MockResponse::Gated {
            response: sse_response("resp_shutdown_b", "msg_shutdown_b", "B"),
            arrived: second_arrived_tx,
            release: second_release_rx,
        },
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let shutdown_token = fixture.state.shutdown_token.clone();
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    for (stream_id, input) in [("shutdown-a", "active a"), ("shutdown-b", "active b")] {
        send_json(
            &mut ws,
            json!({
                "type": "response.create",
                "stream_id": stream_id,
                "model": "test-model",
                "input": input
            }),
        )
        .await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(2), first_arrived_rx)
        .await
        .expect("timed out waiting for first shutdown lane")
        .expect("first arrival sender dropped");
    tokio::time::timeout(std::time::Duration::from_secs(2), second_arrived_rx)
        .await
        .expect("timed out waiting for second shutdown lane")
        .expect("second arrival sender dropped");
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "stream_id": "shutdown-a",
            "model": "test-model",
            "input": "queued and discarded"
        }),
    )
    .await;
    send_ping_and_wait_for_pong(&mut ws, Bytes::from_static(b"queued-before-shutdown")).await;

    shutdown_token.cancel();
    first_release_tx.send(()).expect("release first shutdown lane");
    second_release_tx.send(()).expect("release second shutdown lane");

    let mut events = Vec::new();
    let mut terminal_count = 0;
    while terminal_count < 2 {
        let event = recv_json(&mut ws).await;
        if event["type"] == "response.completed" {
            terminal_count += 1;
        }
        events.push(event);
    }
    assert!(events.iter().any(|event| event["stream_id"] == "shutdown-a"));
    assert!(events.iter().any(|event| event["stream_id"] == "shutdown-b"));
    recv_clean_close(&mut ws).await;
    assert_eq!(mock.request_bodies().await.len(), 2);
}

#[tokio::test]
async fn test_websocket_client_close_cancels_hanging_upstream_stream() {
    let first_chunk = format!(
        "data: {}\n\n",
        json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {"id": "resp_upstream_hanging", "status": "in_progress"}
        })
    );
    let (mock, upstream_dropped) = MockResponsesServer::start_hanging(first_chunk).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "hi"}],
            "store": true,
            "stream": true
        }),
    )
    .await;
    wait_for_request_count(&mock, 1).await;

    ws.close(None).await.unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(2), upstream_dropped)
        .await
        .expect("timed out waiting for upstream stream to be dropped")
        .expect("upstream drop sender should notify");
    assert_eq!(mock.request_bodies().await.len(), 1);
}

#[tokio::test]
async fn websocket_client_close_cancels_all_active_stream_lanes() {
    let mut responses = Vec::new();
    let mut upstream_drops = Vec::new();
    for stream_id in ["disconnect-a", "disconnect-b"] {
        let first_chunk = format!(
            "data: {}\n\n",
            json!({
                "type": "response.created",
                "sequence_number": 0,
                "response": {"id": format!("resp_{stream_id}"), "status": "in_progress"}
            })
        );
        let (drop_tx, drop_rx) = oneshot::channel();
        responses.push(MockResponse::Hanging { first_chunk, drop_tx });
        upstream_drops.push(drop_rx);
    }
    let mock = MockResponsesServer::start_with_responses(responses).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    for stream_id in ["disconnect-a", "disconnect-b"] {
        send_json(
            &mut ws,
            json!({
                "type": "response.create",
                "stream_id": stream_id,
                "model": "test-model",
                "input": "hang"
            }),
        )
        .await;
    }
    wait_for_request_count(&mock, 2).await;
    ws.close(None).await.expect("close websocket");

    for upstream_dropped in upstream_drops {
        tokio::time::timeout(std::time::Duration::from_secs(2), upstream_dropped)
            .await
            .expect("timed out waiting for active upstream stream cancellation")
            .expect("upstream drop sender should notify");
    }
}

async fn assert_no_response_state(pool: &DbPool) {
    for table in ["responses", "items"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "store:false wrote to {table}");
    }
}

#[tokio::test]
async fn websocket_unstored_continuation_is_connection_local() {
    for generate in [false, true] {
        for lane in [None, Some("private")] {
            let mock = MockResponsesServer::start(vec![
                sse_response("resp_first", "msg_first", "PRIVATE"),
                sse_response("resp_second", "msg_second", "CONTINUED"),
            ])
            .await;
            let fixture = storage_backed_state(&mock.url).await;
            let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
            let mut ws = connect_responses_ws(&gateway_url).await;
            let mut request = json!({"type":"response.create", "model":"test-model",
                "input":"private prefix", "store":false, "generate":generate});
            if let Some(lane) = lane {
                request["stream_id"] = json!(lane);
            }
            send_json(&mut ws, request.clone()).await;
            let events = recv_until_completed(&mut ws).await;
            assert_eq!(events.last().unwrap()["type"], "response.completed");
            let parent = events.last().unwrap()["response"]["id"].as_str().unwrap().to_owned();
            assert_no_response_state(&fixture.pool).await;
            request["previous_response_id"] = json!(parent);
            request["input"] = json!("next turn");
            request["generate"] = json!(true);
            send_json(&mut ws, request.clone()).await;
            let events = recv_until_completed(&mut ws).await;
            assert_eq!(events.last().unwrap()["type"], "response.completed");
            let child = events.last().unwrap()["response"]["id"].as_str().unwrap().to_owned();
            assert_no_response_state(&fixture.pool).await;
            let bodies = mock.request_bodies().await;
            let input = bodies.last().unwrap()["input"].to_string();
            assert!(input.contains("private prefix"));
            assert!(input.contains("next turn"));
            ws.close(None).await.unwrap();
            for store in [false, true] {
                let mut fresh = connect_responses_ws(&gateway_url).await;
                request["previous_response_id"] = json!(child);
                request["store"] = json!(store);
                send_json(&mut fresh, request.clone()).await;
                let events = recv_until_completed(&mut fresh).await;
                assert_eq!(events.last().unwrap()["error"]["code"], "previous_response_not_found");
                fresh.close(None).await.unwrap();
            }
            assert_eq!(mock.request_bodies().await.len(), bodies.len());
        }
    }
}

#[tokio::test]
async fn websocket_unstored_prewarm_and_generation_work_without_storage() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_answer", "msg_answer", "READY")]).await;
    let (gateway_url, _gateway) = spawn_gateway(persistence_disabled_state(&mock.url)).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    send_json(
        &mut ws,
        json!({"type":"response.create", "model":"test-model",
        "input":"warmup prefix", "generate":false, "store":false}),
    )
    .await;
    let events = recv_until_completed(&mut ws).await;
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    let parent = events.last().unwrap()["response"]["id"].as_str().unwrap();
    send_json(
        &mut ws,
        json!({"type":"response.create", "model":"test-model",
        "previous_response_id":parent, "input":"next", "store":false}),
    )
    .await;
    let events = recv_until_completed(&mut ws).await;
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    assert!(
        mock.request_bodies().await[0]["input"]
            .to_string()
            .contains("warmup prefix")
    );
}

async fn prewarm_private(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    lane: Option<&str>,
    text: &str,
) -> String {
    let mut request = json!({"type":"response.create", "model":"test-model", "store":false,
        "generate":false, "input":text});
    if let Some(lane) = lane {
        request["stream_id"] = json!(lane);
    }
    send_json(ws, request).await;
    let events = recv_until_completed(ws).await;
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    events.last().unwrap()["response"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn websocket_validation_failure_evicts_only_its_valid_lane_parent() {
    for (routing, should_evict) in [(json!("source"), true), (json!("fork"), false), (Value::Null, false)] {
        let mock = MockResponsesServer::start(vec![]).await;
        let (gateway_url, _gateway) = spawn_gateway(persistence_disabled_state(&mock.url)).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        // Null routing must never be interpreted as the default lane for eviction.
        let lane = if routing.is_null() { None } else { Some("source") };
        let parent = prewarm_private(&mut ws, lane, "private").await;
        send_json(
            &mut ws,
            json!({"type":"response.create", "stream_id":routing,
            "previous_response_id":parent, "model":7, "store":false, "input":[]}),
        )
        .await;
        assert_eq!(recv_json(&mut ws).await["type"], "error");
        let mut request = json!({"type":"response.create", "model":"test-model",
            "previous_response_id":parent, "input":[], "store":false, "generate":false});
        if let Some(lane) = lane {
            request["stream_id"] = json!(lane);
        }
        send_json(&mut ws, request).await;
        let events = recv_until_completed(&mut ws).await;
        if should_evict {
            assert_eq!(events.last().unwrap()["error"]["code"], "previous_response_not_found");
        } else {
            assert_eq!(events.last().unwrap()["type"], "response.completed");
        }
        assert!(mock.request_bodies().await.is_empty());
    }
}

#[tokio::test]
async fn websocket_stored_child_of_unstored_parent_survives_reconnect() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_final", "msg_final", "READY")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    let parent = prewarm_private(&mut ws, Some("source"), "private prefix").await;
    send_json(
        &mut ws,
        json!({"type":"response.create", "stream_id":"source", "model":"test-model",
        "previous_response_id":parent, "input":"stored child", "store":true, "generate":false}),
    )
    .await;
    let events = recv_until_completed(&mut ws).await;
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    let child = events.last().unwrap()["response"]["id"].as_str().unwrap().to_owned();
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM responses")
        .fetch_all(fixture.pool.as_ref())
        .await
        .unwrap();
    assert_eq!(
        ids,
        vec![child.clone()],
        "only the explicitly stored child gets a response row"
    );
    ws.close(None).await.unwrap();
    let mut fresh = connect_responses_ws(&gateway_url).await;
    send_json(
        &mut fresh,
        json!({"type":"response.create", "model":"test-model",
        "previous_response_id":child, "input":"after reconnect", "store":true}),
    )
    .await;
    let events = recv_until_completed(&mut fresh).await;
    assert_eq!(events.last().unwrap()["type"], "response.completed");
    let input = mock.request_bodies().await[0]["input"].to_string();
    assert!(input.contains("private prefix"));
    assert!(input.contains("stored child"));
    assert!(input.contains("after reconnect"));
}

#[tokio::test]
async fn websocket_unstored_custom_tool_round_trip() {
    let mock = MockResponsesServer::start(vec![
        sse_custom_tool_call_response(),
        sse_response("resp_after_custom", "msg_after_custom", "DONE"),
    ])
    .await;
    let (gateway_url, _gateway) = spawn_gateway(persistence_disabled_state(&mock.url)).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    send_json(
        &mut ws,
        json!({"type":"response.create", "model":"test-model", "input":"apply patch",
        "tools":[{"type":"custom", "name":"apply_patch"}], "store":false}),
    )
    .await;
    let events = recv_until_completed(&mut ws).await;
    assert_eq!(
        events.last().unwrap()["response"]["output"][0]["type"],
        "custom_tool_call"
    );
    let parent = events.last().unwrap()["response"]["id"].as_str().unwrap();
    send_json(&mut ws, json!({"type":"response.create", "model":"test-model", "store":false,
        "previous_response_id":parent, "input":[{"type":"custom_tool_call_output", "call_id":"call_custom_1", "output":"Done!"}]})).await;
    let events = recv_until_completed(&mut ws).await;
    assert_eq!(
        events.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "DONE"
    );
    let requests = mock.request_bodies().await;
    let input = requests[1]["input"].as_array().unwrap();
    assert_eq!(input.iter().filter(|item| item["type"] == "function_call").count(), 1);
    assert_eq!(
        input
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .count(),
        1
    );
}

#[tokio::test]
async fn websocket_overload_preserves_parent_for_accepted_queued_continuation() {
    for byte_limit in [false, true] {
        for lane in [None, Some("source")] {
            let (mock, arrived, release) = MockResponsesServer::start_gated(sse_failed_response()).await;
            let (gateway_url, _gateway) = spawn_gateway(persistence_disabled_state(&mock.url)).await;
            let mut ws = connect_responses_ws(&gateway_url).await;
            let parent = prewarm_private(&mut ws, lane, "private parent").await;
            let mut active =
                json!({"type":"response.create", "model":"test-model", "store":false, "input":"independent turn"});
            if let Some(lane) = lane {
                active["stream_id"] = json!(lane);
            }
            send_json(&mut ws, active.clone()).await;
            tokio::time::timeout(std::time::Duration::from_secs(2), arrived)
                .await
                .unwrap()
                .unwrap();
            let mut queued = active.clone();
            queued["previous_response_id"] = json!(parent);
            queued["generate"] = json!(false);
            queued["input"] = json!("accepted continuation");
            if byte_limit {
                queued["metadata"] = json!({"padding":"x".repeat(7 * 1024 * 1024)});
            }
            send_json(&mut ws, queued.clone()).await;
            if !byte_limit {
                let mut filler = active.clone();
                filler["model"] = json!(7);
                for _ in 0..62 {
                    send_json(&mut ws, filler.clone()).await;
                }
            }
            send_json(&mut ws, queued).await;
            let rejected = recv_until_completed(&mut ws).await;
            assert_eq!(
                rejected.last().unwrap()["status"],
                429,
                "overload must reply without waiting for active work"
            );
            release.send(()).unwrap();
            let failed = recv_until_completed(&mut ws).await;
            assert_eq!(failed.last().unwrap()["type"], "response.failed");
            let accepted = recv_until_completed(&mut ws).await;
            assert_eq!(
                accepted.last().unwrap()["type"],
                "response.completed",
                "rejection must not evict accepted work's parent"
            );
            if !byte_limit {
                for _ in 0..62 {
                    assert_eq!(recv_json(&mut ws).await["type"], "error");
                }
            }
            assert_eq!(mock.request_bodies().await.len(), 1, "queued prewarm stays local");
        }
    }
}

#[tokio::test]
async fn websocket_bounds_idle_lane_retention_and_preserves_existing_lanes() {
    let mock = MockResponsesServer::start(vec![]).await;
    let (gateway_url, _gateway) = spawn_gateway(persistence_disabled_state(&mock.url)).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    let parent = prewarm_private(&mut ws, None, "default parent").await;
    for lane in 1..128 {
        prewarm_private(&mut ws, Some(&format!("lane-{lane}")), "private").await;
    }
    send_json(
        &mut ws,
        json!({"type":"response.create", "model":"test-model", "stream_id":"overflow",
        "input":[], "store":false, "generate":false}),
    )
    .await;
    assert_eq!(recv_json(&mut ws).await["status"], 429);
    send_json(
        &mut ws,
        json!({"type":"response.create", "model":"test-model",
        "previous_response_id":parent, "input":[], "store":false, "generate":false}),
    )
    .await;
    assert_eq!(
        recv_until_completed(&mut ws).await.last().unwrap()["type"],
        "response.completed"
    );
    assert!(mock.request_bodies().await.is_empty());
}

#[tokio::test]
async fn websocket_retention_budget_rejects_before_durable_write() {
    let mock = MockResponsesServer::start(vec![]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    let text = "x".repeat(9 * 1024 * 1024);
    for lane in 0..3 {
        prewarm_private(&mut ws, Some(&format!("lane-{lane}")), &text).await;
    }
    send_json(
        &mut ws,
        json!({"type":"response.create", "model":"test-model", "stream_id":"overflow",
        "input":text, "store":true, "generate":false}),
    )
    .await;
    let events = recv_until_completed(&mut ws).await;
    assert_eq!(events.len(), 1, "budget rejection precedes response.created");
    assert_eq!(events[0]["status"], 413);
    assert_no_response_state(&fixture.pool).await;
    assert!(mock.request_bodies().await.is_empty());
}

#[tokio::test]
async fn websocket_unstored_disconnect_cancels_active_and_queued_work() {
    let (first_chunk, _) = phased_sse_response("resp_hanging", "msg_hanging", "never complete");
    let (mock, upstream_dropped) = MockResponsesServer::start_hanging(first_chunk).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    let parent = prewarm_private(&mut ws, None, "private").await;
    let request = json!({"type":"response.create", "model":"test-model", "store":false,
        "previous_response_id":parent, "input":"continue"});
    send_json(&mut ws, request.clone()).await;
    assert_eq!(recv_json(&mut ws).await["type"], "response.created");
    send_json(&mut ws, request).await;
    ws.close(None).await.unwrap();
    drop(ws);
    tokio::time::timeout(std::time::Duration::from_secs(2), upstream_dropped)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        fixture.state.websocket_tracker.wait_until_idle(),
    )
    .await
    .unwrap();
    assert_no_response_state(&fixture.pool).await;
    assert_eq!(mock.request_bodies().await.len(), 1);
    let mut fresh = connect_responses_ws(&gateway_url).await;
    send_json(
        &mut fresh,
        json!({"type":"response.create", "model":"test-model", "store":false,
        "previous_response_id":parent, "input":[], "generate":false}),
    )
    .await;
    assert_eq!(
        recv_json(&mut fresh).await["error"]["code"],
        "previous_response_not_found"
    );
}

#[tokio::test]
async fn websocket_unstored_fork_keeps_pinned_parent_after_source_failure() {
    let (arrived_tx, arrived_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let mock = MockResponsesServer::start_with_responses(vec![
        MockResponse::Gated {
            response: sse_response("resp_fork", "msg_fork", "FORK"),
            arrived: arrived_tx,
            release: release_rx,
        },
        MockResponse::Static(sse_failed_response()),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    let parent = prewarm_private(&mut ws, Some("source"), "shared private parent").await;
    let mut request = json!({"type":"response.create", "model":"test-model", "store":false,
        "stream_id":"fork", "previous_response_id":parent, "input":"fork input"});
    send_json(&mut ws, request.clone()).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), arrived_rx)
        .await
        .unwrap()
        .unwrap();
    request["stream_id"] = json!("source");
    request["input"] = json!("source fails");
    send_json(&mut ws, request.clone()).await;
    let failed = recv_until_completed(&mut ws).await;
    assert_eq!(failed.last().unwrap()["type"], "response.failed");
    assert_eq!(failed.last().unwrap()["stream_id"], "source");
    request["generate"] = json!(false);
    send_json(&mut ws, request.clone()).await;
    assert_eq!(recv_json(&mut ws).await["error"]["code"], "previous_response_not_found");
    release_tx.send(()).unwrap();
    let fork = recv_until_completed(&mut ws).await;
    assert_eq!(fork.last().unwrap()["type"], "response.completed");
    assert_eq!(fork.last().unwrap()["stream_id"], "fork");
    request["stream_id"] = json!("fork");
    request["previous_response_id"] = fork.last().unwrap()["response"]["id"].clone();
    request["input"] = json!([]);
    send_json(&mut ws, request).await;
    assert_eq!(
        recv_until_completed(&mut ws).await.last().unwrap()["type"],
        "response.completed"
    );
    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 2);
    assert!(requests[0]["input"].to_string().contains("shared private parent"));
    assert!(!requests[0]["input"].to_string().contains("source fails"));
    assert_no_response_state(&fixture.pool).await;
}

// --- Image preservation through the Responses WebSocket transport (issue #253) ---
//
// The same 1x1 red and blue PNGs used by the HTTP tests: real, valid, and
// distinguishable, so ordering assertions cannot pass by accident.
const RED_PIXEL_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mP4z8AAAAMBAQD3A0FDAAAAAElFTkSuQmCC";
const BLUE_PIXEL_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mNgYPgPAAEDAQA2dBFAAAAAAElFTkSuQmCC";

fn image_part(image_url: &str, detail: Option<&str>) -> Value {
    match detail {
        Some(detail) => json!({"type": "input_image", "image_url": image_url, "detail": detail}),
        None => json!({"type": "input_image", "image_url": image_url}),
    }
}

#[tokio::test]
async fn test_websocket_preserves_mixed_text_and_image_ordering() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_upstream_1", "msg_upstream_1", "I see red.")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    let content = json!([
        {"type": "input_text", "text": "first"},
        image_part(RED_PIXEL_PNG, Some("low")),
        {"type": "input_text", "text": "between"},
        image_part(BLUE_PIXEL_PNG, Some("high")),
        {"type": "input_text", "text": "last"}
    ]);

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": content}],
            "store": true,
            "stream": true
        }),
    )
    .await;

    let events = recv_until_completed(&mut ws).await;
    assert_eq!(
        events.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "I see red."
    );

    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]["input"][0]["content"], content,
        "streaming must not reorder or drop image parts"
    );
}

#[tokio::test]
async fn test_websocket_multiple_images_across_messages_keep_order() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_upstream_1", "msg_upstream_1", "Both seen.")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    let input = json!([
        {
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "look at this"}, image_part(RED_PIXEL_PNG, None)]
        },
        {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "I see red."}]},
        {
            "type": "message",
            "role": "user",
            "content": [image_part(BLUE_PIXEL_PNG, None), {"type": "input_text", "text": "and this?"}]
        }
    ]);

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": input,
            "store": true,
            "stream": true
        }),
    )
    .await;
    recv_until_completed(&mut ws).await;

    let requests = mock.request_bodies().await;
    let images = requests[0]["input"]
        .as_array()
        .expect("input items")
        .iter()
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .filter(|part| part["type"] == "input_image")
        .map(|part| part["image_url"].as_str().expect("image URL"))
        .collect::<Vec<_>>();
    assert_eq!(images, vec![RED_PIXEL_PNG, BLUE_PIXEL_PNG]);
    let mut expected_input = input.clone();
    expected_input[0]["content"][1]["detail"] = "auto".into();
    expected_input[2]["content"][0]["detail"] = "auto".into();
    assert_eq!(requests[0]["input"], expected_input);
}

#[tokio::test]
async fn test_websocket_view_image_tool_output_reaches_next_round() {
    let mock = MockResponsesServer::start(vec![
        sse_function_call_response("resp_upstream_1", "view_image"),
        sse_response("resp_after_image", "msg_after_image", "A red pixel."),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    let view_image_tool = json!({
        "type": "function",
        "name": "view_image",
        "description": "Attach a local image to the conversation.",
        "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
    });

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "look at diagram.png"}],
            "tools": [view_image_tool],
            "store": true,
            "stream": true
        }),
    )
    .await;

    let first = recv_until_completed(&mut ws).await;
    let first_completed = first.last().unwrap();
    assert_eq!(first_completed["response"]["output"][0]["type"], "function_call");
    assert_eq!(first_completed["response"]["output"][0]["name"], "view_image");
    let previous_response_id = first_completed["response"]["id"].as_str().unwrap();

    // The client executes `view_image` locally and returns structured content.
    let tool_output = json!([
        {"type": "input_text", "text": "attached local image path: diagram.png"},
        image_part(RED_PIXEL_PNG, None)
    ]);
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "previous_response_id": previous_response_id,
            "input": [{"type": "function_call_output", "call_id": "call_1", "output": tool_output}],
            "tools": [view_image_tool],
            "store": true,
            "stream": true
        }),
    )
    .await;

    let second = recv_until_completed(&mut ws).await;
    assert_eq!(
        second.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "A red pixel."
    );

    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 2);
    let continuation = requests[1]["input"].as_array().expect("continuation input");
    let output = continuation
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .map(|item| &item["output"])
        .expect("the client tool output must reach the next inference round");
    assert!(
        output.is_array(),
        "structured tool output must stay an array, not an escaped JSON string: {output}"
    );
    assert_eq!(output, &tool_output);
    assert!(
        continuation
            .iter()
            .any(|item| item["type"] == "function_call" && item["name"] == "view_image"),
        "the call the output resolves must be replayed alongside it"
    );
}

#[tokio::test]
async fn test_websocket_custom_tool_image_output_round_trip() {
    let mock = MockResponsesServer::start(vec![
        sse_custom_tool_call_response(),
        sse_response("resp_after_custom", "msg_after_custom", "Screenshot received."),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": "grab a screenshot"}],
            "tools": [{"type": "custom", "name": "apply_patch", "description": "Apply a patch."}],
            "store": true,
            "stream": true
        }),
    )
    .await;
    let first = recv_until_completed(&mut ws).await;
    let previous_response_id = first.last().unwrap()["response"]["id"].as_str().unwrap();

    let tool_output = json!([
        {"type": "input_text", "text": "screenshot"},
        image_part(BLUE_PIXEL_PNG, Some("auto"))
    ]);
    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "previous_response_id": previous_response_id,
            "input": [{"type": "custom_tool_call_output", "call_id": "call_custom_1", "output": tool_output}],
            "store": true,
            "stream": true
        }),
    )
    .await;
    let second = recv_until_completed(&mut ws).await;
    assert_eq!(
        second.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "Screenshot received."
    );

    let requests = mock.request_bodies().await;
    let output = requests[1]["input"]
        .as_array()
        .expect("continuation input")
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .map(|item| &item["output"])
        .expect("a custom-tool output must normalize to a function-tool output");
    assert!(
        output.is_array(),
        "normalization must not stringify the array: {output}"
    );
    assert_eq!(output, &tool_output);
}

#[tokio::test]
async fn test_websocket_continuation_rehydrates_images() {
    let mock = MockResponsesServer::start(vec![
        sse_response("resp_upstream_1", "msg_upstream_1", "I see red."),
        sse_response("resp_upstream_2", "msg_upstream_2", "Still red."),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    let content = json!([
        {"type": "input_text", "text": "describe this"},
        image_part(RED_PIXEL_PNG, Some("low"))
    ]);

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": content}],
            "store": true,
            "stream": true
        }),
    )
    .await;
    let first = recv_until_completed(&mut ws).await;
    let previous_response_id = first.last().unwrap()["response"]["id"].as_str().unwrap();

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "previous_response_id": previous_response_id,
            "input": [{"type": "message", "role": "user", "content": "and now?"}],
            "store": true,
            "stream": true
        }),
    )
    .await;
    recv_until_completed(&mut ws).await;

    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 2);
    let history = requests[1]["input"].as_array().expect("rehydrated history");
    assert_eq!(
        history[0]["content"], content,
        "the stored image must survive rehydration over the WebSocket transport"
    );
    assert_eq!(history[1]["role"], "assistant");
    assert_eq!(history[2]["content"], "and now?");
}

#[tokio::test]
async fn test_websocket_unmodeled_content_part_is_rejected_not_dropped() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_unused", "msg_unused", "unreachable")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create",
            "model": "test-model",
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "before"},
                {"type": "input_audio", "audio_url": "https://example.com/clip.wav"},
                image_part(RED_PIXEL_PNG, None)
            ]}],
            "store": true,
            "stream": true
        }),
    )
    .await;

    let error = recv_json(&mut ws).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], StatusCode::BAD_REQUEST.as_u16());
    assert_eq!(error["error"]["type"], "invalid_request_error");
    let message = error["error"]["message"].as_str().expect("error message");
    assert!(message.contains("input[0].content[1]"), "{message}");
    assert!(message.contains("`input_audio`"), "{message}");
    assert!(!message.contains("clip.wav"), "do not reflect part contents: {message}");

    assert!(
        mock.request_bodies().await.is_empty(),
        "a rejected message must never reach the upstream"
    );
    for table in ["responses", "items"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(fixture.pool.as_ref())
            .await
            .unwrap();
        assert_eq!(count, 0, "a rejected message must not be persisted in {table}");
    }
}

#[tokio::test]
async fn websocket_injection_unknown_and_retained_completed_use_typed_failures() {
    let mock = MockResponsesServer::start(vec![sse_response("resp_upstream", "msg_done", "done")]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    let input = json!([{"type":"function_call_output","call_id":"call_missing","output":"preserved"}]);
    send_json(
        &mut ws,
        json!({"type":"response.inject","response_id":"resp_unknown","input":input}),
    )
    .await;
    let missing = recv_json(&mut ws).await;
    assert_eq!(missing["type"], "response.inject.failed");
    assert_eq!(missing["error"]["code"], "response_not_found");
    assert_eq!(missing["input"], input);
    send_json(
        &mut ws,
        json!({"type":"response.create","model":"test-model","input":"hello","store":true,"stream_id":"lane"}),
    )
    .await;
    let events = recv_until_completed(&mut ws).await;
    let terminal = events.last().unwrap();
    assert_eq!(terminal["type"], "response.completed", "{events:?}");
    let response_id = terminal["response"]["id"].as_str().unwrap();
    send_json(
        &mut ws,
        json!({"type":"response.inject","response_id":response_id,"input":input}),
    )
    .await;
    let completed = recv_json(&mut ws).await;
    assert_eq!(completed["type"], "response.inject.failed");
    assert_eq!(completed["error"]["code"], "response_already_completed");
    assert_eq!(completed["input"], input);
    assert_eq!(completed["stream_id"], "lane");
    assert!(completed["sequence_number"].as_u64().unwrap() > terminal["sequence_number"].as_u64().unwrap());
    let mut other = connect_responses_ws(&gateway_url).await;
    send_json(
        &mut other,
        json!({"type":"response.inject","response_id":response_id,"input":input}),
    )
    .await;
    assert_eq!(recv_json(&mut other).await["error"]["code"], "response_not_found");
}

#[tokio::test]
async fn websocket_injection_schema_error_closes_connection() {
    let mock = MockResponsesServer::start(vec![]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    for discriminator in ["response.inject", r"response.\u0069nject"] {
        let mut ws = connect_responses_ws(&gateway_url).await;
        let wire = format!(
            r#"{{"type":"{discriminator}","response_id":"resp_unknown","input":[{{"type":"message","content":"invalid"}}]}}"#
        );
        ws.send(Message::Text(wire.into())).await.unwrap();
        assert_eq!(recv_json(&mut ws).await["type"], "error");
        recv_close_or_end(&mut ws).await;
    }
}

#[tokio::test]
async fn websocket_single_agent_injection_rejection_keeps_other_lanes_usable() {
    let created = json!({"type":"response.created","response":{"id":"upstream","status":"in_progress"}});
    let (mock, _dropped) = MockResponsesServer::start_hanging(format!("data: {created}\n\n")).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    send_json(
        &mut ws,
        json!({"type":"response.create","model":"test-model","input":"wait",
        "store":true,"stream_id":"active"}),
    )
    .await;
    let created = recv_json(&mut ws).await;
    assert_eq!(created["type"], "response.created");
    let input = json!([{"type":"function_call_output","call_id":"unknown","output":"unused"}]);
    send_json(
        &mut ws,
        json!({"type":"response.inject","response_id":created["response"]["id"],"input":input}),
    )
    .await;
    let rejected = recv_json(&mut ws).await;
    assert_eq!(rejected["type"], "response.inject.failed");
    assert_eq!(rejected["error"]["code"], "invalid_input");
    assert_eq!(rejected["input"], input);
    assert_eq!(rejected["stream_id"], "active");
    send_json(
        &mut ws,
        json!({"type":"response.create","model":"test-model","input":"warmup",
        "generate":false,"store":true,"stream_id":"other"}),
    )
    .await;
    let completed = recv_until_completed(&mut ws).await;
    assert_eq!(completed.last().unwrap()["stream_id"], "other");
    assert_eq!(mock.request_bodies().await.len(), 1);
}

#[tokio::test]
async fn websocket_invalid_upstream_search_emits_failed_and_retains_injection_route() {
    let malformed = sse_tool_search_call_response().replace(r#"{\"query\":\"weather\"}"#, "not valid JSON");
    assert!(malformed.contains("not valid JSON"));
    let mock = MockResponsesServer::start(vec![malformed]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    send_json(
        &mut ws,
        json!({"type":"response.create","model":"test-model","input":"search",
        "store":true,"tools":[tool_search_declaration(), deferred_weather_function()]}),
    )
    .await;
    let mut sequence = 0;
    let failed = loop {
        let event = recv_json(&mut ws).await;
        assert_ne!(event["type"], "error", "{event}");
        assert_ne!(event["type"], "response.completed");
        assert_eq!(event["sequence_number"], sequence);
        sequence += 1;
        if event["type"] == "response.failed" {
            break event;
        }
    };
    assert_eq!(failed["response"]["status"], "failed");
    assert_eq!(failed["response"]["error"]["code"], "tool_error");
    let input = json!([{"type":"function_call_output","call_id":"unknown","output":"unused"}]);
    send_json(
        &mut ws,
        json!({"type":"response.inject","response_id":failed["response"]["id"],"input":input}),
    )
    .await;
    let rejected = recv_json(&mut ws).await;
    assert_eq!(rejected["type"], "response.inject.failed");
    assert_eq!(rejected["error"]["code"], "response_already_completed");
    assert_eq!(rejected["input"], input);
    send_json(
        &mut ws,
        json!({"type":"response.create","model":"test-model","input":"warmup",
        "generate":false,"store":true}),
    )
    .await;
    recv_until_completed(&mut ws).await;
}

#[tokio::test]
async fn websocket_multi_agent_warmup_resolves_outputs_before_generated_continuation() {
    for discovery in [false, true] {
        let call = if discovery {
            sse_tool_search_call_response()
        } else {
            sse_weather_function_call_response()
        };
        let answer = if discovery {
            sse_weather_function_call_response()
        } else {
            sse_response("answer", "message", "sunny")
        };
        let mock = MockResponsesServer::start(vec![call, answer]).await;
        let fixture = storage_backed_state(&mock.url).await;
        let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        let tools = if discovery {
            json!([tool_search_declaration(), deferred_weather_function()])
        } else {
            json!([{"type":"function","name":"get_weather","parameters":{"type":"object"}}])
        };
        send_json(
            &mut ws,
            json!({"type":"response.create","model":"test-model","input":"weather",
            "store":true,"multi_agent":{"enabled":true},"tools":tools}),
        )
        .await;
        let first = recv_until_completed(&mut ws).await;
        let parent = &first.last().unwrap()["response"]["id"];
        let input = if discovery {
            json!([{"type":"tool_search_output","call_id":"call_search","execution":"client",
                "status":"completed","tools":[deferred_weather_function()]}])
        } else {
            json!([{"type":"function_call_output","call_id":"call_weather","output":"sunny"}])
        };
        // An invalid warmup must leave the parent's pending ownership intact.
        send_json(
            &mut ws,
            json!({"type":"response.create","model":"test-model","store":true,
            "previous_response_id":parent,"generate":false,
            "input":[{"type":"function_call_output","call_id":"unknown","output":"unused"}]}),
        )
        .await;
        assert_eq!(recv_json(&mut ws).await["type"], "error");
        send_json(
            &mut ws,
            json!({"type":"response.create","model":"test-model","store":true,
            "previous_response_id":parent,"generate":false,"input":input}),
        )
        .await;
        let warmup = recv_until_completed(&mut ws).await;
        assert_eq!(mock.request_bodies().await.len(), 1, "warmup must not infer");
        send_json(
            &mut ws,
            json!({"type":"response.create","model":"test-model","store":true,
            "previous_response_id":warmup.last().unwrap()["response"]["id"],"input":[]}),
        )
        .await;
        let generated = recv_until_completed(&mut ws).await;
        let output = &generated.last().unwrap()["response"]["output"];
        if discovery {
            assert!(
                output
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["call_id"] == "call_weather")
            );
        } else {
            assert_eq!(output[0]["content"][0]["text"], "sunny");
        }
        let requests = mock.request_bodies().await;
        assert_eq!(requests.len(), 2);
        assert!(requests[1]["input"].as_array().unwrap().iter().any(|item| {
            item["type"] == "function_call_output"
                && item["call_id"] == if discovery { "call_search" } else { "call_weather" }
        }));
    }
}

// Keep a sibling inference active so client injection tests do not depend on
// winning the response-finalization race when all agents wait for client output.
async fn mock_with_active_sibling(mut responses: Vec<String>) -> (MockResponsesServer, oneshot::Sender<()>) {
    let first = responses.remove(0);
    let mut events: Vec<Value> = first
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect();
    let terminal = events
        .iter()
        .position(|event| event["type"] == "response.completed")
        .unwrap();
    let item = json!({"type":"function_call","id":"fc_spawn","call_id":"spawn_worker",
        "name":"spawn_agent","arguments":"","status":"in_progress"});
    let added = json!({"type":"response.output_item.added","output_index":1,"item":item});
    let mut done = json!({"type":"response.output_item.done","output_index":1,"item":item});
    done["item"]["arguments"] =
        json!(json!({"task_name":"worker","message":"Say worker done","fork_turns":"none"}).to_string());
    done["item"]["status"] = json!("completed");
    events.splice(terminal..terminal, [added, done]);
    let mut first = String::new();
    for (sequence, mut event) in events.into_iter().enumerate() {
        event["sequence_number"] = json!(sequence);
        writeln!(first, "data: {event}\n").unwrap();
    }
    first.push_str("data: [DONE]\n\n");
    let (arrived, _arrival) = oneshot::channel();
    let (release, held) = oneshot::channel();
    let mut responses_with_sibling = vec![
        MockResponse::Static(first),
        MockResponse::Gated {
            response: sse_response("resp_worker", "msg_worker", "worker done"),
            arrived,
            release: held,
        },
    ];
    responses_with_sibling.extend(responses.into_iter().map(MockResponse::Static));
    (
        MockResponsesServer::start_with_responses(responses_with_sibling).await,
        release,
    )
}

#[tokio::test]
async fn websocket_live_injection_bypasses_create_lane_fifo() {
    let (mock, release) = mock_with_active_sibling(vec![
        sse_weather_function_call_response(),
        sse_response("resp_answer", "msg_answer", "sunny"),
        sse_response("resp_next", "msg_next", "next"),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    send_json(
        &mut ws,
        json!({
            "type":"response.create","model":"test-model","input":"weather",
            "store":true,"stream_id":"lane","multi_agent":{"enabled":true},
            "tools":[{"type":"function","name":"get_weather","parameters":{"type":"object"}}]
        }),
    )
    .await;
    let mut events = Vec::new();
    let mut response_id = None;
    loop {
        let event = recv_json(&mut ws).await;
        assert_ne!(event["type"], "error", "{event}");
        if event["type"] == "response.created" {
            response_id = event["response"]["id"].as_str().map(str::to_owned);
        }
        let call_done = event["type"] == "response.output_item.done" && event["item"]["call_id"] == "call_weather";
        events.push(event);
        if call_done {
            break;
        }
    }
    let response_id = response_id.unwrap();
    // This create is queued behind the pending tool call. Injection must bypass it.
    send_json(
        &mut ws,
        json!({"type":"response.create","model":"test-model","input":"next","store":true,"stream_id":"lane"}),
    )
    .await;
    send_json(
        &mut ws,
        json!({
            "type":"response.inject","response_id":response_id,
            "input":[{"type":"function_call_output","call_id":"call_weather","output":"sunny"}]
        }),
    )
    .await;
    let mut release = Some(release);
    let mut accepted = false;
    let mut completed = 0;
    while !accepted || completed < 2 {
        let event = recv_json(&mut ws).await;
        assert_ne!(event["type"], "error", "{event}");
        assert_ne!(event["type"], "response.inject.failed", "{event}");
        if event["type"] == "response.inject.created" {
            assert_eq!(event["response_id"], response_id);
            accepted = true;
            release.take().unwrap().send(()).unwrap();
        }
        if event["type"] == "response.completed" {
            completed += 1;
        }
        if completed == 0 || event["response"]["id"] == response_id || event["response_id"] == response_id {
            events.push(event);
        }
    }
    for pair in events.windows(2) {
        assert!(
            pair[0]["sequence_number"].as_u64().unwrap() < pair[1]["sequence_number"].as_u64().unwrap(),
            "{pair:?}"
        );
    }
    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output"
                && item["call_id"] == "call_weather"
                && item["output"] == "sunny")
    );
}

#[tokio::test]
async fn websocket_multi_agent_continuation_crosses_http_in_both_directions() {
    let mock = MockResponsesServer::start(vec![
        sse_weather_function_call_response(),
        sse_response("resp_ws", "msg_ws", "sunny"),
        sse_response("resp_http", "msg_http", "continued"),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let client = reqwest::Client::new();
    let first = client
        .post(format!("{gateway_url}/v1/responses"))
        .json(&json!({
            "model":"test-model","input":"weather","store":true,"stream":true,
            "multi_agent":{"enabled":true},
            "tools":[{"type":"function","name":"get_weather","parameters":{"type":"object"}}]
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    let first_events: Vec<Value> = first
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let first_terminal = first_events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap();
    let parent = first_terminal["response"]["id"].as_str().unwrap();
    let mut ws = connect_responses_ws(&gateway_url).await;
    // Rejected storage settings must leave the stored parent available.
    send_json(
        &mut ws,
        json!({
            "type":"response.create","model":"test-model","previous_response_id":parent,
            "input":[{"type":"function_call_output","call_id":"call_weather","output":"sunny"}],
            "store":false
        }),
    )
    .await;
    assert_eq!(recv_json(&mut ws).await["type"], "error");
    send_json(
        &mut ws,
        json!({
            "type":"response.create","model":"test-model","previous_response_id":parent,
            "input":[{"type":"function_call_output","call_id":"call_weather","output":"sunny"}],
            "store":true
        }),
    )
    .await;
    let events = recv_until_completed(&mut ws).await;
    let terminal = events.last().unwrap();
    assert_eq!(terminal["type"], "response.completed", "{events:?}");
    let parent = terminal["response"]["id"].as_str().unwrap();
    ws.close(None).await.unwrap();
    let final_response = client
        .post(format!("{gateway_url}/v1/responses"))
        .json(
            &json!({"model":"test-model","previous_response_id":parent,"input":"continue","store":true,"stream":true}),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(final_response.contains("response.completed"), "{final_response}");
    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert!(
            request["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "function_call_output"
                    && item["call_id"] == "call_weather"
                    && item["output"] == "sunny")
        );
    }
}

#[tokio::test]
async fn websocket_disconnect_joins_multi_agent_waiting_for_client_output() {
    assert_pending_multi_agent_teardown(false).await;
}

#[tokio::test]
async fn websocket_shutdown_joins_multi_agent_waiting_for_client_output() {
    assert_pending_multi_agent_teardown(true).await;
}

async fn assert_pending_multi_agent_teardown(shutdown: bool) {
    let (mock, _release) = mock_with_active_sibling(vec![sse_weather_function_call_response()]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    send_json(
        &mut ws,
        json!({
            "type":"response.create","model":"test-model","input":"weather",
            "store":true,"multi_agent":{"enabled":true},
            "tools":[{"type":"function","name":"get_weather","parameters":{"type":"object"}}]
        }),
    )
    .await;
    loop {
        let event = recv_json(&mut ws).await;
        assert_ne!(event["type"], "error", "{event}");
        if event["type"] == "response.output_item.done"
            && event["item"]["type"] == "multi_agent_call_output"
            && event["item"]["call_id"] == "spawn_worker"
        {
            break;
        }
    }
    wait_for_request_count(&mock, 2).await;
    if shutdown {
        fixture.state.shutdown_token.cancel();
        recv_close_or_end(&mut ws).await;
    }
    drop(ws);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        fixture.state.websocket_tracker.wait_until_idle(),
    )
    .await
    .expect("connection teardown must join its pending multi-agent response");
    assert!(!mock.request_bodies().await.is_empty());
}

#[tokio::test]
async fn websocket_live_discovery_output_loads_function_before_resuming() {
    let (mock, release) = mock_with_active_sibling(vec![
        sse_tool_search_call_response(),
        sse_weather_function_call_response(),
        sse_response("resp_answer", "msg_answer", "sunny"),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    send_json(
        &mut ws,
        json!({
            "type":"response.create","model":"test-model","input":"weather","store":true,
            "multi_agent":{"enabled":true},"tools":[tool_search_declaration(), deferred_weather_function()]
        }),
    )
    .await;
    let mut response_id = None;
    let mut release = Some(release);
    let mut acks = 0;
    let mut completed = false;
    while !completed || acks < 2 {
        let event = recv_json(&mut ws).await;
        assert_ne!(event["type"], "error", "{event}");
        assert_ne!(event["type"], "response.inject.failed", "{event}");
        match event["type"].as_str() {
            Some("response.created") => response_id = event["response"]["id"].as_str().map(str::to_owned),
            Some("response.inject.created") => {
                acks += 1;
                if acks == 2 {
                    release.take().unwrap().send(()).unwrap();
                }
            }
            Some("response.completed") => completed = true,
            Some("response.output_item.done") => {
                let output = match event["item"]["type"].as_str() {
                    Some("tool_search_call") => Some(json!({
                        "type":"tool_search_output","call_id":event["item"]["call_id"],
                        "execution":"client","status":"completed","tools":[deferred_weather_function()]
                    })),
                    Some("function_call") => Some(json!({
                        "type":"function_call_output","call_id":event["item"]["call_id"],"output":"sunny"
                    })),
                    _ => None,
                };
                if let Some(output) = output {
                    send_json(
                        &mut ws,
                        json!({"type":"response.inject","response_id":response_id,"input":[output]}),
                    )
                    .await;
                }
            }
            _ => {}
        }
    }
    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 4);
    assert!(
        requests[2]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "get_weather"),
        "{:?}",
        requests[2]
    );
}

#[tokio::test]
async fn websocket_live_shell_and_custom_outputs_resume_their_calls() {
    for (tool, call, output) in [
        (
            json!({"type":"custom","name":"apply_patch"}),
            sse_custom_tool_call_response(),
            json!({"type":"custom_tool_call_output","call_id":"call_custom_1","output":"applied"}),
        ),
        (
            json!({"type":"shell","environment":{"type":"local"}}),
            {
                let created =
                    json!({"type":"response.created","response":{"id":"upstream_shell","status":"in_progress"}});
                let item = json!({"type":"function_call","id":"fc_shell","call_id":"call_shell",
                    "name":"shell","arguments":"{\"commands\":[\"printf ok\"]}","status":"completed"});
                let added = json!({"type":"response.output_item.added","output_index":0,
                    "item":{"type":"function_call","id":"fc_shell","call_id":"call_shell",
                        "name":"shell","arguments":"","status":"in_progress"}});
                let done = json!({"type":"response.output_item.done","output_index":0,"item":item});
                let terminal =
                    json!({"type":"response.completed","response":{"id":"upstream_shell","status":"completed"}});
                format!("data: {created}\n\ndata: {added}\n\ndata: {done}\n\ndata: {terminal}\n\ndata: [DONE]\n\n")
            },
            json!({"type":"shell_call_output","call_id":"call_shell","output":[
                {"stdout":"ok","stderr":"","outcome":{"type":"exit","exit_code":0}}
            ]}),
        ),
    ] {
        let (mock, release) = mock_with_active_sibling(vec![call, sse_response("resp_done", "msg_done", "done")]).await;
        let fixture = storage_backed_state(&mock.url).await;
        let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        send_json(
            &mut ws,
            json!({"type":"response.create","model":"test-model","input":"call the tool",
            "store":true,"multi_agent":{"enabled":true},"tools":[tool]}),
        )
        .await;
        let mut response_id = None;
        let mut release = Some(release);
        let mut accepted = false;
        let mut completed = false;
        while !accepted || !completed {
            let event = recv_json(&mut ws).await;
            assert_ne!(event["type"], "error", "{event}");
            assert_ne!(event["type"], "response.inject.failed", "{event}");
            match event["type"].as_str() {
                Some("response.created") => response_id = event["response"]["id"].as_str().map(str::to_owned),
                Some("response.inject.created") => {
                    accepted = true;
                    release.take().unwrap().send(()).unwrap();
                }
                Some("response.completed") => completed = true,
                Some("response.output_item.done") if event["item"]["call_id"] == output["call_id"] => {
                    send_json(
                        &mut ws,
                        json!({"type":"response.inject","response_id":response_id,"input":[output]}),
                    )
                    .await;
                }
                _ => {}
            }
        }
        assert_eq!(mock.request_bodies().await.len(), 3);
        // The retained failure must return the same structured output for continuation.
        send_json(
            &mut ws,
            json!({"type":"response.inject","response_id":response_id,"input":[output]}),
        )
        .await;
        let failed = recv_json(&mut ws).await;
        assert_eq!(failed["error"]["code"], "response_already_completed");
        assert_eq!(failed["input"], json!([output]));
    }
}

#[tokio::test]
async fn websocket_quiescent_tool_call_completes_and_late_input_continues() {
    let mock = MockResponsesServer::start(vec![
        sse_weather_function_call_response(),
        sse_response("resp_resumed", "msg_resumed", "sunny"),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    send_json(
        &mut ws,
        json!({"type":"response.create","model":"test-model","input":"weather",
            "store":true,"multi_agent":{"enabled":true},
            "tools":[{"type":"function","name":"get_weather","parameters":{"type":"object"}}]
        }),
    )
    .await;
    let events = recv_until_completed(&mut ws).await;
    let terminal = events.last().unwrap();
    assert_eq!(terminal["type"], "response.completed");
    assert_eq!(terminal["response"]["output"][0]["call_id"], "call_weather");
    let response_id = &terminal["response"]["id"];
    let good = json!({"type":"function_call_output","call_id":"call_weather","output":"sunny"});
    let mut sequence = terminal["sequence_number"].as_u64().unwrap();
    // Once committed, finalization takes precedence over batch validation.
    for input in [
        json!([good, good]),
        json!([good, {"type":"function_call_output","call_id":"unknown","output":"unused"}]),
        json!([good]),
    ] {
        send_json(
            &mut ws,
            json!({"type":"response.inject","response_id":response_id,"input":input}),
        )
        .await;
        let rejected = recv_json(&mut ws).await;
        assert_eq!(rejected["type"], "response.inject.failed");
        assert_eq!(rejected["error"]["code"], "response_already_completed");
        assert_eq!(rejected["input"], input);
        assert!(rejected["sequence_number"].as_u64().unwrap() > sequence);
        sequence = rejected["sequence_number"].as_u64().unwrap();
        if input.as_array().unwrap().len() == 1 {
            send_json(
                &mut ws,
                json!({"type":"response.create","model":"test-model","store":true,
                "previous_response_id":response_id,"input":rejected["input"]}),
            )
            .await;
        }
    }
    let resumed = recv_until_completed(&mut ws).await;
    assert_eq!(resumed.last().unwrap()["type"], "response.completed");
    assert_eq!(
        resumed.last().unwrap()["response"]["output"][0]["content"][0]["text"],
        "sunny"
    );
    assert_eq!(mock.request_bodies().await.len(), 2);
}

async fn assert_live_batch_rejections(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    response_id: &Value,
    input: &Value,
) {
    // Rejected batches are atomic: the valid call remains available for the
    // subsequent acceptance and duplicate-output check on this same socket.
    for (batch, message) in [
        (
            json!([input[0], input[0]]),
            "Tool call 'call_weather' already has an output.".to_owned(),
        ),
        (
            json!([input[0], {"type":"function_call_output","call_id":"unknown","output":"unused"}]),
            format!(
                "Tool call 'unknown' is not pending on response '{}'.",
                response_id.as_str().unwrap()
            ),
        ),
        (
            json!([{"type":"custom_tool_call_output","call_id":"call_weather","output":"wrong kind"}]),
            "output for call_weather has kind Custom; expected Function".to_owned(),
        ),
    ] {
        send_json(
            ws,
            json!({"type":"response.inject","response_id":response_id,"input":batch}),
        )
        .await;
        loop {
            let event = recv_json(ws).await;
            assert_ne!(event["type"], "error", "{event}");
            assert_ne!(
                event["type"], "response.completed",
                "sibling must keep the response active"
            );
            assert_ne!(event["type"], "response.inject.created", "invalid batch must be atomic");
            if event["type"] == "response.inject.failed" {
                assert_eq!(event["error"]["code"], "invalid_input");
                assert_eq!(event["error"]["message"], message);
                assert_eq!(event["input"], batch);
                assert_eq!(event["response_id"], *response_id);
                break;
            }
        }
    }
}

#[tokio::test]
async fn websocket_invalid_live_batches_are_atomic_and_do_not_close() {
    let (mock, release) = mock_with_active_sibling(vec![
        sse_weather_function_call_response(),
        sse_response("resp_answer", "msg_answer", "sunny"),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    send_json(
        &mut ws,
        json!({"type":"response.create","model":"test-model","input":"weather",
            "store":true,"stream_id":"lane","multi_agent":{"enabled":true},
            "tools":[{"type":"function","name":"get_weather","parameters":{"type":"object"}}]
        }),
    )
    .await;
    let created = recv_json(&mut ws).await;
    let response_id = &created["response"]["id"];
    loop {
        let event = recv_json(&mut ws).await;
        assert_ne!(event["type"], "error");
        if event["type"] == "response.output_item.done" && event["item"]["call_id"] == "call_weather" {
            break;
        }
    }
    let input = json!([{"type":"function_call_output","call_id":"call_weather","output":"sunny"}]);
    assert_live_batch_rejections(&mut ws, response_id, &input).await;
    let injection = json!({"type":"response.inject","response_id":response_id,"input":input});
    send_json(&mut ws, injection.clone()).await;
    send_json(&mut ws, injection).await;
    let mut accepted = 0;
    let mut rejected = 0;
    let mut sequence = 0;
    while accepted + rejected < 2 {
        let event = recv_json(&mut ws).await;
        assert_ne!(event["type"], "error", "{event}");
        let next = event["sequence_number"].as_u64().unwrap();
        assert!(next > sequence);
        sequence = next;
        match event["type"].as_str() {
            Some("response.inject.created") => accepted += 1,
            Some("response.inject.failed") => {
                rejected += 1;
                assert_eq!(event["error"]["code"], "invalid_input");
                assert_eq!(
                    event["error"]["message"],
                    "Tool call 'call_weather' already has an output."
                );
                assert_eq!(event["input"], input);
                assert_eq!(event["stream_id"], "lane");
                assert_eq!(event["response_id"], *response_id);
            }
            _ => {}
        }
    }
    assert_eq!((accepted, rejected), (1, 1));
    release.send(()).unwrap();
    let terminal = recv_until_completed(&mut ws).await;
    assert_eq!(terminal.last().unwrap()["type"], "response.completed");
    assert_eq!(
        mock.request_bodies().await.len(),
        3,
        "duplicate must not trigger another inference"
    );
    let requests = mock.request_bodies().await;
    assert_eq!(
        requests[2]["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "function_call_output" && item["call_id"] == "call_weather")
            .count(),
        1
    );
}

mod duplex_replay {
    //! Recorded client actions against the real gateway, with explicitly derived
    //! synthetic inference fixtures. These are NOT recorded upstream dependencies.
    use super::*;
    use agentic_core::tool::ToolSearchState;
    use agentic_core::types::event::ResponseStatus;
    use agentic_core::types::io::OutputItem;
    use agentic_core::types::io::output::OutputMessageContent;
    use serde::Deserialize;
    use std::collections::{BTreeSet, HashMap};

    #[derive(Deserialize)]
    struct Capture {
        format: String,
        sessions: Vec<Session>,
    }

    #[derive(Deserialize)]
    struct Session {
        handshake: Value,
        frames: Vec<Frame>,
    }

    #[derive(Clone, Copy, Deserialize)]
    #[serde(rename_all = "lowercase")]
    enum Direction {
        Client,
        Server,
    }

    #[derive(Deserialize)]
    struct Frame {
        ordinal: usize,
        direction: Direction,
        opcode: u8,
        fin: bool,
        text: Option<String>,
    }

    fn inference_fixtures(session: &Session) -> Vec<String> {
        let mut responses = Vec::new();
        let mut response = String::new();
        for frame in &session.frames {
            if !matches!(frame.direction, Direction::Server) || frame.opcode != 1 {
                continue;
            }
            let event: Value = serde_json::from_str(frame.text.as_deref().unwrap()).unwrap();
            let kind = event["type"].as_str().unwrap();
            if kind.starts_with("response.inject.") {
                continue;
            }
            assert_ne!(kind, "error");
            writeln!(response, "data: {event}\n").unwrap();
            if kind == "response.completed" {
                response.push_str("data: [DONE]\n\n");
                responses.push(std::mem::take(&mut response));
            }
        }
        assert!(response.is_empty());
        responses
    }

    fn milestone(event: &Value) -> bool {
        matches!(
            event["type"].as_str(),
            Some(
                "response.created"
                    | "response.output_item.done"
                    | "response.completed"
                    | "response.inject.created"
                    | "response.inject.failed"
                    | "error"
            )
        )
    }

    fn remap_request(event: &mut Value, ids: &HashMap<String, String>) {
        for key in ["response_id", "previous_response_id"] {
            if let Some(recorded) = event[key].as_str() {
                event[key] = json!(
                    ids.get(recorded)
                        .expect("request refers to a previously observed response")
                );
            }
        }
    }

    fn compare(expected: &Value, actual: &Value, ids: &mut HashMap<String, String>) {
        assert_eq!(actual["type"], expected["type"]);
        match expected["type"].as_str().unwrap() {
            "response.created" => {
                ids.insert(
                    expected["response"]["id"].as_str().unwrap().to_owned(),
                    actual["response"]["id"].as_str().unwrap().to_owned(),
                );
            }
            "response.output_item.done" => {
                assert_eq!(actual["item"]["type"], expected["item"]["type"]);
                assert_eq!(actual["item"]["call_id"], expected["item"]["call_id"]);
                assert_eq!(actual["item"]["arguments"], expected["item"]["arguments"]);
                assert_eq!(actual["item"]["content"], expected["item"]["content"]);
            }
            "response.completed" => {
                assert_eq!(actual["response"]["status"], "completed");
                assert_eq!(
                    actual["response"]["id"],
                    ids[expected["response"]["id"].as_str().unwrap()]
                );
            }
            "response.inject.created" | "response.inject.failed" => {
                assert_eq!(actual["response_id"], ids[expected["response_id"].as_str().unwrap()]);
                assert_eq!(actual["input"], expected["input"]);
                assert_eq!(actual["error"]["code"], expected["error"]["code"]);
            }
            _ => panic!("unexpected recorded milestone: {expected}"),
        }
    }

    #[tokio::test]
    async fn recorded_edge_sessions_drive_real_gateway_with_synthetic_inference() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
            "../agentic-server-core/tests/cassettes/multi_agent/multi-agent-gateway-ws-edge-cases-Qwen-Qwen3.6-35B-A3B-FP8-websocket.yaml");
        let capture: Capture = serde_yml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(capture.format, "responses-websocket-v1");
        for session in capture.sessions {
            let last_ordinal = std::cell::Cell::new(0);
            let work = async {
                let responses = inference_fixtures(&session);
                let expected_requests = responses.len();
                let mock = MockResponsesServer::start(responses).await;
                let fixture = storage_backed_state(&mock.url).await;
                let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
                let mut ws = connect_responses_ws(&gateway_url).await;
                let mut ids = HashMap::new();
                let mut last_sequence = None;
                for (ordinal, frame) in session.frames.iter().enumerate() {
                    last_ordinal.set(ordinal);
                    assert_eq!(frame.ordinal, ordinal);
                    assert!(frame.fin, "this fixture contains no fragmented messages");
                    if frame.opcode == 8 {
                        match frame.direction {
                            Direction::Client => ws.close(None).await.unwrap(),
                            Direction::Server => recv_close_or_end(&mut ws).await,
                        }
                        continue;
                    }
                    assert_eq!(frame.opcode, 1, "unsupported control frame in this fixture");
                    let mut event: Value = serde_json::from_str(frame.text.as_deref().unwrap()).unwrap();
                    match frame.direction {
                        Direction::Client => {
                            remap_request(&mut event, &ids);
                            send_json(&mut ws, event).await;
                        }
                        Direction::Server if milestone(&event) => {
                            let actual = loop {
                                let actual = recv_json(&mut ws).await;
                                assert_ne!(actual["type"], "error", "{actual}");
                                assert_ne!(actual["type"], "response.failed", "{actual}");
                                if actual["type"] == "response.created" {
                                    last_sequence = None;
                                }
                                let sequence = actual["sequence_number"].as_u64().unwrap();
                                assert!(last_sequence.is_none_or(|previous| previous < sequence));
                                last_sequence = Some(sequence);
                                if milestone(&actual) {
                                    break actual;
                                }
                            };
                            compare(&event, &actual, &mut ids);
                        }
                        Direction::Server => {}
                    }
                }
                assert_eq!(mock.request_bodies().await.len(), expected_requests);
            };
            tokio::time::timeout(std::time::Duration::from_secs(45), work)
                .await
                .unwrap_or_else(|_| panic!("duplex replay timed out: {}", session.handshake["probe"]["case"]));
        }
    }

    const STEERING_CATEGORIES: [&str; 4] = [
        "mixed-tools-steer",
        "edge-input-lifecycle",
        "edge-tools-settings",
        "edge-connections",
    ];

    fn steering_capture(provider: &str, category: &str) -> Capture {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../agentic-server-core/tests/cassettes/multi_turn_steering");
        let path = if provider == "gateway" {
            let prefix = format!("multi-turn-steering-gateway-{category}-");
            let mut matches = std::fs::read_dir(&directory)
                .unwrap()
                .map(Result::unwrap)
                .filter(|entry| entry.file_type().unwrap().is_file())
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name().and_then(|name| name.to_str()).is_some_and(|name| {
                        name.strip_prefix(&prefix)
                            .and_then(|model| model.strip_suffix("-websocket.yaml"))
                            .is_some_and(|model| !model.is_empty())
                    })
                })
                .collect::<Vec<_>>();
            matches.sort();
            assert_eq!(
                matches.len(),
                1,
                "expected exactly one gateway capture for {category}; archive stale model captures instead of choosing by timestamp or environment: {matches:?}"
            );
            matches.pop().unwrap()
        } else {
            directory.join(format!(
                "multi-turn-steering-{provider}-{category}-gpt-6-sol-websocket.yaml"
            ))
        };
        let capture: Capture = serde_yml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(capture.format, "responses-websocket-v1", "{}", path.display());
        if provider == "gateway" {
            for session in &capture.sessions {
                assert!(
                    session.frames.iter().any(|frame| {
                        frame.opcode == 1
                            && matches!(frame.direction, Direction::Server)
                            && serde_json::from_str::<Value>(frame.text.as_deref().unwrap()).unwrap()["type"]
                                == "response.created"
                    }),
                    "gateway prerequisite missing response.created: {} case {}; check model readiness/configuration before parity replay",
                    path.display(),
                    case(session)
                );
            }
        }
        capture
    }

    fn case(session: &Session) -> &str {
        session.handshake["probe"]["case"].as_str().unwrap()
    }

    fn messages(session: &Session) -> Vec<(usize, Direction, Value)> {
        session
            .frames
            .iter()
            .filter_map(|frame| {
                if frame.opcode != 1 {
                    return None;
                }
                assert!(frame.fin, "steering capture contains an unsupported fragmented frame");
                Some((
                    frame.ordinal,
                    frame.direction,
                    serde_json::from_str(frame.text.as_deref().unwrap()).unwrap(),
                ))
            })
            .collect()
    }

    fn assert_shared_steering_cases(reference: &Capture, gateway: &Capture, category: &str) -> usize {
        let key = |session: &Session| {
            (
                case(session).to_owned(),
                session.handshake["probe"]["connection_index"].as_u64().unwrap_or(0),
            )
        };
        let expected = reference
            .sessions
            .iter()
            .filter(|session| case(session) != "unsupported-model")
            .map(key)
            .collect::<BTreeSet<_>>();
        let actual = gateway.sessions.iter().map(key).collect::<BTreeSet<_>>();
        assert_eq!(
            expected.len(),
            reference
                .sessions
                .iter()
                .filter(|session| case(session) != "unsupported-model")
                .count(),
            "{category}: duplicate reference case/connection"
        );
        assert_eq!(
            actual.len(),
            gateway.sessions.len(),
            "{category}: duplicate gateway case/connection"
        );
        assert_eq!(
            expected, actual,
            "{category}: missing recording peer; every shared case and connection must exist in both providers"
        );
        gateway.sessions.iter().map(case).collect::<BTreeSet<_>>().len()
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Qualification {
        Comparable,
        MixedToolWorkflow,
        ModelOutputLimit,
        ExplicitDefaultMetadata,
        CodeInterpreterRuntime,
        VisionReasoningOnly,
    }

    fn qualification(session: &Session) -> Qualification {
        match case(session) {
            "mixed-tools-steer" => Qualification::MixedToolWorkflow,
            "settings-automatic" | "pending-user-steer" | "settings-explicit-defaults"
                if messages(session)
                    .iter()
                    .rev()
                    .find(|(_, _, event)| {
                        matches!(
                            event["type"].as_str(),
                            Some("response.completed" | "response.incomplete" | "response.failed")
                        )
                    })
                    .is_some_and(|(_, _, event)| {
                        event["type"] == "response.incomplete"
                            && event["response"]["incomplete_details"]["reason"] == "max_output_tokens"
                    }) =>
            {
                Qualification::ModelOutputLimit
            }
            "settings-explicit-defaults"
                if messages(session)
                    .iter()
                    .rev()
                    .find(|(_, _, event)| event["type"] == "response.created")
                    .is_some_and(|(_, _, event)| event["response"]["tool_choice"] == "none") =>
            {
                Qualification::ExplicitDefaultMetadata
            }
            "array-image-user"
                if messages(session)
                    .into_iter()
                    .rfind(|(_, _, event)| event["type"] == "response.completed")
                    .is_some_and(|(_, _, event)| {
                        event["response"]["output"].as_array().is_some_and(|items| {
                            !items.is_empty() && items.iter().all(|item| item["type"] == "reasoning")
                        })
                    }) =>
            {
                Qualification::VisionReasoningOnly
            }
            "hosted-tool-running" => Qualification::CodeInterpreterRuntime,
            _ => Qualification::Comparable,
        }
    }

    fn mixed_tool_plan(session: &Session) -> Value {
        json!(
            messages(session)
                .into_iter()
                .filter(|(_, direction, event)| {
                    matches!(direction, Direction::Server)
                        && matches!(
                            event["type"].as_str(),
                            Some("response.completed" | "response.incomplete")
                        )
                })
                .map(|(_, _, event)| {
                    event["response"]["output"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|item| matches!(item["type"].as_str(), Some("function_call" | "custom_tool_call")))
                        .map(|item| json!([item["type"], item["name"]]))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        )
    }

    #[derive(Debug, Default)]
    struct SteeringContract {
        submitted: usize,
        accepted: usize,
        pending: usize,
        failed: Vec<String>,
        creates: usize,
        input_errors: usize,
        upstream_errors: usize,
        invalid_stub_kinds: usize,
        last_status: Option<ResponseStatus>,
    }

    #[allow(
        clippy::too_many_lines,
        reason = "A single ordered trace pass keeps causal ownership assertions next to their state transitions"
    )]
    fn steering_contract(session: &Session) -> SteeringContract {
        let mut contract = SteeringContract::default();
        let mut submitted = VecDeque::new();
        let mut accepted = HashMap::<String, (Option<String>, Value)>::new();
        let mut terminals = HashMap::<String, Value>::new();
        let mut sequences = HashMap::<String, u64>::new();
        let mut active = HashMap::<Option<String>, String>::new();
        let mut response_lanes = HashMap::<String, Option<String>>::new();
        let mut committed = std::collections::HashSet::new();
        let mut settings = Vec::new();
        for (_, direction, event) in messages(session) {
            let kind = event["type"].as_str().unwrap();
            if matches!(direction, Direction::Client) {
                if kind == "response.steer" {
                    contract.submitted += 1;
                    submitted.push_back((
                        event["previous_response_id"].as_str().map(str::to_owned),
                        event["input"].clone(),
                    ));
                }
                continue;
            }
            let lane = event["stream_id"].as_str().map(str::to_owned);
            let parent = event["steer"]["previous_response_id"].as_str();
            if kind == "response.created" {
                contract.creates += 1;
                let id = event["response"]["id"].as_str().unwrap();
                assert_eq!(event["sequence_number"], 0, "{}: successor sequence", case(session));
                active.insert(lane.clone(), id.to_owned());
                response_lanes.insert(id.to_owned(), lane.clone());
                if let Some(parent) = event["response"]["previous_response_id"].as_str()
                    && response_lanes.get(parent) == Some(&lane)
                {
                    committed.extend(
                        accepted
                            .iter()
                            .filter(|(_, (target, _))| target.as_deref() == Some(parent))
                            .map(|(id, _)| id.clone()),
                    );
                }
                settings.push(event["response"].clone());
            }
            // Controls retain the parent's sequence after a successor begins.
            let response = parent
                .or_else(|| event["response"]["id"].as_str())
                .or_else(|| active.get(&lane).map(String::as_str))
                .unwrap_or("unrouted");
            if let Some(sequence) = event["sequence_number"].as_u64() {
                // Generic creation/input errors may use an independent error envelope.
                if kind != "error"
                    && !(kind == "response.steer.failed" && (parent.is_none() || !sequences.contains_key(response)))
                    && let Some(previous) = sequences.insert(response.to_owned(), sequence)
                {
                    assert!(
                        previous < sequence,
                        "{}: nonmonotonic parent sequence: {event}",
                        case(session)
                    );
                }
            }
            match kind {
                "response.steer.accepted" => {
                    contract.accepted += 1;
                    let submission = submitted.pop_front().expect("accepted without submission");
                    assert_eq!(submission.0.as_deref(), parent);
                    let id = event["steer"]["id"].as_str().unwrap().to_owned();
                    assert!(accepted.insert(id, submission).is_none(), "duplicate steering ID");
                }
                "response.steer.pending" => {
                    contract.pending += 1;
                    let id = event["steer"]["id"].as_str().unwrap();
                    let (target, _) = accepted.get(id).expect("pending before acceptance");
                    assert_eq!(target.as_deref(), parent);
                    let terminal = terminals.get(parent.unwrap()).expect("pending before parent terminal");
                    for stub in event["required_input"].as_array().unwrap() {
                        let call = terminal["output"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|call| call["call_id"] == stub["call_id"])
                            .unwrap_or_else(|| {
                                panic!("{}: stub {stub} not found in terminal {terminal}", case(session))
                            });
                        let valid = match stub["type"].as_str() {
                            Some("function_call_output") => {
                                call["type"] == "function_call" && call["name"] == stub["name"]
                            }
                            Some("custom_tool_call_output") => call["type"] == "custom_tool_call",
                            Some("shell_call_output") => call["type"] == "shell_call",
                            Some("tool_search_output") => {
                                call["type"] == "tool_search_call"
                                    && call["execution"] == "client"
                                    && stub["execution"] == "client"
                            }
                            _ => false,
                        };
                        if !valid {
                            contract.invalid_stub_kinds += 1;
                            eprintln!(
                                "{}: recorded wrong stub kind: {stub}; actual call: {call}",
                                case(session)
                            );
                        }
                    }
                }
                "response.steer.failed" => {
                    contract
                        .failed
                        .push(event["error"]["code"].as_str().unwrap().to_owned());
                    let original = if let Some(id) = event["steer"]["id"].as_str() {
                        assert!(
                            !committed.contains(id),
                            "committed steering cannot be returned as unapplied"
                        );
                        accepted.remove(id).expect("failure uses an unknown accepted ID")
                    } else {
                        submitted.pop_front().expect("failure without submission")
                    };
                    assert_eq!(original.0.as_deref(), parent);
                    assert_eq!(
                        original.1, event["steer"]["input"],
                        "failure did not return original input"
                    );
                }
                "response.completed" | "response.incomplete" | "response.failed" => {
                    contract.last_status = Some(deserialize_from_value(event["response"]["status"].clone()).unwrap());
                    terminals.insert(
                        event["response"]["id"].as_str().unwrap().to_owned(),
                        event["response"].clone(),
                    );
                }
                "error" if event["error"]["type"] == "invalid_request_error" => contract.input_errors += 1,
                "error" => contract.upstream_errors += 1,
                _ => {}
            }
        }
        assert!(submitted.is_empty(), "{}: submission never acknowledged", case(session));
        if case(session) == "settings-automatic" {
            assert_eq!(settings.len(), 2);
            for field in [
                "instructions",
                "tools",
                "tool_choice",
                "parallel_tool_calls",
                "reasoning",
                "text",
                "store",
                "max_tool_calls",
                "max_output_tokens",
            ] {
                assert_eq!(
                    settings[0][field], settings[1][field],
                    "automatic settings field {field}"
                );
            }
        }
        if case(session) == "settings-explicit-defaults" && settings.len() == 2 {
            assert_eq!(settings.len(), 2);
            assert_eq!(settings[1]["instructions"], Value::Null);
            assert_eq!(settings[1]["tools"], json!([]));
            assert!(matches!(settings[1]["tool_choice"].as_str(), Some("auto" | "none")));
            assert_eq!(settings[1]["max_tool_calls"], Value::Null);
        }
        contract
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Keep every captured coverage qualification and its evidence visible in one audit"
    )]
    fn compare_steering_captures(categories: &[&str]) {
        let mut reference_sessions = 0;
        let mut gateway_sessions = 0;
        let mut reference_only_sessions = 0;
        let mut classified = 0;
        for category in categories {
            let reference = steering_capture("openai-reference", category);
            let gateway = steering_capture("gateway", category);
            assert_shared_steering_cases(&reference, &gateway, category);
            reference_sessions += reference.sessions.len();
            reference_only_sessions += reference
                .sessions
                .iter()
                .filter(|session| case(session) == "unsupported-model")
                .count();
            gateway_sessions += gateway.sessions.len();
            for session in &reference.sessions {
                steering_contract(session);
            }
            for session in &gateway.sessions {
                let observed = steering_contract(session);
                let index = session.handshake["probe"]["connection_index"].as_u64().unwrap_or(0);
                let expected = reference
                    .sessions
                    .iter()
                    .find(|other| {
                        case(other) == case(session)
                            && other.handshake["probe"]["connection_index"].as_u64().unwrap_or(0) == index
                    })
                    .unwrap();
                let baseline = steering_contract(expected);
                assert_eq!(
                    observed.invalid_stub_kinds,
                    0,
                    "{}: gateway pending kind",
                    case(session)
                );
                assert_eq!(
                    baseline.invalid_stub_kinds,
                    0,
                    "{}: reference pending kind",
                    case(session)
                );
                assert_eq!(
                    observed.upstream_errors,
                    0,
                    "{}: gateway upstream prerequisite",
                    case(session)
                );
                let status = qualification(session);
                eprintln!(
                    "{category}/{} connection {index}: {status:?}; gateway {observed:?}; OpenAI {baseline:?}",
                    case(session)
                );
                match status {
                    Qualification::Comparable | Qualification::VisionReasoningOnly => {
                        assert_eq!(observed.invalid_stub_kinds, 0);
                        assert_eq!(baseline.invalid_stub_kinds, 0);
                        assert_eq!(observed.accepted, baseline.accepted, "{}", case(session));
                        assert_eq!(observed.submitted, baseline.submitted, "{}", case(session));
                        assert_eq!(observed.input_errors, baseline.input_errors, "{}", case(session));
                        assert_eq!(
                            observed.creates,
                            baseline.creates,
                            "{}: logical response creation count",
                            case(session)
                        );
                        assert_eq!(observed.failed, baseline.failed, "{}", case(session));
                        assert_eq!(observed.pending > 0, baseline.pending > 0, "{}", case(session));
                        assert_eq!(
                            observed.last_status,
                            baseline.last_status,
                            "{}: final response status",
                            case(session)
                        );
                        if matches!(case(session), "array-image-user" | "array-multiple-user") {
                            if case(session) == "array-image-user" {
                                assert_eq!(observed.accepted, 1);
                                assert_eq!(observed.creates, 2);
                                assert_eq!(
                                    qualification(expected),
                                    Qualification::Comparable,
                                    "reference vision prerequisite must include an assistant answer"
                                );
                            }
                            let inputs = |session: &Session| {
                                messages(session)
                                    .into_iter()
                                    .filter(|(_, direction, event)| {
                                        matches!(direction, Direction::Client) && event["type"] == "response.steer"
                                    })
                                    .flat_map(|(_, _, event)| normalized_steering_messages(&event["input"]))
                                    .collect::<Vec<_>>()
                            };
                            assert_eq!(inputs(session), inputs(expected), "shared image/messages fixture");
                            for source in [session, expected] {
                                let terminal = messages(source)
                                    .into_iter()
                                    .rfind(|(_, _, event)| event["type"] == "response.completed")
                                    .expect("media/messages steering must have a completed successor")
                                    .2;
                                let output: Vec<OutputItem> =
                                    deserialize_from_value(terminal["response"]["output"].clone()).unwrap();
                                let answer = output
                                    .iter()
                                    .filter_map(|item| match item {
                                        OutputItem::Message(message) if message.role == "assistant" => Some(message),
                                        _ => None,
                                    })
                                    .flat_map(|message| &message.content)
                                    .map(agentic_core::types::io::OutputMessageContent::text)
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                if case(source) == "array-multiple-user" {
                                    assert_eq!(inputs(source).len(), 2);
                                    assert!(answer.contains("Harbor"));
                                    assert!(answer.contains("Friday"));
                                    assert!(answer.contains("17:00"));
                                    let first = answer.find("MULTI_USER_FIRST_ORCHID").unwrap();
                                    let second = answer.find("MULTI_USER_SECOND_IRIS").unwrap();
                                    assert!(first < second, "final answer must retain both user tokens in order");
                                } else {
                                    let input = inputs(source);
                                    assert_eq!(input.len(), 1, "one steered user message");
                                    let parts: Vec<agentic_core::types::io::InputContent> =
                                        deserialize_from_value(input[0]["content"].clone()).unwrap();
                                    let [
                                        agentic_core::types::io::InputContent::InputText(prompt),
                                        agentic_core::types::io::InputContent::InputImage(image),
                                    ] = parts.as_slice()
                                    else {
                                        panic!("vision steering must retain text followed by the image");
                                    };
                                    assert!(prompt.text.contains("IMAGE_STEER_ORCHID"));
                                    assert_eq!(image.detail.as_deref(), Some("low"));
                                    assert!(
                                        image
                                            .image_url
                                            .as_deref()
                                            .unwrap()
                                            .starts_with("data:image/png;base64,")
                                    );
                                    if qualification(source) == Qualification::VisionReasoningOnly {
                                        // Recorded backend output gap: reasoning is not an
                                        // assistant answer, even when it contains the marker.
                                        assert!(answer.is_empty());
                                        let reasoning = output
                                            .iter()
                                            .filter_map(|item| match item {
                                                OutputItem::Reasoning(item) => Some(item),
                                                _ => None,
                                            })
                                            .flat_map(|item| &item.content)
                                            .map(|part| part.text.as_str())
                                            .collect::<Vec<_>>()
                                            .join("\n");
                                        assert!(reasoning.contains("IMAGE_STEER_ORCHID"));
                                        assert!(reasoning.contains("red"));
                                        assert!(reasoning.contains("blue"));
                                        assert_eq!(baseline.last_status, Some(ResponseStatus::Completed));
                                        classified += 1;
                                    } else {
                                        assert!(answer.contains("IMAGE_STEER_ORCHID"));
                                        let answer = answer.to_ascii_lowercase();
                                        assert!(answer.contains("red"));
                                        assert!(answer.contains("cyan") || answer.contains("blue"));
                                    }
                                    // Exact image forwarding is asserted by the inference spy;
                                    // model color names and interpretation of the marker differ.
                                }
                            }
                        }
                    }
                    Qualification::ModelOutputLimit => {
                        // Assert the actual incomplete evidence. An unexercised
                        // workflow is not a successful steering parity result.
                        let terminal = messages(session)
                            .into_iter()
                            .rfind(|(_, _, event)| {
                                matches!(
                                    event["type"].as_str(),
                                    Some("response.completed" | "response.incomplete" | "response.failed")
                                )
                            })
                            .unwrap()
                            .2;
                        assert_eq!(observed.last_status, Some(ResponseStatus::Incomplete));
                        assert_eq!(baseline.last_status, Some(ResponseStatus::Completed));
                        assert_eq!(
                            terminal["response"]["incomplete_details"]["reason"],
                            "max_output_tokens"
                        );
                        assert!(terminal["response"]["max_output_tokens"].as_u64().unwrap() > 0);
                        assert_eq!(
                            terminal["response"]["usage"]["output_tokens"],
                            terminal["response"]["max_output_tokens"]
                        );
                        assert_eq!(observed.input_errors, 0);
                        assert!(observed.failed.is_empty());
                        match case(session) {
                            "settings-automatic" => {
                                assert_eq!(
                                    (
                                        observed.submitted,
                                        observed.accepted,
                                        observed.pending,
                                        observed.creates
                                    ),
                                    (1, 1, 0, 2)
                                );
                                assert_eq!(
                                    (
                                        baseline.submitted,
                                        baseline.accepted,
                                        baseline.pending,
                                        baseline.creates
                                    ),
                                    (1, 1, 0, 2)
                                );
                            }
                            "pending-user-steer" => {
                                assert_eq!(
                                    (
                                        observed.submitted,
                                        observed.accepted,
                                        observed.pending,
                                        observed.creates
                                    ),
                                    (0, 0, 0, 1)
                                );
                                assert_eq!(
                                    (
                                        baseline.submitted,
                                        baseline.accepted,
                                        baseline.pending,
                                        baseline.creates
                                    ),
                                    (2, 2, 2, 1)
                                );
                                assert_eq!(
                                    mixed_tool_plan(session),
                                    json!([[["function_call", "get_project_status"]]])
                                );
                                assert!(terminal["response"]["output"].as_array().unwrap().iter().any(|item| {
                                    item["type"] == "function_call"
                                        && item["name"] == "get_project_status"
                                        && item["status"] == "completed"
                                }));
                            }
                            "settings-explicit-defaults" => {
                                assert_eq!(
                                    (
                                        observed.submitted,
                                        observed.accepted,
                                        observed.pending,
                                        observed.creates
                                    ),
                                    (0, 0, 0, 1)
                                );
                                assert_eq!(
                                    (
                                        baseline.submitted,
                                        baseline.accepted,
                                        baseline.pending,
                                        baseline.creates
                                    ),
                                    (1, 1, 1, 2)
                                );
                                assert_eq!(mixed_tool_plan(session), json!([[]]));
                            }
                            _ => unreachable!("only the three recorded model-budget gaps are qualified"),
                        }
                        classified += 1;
                    }
                    Qualification::CodeInterpreterRuntime => {
                        assert_eq!(observed.accepted, 1);
                        assert_eq!(observed.creates, 2);
                        classified += 1;
                    }
                    Qualification::ExplicitDefaultMetadata => {
                        assert_eq!(observed.accepted, baseline.accepted);
                        assert_eq!(observed.submitted, baseline.submitted);
                        assert_eq!(observed.failed, baseline.failed);
                        assert_eq!(observed.pending > 0, baseline.pending > 0);
                        assert_eq!(observed.input_errors, 0);
                        assert_eq!(observed.creates, 2);
                        assert_eq!(baseline.creates, 2);
                        let successor = |session: &Session| {
                            messages(session)
                                .into_iter()
                                .rfind(|(_, _, event)| event["type"] == "response.created")
                                .unwrap()
                                .2["response"]
                                .clone()
                        };
                        let actual = successor(session);
                        let reference = successor(expected);
                        assert_eq!(actual["tool_choice"], "none");
                        assert_eq!(reference["tool_choice"], "auto");
                        // The ordinary gateway request defaults predate steering.
                        // The replay below checks those defaults reach inference;
                        // their metadata is not identical to OpenAI's defaults.
                        assert_eq!(actual["parallel_tool_calls"], false);
                        assert_eq!(reference["parallel_tool_calls"], true);
                        classified += 1;
                    }
                    Qualification::MixedToolWorkflow => {
                        assert_eq!(observed.accepted, baseline.accepted);
                        assert_eq!(observed.pending, baseline.pending);
                        assert_eq!(observed.creates, 2);
                        assert_eq!(baseline.creates, 3);
                        assert_eq!(
                            mixed_tool_plan(session),
                            json!([
                                [
                                    ["function_call", "get_weather"],
                                    ["custom_tool_call", "set_temperature_unit"]
                                ],
                                []
                            ])
                        );
                        assert_eq!(
                            mixed_tool_plan(expected),
                            json!([
                                [["function_call", "get_weather"]],
                                [["custom_tool_call", "set_temperature_unit"]],
                                []
                            ])
                        );
                        let inputs = |session: &Session| {
                            messages(session)
                                .into_iter()
                                .filter(|(_, direction, event)| {
                                    matches!(direction, Direction::Client) && event["type"] == "response.steer"
                                })
                                .map(|(_, _, event)| event["input"].clone())
                                .collect::<Vec<_>>()
                        };
                        assert_eq!(inputs(session), inputs(expected), "original mixed steering input order");
                        classified += 1;
                    }
                }
            }
        }
        assert_eq!(
            reference_sessions,
            gateway_sessions + reference_only_sessions,
            "only OpenAI's unsupported-model probe is absent"
        );
        eprintln!("{gateway_sessions} gateway sessions audited; {classified} explicitly qualified outcomes");
    }

    #[test]
    fn steering_captures_compare_causal_contracts_and_explicitly_classify_unqualified_cases() {
        compare_steering_captures(&STEERING_CATEGORIES);
    }

    #[test]
    fn vision_captures_compare_causal_contracts() {
        compare_steering_captures(&["vision-steering"]);
    }

    fn normalize_synthetic_custom(item: &mut Value) {
        if item["type"] != "custom_tool_call" {
            return;
        }
        let custom: agentic_core::types::io::CustomToolCall = deserialize_from_value(item.clone()).unwrap();
        let function = agentic_core::types::io::InputFunctionToolCall::from(custom);
        let call = agentic_core::types::io::FunctionToolCall {
            async_execution: false,
            agent: function.agent,
            id: function.id.expect("recorded custom item has a stable ID"),
            call_id: function.call_id,
            name: function.name,
            namespace: function.namespace,
            arguments: function.arguments,
            status: function
                .status
                .unwrap_or(agentic_core::types::event::MessageStatus::Completed),
        };
        *item = serialize_to_value(&OutputItem::FunctionCall(call)).unwrap();
    }

    /// The public duplex recording is not an upstream HTTP cassette. Derive only
    /// synthetic model frames, retaining original completed items and call IDs.
    /// Open every recorded item before a control gate so an incidental network
    /// race cannot discard another call the recorded client will later resolve.
    type SteeringGates = HashMap<String, (usize, oneshot::Sender<()>)>;

    #[allow(
        clippy::too_many_lines,
        reason = "Keep synthetic dependency adaptations and their causal gate construction in one trace transformation"
    )]
    fn steering_inference(session: &Session) -> (Vec<MockResponse>, SteeringGates) {
        let records = messages(session);
        let mut runs = Vec::<Vec<(usize, Value)>>::new();
        let mut current = Vec::new();
        for (ordinal, direction, event) in &records {
            if !matches!(direction, Direction::Server) {
                continue;
            }
            let kind = event["type"].as_str().unwrap();
            if kind == "response.created" {
                assert!(current.is_empty());
            }
            if matches!(
                kind,
                "response.created"
                    | "response.in_progress"
                    | "response.output_item.added"
                    | "response.output_item.done"
                    | "response.output_text.delta"
                    | "response.completed"
                    | "response.incomplete"
                    | "response.failed"
            ) {
                current.push((*ordinal, event.clone()));
                if matches!(kind, "response.completed" | "response.incomplete" | "response.failed") {
                    runs.push(std::mem::take(&mut current));
                }
            }
        }
        assert!(
            current.is_empty(),
            "captured model run lacks terminal: {}",
            case(session)
        );
        let mut gates = HashMap::new();
        let mut fixtures = Vec::new();
        for mut run in runs {
            let id = run[0].1["response"]["id"].as_str().unwrap().to_owned();
            let run_start = run[0].0;
            let terminal_ordinal = run.last().unwrap().0;
            let steer = records.iter().find(|(ordinal, direction, event)| {
                matches!(direction, Direction::Client)
                    && event["type"] == "response.steer"
                    && *ordinal > run[0].0
                    && *ordinal < terminal_ordinal
            });
            let split = steer.map(|(ordinal, _, _)| *ordinal);
            let trigger_delta = split.and_then(|ordinal| {
                run.iter()
                    .rev()
                    .find(|(at, event)| *at < ordinal && event["type"] == "response.output_text.delta")
                    .map(|(at, _)| *at)
            });
            if split.is_some() {
                // Register every already-observed item before a fast steering
                // reader can reach a completed earlier slot in the same chunk.
                run.sort_by_key(|(ordinal, event)| {
                    let priority = match event["type"].as_str().unwrap() {
                        "response.created" | "response.in_progress" => 0,
                        "response.output_item.added" => 1,
                        _ => 2,
                    };
                    (priority, *ordinal)
                });
            }
            let mut prefix = String::new();
            let mut suffix = String::new();
            for (ordinal, mut event) in run {
                let kind = event["type"].as_str().unwrap().to_owned();
                if kind == "response.output_text.delta" && Some(ordinal) != trigger_delta {
                    continue;
                }
                if kind == "response.incomplete" && event["response"]["incomplete_details"]["reason"] == "steered" {
                    event["type"] = json!("response.completed");
                    event["response"]["status"] = json!("completed");
                    event["response"]["incomplete_details"] = Value::Null;
                }
                // The corrected-intent source can contain provider-opaque
                // reasoning. A synthetic vLLM fixture supplies plaintext instead;
                // this is an explicit dependency adaptation, not recorded history.
                if event["item"]["type"] == "reasoning" && kind == "response.output_item.done" {
                    let item = event["item"].as_object_mut().unwrap();
                    item.remove("encrypted_content");
                    item.insert(
                        "content".to_owned(),
                        json!([{
                        "type":"reasoning_text", "text":"Synthetic local reasoning for protocol replay."}]),
                    );
                    item.insert("summary".to_owned(), json!([]));
                }
                if let Some(item) = event.get_mut("item") {
                    normalize_synthetic_custom(item);
                }
                if let Some(output) = event
                    .get_mut("response")
                    .and_then(|response| response.get_mut("output"))
                    .and_then(Value::as_array_mut)
                {
                    for item in output {
                        normalize_synthetic_custom(item);
                    }
                }
                event.as_object_mut().unwrap().remove("stream_id");
                let before_gate = split.is_some_and(|split| ordinal < split)
                    || (split.is_some() && kind == "response.output_item.added");
                writeln!(if before_gate { &mut prefix } else { &mut suffix }, "data: {event}\n").unwrap();
            }
            suffix.push_str("data: [DONE]\n\n");
            if split.is_some() {
                let (arrived, _arrival) = oneshot::channel();
                let (release, wait) = oneshot::channel();
                let acknowledgements = records
                    .iter()
                    .filter(|(ordinal, direction, event)| {
                        matches!(direction, Direction::Client)
                            && event["type"] == "response.steer"
                            && *ordinal > run_start
                            && *ordinal < terminal_ordinal
                    })
                    .count();
                gates.insert(id, (acknowledgements, release));
                fixtures.push(MockResponse::ObservedPhased {
                    first_chunk: prefix,
                    remaining_chunks: suffix,
                    arrived,
                    release: wait,
                });
            } else {
                fixtures.push(MockResponse::Static(suffix));
            }
        }
        (fixtures, gates)
    }

    fn steering_milestone(event: &Value) -> bool {
        milestone(event)
            || matches!(
                event["type"].as_str(),
                Some(
                    "response.incomplete"
                        | "response.failed"
                        | "response.steer.accepted"
                        | "response.steer.pending"
                        | "response.steer.failed"
                )
            )
    }

    /// A gated synthetic dependency can make steering win just ahead of a
    /// recorded native token-limit terminal. The engine deliberately completes
    /// that parent for client action. This equivalence requires the same closed
    /// client obligations; message-only or successor budget outcomes never qualify.
    fn completed_client_budget_parent(expected: &Value, actual: &Value) -> bool {
        if expected["type"] != "response.incomplete"
            || actual["type"] != "response.completed"
            || expected["response"]["incomplete_details"]["reason"] != "max_output_tokens"
        {
            return false;
        }
        let calls = |event: &Value| {
            event["response"]["output"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| matches!(item["type"].as_str(), Some("function_call" | "custom_tool_call")))
                .cloned()
                .collect::<Vec<_>>()
        };
        let expected_calls = calls(expected);
        let actual_calls = calls(actual);
        !expected_calls.is_empty()
            && expected_calls.len() == actual_calls.len()
            && expected_calls.iter().all(|before| {
                before["status"] == "completed"
                    && actual_calls.iter().any(|after| {
                        after["status"] == "completed"
                            && ["type", "call_id", "name", "namespace", "arguments", "input"]
                                .iter()
                                .all(|key| before[*key] == after[*key])
                    })
            })
    }

    fn same_steering_milestone(expected: &Value, actual: &Value, ids: &HashMap<String, String>) -> bool {
        if (expected["type"] != actual["type"] && !completed_client_budget_parent(expected, actual))
            || expected["stream_id"] != actual["stream_id"]
        {
            return false;
        }
        let target = expected["response"]["id"]
            .as_str()
            .or_else(|| expected["steer"]["previous_response_id"].as_str());
        target.is_none_or(|id| {
            ids.get(id).is_none_or(|actual_id| {
                actual["response"]["id"] == *actual_id || actual["steer"]["previous_response_id"] == *actual_id
            })
        })
    }

    fn compare_steering(
        expected: &Value,
        actual: &Value,
        ids: &mut HashMap<String, String>,
        steer_ids: &mut HashMap<String, String>,
    ) {
        assert!(expected["type"] == actual["type"] || completed_client_budget_parent(expected, actual));
        assert_eq!(expected["stream_id"], actual["stream_id"]);
        if expected["type"].as_str().unwrap().starts_with("response.steer.") {
            let expected_target = expected["steer"]["previous_response_id"]
                .as_str()
                .map(|parent| ids.get(parent).map_or(parent, String::as_str));
            assert_eq!(actual["steer"]["previous_response_id"].as_str(), expected_target);
        }
        match expected["type"].as_str().unwrap() {
            "response.created" => {
                let original = expected["response"]["id"].as_str().unwrap();
                ids.insert(
                    original.to_owned(),
                    actual["response"]["id"].as_str().unwrap().to_owned(),
                );
                assert_eq!(actual["sequence_number"], 0);
                if let Some(parent) = expected["response"]["previous_response_id"].as_str() {
                    assert_eq!(actual["response"]["previous_response_id"], ids[parent]);
                }
            }
            "response.steer.accepted" => {
                steer_ids.insert(
                    expected["steer"]["id"].as_str().unwrap().to_owned(),
                    actual["steer"]["id"].as_str().unwrap().to_owned(),
                );
            }
            "response.steer.pending" => {
                assert_eq!(
                    actual["steer"]["id"],
                    steer_ids[expected["steer"]["id"].as_str().unwrap()]
                );
                assert_eq!(actual["required_input"], expected["required_input"]);
            }
            "response.steer.failed" => {
                assert_eq!(actual["error"]["code"], expected["error"]["code"]);
                assert_eq!(actual["steer"]["input"], expected["steer"]["input"]);
                if let Some(id) = expected["steer"]["id"].as_str() {
                    assert_eq!(actual["steer"]["id"], steer_ids[id]);
                }
            }
            "response.output_item.done" => {
                compare_preserved_steering_item(&expected["item"], &actual["item"]);
            }
            "error" => {
                assert_eq!(actual["status"], expected["status"]);
                assert_eq!(actual["error"]["type"], "invalid_request_error");
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                let before = expected["response"]["output"].as_array().unwrap();
                let after = actual["response"]["output"].as_array().unwrap();
                assert_eq!(before.len(), after.len(), "terminal retained output length");
                for (before, after) in before.iter().zip(after) {
                    compare_preserved_steering_item(before, after);
                }
                if !completed_client_budget_parent(expected, actual) {
                    assert_eq!(actual["response"]["status"], expected["response"]["status"]);
                    assert_eq!(
                        actual["response"]["incomplete_details"]["reason"],
                        expected["response"]["incomplete_details"]["reason"]
                    );
                }
            }
            _ => unreachable!(),
        }
    }

    fn compare_preserved_steering_item(expected: &Value, actual: &Value) {
        assert_eq!(actual["type"], expected["type"]);
        // Recorded opaque/source-specific reasoning is replaced with a plaintext
        // model dependency sentinel; public nonreasoning semantics stay intact.
        if expected["type"] != "reasoning" {
            // Missing optional collections and empty collections carry the same
            // content here; preserve populated annotations and log probabilities.
            let mut expected: OutputItem = deserialize_from_value(expected.clone()).unwrap();
            let mut actual: OutputItem = deserialize_from_value(actual.clone()).unwrap();
            for item in [&mut expected, &mut actual] {
                if let OutputItem::Message(message) = item {
                    for content in &mut message.content {
                        if let OutputMessageContent::OutputText(part) = content {
                            part.annotations.get_or_insert_default();
                            part.logprobs.get_or_insert_default();
                        }
                    }
                }
            }
            let expected = serialize_to_value(&expected).unwrap();
            let actual = serialize_to_value(&actual).unwrap();
            for field in [
                "call_id",
                "name",
                "namespace",
                "arguments",
                "input",
                "action",
                "execution",
                "role",
                "content",
                "status",
            ] {
                assert_eq!(actual[field], expected[field], "retained output field {field}");
            }
        }
    }

    fn acknowledge_steering_gate(source: &str, gates: &mut SteeringGates) {
        let Some((remaining, _)) = gates.get_mut(source) else {
            return;
        };
        assert!(*remaining > 0);
        *remaining -= 1;
        if *remaining == 0 {
            let _ = gates.remove(source).unwrap().1.send(());
        }
    }

    fn release_steering_gate(event: &Value, ids: &HashMap<String, String>, gates: &mut SteeringGates) {
        if !matches!(
            event["type"].as_str(),
            Some("response.steer.accepted" | "response.steer.failed")
        ) {
            return;
        }
        let source = event["steer"]["previous_response_id"].as_str().and_then(|parent| {
            ids.iter()
                .find(|(_, actual)| actual.as_str() == parent)
                .map(|(source, _)| source)
        });
        if let Some(source) = source.filter(|source| gates.contains_key(*source)) {
            acknowledge_steering_gate(source, gates);
        } else if event["type"] == "response.steer.failed" {
            // A rejected unrelated target may belong to the currently blocked
            // dependency, but cannot release a future response generation.
            let published = gates
                .keys()
                .filter(|source| ids.contains_key(*source))
                .cloned()
                .collect::<Vec<_>>();
            for source in published {
                acknowledge_steering_gate(&source, gates);
            }
        }
    }

    fn user_messages(input: &Value) -> Vec<Value> {
        input
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["role"] == "user")
            .map(|item| json!({"role":"user", "content":item["content"]}))
            .collect()
    }

    fn normalized_steering_messages(input: &Value) -> Vec<Value> {
        let input: ResponsesInput = deserialize_from_value(input.clone()).unwrap();
        let mut request: RequestPayload = deserialize_from_value(json!({
            "model":"synthetic-model", "input":[]}))
        .unwrap();
        request.input = ResponsesInput::from(input.into_canonical_items());
        // to_upstream_request invokes the existing crate-private
        // ResponsesInput::normalized_model_input, including image defaults.
        let upstream = request.to_upstream_request(true).unwrap();
        user_messages(&serialize_to_value(&upstream.input).unwrap())
    }

    #[derive(Default)]
    struct ArrivalFacts {
        terminals: std::collections::HashSet<String>,
        lanes: HashMap<String, Value>,
        accepted: HashMap<String, String>,
        committed: std::collections::HashSet<String>,
        superseded: std::collections::HashSet<String>,
    }

    impl ArrivalFacts {
        fn observe(&mut self, event: &Value) {
            let kind = event["type"].as_str().unwrap();
            match kind {
                "response.created" => {
                    let lane = event["stream_id"].clone();
                    if let Some(parent) = event["response"]["previous_response_id"].as_str() {
                        assert!(
                            self.terminals.contains(parent),
                            "successor arrived before parent terminal: {event}"
                        );
                        if self.lanes.get(parent) == Some(&lane) {
                            self.superseded.insert(parent.to_owned());
                            self.committed.extend(
                                self.accepted
                                    .iter()
                                    .filter(|(_, target)| target.as_str() == parent)
                                    .map(|(id, _)| id.clone()),
                            );
                        }
                    }
                    self.lanes
                        .insert(event["response"]["id"].as_str().unwrap().to_owned(), lane);
                }
                "response.completed" | "response.incomplete" | "response.failed" => {
                    assert!(
                        self.terminals
                            .insert(event["response"]["id"].as_str().unwrap().to_owned()),
                        "duplicate terminal: {event}"
                    );
                }
                "response.steer.accepted" => {
                    let target = event["steer"]["previous_response_id"].as_str().unwrap();
                    assert_eq!(
                        self.lanes.get(target),
                        Some(&event["stream_id"]),
                        "accepted on a different lane"
                    );
                    assert!(
                        !self.superseded.contains(target),
                        "old parent accepted after a same-lane successor"
                    );
                    assert!(
                        !self
                            .accepted
                            .iter()
                            .any(|(id, parent)| parent == target && self.committed.contains(id)),
                        "old parent accepted input after successor creation"
                    );
                    assert!(
                        self.accepted
                            .insert(event["steer"]["id"].as_str().unwrap().to_owned(), target.to_owned())
                            .is_none()
                    );
                }
                "response.steer.pending" => {
                    let target = event["steer"]["previous_response_id"].as_str().unwrap();
                    assert!(
                        self.terminals.contains(target),
                        "pending arrived before parent terminal: {event}"
                    );
                    assert_eq!(
                        self.accepted
                            .get(event["steer"]["id"].as_str().unwrap())
                            .map(String::as_str),
                        Some(target)
                    );
                }
                "response.steer.failed" => {
                    if let Some(id) = event["steer"]["id"].as_str() {
                        assert!(!self.committed.contains(id), "committed input returned as unapplied");
                    }
                }
                _ => {}
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Replay connection-local actions sequentially with publication, ownership, and model-spy assertions"
    )]
    async fn replay_steering_sessions(sessions: &[&Session]) {
        let mut fixtures = Vec::new();
        let mut gates = HashMap::new();
        for session in sessions {
            let (responses, controls) = steering_inference(session);
            fixtures.extend(responses);
            gates.extend(controls);
        }
        let expected_requests = fixtures.len();
        let mock = MockResponsesServer::start_with_responses(fixtures).await;
        let search = MockYouSearchServer::start().await;
        let fixture = storage_backed_state_with_web_search(&mock.url, Some(&search.url)).await;
        let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ids = HashMap::new();
        let mut steer_ids = HashMap::new();
        let mut source_lanes = HashMap::<String, Value>::new();
        let mut pending_input = HashMap::<String, Vec<Value>>::new();
        let mut applied_input = HashMap::<String, Vec<Value>>::new();
        let mut expired_input = Vec::new();
        let mut spies = Vec::<(usize, Vec<Value>, bool)>::new();
        let mut arrivals = ArrivalFacts::default();
        let mut next_model = 0;
        let mut profiles = HashMap::<String, RequestPayload>::new();
        let mut loaded_tool_profiles = HashMap::<String, Vec<ResponsesTool>>::new();
        let mut settings_spies = Vec::new();
        let mut budget_adaptations = 0;
        for session in sessions {
            let mut submissions = VecDeque::new();
            let mut creates = VecDeque::<Value>::new();
            let mut ws = connect_responses_ws(&gateway_url).await;
            let mut received = VecDeque::<Value>::new();
            for (_ordinal, direction, mut event) in messages(session) {
                if matches!(direction, Direction::Client) {
                    if event["type"] == "response.steer" {
                        submissions.push_back(event["input"].clone());
                    }
                    if event["type"] == "response.create" {
                        creates.push_back(event.clone());
                    }
                    for key in ["response_id", "previous_response_id"] {
                        if let Some(actual) = event[key].as_str().and_then(|id| ids.get(id)) {
                            event[key] = json!(actual);
                        }
                    }
                    send_json(&mut ws, event).await;
                } else if steering_milestone(&event) {
                    let actual = loop {
                        if let Some(index) = received
                            .iter()
                            .position(|actual| same_steering_milestone(&event, actual, &ids))
                        {
                            break received.remove(index).unwrap();
                        }
                        let actual = recv_json(&mut ws).await;
                        arrivals.observe(&actual);
                        release_steering_gate(&actual, &ids, &mut gates);
                        if steering_milestone(&actual) {
                            received.push_back(actual);
                        }
                    };
                    if completed_client_budget_parent(&event, &actual) {
                        budget_adaptations += 1;
                        eprintln!(
                            "{}: synthetic dependency moved complete client-call steering ahead of native token-limit terminal",
                            case(session)
                        );
                    }
                    compare_steering(&event, &actual, &mut ids, &mut steer_ids);
                    match event["type"].as_str().unwrap() {
                        "response.created" => {
                            let source = event["response"]["id"].as_str().unwrap();
                            let lane = event["stream_id"].clone();
                            source_lanes.insert(source.to_owned(), lane.clone());
                            let mut retained = event["response"]["previous_response_id"]
                                .as_str()
                                .and_then(|parent| applied_input.get(parent))
                                .cloned()
                                .unwrap_or_default();
                            let parent = event["response"]["previous_response_id"].as_str();
                            let explicit = creates.iter().position(|request| {
                                request["stream_id"] == lane && request["previous_response_id"].as_str() == parent
                            });
                            let profile = if let Some(at) = explicit {
                                let request = creates.remove(at).unwrap();
                                let create: agentic_core::types::websocket::ResponseCreateRequest =
                                    deserialize_from_value(request).unwrap();
                                create.payload
                            } else {
                                profiles
                                    .get(parent.expect("automatic successor needs its parent"))
                                    .unwrap()
                                    .clone()
                            };
                            let mut loaded_tools = parent
                                .and_then(|parent| loaded_tool_profiles.get(parent))
                                .cloned()
                                .unwrap_or_default();
                            if let ResponsesInput::Items(items) = &profile.input {
                                for item in items {
                                    if let InOutItem::Input(InputItem::ToolSearchOutput(output)) = item {
                                        loaded_tools.extend(output.tools.clone());
                                    }
                                }
                            }
                            // Settings follow the same discovery preparation as inference.
                            // Input ordering and exactly-once steering are checked by the
                            // independent upstream history spies below.
                            let mut inference_settings = RequestPayload {
                                input: ResponsesInput::default(),
                                ..profile.clone()
                            };
                            let mut search =
                                ToolSearchState::build_with_loaded_tools(&inference_settings, &loaded_tools, false)
                                    .unwrap();
                            if search.is_active() {
                                search.prepare_inference_request(&mut inference_settings).unwrap();
                            }
                            let normalized =
                                serialize_to_value(&inference_settings.to_upstream_request(true).unwrap()).unwrap();
                            loaded_tool_profiles.insert(source.to_owned(), loaded_tools);
                            settings_spies.push((next_model, normalized));
                            profiles.insert(source.to_owned(), profile);
                            if let Some(parent) = event["response"]["previous_response_id"].as_str()
                                && let Some(input) = pending_input.get(parent)
                            {
                                let consumes = source_lanes.get(parent) == Some(&lane);
                                if consumes {
                                    retained.extend(input.clone());
                                    pending_input.remove(parent);
                                } else {
                                    spies.push((next_model, input.clone(), false));
                                }
                            }
                            spies.push((next_model, retained.clone(), true));
                            applied_input.insert(source.to_owned(), retained);
                            spies.push((next_model, expired_input.clone(), false));
                            next_model += 1;
                        }
                        "response.steer.accepted" => {
                            let input = submissions.pop_front().unwrap();
                            pending_input
                                .entry(event["steer"]["previous_response_id"].as_str().unwrap().to_owned())
                                .or_default()
                                .extend(normalized_steering_messages(&input));
                        }
                        "response.steer.failed" if event["steer"]["id"].is_string() => {
                            pending_input.remove(event["steer"]["previous_response_id"].as_str().unwrap());
                        }
                        "response.steer.failed" => {
                            submissions.pop_front().unwrap();
                        }
                        "error" => {
                            let at = creates
                                .iter()
                                .position(|request| request["stream_id"] == event["stream_id"])
                                .expect("recorded validation error needs its explicit contender");
                            creates.remove(at);
                        }
                        _ => {}
                    }
                }
            }
            assert!(
                received.is_empty(),
                "unexpected causal events while replaying {}: {received:?}",
                case(session)
            );
            ws.close(None).await.unwrap();
            recv_close_or_end(&mut ws).await;
            expired_input.extend(std::mem::take(&mut pending_input).into_values().flatten());
        }
        let requests = mock.request_bodies().await;
        eprintln!(
            "{}: {budget_adaptations} explicitly verified native-budget/client-completion fixture adaptations",
            case(sessions[0])
        );
        assert_eq!(requests.len(), expected_requests);
        assert_eq!(next_model, expected_requests);
        for (index, expected) in settings_spies {
            for field in [
                "model",
                "instructions",
                "tools",
                "tool_choice",
                "parallel_tool_calls",
                "reasoning",
                "text",
                "temperature",
                "top_p",
                "max_output_tokens",
                "metadata",
                "prompt_cache_key",
                "prompt_cache_retention",
            ] {
                assert_eq!(
                    requests[index][field], expected[field],
                    "successor inference setting {field}"
                );
            }
        }
        for (index, expected, present) in spies {
            let messages = user_messages(&requests[index]["input"]);
            let mut after = 0;
            for value in expected {
                let positions = messages
                    .iter()
                    .enumerate()
                    .filter_map(|(at, actual)| (actual == &value).then_some(at))
                    .collect::<Vec<_>>();
                if present {
                    assert_eq!(
                        positions.len(),
                        1,
                        "accepted input must reach successor inference exactly once: {value}"
                    );
                    assert!(positions[0] >= after, "accepted input must retain submission order");
                    after = positions[0] + 1;
                } else {
                    assert!(
                        positions.is_empty(),
                        "another lane or reconnect must not consume pending input: {value}"
                    );
                }
            }
        }
    }

    async fn replay_steering_captures(categories: &[&str]) {
        let mut replayed = 0;
        let mut corrected_intents = 0;
        let mut reference_graphs = 0;
        let mut expected_replays = 0;
        let mut graphs = 0;
        for category in categories {
            let gateway = steering_capture("gateway", category);
            let reference = steering_capture("openai-reference", category);
            expected_replays += assert_shared_steering_cases(&reference, &gateway, category)
                - usize::from(
                    gateway
                        .sessions
                        .iter()
                        .any(|session| case(session) == "hosted-tool-running"),
                );
            let mut seen = std::collections::HashSet::new();
            for gateway_session in &gateway.sessions {
                let name = case(gateway_session);
                if !seen.insert(name) {
                    continue;
                }
                if name == "hosted-tool-running" {
                    // This captured declaration requires a ready operator-gated
                    // interpreter. The companion executor test uses its existing
                    // private deterministic provider through the real gateway loop.
                    assert_eq!(qualification(gateway_session), Qualification::CodeInterpreterRuntime);
                    eprintln!(
                        "hosted-tool-running: actual code-interpreter core replay requires the companion provider harness"
                    );
                    continue;
                }
                let corrected = qualification(gateway_session) == Qualification::ModelOutputLimit;
                let replay_reference = corrected || *category == "vision-steering";
                let reference_label = if corrected {
                    "OpenAI corrected-intent"
                } else {
                    "OpenAI reference"
                };
                for (source, label) in std::iter::once((&gateway, "gateway"))
                    .chain(replay_reference.then_some((&reference, reference_label)))
                {
                    let sessions = source
                        .sessions
                        .iter()
                        .filter(|session| case(session) == name)
                        .collect::<Vec<_>>();
                    assert!(!sessions.is_empty());
                    eprintln!("replaying {category}/{name}: {label} recorded client graph");
                    tokio::time::timeout(std::time::Duration::from_secs(30), replay_steering_sessions(&sessions))
                        .await
                        .unwrap_or_else(|_| panic!("steering duplex replay timed out: {category}/{name} ({label})"));
                    graphs += 1;
                }
                corrected_intents += usize::from(corrected);
                reference_graphs += usize::from(replay_reference);
                replayed += 1;
            }
        }
        assert_eq!(
            replayed, expected_replays,
            "every recorded gateway case must run through WebSocket or its hosted interpreter companion"
        );
        assert_eq!(
            graphs,
            expected_replays + reference_graphs,
            "reference graphs cover vision forwarding and explicitly classified recorder prerequisites"
        );
        eprintln!(
            "{replayed} gateway case graphs and {reference_graphs} additional reference graphs replayed ({corrected_intents} corrected intents)"
        );
    }

    #[tokio::test]
    async fn steering_duplex_captures_drive_real_handlers_and_correct_missing_reference_workflows() {
        replay_steering_captures(&STEERING_CATEGORIES).await;
    }

    #[tokio::test]
    async fn vision_duplex_captures_drive_real_handlers() {
        replay_steering_captures(&["vision-steering"]).await;
    }

    fn assert_client_tool_steering(session: &Session) -> Vec<Value> {
        let contract = steering_contract(session);
        assert_eq!(contract.accepted, contract.submitted);
        assert_eq!(contract.pending, contract.submitted);
        assert_eq!(contract.creates, contract.submitted + 1);
        assert!(contract.failed.is_empty());
        assert_eq!(contract.invalid_stub_kinds, 0);
        assert_eq!(contract.input_errors, 0);
        assert_eq!(contract.upstream_errors, 0);
        assert_eq!(contract.last_status, Some(ResponseStatus::Completed));
        let records = messages(session);
        let mut obligations = Vec::new();
        for (ordinal, _, pending) in records
            .iter()
            .filter(|(_, _, event)| event["type"] == "response.steer.pending")
        {
            let parent = &pending["steer"]["previous_response_id"];
            let continuations = records
                .iter()
                .filter(|(_, direction, event)| {
                    matches!(direction, Direction::Client)
                        && event["type"] == "response.create"
                        && event["previous_response_id"] == *parent
                })
                .collect::<Vec<_>>();
            assert_eq!(
                continuations.len(),
                1,
                "each pending batch must have one output continuation"
            );
            let (continued_at, _, continuation) = continuations[0];
            assert!(
                ordinal < continued_at,
                "client outputs must remain withheld until pending"
            );
            let required = pending["required_input"].as_array().unwrap();
            let returned = continuation["input"].as_array().unwrap();
            assert_eq!(required.len(), returned.len());
            for (stub, output) in required.iter().zip(returned) {
                assert_eq!(stub["call_id"], output["call_id"]);
                assert_eq!(stub["type"], output["type"]);
                obligations.push(json!([stub["type"], stub["name"], stub["execution"]]));
            }
        }
        assert_eq!(
            obligations,
            vec![
                json!(["tool_search_output", null, "client"]),
                json!(["shell_call_output", null, null]),
                json!(["custom_tool_call_output", null, null]),
                json!(["function_call_output", "get_weather", null]),
                json!(["function_call_output", "get_timezone", null]),
            ]
        );
        let completed = records
            .iter()
            .filter(|(_, _, event)| event["type"] == "response.completed")
            .count();
        assert_eq!(completed, contract.creates, "every response must complete");
        let terminal = &records
            .iter()
            .rfind(|(_, _, event)| event["type"] == "response.completed")
            .unwrap()
            .2;
        let answer = terminal["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "message")
            .flat_map(|item| item["content"].as_array().unwrap())
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for index in 1..=contract.submitted {
            assert!(answer.contains(&format!("CLIENT_STEER_{index:03}")));
        }
        for result in ["55", "CUSTOM_CASSETTE_OUTPUT_OK", "21", "Europe/Paris"] {
            assert!(answer.contains(result), "missing client result {result}");
        }
        obligations
    }

    #[tokio::test]
    async fn client_tools_captures_compare_obligations_and_drive_real_handlers() {
        let reference = steering_capture("openai-reference", "client-tools-steer");
        let gateway = steering_capture("gateway", "client-tools-steer");
        assert_eq!(
            assert_shared_steering_cases(&reference, &gateway, "client-tools-steer"),
            1
        );
        assert_eq!(reference.sessions.len(), 1);
        assert_eq!(gateway.sessions.len(), 1);
        let reference_records = messages(&reference.sessions[0]);
        let gateway_records = messages(&gateway.sessions[0]);
        for field in [
            "type",
            "input",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "store",
            "max_output_tokens",
        ] {
            assert_eq!(
                reference_records[0].2[field], gateway_records[0].2[field],
                "shared initial {field}"
            );
        }
        assert_eq!(
            assert_client_tool_steering(&reference.sessions[0]),
            assert_client_tool_steering(&gateway.sessions[0])
        );
        // Models group the two discovered function calls differently. Compare
        // their obligations, then preserve each provider's exact client graph.
        for capture in [&reference, &gateway] {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                replay_steering_sessions(&[&capture.sessions[0]]),
            )
            .await
            .expect("client tools capture replay timed out");
        }
    }
}

#[tokio::test]
async fn test_websocket_rejects_invalid_prompt_cache_retention_before_inference() {
    let mock = MockResponsesServer::start(vec![]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    for value in [json!("30m"), json!(1), json!(true), json!([]), json!({})] {
        send_json(
            &mut ws,
            json!({"type":"response.create","model":"test-model",
            "input":"hi","prompt_cache_retention":value,"store":true,"stream":true}),
        )
        .await;
        let error = recv_json(&mut ws).await;
        assert_eq!(error["type"], "error");
        assert_eq!(error["status"], StatusCode::BAD_REQUEST.as_u16());
    }
    assert!(mock.request_bodies().await.is_empty());
}

#[tokio::test]
async fn cache_usage_survives_websocket_storage_without_carrying_into_next_turn() {
    for writes in [None, Some(Value::Null), Some(json!(0)), Some(json!(7))] {
        let mut usage = json!({"input_tokens":30,"output_tokens":4,"total_tokens":37,
            "input_tokens_details":{"cached_tokens":9},"output_tokens_details":{"reasoning_tokens":2}});
        if let Some(writes) = &writes {
            usage["input_tokens_details"]["cache_write_tokens"] = writes.clone();
        }
        let next_usage = json!({"input_tokens":12,"output_tokens":1,"total_tokens":13,
            "input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":0}});
        let first =
            sse_response("up_first", "msg_first", "first").replace("\"usage\":null", &format!("\"usage\":{usage}"));
        let second =
            sse_response("up_next", "msg_next", "next").replace("\"usage\":null", &format!("\"usage\":{next_usage}"));
        let mock = MockResponsesServer::start(vec![first, second]).await;
        let fixture = storage_backed_state(&mock.url).await;
        let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
        let mut ws = connect_responses_ws(&gateway_url).await;
        let mut previous = None;
        if writes.as_ref().is_some_and(Value::is_null) {
            usage["input_tokens_details"]
                .as_object_mut()
                .unwrap()
                .remove("cache_write_tokens");
        }
        for expected in [usage, next_usage] {
            send_json(
                &mut ws,
                json!({"type":"response.create","model":"test-model","input":"hi",
                "previous_response_id":previous,"store":true,"stream":true}),
            )
            .await;
            let events = recv_until_completed(&mut ws).await;
            let terminal = events.last().unwrap();
            assert_eq!(terminal["type"], "response.completed");
            assert_eq!(terminal["response"]["usage"], expected);
            let id = terminal["response"]["id"].as_str().unwrap();
            let stored: Value = reqwest::Client::new()
                .get(format!("{gateway_url}/v1/responses/{id}"))
                .bearer_auth("test-key")
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(stored["usage"], expected);
            previous = Some(id.to_owned());
        }
        assert_eq!(mock.request_bodies().await.len(), 2);
    }
}

/// Async client tools behave over WebSocket as over HTTP: the gateway continues the response
/// after the async call, and a follow-up on the same socket may omit the pending output.
#[tokio::test]
async fn websocket_async_tool_call_continues_and_accepts_follow_ups_without_output() {
    let mock = MockResponsesServer::start(vec![
        sse_function_call_response("resp_upstream_1", "get_weather"),
        sse_response("resp_upstream_2", "msg_upstream_2", "Essentials: shoes."),
        sse_response("resp_upstream_3", "msg_upstream_3", "A compact umbrella."),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;
    let tools = json!([{"type": "function", "name": "get_weather", "async": true,
        "parameters": {"type": "object", "properties": {"numbers": {"type": "array"}}}}]);

    send_json(
        &mut ws,
        json!({"type": "response.create", "model": "test-model", "input": "Check the weather; list essentials.",
            "tools": tools, "store": false, "stream": true}),
    )
    .await;
    let first = recv_until_completed(&mut ws).await;
    let lifecycle: Vec<&Value> = first
        .iter()
        .filter(|event| {
            matches!(
                event["type"].as_str(),
                Some("response.output_item.added" | "response.output_item.done")
            ) && event["item"]["type"] == "function_call"
        })
        .collect();
    assert_eq!(lifecycle.len(), 2, "the call's added and done events");
    assert!(lifecycle.iter().all(|event| event["item"]["async"] == true));
    let response = &first.last().expect("terminal event")["response"];
    let output = response["output"].as_array().expect("output");
    assert_eq!(output[0]["type"], "function_call");
    assert_eq!(output[0]["async"], true);
    assert_eq!(output[1]["type"], "message", "the answer follows in the same response");
    let response_id = response["id"].as_str().expect("response id");

    send_json(
        &mut ws,
        json!({"type": "response.create", "model": "test-model", "input": "Which umbrella?",
            "tools": tools, "previous_response_id": response_id, "store": false, "stream": true}),
    )
    .await;
    let second = recv_until_completed(&mut ws).await;
    assert_eq!(second.last().expect("terminal event")["type"], "response.completed");

    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        let notes = request["input"]
            .as_array()
            .expect("input items")
            .iter()
            .filter(|item| item["role"] == "developer")
            .count();
        assert_eq!(notes, 1, "the pending call carries its note upstream");
    }
}

/// Replays the recorded mixed-tool capture through the WebSocket transport: every lifecycle
/// frame reports the declarations the client submitted rather than the lowered function
/// schema the model server echoed, and retrieval returns them too.
#[tokio::test]
async fn recorded_mixed_tool_capture_stays_public_over_websocket() {
    let capture = common::load_mixed_gateway_capture(true);
    let public_tools = common::mixed_capture_public_tools(&capture);
    let mock = MockResponsesServer::start(vec![common::mixed_capture_upstream_fixture(&capture)]).await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    let mut create = common::mixed_capture_client_request(&capture);
    create["type"] = json!("response.create");
    send_json(&mut ws, create).await;

    let events = recv_until_completed(&mut ws).await;
    assert_eq!(events.last().expect("terminal frame")["type"], "response.completed");
    let snapshots: Vec<&str> = events
        .iter()
        .filter(|event| event.get("response").is_some())
        .filter_map(|event| event["type"].as_str())
        .collect();
    assert_eq!(
        snapshots,
        ["response.created", "response.in_progress", "response.completed"],
        "the replay covers the three lifecycle snapshots"
    );
    for event in &events {
        if let Some(response) = event.get("response") {
            common::assert_public_mixed_tools(response, &public_tools, event["type"].as_str().unwrap_or_default());
        }
    }
    common::assert_recorded_mixed_output_kinds(
        &events.last().expect("terminal frame")["response"],
        &capture,
        "terminal snapshot",
    );

    let id = events.last().expect("terminal frame")["response"]["id"]
        .as_str()
        .expect("response id");
    let stored: Value = reqwest::Client::new()
        .get(format!("{gateway_url}/v1/responses/{id}"))
        .bearer_auth("test-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    common::assert_public_mixed_tools(&stored, &public_tools, "stored response");

    let requests = mock.request_bodies().await;
    assert_eq!(requests.len(), 1, "the replayed turn runs one inference round");
    common::assert_lowered_mixed_upstream(&requests[0]);
}

#[tokio::test]
async fn websocket_hosted_tool_search_streams_its_items_and_continues() {
    let hosted_search =
        sse_tool_search_call_response().replace(r#"{\"query\":\"weather\"}"#, r#"{\"paths\":[\"get_weather\"]}"#);
    let mock = MockResponsesServer::start(vec![
        hosted_search,
        sse_weather_function_call_response(),
        sse_response("resp_answer", "msg_answer", "sunny"),
    ])
    .await;
    let fixture = storage_backed_state(&mock.url).await;
    let (gateway_url, _gateway) = spawn_gateway(fixture.state.clone()).await;
    let mut ws = connect_responses_ws(&gateway_url).await;

    send_json(
        &mut ws,
        json!({
            "type": "response.create", "model": "test-model", "input": "weather?", "store": true,
            "tools": [{"type": "tool_search"}, deferred_weather_function()]
        }),
    )
    .await;
    let first = recv_until_completed(&mut ws).await;
    let items: Vec<_> = first
        .iter()
        .filter(|event| event["item"].is_object())
        .map(|event| {
            (
                event["type"].as_str().unwrap_or_default().to_owned(),
                event["output_index"].as_u64(),
                event["item"]["type"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let expected: Vec<_> = [(0, "tool_search_call"), (1, "tool_search_output"), (2, "function_call")]
        .into_iter()
        .flat_map(|(index, item)| {
            ["response.output_item.added", "response.output_item.done"]
                .map(|kind| (kind.to_owned(), Some(index), item.to_owned()))
        })
        .collect();
    assert_eq!(items, expected);
    let completed = first.last().expect("completed event");
    assert_eq!(completed["type"], "response.completed");
    let output = &completed["response"]["output"];
    assert_eq!(output[0]["execution"], "server");
    assert!(output[0]["call_id"].is_null());
    assert_eq!(output[1]["tools"][0]["name"], "get_weather");
    assert_eq!(output[2]["namespace"], "get_weather");
    let response_id = completed["response"]["id"].as_str().expect("response id").to_owned();

    send_json(
        &mut ws,
        json!({
            "type": "response.create", "model": "test-model", "store": true,
            "previous_response_id": response_id,
            "input": [{"type": "function_call_output", "call_id": "call_weather", "output": "sunny"}]
        }),
    )
    .await;
    let second = recv_until_completed(&mut ws).await;
    assert_eq!(second.last().expect("completed event")["type"], "response.completed");

    let requests = mock.request_bodies().await;
    let tool_names = |request: &Value| -> Vec<String> {
        request["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|tool| tool["name"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    assert_eq!(tool_names(&requests[0]), ["tool_search"]);
    assert_eq!(tool_names(&requests[2]), ["tool_search", "get_weather"]);
}
