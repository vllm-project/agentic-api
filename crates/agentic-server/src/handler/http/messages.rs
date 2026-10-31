use std::sync::Arc;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http::HeaderMap;
use tracing::debug;

use agentic_core::executor::telemetry::Api;
use agentic_core::executor::{
    ExecutorError, MessagesRequestContext, MessagesUpstream, ParsedMessagesRequest, declares_native_web_fetch,
    may_carry_gateway_server_tool_blocks, normalize_native_server_tools_for_upstream, replay_server_tool_blocks,
    run_messages_loop, run_messages_stream,
};
use agentic_core::proxy::{ProxyAuth, ProxyRequest, error_response_for_auth, upstream_request_headers};

use super::super::common::{
    convert_response, read_bytes_with_auth, sse_response_with_headers, upstream_error_response,
};
use crate::app::AppState;

async fn proxy_messages(
    state: &AppState,
    parts: axum::http::request::Parts,
    body: Bytes,
    path: &'static str,
) -> Response {
    convert_response(
        crate::telemetry::proxy::trace_proxy_request(
            Api::Messages,
            ProxyRequest {
                headers: parts.headers,
                body,
                query: parts.uri.query().map(str::to_owned),
            },
            path,
            state,
        )
        .await,
    )
}

/// Preserve upstream Messages errors verbatim; render local executor failures
/// as an Anthropic error envelope, consistent with the proxy path (E14).
fn messages_error_response(err: ExecutorError) -> Response {
    if let ExecutorError::LLMRequest { status, body, headers } = err {
        return upstream_error_response(status, body, headers);
    }
    convert_response(error_response_for_auth(
        err.http_status(),
        err.error_code(),
        &err.to_string(),
        ProxyAuth::Anthropic,
    ))
}

/// Drive the Messages-native gateway tool loop (non-streaming or streaming) for
/// a request that declares a gateway-owned tool.
async fn execute_messages(
    state: &AppState,
    headers: &HeaderMap,
    query: Option<&str>,
    parsed: ParsedMessagesRequest<'_>,
) -> Response {
    let mut ctx = match MessagesRequestContext::new(parsed) {
        Ok(ctx) => ctx,
        Err(e) => return messages_error_response(e),
    };

    // Build the request-scoped registry from the declared tools (M6). Gateway
    // ownership (incl. configured aliases like Claude Code's `WebSearch`) is
    // resolved against the operator-configured map.
    let gateway_map = &state.exec_ctx.messages_gateway_tools;
    let mut executors = state.exec_ctx.gateway_executors.clone();
    let registry = match ctx.prepare_registry(gateway_map, &mut executors).await {
        Ok(r) => r,
        Err(e) => return messages_error_response(e),
    };

    let upstream = MessagesUpstream::new(
        &state.exec_ctx.llm_base_url,
        query,
        upstream_request_headers(headers, &state.proxy_state.config, ProxyAuth::Anthropic),
    );
    if ctx.stream() {
        match run_messages_stream(ctx, Arc::new(registry), Arc::clone(&state.exec_ctx), upstream).await {
            Ok(response) => sse_response_with_headers(response.body, response.headers),
            Err(e) => messages_error_response(e),
        }
    } else {
        match run_messages_loop(ctx, &registry, &state.exec_ctx, &upstream).await {
            Ok(message) => {
                let mut response = axum::Json(message.body).into_response();
                response.headers_mut().extend(message.headers);
                response.headers_mut().insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/json"),
                );
                response
            }
            Err(e) => messages_error_response(e),
        }
    }
}

#[cfg_attr(feature = "openapi", utoipa::path(
    post,
    path = "/v1/messages",
    request_body = agentic_core::types::messages::MessagesRequest,
    responses(
        (status = 200, description = "JSON when stream=false, SSE when stream=true",
            content(
                (crate::openapi::MessagesResponse = "application/json"),
                (() = "text/event-stream"),
            )),
        (status = 400, description = "Invalid request", body = crate::openapi::AnthropicErrorResponse),
        (status = 502, description = "Upstream error", body = crate::openapi::AnthropicErrorResponse),
    ),
    security(("bearer_auth" = [])),
    tag = "messages",
))]
pub async fn messages(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes: Bytes = match read_bytes_with_auth(body, ProxyAuth::Anthropic, state.max_request_body_size).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };

    // Route to the loop only when a gateway-owned tool is declared; everything
    // else keeps the transparent proxy path.
    match ParsedMessagesRequest::parse_for_gateway(&bytes, &state.exec_ctx.messages_gateway_tools) {
        Ok(Some(parsed)) => {
            debug!(
                route = "messages_loop",
                stream = parsed.stream(),
                tools = parsed.tools().map_or(0, Vec::len),
                "routing HTTP messages request"
            );
            return execute_messages(&state, &parts.headers, parts.uri.query(), parsed).await;
        }
        Err(error) => return messages_error_response(error),
        Ok(None) => debug!(route = "proxy", "routing HTTP messages request"),
    }

    // Server tool blocks the gateway returned earlier go upstream as the rounds
    // the upstream saw, even when this request declares no gateway tool.
    let bytes = if may_carry_gateway_server_tool_blocks(&bytes)
        && let Ok(mut request) = serde_json::from_slice::<serde_json::Value>(&bytes)
    {
        match replayed_body(&state, &mut request) {
            ReplayedBody::Changed(body) => body,
            ReplayedBody::Unchanged => bytes,
            ReplayedBody::Refused(response) => return response,
        }
    } else {
        bytes
    };
    proxy_messages(&state, parts, bytes, "/v1/messages").await
}

/// A request body after its gateway-made server tool blocks were replayed.
enum ReplayedBody {
    Changed(Bytes),
    Unchanged,
    Refused(Response),
}

/// Turn the gateway-made server tool blocks of `request` back into the rounds
/// the upstream saw.
fn replayed_body(state: &AppState, request: &mut serde_json::Value) -> ReplayedBody {
    let replayed = match state.exec_ctx.messages_result_key() {
        Ok(key) => replay_server_tool_blocks(request, key),
        Err(error) => Err(error),
    };
    match replayed {
        Ok(true) => match serde_json::to_vec(request) {
            Ok(body) => ReplayedBody::Changed(Bytes::from(body)),
            Err(error) => ReplayedBody::Refused(messages_error_response(ExecutorError::from(error))),
        },
        Ok(false) => ReplayedBody::Unchanged,
        Err(error) => ReplayedBody::Refused(messages_error_response(error)),
    }
}

#[cfg_attr(feature = "openapi", utoipa::path(
    post,
    path = "/v1/messages/count_tokens",
    request_body = crate::openapi::CountTokensRequest,
    responses(
        (status = 200, description = "Token count result", body = crate::openapi::CountTokensResponse),
        (status = 400, description = "Invalid request", body = crate::openapi::AnthropicErrorResponse),
        (status = 502, description = "Upstream error", body = crate::openapi::AnthropicErrorResponse),
    ),
    security(("bearer_auth" = [])),
    tag = "messages",
))]
pub async fn count_tokens(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let mut bytes: Bytes = match read_bytes_with_auth(body, ProxyAuth::Anthropic, state.max_request_body_size).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    if let Ok(mut request_json) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        // The executors own the availability policy: a disabled executor
        // refuses the declaration here too, so counting tokens and sending the
        // request answer alike.
        if declares_native_web_fetch(&request_json)
            && let Err(error) = state.exec_ctx.gateway_executors.require_web_fetch()
        {
            return messages_error_response(ExecutorError::from(error));
        }
        // Count what the upstream would receive: rewritten native declarations
        // and the rounds behind earlier gateway server tool blocks.
        let normalized = match normalize_native_server_tools_for_upstream(&mut request_json) {
            Ok(normalized) => normalized,
            Err(error) => return messages_error_response(error),
        };
        match replayed_body(&state, &mut request_json) {
            ReplayedBody::Changed(body) => bytes = body,
            ReplayedBody::Unchanged if normalized => match serde_json::to_vec(&request_json) {
                Ok(body) => bytes = Bytes::from(body),
                Err(error) => return messages_error_response(ExecutorError::from(error)),
            },
            ReplayedBody::Unchanged => {}
            ReplayedBody::Refused(response) => return response,
        }
    }
    let mut executors = state.exec_ctx.gateway_executors.clone();
    match agentic_core::executor::prepare_messages_count_tokens(
        &bytes,
        &state.exec_ctx.messages_gateway_tools,
        &mut executors,
    )
    .await
    {
        Ok(Some(body)) => bytes = Bytes::from(body),
        Ok(None) => {}
        Err(error) => return messages_error_response(error),
    }
    proxy_messages(&state, parts, bytes, "/v1/messages/count_tokens").await
}
