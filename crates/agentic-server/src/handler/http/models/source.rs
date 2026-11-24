//! Obtain the model catalog from the selected inference backend.

use agentic_core::proxy::{ProxyBody, ProxyResponse, error_response, proxy_get};
use http::{HeaderMap, StatusCode};
use serde::Serialize;

use crate::app::AppState;
use crate::model_capabilities::ServedModel;

#[derive(Serialize)]
struct ModelList<'a> {
    object: &'static str,
    data: [&'a ServedModel; 1],
}

/// Both inference transports feed the existing catalog projection and retrieval path.
pub(super) async fn model_list(state: &AppState, headers: &HeaderMap) -> ProxyResponse {
    let Some(status) = state.direct_status.as_deref() else {
        return proxy_get("/v1/models", headers, &state.proxy_state).await;
    };
    match serde_json::to_vec(&ModelList {
        object: "list",
        data: [&status.model],
    }) {
        Ok(body) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            ProxyResponse {
                status: StatusCode::OK,
                headers,
                body: ProxyBody::Full(body.into()),
            }
        }
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "could not encode model catalog",
        ),
    }
}
