//! One route declaration for both the HTTP router and `OpenAPI`.

#[cfg(not(feature = "openapi"))]
use axum::Router;
#[cfg(feature = "openapi")]
use utoipa_axum::{router::OpenApiRouter, routes};

use super::AppState;

// The declarations below drive both Axum and `OpenAPI`. The feature-disabled
// build uses the same groups to register Axum routes without schema metadata.
macro_rules! api_route_group {
    ($name:ident { $($path:expr => [$($method:ident($handler:path)),+ $(,)?]),+ $(,)? }) => {
        #[cfg(feature = "openapi")]
        pub(crate) fn $name() -> OpenApiRouter<AppState> {
            let mut router = OpenApiRouter::new();
            $(
                let route = routes!($($handler),+);
                assert_eq!(route.1.paths.len(), 1, "route group must declare one path");
                let item = route.1.paths.get($path).expect("route annotation differs from registered path");
                $(assert!(item.$method.is_some(), "route annotation differs from registered method");)+
                router = router.routes(route);
            )+
            router
        }

        #[cfg(not(feature = "openapi"))]
        pub(crate) fn $name() -> Router<AppState> {
            Router::new()$(.route($path, axum::routing::MethodRouter::new()$(.$method($handler))+))+
        }
    };
}

api_route_group!(documented_public_routes {
    "/health" => [get(crate::handler::http::models::health)],
    "/ready" => [get(crate::handler::http::models::ready)],
});

api_route_group!(documented_protected_routes {
    "/v1/conversations" => [post(crate::handler::http::conversations::create_conversation)],
    "/v1/conversations/{conversation_id}" => [
        get(crate::handler::http::conversations::retrieve_conversation),
        post(crate::handler::http::conversations::update_conversation),
        delete(crate::handler::http::conversations::delete_conversation),
    ],
    "/v1/conversations/{conversation_id}/items" => [
        post(crate::handler::http::conversation_items::create_item),
        get(crate::handler::http::conversation_items::list_items),
    ],
    "/v1/conversations/{conversation_id}/items/{item_id}" => [
        get(crate::handler::http::conversation_items::retrieve_item),
        delete(crate::handler::http::conversation_items::delete_item),
    ],
    "/v1/chat/completions" => [post(crate::handler::http::chat_completions::chat_completions)],
    "/v1/completions" => [post(crate::handler::http::chat_completions::completions)],
    "/v1/embeddings" => [post(crate::handler::http::embeddings::embeddings)],
    "/v1/audio/transcriptions" => [post(crate::handler::http::audio::transcriptions)],
    "/v1/audio/translations" => [post(crate::handler::http::audio::translations)],
    "/v1/models" => [get(crate::handler::http::models::models)],
    "/v1/messages" => [post(crate::handler::http::messages::messages)],
    "/v1/messages/count_tokens" => [post(crate::handler::http::messages::count_tokens)],
    "/v1/responses" => [
        post(crate::handler::http::responses::responses),
        get(crate::handler::websocket::responses::responses_ws_with_auth),
    ],
    "/v1/responses/compact" => [post(crate::handler::http::responses::compact_response)],
});

api_route_group!(documented_file_search_routes {
    "/v1/files" => [post(crate::handler::http::file_search::upload_file), get(crate::handler::http::file_search::list_files)],
    "/v1/files/{file_id}" => [get(crate::handler::http::file_search::get_file), delete(crate::handler::http::file_search::delete_file)],
    "/v1/files/{file_id}/content" => [get(crate::handler::http::file_search::file_content)],
    "/v1/vector_stores" => [post(crate::handler::http::file_search::create_vector_store), get(crate::handler::http::file_search::list_vector_stores)],
    "/v1/vector_stores/{store_id}" => [get(crate::handler::http::file_search::get_vector_store), delete(crate::handler::http::file_search::delete_vector_store)],
    "/v1/vector_stores/{store_id}/files" => [post(crate::handler::http::file_search::attach_file), get(crate::handler::http::file_search::list_vector_store_files)],
    "/v1/vector_stores/{store_id}/files/{file_id}" => [get(crate::handler::http::file_search::get_vector_store_file), delete(crate::handler::http::file_search::detach_file)],
    "/v1/vector_stores/{store_id}/search" => [post(crate::handler::http::file_search::search)],
});

api_route_group!(documented_retrieval_routes {
    "/v1/responses/{response_id}" => [
        get(crate::handler::http::responses::retrieve_response),
        delete(crate::handler::http::responses::delete_response),
    ],
    "/v1/responses/{response_id}/input_items" => [get(crate::handler::http::responses::list_input_items)],
});

// Axum needs `{*model}` to capture IDs containing `/`; OpenAPI describes the
// same parameter as `{model}`. Keep the handler's generated operation and only
// translate the router path at this boundary.
#[cfg(feature = "openapi")]
pub(crate) fn documented_model_wildcard_routes() -> OpenApiRouter<AppState> {
    let (schemas, mut paths, method_router) = routes!(crate::handler::http::models::retrieve_model);
    let item = paths
        .paths
        .remove("/v1/models/{model}")
        .expect("model route annotation must match OpenAPI path");
    paths.paths.insert("/v1/models/{*model}".to_owned(), item);
    let mut router = OpenApiRouter::new().routes((schemas, paths, method_router));
    let item = router
        .get_openapi_mut()
        .paths
        .paths
        .remove("/v1/models/{*model}")
        .expect("registered model wildcard route must have an operation");
    router
        .get_openapi_mut()
        .paths
        .paths
        .insert("/v1/models/{model}".to_owned(), item);
    router
}

#[cfg(not(feature = "openapi"))]
pub(crate) fn documented_model_wildcard_routes() -> Router<AppState> {
    Router::new().route(
        "/v1/models/{*model}",
        axum::routing::get(crate::handler::http::models::retrieve_model),
    )
}
