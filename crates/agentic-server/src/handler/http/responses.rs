use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use either::Either;

use agentic_core::executor::execute;
use agentic_core::proxy::{ProxyRequest, proxy_request};
use agentic_core::types::request_response::RequestPayload;

use super::super::common::{convert_response, executor_error_response, read_and_parse, resolve_exec_ctx, sse_response};
use crate::app::AppState;

async fn proxy_responses(state: &AppState, parts: Parts, body: Bytes) -> Response {
    let proxy_req = ProxyRequest {
        headers: parts.headers,
        body,
        query: parts.uri.query().map(str::to_string),
    };
    convert_response(proxy_request(proxy_req, &state.proxy_state).await)
}

async fn execute_responses(state: &AppState, parts: Parts, payload: RequestPayload) -> Response {
    match execute(payload, resolve_exec_ctx(state, &parts)).await {
        Ok(Either::Left(response_payload)) => axum::Json(response_payload).into_response(),
        Ok(Either::Right(stream)) => sse_response(stream),
        Err(e) => executor_error_response(e),
    }
}

pub async fn responses(State(state): State<AppState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let (bytes, payload) = match read_and_parse(body).await {
        Ok(v) => v,
        Err(e) => return e,
    };

    let should_persist = payload.store || payload.previous_response_id.is_some() || payload.conversation_id.is_some();

    if should_persist {
        execute_responses(&state, parts, payload).await
    } else {
        proxy_responses(&state, parts, bytes).await
    }
}
