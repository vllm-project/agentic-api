use axum::extract::{Path, RawQuery, Request, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use either::Either;
use tracing::debug;

use std::sync::Arc;

use agentic_core::executor::{BackgroundError, ExecuteRequest, compact_response as execute_compaction};
use agentic_core::proxy::{ProxyAuth, upstream_request_headers};
use agentic_core::types::io::DeletedResponse;
use agentic_core::types::request_response::{CompactRequest, RequestPayload};

use super::super::common::{executor_error_response, extract_bearer, read_json, sse_response};
use super::items::ListItemsQuery;
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

/// Streaming retrieval and resume are not served yet: reject them rather than answer with JSON.
fn reject_streaming_retrieval(query: Option<&str>) -> Result<(), BackgroundError> {
    for (key, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "stream" if !matches!(value.as_ref(), "false" | "0") => {
                return Err(BackgroundError::StreamingRetrieval { param: "stream" });
            }
            "starting_after" => {
                return Err(BackgroundError::StreamingRetrieval {
                    param: "starting_after",
                });
            }
            _ => {}
        }
    }
    Ok(())
}

/// Return a locally persisted Responses payload without invoking inference.
///
/// A background response reports its current status, including `queued` and `in_progress`.
#[cfg_attr(feature = "openapi", utoipa::path(
    get,
    path = "/v1/responses/{response_id}",
    params(("response_id" = String, Path, description = "Stored response ID")),
    responses(
        (status = 200, description = "Stored response", body = agentic_core::types::request_response::ResponsePayload),
        (status = 400, description = "Streaming retrieval (`stream=true` or `starting_after`) is not supported", body = crate::openapi::ApiErrorResponse),
        (status = 401, description = "Missing or invalid bearer token", body = crate::openapi::ApiErrorResponse),
        (status = 404, description = "Response not found", body = crate::openapi::ApiErrorResponse),
        (status = 409, description = "Legacy response has no retrievable payload", body = crate::openapi::ApiErrorResponse),
    ),
    security(("bearer_auth" = [])),
    tag = "responses",
))]
pub async fn retrieve_response(
    State(state): State<AppState>,
    Path(response_id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    if let Err(error) = reject_streaming_retrieval(query.as_deref()) {
        return executor_error_response(error.into());
    }
    match state.exec_ctx.resp_handler.retrieve(&response_id).await {
        Ok(payload) => axum::Json(payload).into_response(),
        Err(error) => executor_error_response(error),
    }
}

/// Cancel a background response, on whichever gateway instance runs it.
///
/// Cancelling a cancelled response returns it again.
#[cfg_attr(feature = "openapi", utoipa::path(
    post,
    path = "/v1/responses/{response_id}/cancel",
    params(("response_id" = String, Path, description = "Background response ID")),
    responses(
        (status = 200, description = "Cancelled response", body = agentic_core::types::request_response::ResponsePayload),
        (status = 400, description = "Not a background response, or already completed, incomplete, or failed", body = crate::openapi::ApiErrorResponse),
        (status = 401, description = "Missing or invalid bearer token", body = crate::openapi::ApiErrorResponse),
        (status = 404, description = "Response not found", body = crate::openapi::ApiErrorResponse),
    ),
    security(("bearer_auth" = [])),
    tag = "responses",
))]
pub async fn cancel_response(State(state): State<AppState>, Path(response_id): Path<String>) -> Response {
    match state.exec_ctx.background_jobs().cancel(&response_id).await {
        Ok(payload) => axum::Json(payload).into_response(),
        Err(error) => executor_error_response(error),
    }
}

/// Delete a locally persisted response without invoking inference.
///
/// Descendants created with `previous_response_id` stay retrievable; a new request
/// naming the deleted ID as `previous_response_id` is rejected as not found.
#[cfg_attr(feature = "openapi", utoipa::path(
    delete,
    path = "/v1/responses/{response_id}",
    params(("response_id" = String, Path, description = "Stored response ID")),
    responses(
        (status = 200, description = "Response deleted", body = agentic_core::types::io::DeletedResponse),
        (status = 401, description = "Missing or invalid bearer token", body = crate::openapi::ApiErrorResponse),
        (status = 404, description = "Response not found", body = crate::openapi::ApiErrorResponse),
    ),
    security(("bearer_auth" = [])),
    tag = "responses",
))]
pub async fn delete_response(State(state): State<AppState>, Path(response_id): Path<String>) -> Response {
    match state.exec_ctx.resp_handler.delete(&response_id).await {
        Ok(()) => {
            // A job running here stops now; one on another instance stops at its next lease renewal.
            state.exec_ctx.background_jobs().stop_local(&response_id);
            axum::Json(DeletedResponse::response(response_id)).into_response()
        }
        Err(error) => executor_error_response(error),
    }
}

/// List the input items of a locally persisted response.
///
/// The list is the full context the response was generated from — items inherited
/// through `previous_response_id` or its conversation, then this turn's input —
/// without the response's own output.
#[cfg_attr(feature = "openapi", utoipa::path(
    get,
    path = "/v1/responses/{response_id}/input_items",
    params(
        ("response_id" = String, Path, description = "Stored response ID"),
        ListItemsQuery
    ),
    responses(
        (status = 200, description = "Input items", body = agentic_core::types::ListItemsResponse),
        (status = 400, description = "Invalid pagination parameters", body = crate::openapi::ApiErrorResponse),
        (status = 401, description = "Missing or invalid bearer token", body = crate::openapi::ApiErrorResponse),
        (status = 404, description = "Response not found", body = crate::openapi::ApiErrorResponse),
        (status = 409, description = "Legacy response has no retrievable payload", body = crate::openapi::ApiErrorResponse),
    ),
    security(("bearer_auth" = [])),
    tag = "responses",
))]
pub async fn list_input_items(
    State(state): State<AppState>,
    Path(response_id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let query = match ListItemsQuery::parse(query.as_deref()) {
        Ok(query) => query,
        Err(error) => return *error,
    };
    match state
        .exec_ctx
        .resp_handler
        .list_input_items(&response_id, query.page())
        .await
    {
        Ok(page) => axum::Json(page).into_response(),
        Err(error) => executor_error_response(error),
    }
}
