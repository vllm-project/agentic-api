//! Audio transcription and translation pass-through.
//!
//! The gateway does not own the audio contracts, so both routes forward the client's multipart
//! upload to `{llm_api_base}` verbatim. The body is streamed as it arrives instead of being
//! buffered, within the same request-size limit as the other `/v1/*` routes, and event streams
//! (`stream=true`) are relayed as they arrive.

use axum::extract::{Request, State};
use axum::response::Response;

use super::super::common::stream_passthrough;
use crate::app::AppState;

#[cfg_attr(feature = "openapi", utoipa::path(
    post,
    path = "/v1/audio/transcriptions",
    request_body(content = crate::openapi::AudioUploadForm, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Upstream transcription, forwarded verbatim: JSON or text by \
            response_format, SSE when stream=true"),
        (status = 413, description = "Request body too large", body = crate::openapi::ApiErrorResponse),
        (status = 502, description = "Upstream error", body = crate::openapi::ApiErrorResponse),
    ),
    security(("bearer_auth" = [])),
    tag = "audio",
))]
pub async fn transcriptions(State(state): State<AppState>, req: Request) -> Response {
    stream_passthrough(&state, req, "/v1/audio/transcriptions").await
}

#[cfg_attr(feature = "openapi", utoipa::path(
    post,
    path = "/v1/audio/translations",
    request_body(content = crate::openapi::AudioUploadForm, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Upstream translation, forwarded verbatim: JSON or text by \
            response_format, SSE when stream=true"),
        (status = 413, description = "Request body too large", body = crate::openapi::ApiErrorResponse),
        (status = 502, description = "Upstream error", body = crate::openapi::ApiErrorResponse),
    ),
    security(("bearer_auth" = [])),
    tag = "audio",
))]
pub async fn translations(State(state): State<AppState>, req: Request) -> Response {
    stream_passthrough(&state, req, "/v1/audio/translations").await
}
