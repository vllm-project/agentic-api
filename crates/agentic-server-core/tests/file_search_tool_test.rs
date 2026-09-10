use std::fmt::Write;
use std::sync::Arc;

use agentic_core::executor::{ConversationHandler, ExecuteRequest, ExecutionContext, ResponseHandler};
use agentic_core::storage::{ConversationStore, ResponseStore, create_pool_with_schema};
use agentic_core::tool::GatewayExecutorRegistration;
use agentic_core::tool::file_search::{FileSearchHandler, FileSearchService};
use agentic_core::types::file_search::{AttachFileRequest, CreateVectorStoreRequest, FileSearchConfig};
use agentic_core::types::request_response::ResponsePayload;
use either::Either;
use futures::StreamExt;
use serde_json::{Value, json};

mod support;

async fn fixture() -> (FileSearchService, String, String) {
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let service = FileSearchService::new(pool, Arc::new(reqwest::Client::new()), FileSearchConfig::default()).unwrap();
    let file = service
        .upload_file(
            "policy.txt",
            "text/plain",
            "assistants",
            b"The policy protects coral reefs.".to_vec(),
        )
        .await
        .unwrap();
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    service
        .attach_file(
            &store.id,
            AttachFileRequest {
                file_id: file.id.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    (service, store.id, file.id)
}

async fn context(service: FileSearchService, url: &str) -> Arc<ExecutionContext> {
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    Arc::new(
        ExecutionContext::new(
            ConversationHandler::new(ConversationStore::new(Arc::clone(&pool))),
            ResponseHandler::new(ResponseStore::new(pool)),
            Arc::new(reqwest::Client::new()),
            url.to_owned(),
        )
        .with_gateway_executor(GatewayExecutorRegistration::FileSearch(Arc::new(
            FileSearchHandler::new(service),
        ))),
    )
}

fn search_call() -> Value {
    json!({"type":"function_call","id":"fc_search","call_id":"call_search","name":"file_search","arguments":"{\"queries\":[\"coral\"]}","status":"completed"})
}

fn upstream(items: Vec<Value>, streaming: bool) -> support::MockResponse {
    let response = json!({"id":"resp_upstream","object":"response","created_at":0,"model":"test-model","status":"completed","output":items});
    if !streaming {
        return support::MockResponse::Json(response.to_string());
    }
    let mut events = vec![
        json!({"type":"response.created","response":{"id":"resp_upstream","status":"in_progress"}}),
        json!({"type":"response.in_progress","response":{"id":"resp_upstream","status":"in_progress"}}),
    ];
    for (index, item) in items.into_iter().enumerate() {
        let mut started = item.clone();
        started["status"] = json!("in_progress");
        if item["type"] == "message" {
            started["content"] = json!([]);
        }
        events.push(json!({"type":"response.output_item.added","output_index":index,"item":started}));
        if item["type"] == "message" {
            let text = item["content"][0]["text"].as_str().unwrap();
            events.push(json!({"type":"response.content_part.added","output_index":index,"content_index":0,"item_id":item["id"],"part":{"type":"output_text","text":"","annotations":[]}}));
            events.push(json!({"type":"response.output_text.delta","output_index":index,"content_index":0,"item_id":item["id"],"delta":text}));
            events.push(json!({"type":"response.output_text.done","output_index":index,"content_index":0,"item_id":item["id"],"text":text}));
            events.push(json!({"type":"response.content_part.done","output_index":index,"content_index":0,"item_id":item["id"],"part":item["content"][0]}));
        }
        events.push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
    }
    events.push(json!({"type":"response.completed","response":response}));
    let mut body = String::new();
    for event in events {
        write!(body, "data: {event}\n\n").unwrap();
    }
    body.push_str("data: [DONE]\n\n");
    support::MockResponse::Sse(body)
}

fn answer(file_id: &str) -> Value {
    json!({"type":"message","id":"msg_answer","role":"assistant","status":"completed","content":[{"type":"output_text","text":format!("The policy protects coral reefs. 【{file_id}】"),"annotations":[]}]})
}

async fn collect(result: Either<ResponsePayload, agentic_core::executor::BoxStream>) -> (ResponsePayload, Vec<Value>) {
    match result {
        Either::Left(response) => (response, vec![]),
        Either::Right(stream) => {
            let chunks: Vec<_> = stream.collect().await;
            let events = support::streamed_sse_events(&chunks);
            let terminal = events
                .iter()
                .find(|event| event["type"] == "response.completed")
                .unwrap_or_else(|| panic!("missing completion: {events:?}"));
            (serde_json::from_value(terminal["response"].clone()).unwrap(), events)
        }
    }
}

fn assert_file_search_stream(events: &[Value], output: &Value) {
    let lifecycle: Vec<_> = events
        .iter()
        .filter(|event| {
            event["type"]
                .as_str()
                .unwrap()
                .starts_with("response.file_search_call.")
                || event["item"]["type"] == "file_search_call"
        })
        .collect();
    assert_eq!(
        lifecycle
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "response.output_item.added",
            "response.file_search_call.in_progress",
            "response.file_search_call.searching",
            "response.file_search_call.completed",
            "response.output_item.done"
        ]
    );
    assert!(lifecycle.iter().all(|event| event["output_index"] == 0));
    assert_eq!(lifecycle.last().unwrap()["item"], output[0]);
    let message_done = events
        .iter()
        .find(|event| event["type"] == "response.output_item.done" && event["item"]["type"] == "message")
        .unwrap();
    assert_eq!(message_done["item"], output[1]);
    let content_done = events
        .iter()
        .find(|event| event["type"] == "response.content_part.done")
        .unwrap();
    assert_eq!(content_done["part"], output[1]["content"][0]);
    assert_eq!(
        events
            .iter()
            .map(|event| event["sequence_number"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        (0..u64::try_from(events.len()).unwrap()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn file_search_blocking_and_streaming_preserve_context_include_and_citations() {
    for streaming in [false, true] {
        for include_results in [false, true] {
            let (service, store_id, file_id) = fixture().await;
            let llm = support::MockServer::start_deque(vec![
                upstream(vec![search_call()], streaming),
                upstream(vec![answer(&file_id)], streaming),
                upstream(vec![answer(&file_id)], false),
            ])
            .await;
            let ctx = context(service, llm.url()).await;
            let mut request = support::make_request("What does the coral policy protect?", true, streaming, None, None);
            request.tools =
                Some(serde_json::from_value(json!([{"type":"file_search","vector_store_ids":[store_id]}])).unwrap());
            request.include = include_results.then(|| vec!["file_search_call.results".to_owned()]);
            let (response, events) = collect(ExecuteRequest::new(request, Arc::clone(&ctx)).run().await.unwrap()).await;
            let output = serde_json::to_value(&response.output).unwrap();
            assert_eq!(output[0]["type"], "file_search_call");
            assert_eq!(output[0]["status"], "completed");
            assert_eq!(output[0].get("results").is_some(), include_results);
            if include_results {
                assert_eq!(output[0]["results"][0]["file_id"], file_id);
                assert_eq!(output[0]["results"][0]["filename"], "policy.txt");
                assert!(
                    output[0]["results"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("coral reefs")
                );
            }
            assert_eq!(output[1]["content"][0]["annotations"][0]["file_id"], file_id);
            assert_eq!(output[1]["content"][0]["annotations"][0]["filename"], "policy.txt");
            let requests = llm.request_bodies().await;
            assert_eq!(requests.len(), 2);
            let private_output = requests[1]["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["type"] == "function_call_output")
                .unwrap();
            assert!(private_output["output"].as_str().unwrap().contains("coral reefs"));
            assert!(
                private_output["output"]
                    .as_str()
                    .unwrap()
                    .contains("untrusted document text")
            );
            if streaming {
                assert_file_search_stream(&events, &output);
            }
            let continuation = support::make_request("Explain that policy", true, false, Some(response.id), None);
            let (continued, _) = collect(ExecuteRequest::new(continuation, ctx).run().await.unwrap()).await;
            let continued = serde_json::to_value(continued).unwrap();
            assert_eq!(
                continued["output"][0]["content"][0]["annotations"][0]["file_id"],
                file_id
            );
            let requests = llm.request_bodies().await;
            assert!(
                requests[2]["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["type"] == "function_call_output"
                        && item["output"].as_str().is_some_and(|text| text.contains("coral reefs")))
            );
        }
    }
}

#[tokio::test]
async fn file_search_mixed_client_call_waits_and_preserves_private_search_output() {
    let (service, store_id, file_id) = fixture().await;
    let client_call = json!({"type":"function_call","id":"fc_weather","call_id":"call_weather","name":"weather","arguments":"{}","status":"completed"});
    let llm = support::MockServer::start_deque(vec![
        upstream(vec![search_call(), client_call], false),
        upstream(vec![answer(&file_id)], false),
    ])
    .await;
    let ctx = context(service, llm.url()).await;
    let mut request = support::make_request("Search and get weather", true, false, None, None);
    request.tools = Some(serde_json::from_value(json!([{"type":"file_search","vector_store_ids":[store_id]},{"type":"function","name":"weather","parameters":{"type":"object","properties":{}}}])).unwrap());
    let (response, _) = collect(ExecuteRequest::new(request, Arc::clone(&ctx)).run().await.unwrap()).await;
    assert_eq!(llm.request_bodies().await.len(), 1);
    let wire = serde_json::to_value(&response).unwrap();
    assert_eq!(wire["output"][0]["type"], "file_search_call");
    assert_eq!(wire["output"][1]["name"], "weather");
    let continuation = support::make_request(
        json!([{"type":"function_call_output","call_id":"call_weather","output":"Sunny"}]),
        true,
        false,
        Some(response.id),
        None,
    );
    collect(ExecuteRequest::new(continuation, ctx).run().await.unwrap()).await;
    let requests = llm.request_bodies().await;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output"
                && item["call_id"] == "call_search"
                && item["output"].as_str().unwrap().contains("coral reefs"))
    );
}

#[tokio::test]
async fn file_search_empty_results_never_create_citations() {
    let (service, store_id, file_id) = fixture().await;
    let mut call = search_call();
    call["arguments"] = json!("{\"queries\":[\"unmatchedzzzz\"]}");
    let llm = support::MockServer::start_deque(vec![
        upstream(vec![call], false),
        upstream(vec![answer(&file_id)], false),
    ])
    .await;
    let ctx = context(service, llm.url()).await;
    let mut request = support::make_request("Find nonexistent data", false, false, None, None);
    request.tools =
        Some(serde_json::from_value(json!([{"type":"file_search","vector_store_ids":[store_id]}])).unwrap());
    request.include = Some(vec!["file_search_call.results".to_owned()]);
    let (response, _) = collect(ExecuteRequest::new(request, ctx).run().await.unwrap()).await;
    let wire = serde_json::to_value(response).unwrap();
    assert_eq!(wire["output"][0]["results"], json!([]));
    assert_eq!(wire["output"][1]["content"][0]["annotations"], json!([]));
}

#[tokio::test]
async fn file_search_failure_is_a_failed_call_with_a_complete_stream_lifecycle() {
    let (service, _, file_id) = fixture().await;
    let llm = support::MockServer::start_deque(vec![
        upstream(vec![search_call()], true),
        upstream(vec![answer(&file_id)], true),
    ])
    .await;
    let ctx = context(service, llm.url()).await;
    let mut request = support::make_request("Search missing store", false, true, None, None);
    request.tools =
        Some(serde_json::from_value(json!([{"type":"file_search","vector_store_ids":["vs_missing"]}])).unwrap());
    let (response, events) = collect(ExecuteRequest::new(request, ctx).run().await.unwrap()).await;
    let wire = serde_json::to_value(response).unwrap();
    assert_eq!(wire["output"][0]["status"], "failed");
    assert_eq!(wire["output"][1]["content"][0]["annotations"], json!([]));
    let done = events
        .iter()
        .find(|event| event["type"] == "response.output_item.done" && event["item"]["type"] == "file_search_call")
        .unwrap();
    assert_eq!(done["item"], wire["output"][0]);
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "response.file_search_call.completed")
    );
    let requests = llm.request_bodies().await;
    let output = requests[1]["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    assert!(output["output"].as_str().unwrap().contains("Vector store not found"));
}

#[tokio::test]
async fn file_search_selectors_normalize_and_release_after_search() {
    for (public, normalized) in [
        (
            json!({"type":"file_search"}),
            json!({"type":"function","name":"file_search"}),
        ),
        (
            json!({"type":"allowed_tools","mode":"required","tools":[{"type":"file_search"}]}),
            json!({"type":"allowed_tools","mode":"required","tools":[{"type":"function","name":"file_search"}]}),
        ),
    ] {
        for streaming in [false, true] {
            let (service, store_id, file_id) = fixture().await;
            let llm = support::MockServer::start_deque(vec![
                upstream(vec![search_call()], streaming),
                upstream(vec![answer(&file_id)], streaming),
            ])
            .await;
            let ctx = context(service, llm.url()).await;
            let mut request = support::make_request("Search coral policy", false, streaming, None, None);
            request.tools =
                Some(serde_json::from_value(json!([{"type":"file_search","vector_store_ids":[store_id]}])).unwrap());
            request.tool_choice = Some(serde_json::from_value(public.clone()).unwrap());
            let (response, _) = collect(ExecuteRequest::new(request, ctx).run().await.unwrap()).await;
            assert_eq!(response.status, "completed");
            let requests = llm.request_bodies().await;
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0]["tool_choice"], normalized);
            assert!(
                requests[1].get("tool_choice").is_none(),
                "follow-up inference must be free to answer"
            );
        }
    }
}

#[tokio::test]
async fn file_search_selector_rejects_missing_declaration_before_inference() {
    for public in [
        json!({"type":"file_search"}),
        json!({"type":"allowed_tools","mode":"auto","tools":[{"type":"file_search"}]}),
    ] {
        let (service, _, _) = fixture().await;
        let llm = support::MockServer::start_deque(vec![]).await;
        let ctx = context(service, llm.url()).await;
        let mut request = support::make_request("Search", false, false, None, None);
        request.tool_choice = Some(serde_json::from_value(public).unwrap());
        let error = ExecuteRequest::new(request, ctx)
            .run()
            .await
            .err()
            .expect("missing declaration should fail");
        assert_eq!(error.http_status().as_u16(), 400);
        assert!(error.error_message().contains("requires a declared file_search tool"));
        assert!(llm.request_bodies().await.is_empty());
    }
}
