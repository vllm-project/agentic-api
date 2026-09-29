//! Authentication for local routes that cannot delegate credential validation upstream.

use axum::extract::{Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;
use subtle::ConstantTimeEq;

use super::{AuthErrorFormat, authentication_error};

pub(crate) async fn require_api_key(State(key): State<String>, request: Request, next: Next) -> Response {
    let Some(token) = bearer_token(request.headers()) else {
        return authentication_error(AuthErrorFormat::OpenAi, "missing_bearer_token", "missing bearer token");
    };
    if !bool::from(token.as_bytes().ct_eq(key.as_bytes())) {
        return authentication_error(AuthErrorFormat::OpenAi, "invalid_token", "invalid bearer token");
    }
    next.run(request).await
}

pub(super) fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .split_once(' ')
        .and_then(|(scheme, token)| {
            let token = token.trim();
            (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
        })
}
