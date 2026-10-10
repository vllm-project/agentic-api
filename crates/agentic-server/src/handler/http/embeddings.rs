//! Embeddings pass-through.
//!
//! Embeddings are stateless and owned by the inference server, so `/v1/embeddings` is
//! forwarded to `{llm_api_base}` verbatim, like Chat Completions.

use axum::extract::{Request, State};
use axum::response::Response;

use super::super::common::passthrough;
use crate::app::AppState;

#[cfg_attr(feature = "openapi", utoipa::path(
    post,
    path = "/v1/embeddings",
    responses(
        (status = 200, description = "Upstream Embeddings response, forwarded verbatim"),
        (status = 413, description = "Request body too large", body = crate::openapi::ApiErrorResponse),
        (status = 502, description = "Upstream error", body = crate::openapi::ApiErrorResponse),
    ),
    security(("bearer_auth" = [])),
    tag = "embeddings",
))]
pub async fn embeddings(State(state): State<AppState>, req: Request) -> Response {
    passthrough(&state, req, "/v1/embeddings").await
}
