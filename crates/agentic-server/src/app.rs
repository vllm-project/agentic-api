use agentic_core::proxy::ProxyState;
use axum::Router;
use axum::routing::{get, post};
use tower_http::cors::{Any, CorsLayer};

use crate::handler::{health, proxy_responses, ready};

pub fn build_router(state: ProxyState) -> Router {
    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any);

    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/v1/responses", post(proxy_responses))
        .layer(cors)
        .with_state(state)
}
