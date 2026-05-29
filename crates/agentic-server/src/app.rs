use agentic_core::proxy::ProxyState;
use axum::Router;
use axum::routing::{get, post};
use http::HeaderValue;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use crate::handler::{health, proxy_responses, ready};

/// Server-level configuration read from environment variables.
pub struct ServerConfig {
    pub cors_allowed_origins: Vec<String>,
}

impl ServerConfig {
    /// Read `CORS_ALLOWED_ORIGINS` (comma-separated). Unset or empty = permissive.
    #[must_use]
    pub fn from_env() -> Self {
        let cors_allowed_origins = std::env::var("CORS_ALLOWED_ORIGINS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|o| !o.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Self { cors_allowed_origins }
    }

    fn cors_layer(&self) -> CorsLayer {
        let allow_origin = if self.cors_allowed_origins.is_empty() {
            AllowOrigin::any()
        } else {
            let origins: Vec<HeaderValue> = self
                .cors_allowed_origins
                .iter()
                .filter_map(|o| o.parse().ok())
                .collect();
            AllowOrigin::list(origins)
        };

        CorsLayer::new()
            .allow_origin(allow_origin)
            .allow_methods(Any)
            .allow_headers(Any)
    }
}

pub fn build_router(state: ProxyState, server_config: &ServerConfig) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/v1/responses", post(proxy_responses))
        .layer(server_config.cors_layer())
        .with_state(state)
}
