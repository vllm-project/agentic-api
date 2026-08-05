use axum::extract::{Extension, Path, Query, Request, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use agentic_core::executor::ExecutorError;
use agentic_core::{InOutItem, InputItem};

use super::super::common::{executor_error_response, read_bytes};
use crate::app::AppState;
use crate::auth::AuthenticatedPrincipal;

const MAX_ITEMS_PER_REQUEST: usize = 20;

#[derive(Debug, Default, Deserialize)]
struct CreateConversationRequest {
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    items: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct UpdateConversationRequest {
    metadata: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct CreateItemsRequest {
    items: Vec<Value>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ListItemsQuery {
    after: Option<String>,
    limit: Option<usize>,
    order: Option<String>,
    #[allow(dead_code)]
    include: Option<Vec<String>>,
}

fn tenant_id(principal: Option<Extension<AuthenticatedPrincipal>>) -> Option<String> {
    principal.map(|Extension(principal)| principal.tenant_id())
}

#[allow(clippy::result_large_err)]
fn parse_body<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, Response> {
    serde_json::from_slice(bytes).map_err(|error| executor_error_response(ExecutorError::from(error)))
}

#[allow(clippy::result_large_err)]
fn parse_input_items(values: Vec<Value>) -> Result<Vec<InOutItem>, Response> {
    if values.len() > MAX_ITEMS_PER_REQUEST {
        return Err(executor_error_response(ExecutorError::InvalidRequest(format!(
            "items must contain at most {MAX_ITEMS_PER_REQUEST} items"
        ))));
    }
    values
        .into_iter()
        .map(|value| {
            let item: InputItem = serde_json::from_value(value).map_err(|error| {
                executor_error_response(ExecutorError::InvalidRequest(format!(
                    "invalid conversation item: {error}"
                )))
            })?;
            if matches!(item, InputItem::Unknown) {
                return Err(executor_error_response(ExecutorError::InvalidRequest(
                    "unsupported conversation item type".to_owned(),
                )));
            }
            Ok(InOutItem::Input(item))
        })
        .collect()
}

fn conversation_json(data: &agentic_core::ConversationData) -> Value {
    let metadata = data
        .metadata
        .as_deref()
        .and_then(agentic_core::utils::common::deserialize_from_str_opt::<Value>)
        .unwrap_or_else(|| json!({}));
    json!({
        "id": data.conversation_id,
        "created_at": data.created_at,
        "object": "conversation",
        "metadata": metadata,
    })
}

fn item_list_json(page: agentic_core::ConversationItemPage) -> Value {
    let first_id = page.data.first().map(|item| item.id.clone());
    let last_id = page.data.last().map(|item| item.id.clone());
    json!({
        "object": "list",
        "data": page.data.into_iter().map(|item| item.item).collect::<Vec<_>>(),
        "first_id": first_id,
        "last_id": last_id,
        "has_more": page.has_more,
    })
}

pub async fn conversations(
    State(state): State<AppState>,
    principal: Option<Extension<AuthenticatedPrincipal>>,
    req: Request,
) -> Response {
    let (_, body) = req.into_parts();
    let bytes = match read_bytes(body).await {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let request = if bytes.is_empty() {
        CreateConversationRequest::default()
    } else {
        match parse_body::<CreateConversationRequest>(&bytes) {
            Ok(request) => request,
            Err(error) => return error,
        }
    };
    let items = match parse_input_items(request.items) {
        Ok(items) => items,
        Err(error) => return error,
    };
    let tenant_id = tenant_id(principal);
    match state
        .exec_ctx
        .conv_handler
        .create_with_items(tenant_id.as_deref(), request.metadata.as_ref(), items)
        .await
    {
        Ok(data) => axum::Json(conversation_json(&data)).into_response(),
        Err(error) => executor_error_response(error),
    }
}

pub async fn retrieve_conversation(
    State(state): State<AppState>,
    principal: Option<Extension<AuthenticatedPrincipal>>,
    Path(conversation_id): Path<String>,
) -> Response {
    let tenant_id = tenant_id(principal);
    match state
        .exec_ctx
        .conv_handler
        .get_by_id(&conversation_id, tenant_id.as_deref())
        .await
    {
        Ok(data) => axum::Json(conversation_json(&data)).into_response(),
        Err(error) => executor_error_response(error),
    }
}

pub async fn update_conversation(
    State(state): State<AppState>,
    principal: Option<Extension<AuthenticatedPrincipal>>,
    Path(conversation_id): Path<String>,
    req: Request,
) -> Response {
    let (_, body) = req.into_parts();
    let bytes = match read_bytes(body).await {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let request = match parse_body::<UpdateConversationRequest>(&bytes) {
        Ok(request) => request,
        Err(error) => return error,
    };
    let tenant_id = tenant_id(principal);
    match state
        .exec_ctx
        .conv_handler
        .update_metadata(&conversation_id, tenant_id.as_deref(), request.metadata.as_ref())
        .await
    {
        Ok(data) => axum::Json(conversation_json(&data)).into_response(),
        Err(error) => executor_error_response(error),
    }
}

pub async fn delete_conversation(
    State(state): State<AppState>,
    principal: Option<Extension<AuthenticatedPrincipal>>,
    Path(conversation_id): Path<String>,
) -> Response {
    let tenant_id = tenant_id(principal);
    match state
        .exec_ctx
        .conv_handler
        .delete(&conversation_id, tenant_id.as_deref())
        .await
    {
        Ok(()) => axum::Json(json!({
            "id": conversation_id,
            "object": "conversation.deleted",
            "deleted": true,
        }))
        .into_response(),
        Err(error) => executor_error_response(error),
    }
}

pub async fn create_items(
    State(state): State<AppState>,
    principal: Option<Extension<AuthenticatedPrincipal>>,
    Path(conversation_id): Path<String>,
    req: Request,
) -> Response {
    let (_, body) = req.into_parts();
    let bytes = match read_bytes(body).await {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let request = match parse_body::<CreateItemsRequest>(&bytes) {
        Ok(request) => request,
        Err(error) => return error,
    };
    let items = match parse_input_items(request.items) {
        Ok(items) => items,
        Err(error) => return error,
    };
    let tenant_id = tenant_id(principal);
    match state
        .exec_ctx
        .conv_handler
        .append_items(&conversation_id, tenant_id.as_deref(), items)
        .await
    {
        Ok(items) => axum::Json(item_list_json(agentic_core::ConversationItemPage {
            data: items,
            has_more: false,
        }))
        .into_response(),
        Err(error) => executor_error_response(error),
    }
}

pub async fn list_items(
    State(state): State<AppState>,
    principal: Option<Extension<AuthenticatedPrincipal>>,
    Path(conversation_id): Path<String>,
    Query(query): Query<ListItemsQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return executor_error_response(ExecutorError::InvalidRequest(
            "limit must be between 1 and 100".to_owned(),
        ));
    }
    let descending = match query.order.as_deref().unwrap_or("desc") {
        "asc" => false,
        "desc" => true,
        _ => {
            return executor_error_response(ExecutorError::InvalidRequest(
                "order must be either asc or desc".to_owned(),
            ));
        }
    };
    let tenant_id = tenant_id(principal);
    match state
        .exec_ctx
        .conv_handler
        .list_items(
            &conversation_id,
            tenant_id.as_deref(),
            query.after.as_deref(),
            limit,
            descending,
        )
        .await
    {
        Ok(page) => axum::Json(item_list_json(page)).into_response(),
        Err(error) => executor_error_response(error),
    }
}

pub async fn retrieve_item(
    State(state): State<AppState>,
    principal: Option<Extension<AuthenticatedPrincipal>>,
    Path((conversation_id, item_id)): Path<(String, String)>,
) -> Response {
    let tenant_id = tenant_id(principal);
    match state
        .exec_ctx
        .conv_handler
        .get_item(&conversation_id, &item_id, tenant_id.as_deref())
        .await
    {
        Ok(item) => axum::Json(item.item).into_response(),
        Err(error) => executor_error_response(error),
    }
}

pub async fn delete_item(
    State(state): State<AppState>,
    principal: Option<Extension<AuthenticatedPrincipal>>,
    Path((conversation_id, item_id)): Path<(String, String)>,
) -> Response {
    let tenant_id = tenant_id(principal);
    match state
        .exec_ctx
        .conv_handler
        .delete_item(&conversation_id, &item_id, tenant_id.as_deref())
        .await
    {
        Ok(()) => match state
            .exec_ctx
            .conv_handler
            .get_by_id(&conversation_id, tenant_id.as_deref())
            .await
        {
            Ok(data) => axum::Json(conversation_json(&data)).into_response(),
            Err(error) => executor_error_response(error),
        },
        Err(error) => executor_error_response(error),
    }
}
