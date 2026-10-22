use axum::extract::{Path, Request, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use either::Either;
use tracing::debug;

use std::sync::Arc;

use agentic_core::executor::{ExecuteRequest, compact_response as execute_compaction};
use agentic_core::proxy::{ProxyAuth, upstream_request_headers};
use agentic_core::types::request_response::{CompactRequest, RequestPayload};

use super::super::common::{executor_error_response, extract_bearer, read_json, sse_response};
use crate::app::AppState;

async fn execute_responses(state: &AppState, parts: Parts, payload: RequestPayload) -> Response {
    let headers = upstream_request_headers(&parts.headers, &state.proxy_state.config, ProxyAuth::OpenAiBearer);
    match ExecuteRequest::new(payload, Arc::clone(&state.exec_ctx))
        .with_upstream_headers(headers)
        .run_http()
        .await
    {
        Ok(Either::Left(response_payload)) => axum::Json(response_payload).into_response(),
        Ok(Either::Right(stream)) => sse_response(stream),
        Err(e) => executor_error_response(e),
    }
}

#[cfg_attr(feature = "openapi", utoipa::path(
    post,
    path = "/v1/responses",
    request_body = agentic_core::types::request_response::RequestPayload,
    responses(
        (status = 200, description = "JSON when stream=false, SSE when stream=true",
            content(
                (agentic_core::types::request_response::ResponsePayload = "application/json"),
                (() = "text/event-stream"),
            )),
        (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse),
        (status = 502, description = "Upstream error", body = crate::openapi::ApiErrorResponse),
    ),
    security(("bearer_auth" = [])),
    tag = "responses",
))]
pub async fn responses(State(state): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let payload: RequestPayload = match read_json(body, state.max_request_body_size).await {
        Ok(payload) => payload,
        Err(response) => return response,
    };

    debug!(
        route = "executor",
        store = payload.store,
        stream = payload.stream,
        has_previous_response_id = payload.previous_response_id.is_some(),
        has_conversation_id = payload.conversation.is_some(),
        has_compaction = payload.input.contains_compaction(),
        has_compaction_trigger = payload.input.has_compaction_trigger(),
        context_management = payload.context_management.as_ref().map_or(0, Vec::len),
        tools = payload.tools.as_ref().map_or(0, Vec::len),
        "executing HTTP responses request"
    );

    execute_responses(&state, parts, payload).await
}

#[cfg_attr(feature = "openapi", utoipa::path(
    post,
    path = "/v1/responses/compact",
    request_body = agentic_core::types::request_response::CompactRequest,
    responses(
        (status = 200, description = "Compacted response", body = agentic_core::types::request_response::CompactedResponse),
        (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse),
        (status = 502, description = "Upstream error", body = crate::openapi::ApiErrorResponse),
    ),
    security(("bearer_auth" = [])),
    tag = "responses",
))]
pub async fn compact_response(State(state): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let request: CompactRequest = match read_json(body, state.max_request_body_size).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let auth = extract_bearer(&parts.headers, state.openai_api_key.as_deref());
    match execute_compaction(request, state.exec_ctx.as_ref(), auth.as_deref()).await {
        Ok(response) => axum::Json(response).into_response(),
        Err(error) => executor_error_response(error),
    }
}

/// Return a locally persisted Responses payload without invoking inference.
#[cfg_attr(feature = "openapi", utoipa::path(
    get,
    path = "/v1/responses/{response_id}",
    params(("response_id" = String, Path, description = "Stored response ID")),
    responses(
        (status = 200, description = "Stored response", body = agentic_core::types::request_response::ResponsePayload),
        (status = 401, description = "Missing or invalid bearer token", body = crate::openapi::ApiErrorResponse),
        (status = 404, description = "Response not found", body = crate::openapi::ApiErrorResponse),
        (status = 409, description = "Legacy response has no retrievable payload", body = crate::openapi::ApiErrorResponse),
    ),
    security(("bearer_auth" = [])),
    tag = "responses",
))]
pub async fn retrieve_response(State(state): State<AppState>, Path(response_id): Path<String>) -> Response {
    match state.exec_ctx.resp_handler.retrieve(&response_id).await {
        Ok(payload) => axum::Json(payload).into_response(),
        Err(error) => executor_error_response(error),
    }
}
