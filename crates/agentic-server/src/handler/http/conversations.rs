use std::collections::BTreeMap;

use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;

use agentic_core::executor::ExecutorError;
use agentic_core::storage::ConversationData;
use agentic_core::types::{
    ConversationMetadata, ConversationResponse, CreateConversationRequest, DeletedResponse, UpdateConversationRequest,
};

use super::super::common::{executor_error_response, extract_json, extract_store, read_bytes};
use crate::app::AppState;

// Authentication is not wired into these management routes yet.
pub(super) const DEFAULT_TENANT_ID: &str = "default_tenant";

/// Create a new conversation with optional metadata and initial items.
#[cfg_attr(feature = "openapi", utoipa::path(
    post,
    path = "/v1/conversations",
    request_body(content = Option<CreateConversationRequest>, content_type = "application/json"),
    responses(
        (status = 200, description = "Conversation created", body = ConversationResponse),
        (status = 400, description = "Invalid request"),
    ),
    security(("bearer_auth" = [])),
    tag = "conversations",
))]
pub async fn create_conversation(State(state): State<AppState>, req: Request) -> Response {
    let (_, body) = req.into_parts();
    let bytes = match read_bytes(body, state.max_request_body_size).await {
        Ok(b) => b,
        Err(e) => return e,
    };

    if !extract_store(&bytes) {
        return executor_error_response(ExecutorError::InvalidRequest("conversations require store=true".into()));
    }

    let request: CreateConversationRequest = if bytes.is_empty() {
        CreateConversationRequest {
            metadata: None,
            items: None,
        }
    } else {
        match extract_conversation_request(&bytes) {
            Ok(request) => request,
            Err(error) => return error,
        }
    };

    match state
        .exec_ctx
        .conv_handler
        .create(
            DEFAULT_TENANT_ID,
            &request.metadata.unwrap_or_default(),
            &request.items.unwrap_or_default(),
        )
        .await
    {
        Ok(data) => conversation_response(data),
        Err(error) => executor_error_response(error),
    }
}

/// Retrieve a conversation by ID.
#[cfg_attr(feature = "openapi", utoipa::path(
    get,
    path = "/v1/conversations/{conversation_id}",
    params(
        ("conversation_id" = String, Path, description = "Conversation ID")
    ),
    responses(
        (status = 200, description = "Conversation retrieved", body = ConversationResponse),
        (status = 404, description = "Conversation not found"),
    ),
    security(("bearer_auth" = [])),
    tag = "conversations",
))]
pub async fn retrieve_conversation(State(state): State<AppState>, Path(conversation_id): Path<String>) -> Response {
    match state
        .exec_ctx
        .conv_handler
        .retrieve(DEFAULT_TENANT_ID, &conversation_id)
        .await
    {
        Ok(data) => conversation_response(data),
        Err(error) => executor_error_response(error),
    }
}

/// Update a conversation's metadata.
#[cfg_attr(feature = "openapi", utoipa::path(
    post,
    path = "/v1/conversations/{conversation_id}",
    params(
        ("conversation_id" = String, Path, description = "Conversation ID")
    ),
    request_body = UpdateConversationRequest,
    responses(
        (status = 200, description = "Conversation updated", body = ConversationResponse),
        (status = 404, description = "Conversation not found"),
    ),
    security(("bearer_auth" = [])),
    tag = "conversations",
))]
pub async fn update_conversation(
    State(state): State<AppState>,
    Path(conversation_id): Path<String>,
    req: Request,
) -> Response {
    let (_, body) = req.into_parts();
    let bytes = match read_bytes(body, state.max_request_body_size).await {
        Ok(b) => b,
        Err(e) => return e,
    };

    let request: UpdateConversationRequest = match extract_conversation_request(&bytes) {
        Ok(r) => r,
        Err(e) => return e,
    };

    match state
        .exec_ctx
        .conv_handler
        .update_metadata(DEFAULT_TENANT_ID, &conversation_id, request.metadata)
        .await
    {
        Ok(data) => conversation_response(data),
        Err(error) => executor_error_response(error),
    }
}

/// Delete a conversation by ID.
#[cfg_attr(feature = "openapi", utoipa::path(
    delete,
    path = "/v1/conversations/{conversation_id}",
    params(
        ("conversation_id" = String, Path, description = "Conversation ID")
    ),
    responses(
        (status = 200, description = "Conversation deleted", body = DeletedResponse),
        (status = 404, description = "Conversation not found"),
    ),
    security(("bearer_auth" = [])),
    tag = "conversations",
))]
pub async fn delete_conversation(State(state): State<AppState>, Path(conversation_id): Path<String>) -> Response {
    match state
        .exec_ctx
        .conv_handler
        .delete(DEFAULT_TENANT_ID, &conversation_id)
        .await
    {
        Ok(()) => {
            let response = DeletedResponse::conversation(conversation_id);
            axum::Json(response).into_response()
        }
        Err(error) => executor_error_response(error),
    }
}

pub(super) fn conversation_response(data: ConversationData) -> Response {
    axum::Json(ConversationResponse::new(
        data.conversation_id,
        data.created_at,
        data.metadata,
    ))
    .into_response()
}

/// Decode a conversation request body, surfacing an out-of-limit `metadata`
/// object as the typed `invalid_request_error` that carries its `param` and
/// `code`.
///
/// The validated [`ConversationMetadata`] type already rejects such values
/// while deserializing, but `serde_json` reduces a `try_from` custom error to
/// its rendered message and drops the parameter and code. Check metadata
/// against the same rule first so the client receives the documented envelope;
/// every other decode error still comes from the typed request.
#[allow(
    clippy::result_large_err,
    reason = "mirrors extract_json, whose error is an axum response"
)]
fn extract_conversation_request<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Response> {
    let metadata_error = serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|value| value.get("metadata").cloned())
        .and_then(|metadata| serde_json::from_value::<BTreeMap<String, String>>(metadata).ok())
        .and_then(|map| ConversationMetadata::try_from(map).err())
        .map(|error| executor_error_response(ExecutorError::from(error)));
    if let Some(response) = metadata_error {
        return Err(response);
    }
    extract_json(bytes)
}
