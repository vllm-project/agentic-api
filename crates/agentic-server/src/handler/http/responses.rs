use axum::extract::{Extension, Request, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use either::Either;
use tracing::debug;

use std::sync::Arc;

use agentic_core::executor::{ExecuteRequest, compact_response_for_tenant as execute_compaction};
use agentic_core::proxy::{ProxyRequest, proxy_request};
use agentic_core::types::request_response::{CompactRequest, RequestPayload};
use agentic_core::types::tools::ResponsesTool;

use super::super::common::{
    convert_response, executor_error_response, extract_bearer, read_and_parse, read_json, sse_response,
};
use crate::app::AppState;
use crate::auth::AuthenticatedPrincipal;

async fn proxy_responses(state: &AppState, parts: Parts, body: Bytes) -> Response {
    let proxy_req = ProxyRequest {
        headers: parts.headers,
        body,
        query: parts.uri.query().map(str::to_string),
    };
    convert_response(proxy_request(proxy_req, &state.proxy_state).await)
}

async fn execute_responses(
    state: &AppState,
    parts: Parts,
    payload: RequestPayload,
    tenant_id: Option<String>,
) -> Response {
    let auth = extract_bearer(&parts.headers, state.openai_api_key.as_deref());
    match ExecuteRequest::new(payload, Arc::clone(&state.exec_ctx))
        .with_auth(auth)
        .with_tenant_id(tenant_id)
        .run()
        .await
    {
        Ok(Either::Left(response_payload)) => axum::Json(response_payload).into_response(),
        Ok(Either::Right(stream)) => sse_response(stream),
        Err(e) => executor_error_response(e),
    }
}

fn has_gateway_tools(payload: &RequestPayload) -> bool {
    payload
        .tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|tool| !matches!(tool, ResponsesTool::Function(_))))
}

pub async fn responses(
    State(state): State<AppState>,
    principal: Option<Extension<AuthenticatedPrincipal>>,
    req: Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let (bytes, payload) = match read_and_parse(body).await {
        Ok(v) => v,
        Err(e) => return e,
    };

    let should_execute = payload.store
        || payload.previous_response_id.is_some()
        || payload.conversation_id.is_some()
        || payload.input.contains_compaction()
        || payload
            .context_management
            .as_ref()
            .is_some_and(|entries| !entries.is_empty())
        || has_gateway_tools(&payload);
    debug!(
        route = if should_execute { "executor" } else { "proxy" },
        store = payload.store,
        stream = payload.stream,
        has_previous_response_id = payload.previous_response_id.is_some(),
        has_conversation_id = payload.conversation_id.is_some(),
        has_compaction = payload.input.contains_compaction(),
        context_management = payload.context_management.as_ref().map_or(0, Vec::len),
        tools = payload.tools.as_ref().map_or(0, Vec::len),
        "routing HTTP responses request"
    );

    let tenant_id = principal.map(|Extension(principal)| principal.tenant_id());
    if should_execute {
        execute_responses(&state, parts, payload, tenant_id).await
    } else {
        proxy_responses(&state, parts, bytes).await
    }
}

pub async fn compact_response(
    State(state): State<AppState>,
    principal: Option<Extension<AuthenticatedPrincipal>>,
    req: Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let request: CompactRequest = match read_json(body).await {
        Ok(request) => request,
        Err(response) => return response,
    };
    let auth = extract_bearer(&parts.headers, state.openai_api_key.as_deref());
    let tenant_id = principal.map(|Extension(principal)| principal.tenant_id());
    match execute_compaction(request, state.exec_ctx.as_ref(), auth.as_deref(), tenant_id).await {
        Ok(response) => axum::Json(response).into_response(),
        Err(error) => executor_error_response(error),
    }
}
