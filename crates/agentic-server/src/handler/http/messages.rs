use std::sync::Arc;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::HeaderMap;
use tracing::debug;

use agentic_core::executor::{ExecutorError, run_messages_loop, run_messages_stream};
use agentic_core::proxy::{ProxyAuth, ProxyRequest, error_response_for_auth, proxy_request_with_path};
use agentic_core::tool::ToolRegistry;
use agentic_core::types::messages::{MessagesRequest, has_gateway_tool, registry_tools};

use super::super::common::{convert_response, read_bytes_with_auth, sse_response};
use crate::app::AppState;

async fn proxy_messages(
    state: &AppState,
    parts: axum::http::request::Parts,
    body: Bytes,
    path: &'static str,
) -> Response {
    convert_response(
        proxy_request_with_path(
            ProxyRequest {
                headers: parts.headers,
                body,
                query: parts.uri.query().map(str::to_owned),
            },
            path,
            ProxyAuth::Anthropic,
            &state.proxy_state,
        )
        .await,
    )
}

/// Extract the client's Anthropic credential — `x-api-key` (Anthropic-native) or
/// an `Authorization: Bearer` — falling back to the server's configured key.
/// Consistent with the proxy path forwarding the client's `x-api-key` (E15).
fn extract_client_key(headers: &HeaderMap, config_key: Option<&str>) -> Option<String> {
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| config_key.filter(|s| !s.is_empty()).map(str::to_owned))
}

/// Render an executor error as the Anthropic error envelope
/// (`{"type":"error","error":{"type","message"}}`), consistent with the proxy
/// path (E14).
fn messages_error_response(err: &ExecutorError) -> Response {
    convert_response(error_response_for_auth(
        err.http_status(),
        err.error_code(),
        &err.to_string(),
        ProxyAuth::Anthropic,
    ))
}

/// Drive the Messages-native gateway tool loop (non-streaming or streaming) for
/// a request that declares a gateway-owned tool.
async fn execute_messages(state: &AppState, headers: &HeaderMap, req: &MessagesRequest, body: &Bytes) -> Response {
    let auth = extract_client_key(headers, state.openai_api_key.as_deref());

    // Build the request-scoped registry from the declared tools (M6). Gateway
    // ownership (incl. configured aliases like Claude Code's `WebSearch`) is
    // resolved against the operator-configured map.
    let gateway_map = &state.exec_ctx.messages_gateway_tools;
    let registry = match ToolRegistry::build_with_handlers(
        &registry_tools(req.tools.as_ref(), gateway_map),
        &state.exec_ctx.gateway_executors,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return messages_error_response(&ExecutorError::from(e)),
    };

    // Parse the raw body to a JSON Value the loop forwards upstream untouched —
    // preserving every Anthropic field (tool_choice, stop_sequences, …).
    let request_json: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return messages_error_response(&ExecutorError::from(e)),
    };

    if req.stream {
        let stream = run_messages_stream(request_json, Arc::new(registry), Arc::clone(&state.exec_ctx), auth);
        sse_response(stream)
    } else {
        match run_messages_loop(request_json, &registry, &state.exec_ctx, auth.as_deref()).await {
            Ok(message) => axum::Json(message).into_response(),
            Err(e) => messages_error_response(&e),
        }
    }
}

pub async fn messages(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes: Bytes = match read_bytes_with_auth(body, ProxyAuth::Anthropic).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };

    // Route to the loop only when a gateway-owned tool is declared; everything
    // else keeps the transparent proxy path.
    if let Ok(req) = serde_json::from_slice::<MessagesRequest>(&bytes) {
        let route_to_loop = has_gateway_tool(req.tools.as_ref(), &state.exec_ctx.messages_gateway_tools);
        debug!(
            route = if route_to_loop { "messages_loop" } else { "proxy" },
            stream = req.stream,
            tools = req.tools.as_ref().map_or(0, Vec::len),
            "routing HTTP messages request"
        );
        if route_to_loop {
            return execute_messages(&state, &parts.headers, &req, &bytes).await;
        }
    }

    proxy_messages(&state, parts, bytes, "/v1/messages").await
}

pub async fn count_tokens(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes: Bytes = match read_bytes_with_auth(body, ProxyAuth::Anthropic).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    proxy_messages(&state, parts, bytes, "/v1/messages/count_tokens").await
}
