use agentic_core::proxy::ProxyState;
use axum::Router;
use axum::routing::post;

use crate::handler::proxy_responses;

pub fn build_router(state: ProxyState) -> Router {
    Router::new()
        .route("/v1/responses", post(proxy_responses))
        .with_state(state)
}
