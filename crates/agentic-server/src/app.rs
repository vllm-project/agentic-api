use axum::Router;
use axum::routing::{get, post};

use crate::handler::{health, proxy_responses, ready};
use crate::proxy::ProxyState;

pub fn build_router(state: ProxyState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/v1/responses", post(proxy_responses))
        .with_state(state)
}
