use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use http::HeaderValue;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use agentic_core::executor::ExecutionContext;
use agentic_core::proxy::ProxyState;

use crate::handler::{conversations, health, models, ready, responses, responses_ws};

/// Server-level configuration read from environment variables.
pub struct ServerConfig {
    pub cors_allowed_origins: Vec<String>,
}

impl ServerConfig {
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

/// Shared application state injected into every handler.
///
/// Both states are always present:
/// - `proxy_state` handles `store=false` requests (direct passthrough to vLLM)
/// - `exec_ctx` handles `store=true` requests (stateful executor with DB)
#[derive(Clone)]
pub struct AppState {
    pub proxy_state: ProxyState,
    pub exec_ctx: Arc<ExecutionContext>,
    /// Shared cancellation signal used to drain long-lived handlers.
    pub shutdown_token: CancellationToken,
    /// vLLM base URL — used by the `/ready` health probe.
    pub llm_api_base: String,
    /// Server-configured API key; used as fallback when the request carries no
    /// `Authorization` header on the executor path.
    pub openai_api_key: Option<String>,
}

pub fn build_router(state: AppState, server_config: &ServerConfig) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/v1/conversations", post(conversations))
        .route("/v1/models", get(models))
        .route("/v1/responses", post(responses).get(responses_ws))
        .layer(server_config.cors_layer())
        .with_state(state)
}
