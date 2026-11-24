use std::sync::Arc;

use agentic_core::config::Config;
use agentic_core::executor::inference::direct::DirectInferenceBackend;
use agentic_core::proxy::error_response;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use http::StatusCode;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::{GatewayOptions, ServerError, build_state, drain_gateway, serve_gateway};
use crate::auth::OidcAuthenticator;
use crate::handler::convert_response;
use crate::model_capabilities::ServedModel;

/// Gateway-facing metadata and readiness from the supervised native runtime.
pub struct DirectInferenceStatus {
    pub model: ServedModel,
    readiness: watch::Receiver<bool>,
}

impl DirectInferenceStatus {
    #[must_use]
    pub fn new(model: ServedModel, readiness: watch::Receiver<bool>) -> Self {
        Self { model, readiness }
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.readiness.has_changed().is_ok() && *self.readiness.borrow()
    }
}

/// Serve the shared gateway with an already-connected, supervised native engine.
/// The runtime owner cancels `shutdown` on a signal or engine exit, then awaits this future.
///
/// # Errors
/// Returns authentication, storage, or listener failures.
pub async fn run_direct(
    config: Config,
    gateway: GatewayOptions<'_>,
    backend: Arc<dyn DirectInferenceBackend>,
    status: DirectInferenceStatus,
    shutdown: CancellationToken,
) -> Result<(), ServerError> {
    let startup = async {
        let authenticator = match gateway.oidc {
            Some(oidc) => Some(OidcAuthenticator::discover(oidc).await?),
            None => None,
        };
        let mut state = build_state(&config, shutdown.clone(), gateway.max_request_body_size, Some(backend)).await?;
        state.direct_status = Some(Arc::new(status));
        Ok::<_, ServerError>((state, authenticator))
    };
    let (state, authenticator) = tokio::select! {
        biased;
        () = shutdown.cancelled() => return Ok(()),
        result = startup => result?,
    };
    let serving = serve_gateway(state, gateway.host, gateway.port, authenticator, gateway.rate_limit);
    tokio::pin!(serving);
    tokio::select! {
        result = &mut serving => result,
        () = shutdown.cancelled() => drain_gateway(serving.as_mut()).await,
    }
}

pub(crate) async fn unsupported_endpoint(_request: Request, _next: Next) -> Response {
    let error = error_response(
        StatusCode::NOT_IMPLEMENTED,
        "unsupported_endpoint",
        "direct inference currently supports the Responses API; configure an HTTP upstream for this endpoint",
    );
    // Use the same HTTP error envelope as the proxy handlers.
    convert_response(error)
}
