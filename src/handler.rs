use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use tracing::{debug, error, warn};

use crate::store::{ResponseStore, VectorSearch};
use crate::types::{OutputItem, ResponseBody, ResponseRequest, SearchResult};

pub struct AppState {
    pub vllm_base_url: String,
    pub openai_api_key: Option<String>,
    pub max_iterations: u32,
    pub client: reqwest::Client,
    pub response_store: Arc<dyn ResponseStore>,
    pub vector_search: Arc<dyn VectorSearch>,
}

#[allow(clippy::unused_async)]
pub async fn health() -> StatusCode {
    StatusCode::OK
}

pub async fn handle_responses(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let mut request: ResponseRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                serde_json::json!({"error": {"message": format!("invalid request body: {e}")}}).to_string(),
            )
                .into_response();
        }
    };

    let mut hydrated = false;
    if let Some(prev_id) = request.previous_response_id.take() {
        if let Err(e) = hydrate_state(&state, &prev_id, &mut request).await {
            warn!(%prev_id, error = %e, "failed to hydrate conversation state");
            return error_response(StatusCode::BAD_GATEWAY, &format!("state hydration failed: {e}"));
        }
        hydrated = true;
    }

    let has_file_search = request.tools.iter().any(|t| t.r#type == "file_search");

    if has_file_search {
        match agentic_loop(&state, &headers, request).await {
            Ok(resp) => resp,
            Err(e) => {
                error!(error = %e, "agentic loop failed");
                error_response(StatusCode::BAD_GATEWAY, &format!("agentic loop error: {e}"))
            }
        }
    } else {
        let forward_body = if hydrated {
            match serde_json::to_vec(&request) {
                Ok(b) => Bytes::from(b),
                Err(e) => {
                    return error_response(StatusCode::INTERNAL_SERVER_ERROR, &format!("serialization failed: {e}"));
                }
            }
        } else {
            body
        };
        proxy_to_vllm(&state, &headers, &forward_body, request.stream).await
    }
}

async fn hydrate_state(
    state: &AppState,
    previous_response_id: &str,
    request: &mut ResponseRequest,
) -> Result<(), crate::error::Error> {
    debug!(%previous_response_id, "hydrating conversation state");

    let input_items = state.response_store.list_input_items(previous_response_id).await?;

    let response = state.response_store.get_response(previous_response_id).await?;

    let output_items = response
        .get("output")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut history = Vec::with_capacity(input_items.len() + output_items.len());
    history.extend(input_items);
    history.extend(output_items);

    let current_input = std::mem::take(&mut request.input);
    history.extend(current_input);
    request.input = history;

    debug!(
        items = request.input.len(),
        "hydrated conversation with previous context"
    );
    Ok(())
}

async fn agentic_loop(
    state: &AppState,
    client_headers: &HeaderMap,
    mut request: ResponseRequest,
) -> Result<Response, crate::error::Error> {
    let vector_store_ids: Vec<String> = request
        .tools
        .iter()
        .filter(|t| t.r#type == "file_search")
        .filter_map(|t| t.vector_store_ids.clone())
        .flatten()
        .collect();

    for iteration in 0..state.max_iterations {
        debug!(iteration, "agentic loop iteration");

        let mut loop_request = build_vllm_request(state, client_headers);

        let mut body = serde_json::to_value(&request)
            .map_err(|e| crate::error::Error::Config(format!("failed to serialize request: {e}")))?;
        body.as_object_mut()
            .map(|m| m.insert("stream".to_owned(), serde_json::Value::Bool(false)));

        loop_request = loop_request.json(&body);

        let resp = loop_request.send().await.map_err(crate::error::Error::Proxy)?;

        let status = resp.status();
        if !status.is_success() {
            let resp_body = resp.text().await.unwrap_or_default();
            return Ok((
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                resp_body,
            )
                .into_response());
        }

        let response_body: ResponseBody = resp.json().await.map_err(crate::error::Error::Proxy)?;

        let tool_calls: Vec<_> = response_body
            .output
            .iter()
            .filter_map(|item| match item {
                OutputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } if name == "file_search" => Some((call_id.clone(), arguments.clone())),
                _ => None,
            })
            .collect();

        if tool_calls.is_empty() {
            debug!(iteration, "no tool calls, returning final response");
            let final_json = serde_json::to_string(&response_body)
                .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_owned());
            return Ok((StatusCode::OK, [("content-type", "application/json")], final_json).into_response());
        }

        for output_item in &response_body.output {
            request
                .input
                .push(serde_json::to_value(output_item).unwrap_or_default());
        }

        for (call_id, arguments) in &tool_calls {
            let query = extract_query(arguments);
            debug!(%call_id, %query, "executing file_search tool call");

            let results = execute_file_search(state, &vector_store_ids, &query).await;

            let tool_output = serde_json::json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": serde_json::to_string(&results).unwrap_or_default()
            });
            request.input.push(tool_output);
        }

        debug!(
            iteration,
            tool_calls = tool_calls.len(),
            "fed tool results back, continuing loop"
        );
    }

    warn!(
        max_iterations = state.max_iterations,
        "agentic loop reached max iterations"
    );
    Err(crate::error::Error::MaxIterations {
        max_iterations: state.max_iterations,
    })
}

async fn execute_file_search(state: &AppState, vector_store_ids: &[String], query: &str) -> Vec<SearchResult> {
    let mut all_results = Vec::new();
    for store_id in vector_store_ids {
        match state.vector_search.search(store_id, query).await {
            Ok(results) => all_results.extend(results),
            Err(e) => {
                warn!(%store_id, error = %e, "file_search failed for vector store");
            }
        }
    }
    all_results
}

fn extract_query(arguments: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|v| v.get("query").and_then(serde_json::Value::as_str).map(String::from))
        .unwrap_or_default()
}

async fn proxy_to_vllm(state: &AppState, client_headers: &HeaderMap, body: &Bytes, stream: bool) -> Response {
    let req = build_vllm_request(state, client_headers).body(body.clone());

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, "failed to connect to vLLM");
            return error_response(StatusCode::BAD_GATEWAY, &format!("vLLM connection failed: {e}"));
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    if stream {
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/event-stream")
            .to_owned();

        let stream = resp.bytes_stream();
        let body = Body::from_stream(stream);

        (status, [("content-type", content_type)], body).into_response()
    } else {
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_owned();

        match resp.bytes().await {
            Ok(bytes) => (status, [("content-type", content_type)], bytes).into_response(),
            Err(e) => error_response(StatusCode::BAD_GATEWAY, &format!("failed to read vLLM response: {e}")),
        }
    }
}

fn build_vllm_request(state: &AppState, client_headers: &HeaderMap) -> reqwest::RequestBuilder {
    let url = format!("{}/v1/responses", state.vllm_base_url);
    let mut req = state.client.post(&url).header("content-type", "application/json");

    if let Some(auth) = client_headers.get(http::header::AUTHORIZATION) {
        if let Ok(v) = auth.to_str() {
            req = req.header("authorization", v);
        }
    } else if let Some(key) = &state.openai_api_key {
        req = req.header("authorization", format!("Bearer {key}"));
    }

    req
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (
        status,
        [("content-type", "application/json")],
        serde_json::json!({"error": {"message": message}}).to_string(),
    )
        .into_response()
}
