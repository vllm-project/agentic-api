use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};

use agentic_core::executor::ExecutorError;
use agentic_core::storage::{ConversationData, StorageError};
use agentic_core::types::{
    ConversationResponse, CreateConversationRequest, DeletedResponse, UpdateConversationRequest,
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
        match extract_json(&bytes) {
            Ok(request) => request,
            Err(error) => return error,
        }
    };

    match state
        .exec_ctx
        .conv_handler
        .create_with_metadata_and_items(DEFAULT_TENANT_ID, request.metadata, request.items.unwrap_or_default())
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

    let request: UpdateConversationRequest = match extract_json(&bytes) {
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

pub(super) fn storage_error(error: StorageError) -> Response {
    executor_error_response(ExecutorError::Storage(error))
}

pub(super) fn conversation_response(data: ConversationData) -> Response {
    let metadata = match data.metadata.as_deref().map(serde_json::from_str).transpose() {
        Ok(metadata) => metadata,
        Err(error) => return storage_error(StorageError::Serialization(error)),
    };
    axum::Json(ConversationResponse::new(
        data.conversation_id,
        data.created_at,
        metadata,
    ))
    .into_response()
}
