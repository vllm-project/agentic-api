//! Chat Completions and Completions pass-through.
//!
//! Agentic API owns the stateful agentic APIs; it does not own the Chat
//! Completions contract. When the gateway is deployed as the entry point in
//! front of an inference stack, clients on those endpoints must keep working,
//! so both paths are forwarded to `{llm_api_base}` verbatim: no translation, no
//! state, no gateway tool loop. Streaming responses are relayed as they arrive.

use axum::extract::{Request, State};
use axum::response::Response;

use super::super::common::passthrough;
use crate::app::AppState;

#[cfg_attr(feature = "openapi", utoipa::path(
    post,
    path = "/v1/chat/completions",
    responses(
        (status = 200, description = "Upstream Chat Completions response, forwarded verbatim: JSON when \
            stream=false, SSE when stream=true"),
        (status = 413, description = "Request body too large", body = crate::openapi::ApiErrorResponse),
        (status = 502, description = "Upstream error", body = crate::openapi::ApiErrorResponse),
    ),
    security(("bearer_auth" = [])),
    tag = "chat",
))]
pub async fn chat_completions(State(state): State<AppState>, req: Request) -> Response {
    passthrough(&state, req, "/v1/chat/completions").await
}

#[cfg_attr(feature = "openapi", utoipa::path(
    post,
    path = "/v1/completions",
    responses(
        (status = 200, description = "Upstream Completions response, forwarded verbatim: JSON when \
            stream=false, SSE when stream=true"),
        (status = 413, description = "Request body too large", body = crate::openapi::ApiErrorResponse),
        (status = 502, description = "Upstream error", body = crate::openapi::ApiErrorResponse),
    ),
    security(("bearer_auth" = [])),
    tag = "chat",
))]
pub async fn completions(State(state): State<AppState>, req: Request) -> Response {
    passthrough(&state, req, "/v1/completions").await
}
