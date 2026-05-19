use agentic_core::proxy::ProxyState;
use axum::Router;
use axum::routing::post;
use tower_http::cors::CorsLayer;

use crate::handler::proxy_responses;

pub fn build_router(state: ProxyState) -> Router {
    Router::new()
        .route("/v1/responses", post(proxy_responses))
        .layer(CorsLayer::very_permissive())
        .with_state(state)
}
