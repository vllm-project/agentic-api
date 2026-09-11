//! Files and vector store routes backed by the core retrieval service.

use agentic_core::executor::ExecutorError;
use agentic_core::tool::ToolError;
use agentic_core::tool::file_search::{FileSearchService, MAX_FILE_BYTES};
use agentic_core::types::file_search::{
    AttachFileRequest, CreateVectorStoreRequest, FileExpirationAnchor, FileExpiresAfter, FileSearchError, ListParams,
    SearchRequest,
};
#[path = "multipart_limits.rs"]
mod multipart_limits;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use http::{StatusCode, header};
use multipart_limits::BoundedMultipart;
use serde::Serialize;

use crate::app::AppState;
use crate::handler::common::executor_error_response;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/files",
            post(upload_file)
                .get(list_files)
                .layer(DefaultBodyLimit::max(MAX_FILE_BYTES + 64 * 1024)),
        )
        .route("/v1/files/{file_id}", get(get_file).delete(delete_file))
        .route("/v1/files/{file_id}/content", get(file_content))
        .route("/v1/vector_stores", post(create_vector_store).get(list_vector_stores))
        .route(
            "/v1/vector_stores/{store_id}",
            get(get_vector_store).delete(delete_vector_store),
        )
        .route(
            "/v1/vector_stores/{store_id}/files",
            post(attach_file).get(list_vector_store_files),
        )
        .route(
            "/v1/vector_stores/{store_id}/files/{file_id}",
            get(get_vector_store_file).delete(detach_file),
        )
        .route("/v1/vector_stores/{store_id}/search", post(search))
}

fn service(state: &AppState) -> Result<&FileSearchService, Box<Response>> {
    state.exec_ctx.file_search.as_ref().ok_or_else(|| {
        Box::new(error(FileSearchError::Unavailable(
            "File search requires configured persistence".into(),
        )))
    })
}

fn error(error: FileSearchError) -> Response {
    executor_error_response(ExecutorError::Tool(ToolError::FileSearch(error)))
}

fn invalid(message: impl Into<String>) -> Response {
    error(FileSearchError::InvalidRequest(message.into()))
}

fn result<T: Serialize>(value: Result<T, FileSearchError>) -> Response {
    match value {
        Ok(value) => Json(value).into_response(),
        Err(failure) => error(failure),
    }
}

fn body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, Box<Response>> {
    body.map(|Json(value)| value).map_err(|rejection| {
        Box::new({
            if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                executor_error_response(ExecutorError::PayloadTooLarge("Request body exceeds the limit".into()))
            } else {
                invalid(rejection.body_text())
            }
        })
    })
}

fn query(params: Result<Query<ListParams>, QueryRejection>) -> Result<ListParams, Box<Response>> {
    params
        .map(|Query(value)| value)
        .map_err(|rejection| Box::new(invalid(rejection.body_text())))
}

#[cfg_attr(feature = "openapi", utoipa::path(
    post, path = "/v1/files",
    request_body(content = crate::openapi::FileUploadRequest, content_type = "multipart/form-data"),
    responses((status = 200, description = "Success", body = agentic_core::types::file_search::FileObject), (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse), (status = 404, description = "Object not found", body = crate::openapi::ApiErrorResponse)),
    security(("bearer_auth" = [])), tag = "file_search",
))]
pub(crate) async fn upload_file(
    State(state): State<AppState>,
    multipart: Result<BoundedMultipart, Response>,
) -> Response {
    let search = match service(&state) {
        Ok(service) => service,
        Err(error) => return *error,
    };
    let mut multipart = match multipart {
        Ok(BoundedMultipart(multipart)) => multipart,
        Err(rejection) => return rejection,
    };
    let mut purpose = None;
    let mut anchor = None;
    let mut seconds = None;
    let mut uploaded = None;
    loop {
        let mut field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(failure) => return multipart_error(&failure),
        };
        match field.name() {
            Some("purpose" | "expires_after[anchor]" | "expires_after[seconds]") => {
                let name = field.name().unwrap_or_default().to_owned();
                let slot = match name.as_str() {
                    "purpose" => &mut purpose,
                    "expires_after[anchor]" => &mut anchor,
                    _ => &mut seconds,
                };
                if slot.is_some() {
                    return invalid("Duplicate multipart field");
                }
                let mut value = Vec::new();
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            if value.len().saturating_add(chunk.len()) > 32 {
                                return invalid("Multipart scalar exceeds 32 bytes");
                            }
                            value.extend_from_slice(&chunk);
                        }
                        Ok(None) => break,
                        Err(failure) => return multipart_error(&failure),
                    }
                }
                let Ok(value) = String::from_utf8(value) else {
                    return invalid("Multipart scalar must be UTF-8");
                };
                *slot = Some(value);
            }
            Some("file") if uploaded.is_none() => {
                let Some(filename) = field.file_name() else {
                    return invalid("File field requires a filename");
                };
                let mut upload = match search
                    .begin_file_upload(filename, field.content_type().unwrap_or("application/octet-stream"))
                {
                    Ok(upload) => upload,
                    Err(failure) => return error(failure),
                };
                let mut size = 0usize;
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            size = size.saturating_add(chunk.len());
                            if size > MAX_FILE_BYTES {
                                return executor_error_response(ExecutorError::PayloadTooLarge(
                                    "File exceeds 512 MiB".into(),
                                ));
                            }
                            if let Err(failure) = upload.write(&chunk).await {
                                return error(failure);
                            }
                        }
                        Ok(None) => break,
                        Err(failure) => return multipart_error(&failure),
                    }
                }
                uploaded = Some(upload);
            }
            _ => return invalid("Expected one file, one purpose, and optional expires_after fields"),
        }
    }
    let (Some(purpose), Some(upload)) = (purpose, uploaded) else {
        return invalid("Both file and purpose are required");
    };
    let expires = match (anchor, seconds) {
        (None, None) => None,
        (Some(anchor), Some(seconds)) if anchor == "created_at" => {
            let Ok(seconds) = seconds.parse::<u32>() else {
                return invalid("expires_after.seconds must be an integer");
            };
            Some(FileExpiresAfter {
                anchor: FileExpirationAnchor::CreatedAt,
                seconds,
            })
        }
        _ => return invalid("expires_after requires anchor=created_at and seconds"),
    };
    result(upload.finish(&purpose, expires).await)
}

fn multipart_error(failure: &axum::extract::multipart::MultipartError) -> Response {
    if multipart_limits::is_framing_limit(failure) {
        executor_error_response(ExecutorError::PayloadTooLarge("Multipart framing exceeds 8 KiB".into()))
    } else if failure.status() == StatusCode::PAYLOAD_TOO_LARGE {
        executor_error_response(ExecutorError::PayloadTooLarge("Upload exceeds 512 MiB".into()))
    } else {
        invalid("Malformed multipart upload")
    }
}

#[cfg_attr(feature = "openapi", utoipa::path(
    get, path = "/v1/files",
    params(("limit" = Option<usize>, Query, description = "Page size, 1 to 10000; defaults to 10000"), ("purpose" = Option<String>, Query), ("after" = Option<String>, Query), ("before" = Option<String>, Query), ("order" = Option<String>, Query)),
    responses((status = 200, description = "Success", body = agentic_core::types::file_search::ListResponse<agentic_core::types::file_search::FileObject>), (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse), (status = 404, description = "Object not found", body = crate::openapi::ApiErrorResponse)),
    security(("bearer_auth" = [])), tag = "file_search",
))]
pub(crate) async fn list_files(
    State(state): State<AppState>,
    params: Result<Query<ListParams>, QueryRejection>,
) -> Response {
    let search = match service(&state) {
        Ok(service) => service,
        Err(error) => return *error,
    };
    let params = match query(params) {
        Ok(params) => params,
        Err(error) => return *error,
    };
    result(search.list_files(&params).await)
}

#[cfg_attr(feature = "openapi", utoipa::path(
    get, path = "/v1/files/{file_id}",
    params(("file_id" = String, Path, description = "Object identifier")),
    responses((status = 200, description = "Success", body = agentic_core::types::file_search::FileObject), (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse), (status = 404, description = "Object not found", body = crate::openapi::ApiErrorResponse)),
    security(("bearer_auth" = [])), tag = "file_search",
))]
pub(crate) async fn get_file(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let search = match service(&state) {
        Ok(service) => service,
        Err(error) => return *error,
    };
    result(search.get_file(&id).await)
}

#[cfg_attr(feature = "openapi", utoipa::path(
    get, path = "/v1/files/{file_id}/content",
    params(("file_id" = String, Path, description = "Object identifier")),
    responses((status = 200, description = "Original uploaded bytes", body = Vec<u8>, content_type = "application/octet-stream"), (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse), (status = 404, description = "Object not found", body = crate::openapi::ApiErrorResponse)),
    security(("bearer_auth" = [])), tag = "file_search",
))]
pub(crate) async fn file_content(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let search = match service(&state) {
        Ok(service) => service,
        Err(error) => return *error,
    };
    match search.download_file(&id).await {
        Ok(download) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (header::CONTENT_DISPOSITION, "attachment".to_owned()),
                (header::CONTENT_LENGTH, download.bytes.to_string()),
            ],
            axum::body::Body::from_stream(download),
        )
            .into_response(),
        Err(failure) => error(failure),
    }
}

#[cfg_attr(feature = "openapi", utoipa::path(
    delete, path = "/v1/files/{file_id}",
    params(("file_id" = String, Path, description = "Object identifier")),
    responses((status = 200, description = "Success", body = agentic_core::types::file_search::DeleteObject), (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse), (status = 404, description = "Object not found", body = crate::openapi::ApiErrorResponse)),
    security(("bearer_auth" = [])), tag = "file_search",
))]
pub(crate) async fn delete_file(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let search = match service(&state) {
        Ok(service) => service,
        Err(error) => return *error,
    };
    result(search.delete_file(&id).await)
}

#[cfg_attr(feature = "openapi", utoipa::path(
    post, path = "/v1/vector_stores",
    request_body = agentic_core::types::file_search::CreateVectorStoreRequest,
    responses((status = 200, description = "Success", body = agentic_core::types::file_search::VectorStoreObject), (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse), (status = 404, description = "Object not found", body = crate::openapi::ApiErrorResponse)),
    security(("bearer_auth" = [])), tag = "file_search",
))]
pub(crate) async fn create_vector_store(
    State(state): State<AppState>,
    request: Result<Json<CreateVectorStoreRequest>, JsonRejection>,
) -> Response {
    let search = match service(&state) {
        Ok(service) => service,
        Err(error) => return *error,
    };
    let request = match body(request) {
        Ok(request) => request,
        Err(error) => return *error,
    };
    result(search.create_vector_store(request).await)
}

#[cfg_attr(feature = "openapi", utoipa::path(
    get, path = "/v1/vector_stores",
    params(("limit" = Option<usize>, Query, description = "Page size, 1 to 100"), ("after" = Option<String>, Query), ("before" = Option<String>, Query), ("order" = Option<String>, Query)),
    responses((status = 200, description = "Success", body = agentic_core::types::file_search::ListResponse<agentic_core::types::file_search::VectorStoreObject>), (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse), (status = 404, description = "Object not found", body = crate::openapi::ApiErrorResponse)),
    security(("bearer_auth" = [])), tag = "file_search",
))]
pub(crate) async fn list_vector_stores(
    State(state): State<AppState>,
    params: Result<Query<ListParams>, QueryRejection>,
) -> Response {
    let search = match service(&state) {
        Ok(service) => service,
        Err(error) => return *error,
    };
    let params = match query(params) {
        Ok(params) => params,
        Err(error) => return *error,
    };
    result(search.list_vector_stores(&params).await)
}

#[cfg_attr(feature = "openapi", utoipa::path(
    get, path = "/v1/vector_stores/{store_id}",
    params(("store_id" = String, Path, description = "Object identifier")),
    responses((status = 200, description = "Success", body = agentic_core::types::file_search::VectorStoreObject), (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse), (status = 404, description = "Object not found", body = crate::openapi::ApiErrorResponse)),
    security(("bearer_auth" = [])), tag = "file_search",
))]
pub(crate) async fn get_vector_store(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let search = match service(&state) {
        Ok(service) => service,
        Err(error) => return *error,
    };
    result(search.get_vector_store(&id).await)
}

#[cfg_attr(feature = "openapi", utoipa::path(
    delete, path = "/v1/vector_stores/{store_id}",
    params(("store_id" = String, Path, description = "Object identifier")),
    responses((status = 200, description = "Success", body = agentic_core::types::file_search::DeleteObject), (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse), (status = 404, description = "Object not found", body = crate::openapi::ApiErrorResponse)),
    security(("bearer_auth" = [])), tag = "file_search",
))]
pub(crate) async fn delete_vector_store(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let search = match service(&state) {
        Ok(service) => service,
        Err(error) => return *error,
    };
    result(search.delete_vector_store(&id).await)
}

#[cfg_attr(feature = "openapi", utoipa::path(
    post, path = "/v1/vector_stores/{store_id}/files",
    params(("store_id" = String, Path, description = "Object identifier")),
    request_body = agentic_core::types::file_search::AttachFileRequest,
    responses((status = 200, description = "Success", body = agentic_core::types::file_search::VectorStoreFileObject), (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse), (status = 404, description = "Object not found", body = crate::openapi::ApiErrorResponse)),
    security(("bearer_auth" = [])), tag = "file_search",
))]
pub(crate) async fn attach_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    request: Result<Json<AttachFileRequest>, JsonRejection>,
) -> Response {
    let search = match service(&state) {
        Ok(service) => service,
        Err(error) => return *error,
    };
    let request = match body(request) {
        Ok(request) => request,
        Err(error) => return *error,
    };
    result(search.attach_file(&id, request).await)
}

#[cfg_attr(feature = "openapi", utoipa::path(
    get, path = "/v1/vector_stores/{store_id}/files",
    params(("store_id" = String, Path, description = "Object identifier"), ("limit" = Option<usize>, Query, description = "Page size, 1 to 100"), ("after" = Option<String>, Query), ("before" = Option<String>, Query), ("order" = Option<String>, Query)),
    responses((status = 200, description = "Success", body = agentic_core::types::file_search::ListResponse<agentic_core::types::file_search::VectorStoreFileObject>), (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse), (status = 404, description = "Object not found", body = crate::openapi::ApiErrorResponse)),
    security(("bearer_auth" = [])), tag = "file_search",
))]
pub(crate) async fn list_vector_store_files(
    State(state): State<AppState>,
    Path(id): Path<String>,
    params: Result<Query<ListParams>, QueryRejection>,
) -> Response {
    let search = match service(&state) {
        Ok(service) => service,
        Err(error) => return *error,
    };
    let params = match query(params) {
        Ok(params) => params,
        Err(error) => return *error,
    };
    result(search.list_vector_store_files(&id, &params).await)
}

#[cfg_attr(feature = "openapi", utoipa::path(
    get, path = "/v1/vector_stores/{store_id}/files/{file_id}",
    params(("store_id" = String, Path, description = "Object identifier"), ("file_id" = String, Path, description = "Object identifier")),
    responses((status = 200, description = "Success", body = agentic_core::types::file_search::VectorStoreFileObject), (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse), (status = 404, description = "Object not found", body = crate::openapi::ApiErrorResponse)),
    security(("bearer_auth" = [])), tag = "file_search",
))]
pub(crate) async fn get_vector_store_file(
    State(state): State<AppState>,
    Path((store_id, file_id)): Path<(String, String)>,
) -> Response {
    let search = match service(&state) {
        Ok(service) => service,
        Err(error) => return *error,
    };
    result(search.get_vector_store_file(&store_id, &file_id).await)
}

#[cfg_attr(feature = "openapi", utoipa::path(
    delete, path = "/v1/vector_stores/{store_id}/files/{file_id}",
    params(("store_id" = String, Path, description = "Object identifier"), ("file_id" = String, Path, description = "Object identifier")),
    responses((status = 200, description = "Success", body = agentic_core::types::file_search::DeleteObject), (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse), (status = 404, description = "Object not found", body = crate::openapi::ApiErrorResponse)),
    security(("bearer_auth" = [])), tag = "file_search",
))]
pub(crate) async fn detach_file(
    State(state): State<AppState>,
    Path((store_id, file_id)): Path<(String, String)>,
) -> Response {
    let search = match service(&state) {
        Ok(service) => service,
        Err(error) => return *error,
    };
    result(search.detach_file(&store_id, &file_id).await)
}

#[cfg_attr(feature = "openapi", utoipa::path(
    post, path = "/v1/vector_stores/{store_id}/search",
    params(("store_id" = String, Path, description = "Object identifier")),
    request_body = agentic_core::types::file_search::SearchRequest,
    responses((status = 200, description = "Success", body = agentic_core::types::file_search::SearchResponse), (status = 400, description = "Invalid request", body = crate::openapi::ApiErrorResponse), (status = 404, description = "Object not found", body = crate::openapi::ApiErrorResponse)),
    security(("bearer_auth" = [])), tag = "file_search",
))]
pub(crate) async fn search(
    State(state): State<AppState>,
    Path(id): Path<String>,
    request: Result<Json<SearchRequest>, JsonRejection>,
) -> Response {
    let search = match service(&state) {
        Ok(service) => service,
        Err(error) => return *error,
    };
    let request = match body(request) {
        Ok(request) => request,
        Err(error) => return *error,
    };
    result(search.search(&[id], &request).await)
}
