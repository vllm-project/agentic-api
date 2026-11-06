//! One tenant-scoped session id per Messages request.
//!
//! Claude Code names its session with `x-claude-code-session-id`; other clients
//! may send the provider-neutral `X-Session-ID`. Equal values name one session,
//! different values are rejected before any upstream call, and the forwarded
//! `X-Session-ID` always carries the gateway-derived value, which vLLM reads as
//! the request's typed `session_id`. `x-claude-code-session-id` and the body are
//! forwarded unchanged.

use axum::http::request::Parts;
use axum::response::Response;
use http::{Extensions, HeaderMap, HeaderValue, StatusCode};

use agentic_core::proxy::{ProxyAuth, error_response_for_auth};
use agentic_core::types::session::{ScopedSessionId, SessionTenant};

use super::super::common::convert_response;
use crate::auth::AuthenticatedPrincipal;

const CLAUDE_CODE_SESSION_ID: &str = "x-claude-code-session-id";
const SESSION_ID: &str = "x-session-id";
/// Claude Code sends a 36-byte UUID; the bound only rejects unbounded input.
const MAX_SESSION_ID_LEN: usize = 256;

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
enum SessionIdError {
    #[error("x-claude-code-session-id and X-Session-ID must not name different sessions")]
    Conflict,
    #[error("session id headers must contain only visible ASCII characters")]
    NotVisibleAscii,
    #[error("session id headers must be at most {} bytes", MAX_SESSION_ID_LEN)]
    TooLong,
}

/// Read the client's session id from both headers, including repeated lines.
/// Surrounding whitespace is ignored and empty values count as absent; every
/// other value must be identical.
fn client_session_id(headers: &HeaderMap) -> Result<Option<&str>, SessionIdError> {
    let mut session_id = None;
    for name in [CLAUDE_CODE_SESSION_ID, SESSION_ID] {
        for value in headers.get_all(name) {
            let value = value
                .to_str()
                .map_err(|_| SessionIdError::NotVisibleAscii)?
                .trim_ascii();
            if value.is_empty() {
                continue;
            }
            if value.len() > MAX_SESSION_ID_LEN {
                return Err(SessionIdError::TooLong);
            }
            match session_id {
                None => session_id = Some(value),
                Some(seen) if seen == value => {}
                Some(_) => return Err(SessionIdError::Conflict),
            }
        }
    }
    Ok(session_id)
}

/// Until tenants are modeled (#107), a verified OIDC principal owns its
/// sessions; everything else shares the default tenant. A client header never
/// chooses the tenant.
fn session_tenant(extensions: &Extensions) -> SessionTenant<'_> {
    extensions
        .get::<AuthenticatedPrincipal>()
        .map_or(SessionTenant::Default, |principal| SessionTenant::Principal {
            issuer: principal.issuer(),
            subject: principal.subject(),
        })
}

/// Set the forwarded `X-Session-ID` to the tenant-scoped session id, or remove it
/// when the client named no session.
///
/// Returns an Anthropic `invalid_request_error` response for conflicting,
/// non-ASCII, or oversized values.
#[allow(clippy::result_large_err)]
pub(super) fn scope_session_id(parts: &mut Parts) -> Result<(), Response> {
    let scoped = client_session_id(&parts.headers)
        .map_err(|error| {
            convert_response(error_response_for_auth(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                &error.to_string(),
                ProxyAuth::Anthropic,
            ))
        })?
        .map(|session_id| ScopedSessionId::new(session_tenant(&parts.extensions), session_id));
    parts.headers.remove(SESSION_ID);
    if let Some(scoped) = scoped {
        // Invariant: a lowercase hex digest is always a valid header value.
        let value = HeaderValue::from_str(scoped.as_str()).expect("hex digest is a valid header value");
        parts.headers.insert(SESSION_ID, value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use http::{HeaderMap, HeaderValue};

    use super::{MAX_SESSION_ID_LEN, SessionIdError, client_session_id};

    type Pairs<'a> = &'a [(&'a str, &'a [u8])];

    fn headers(pairs: Pairs<'_>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_bytes(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn either_header_or_matching_values_name_one_session() {
        let cases: [Pairs<'_>; 6] = [
            &[("x-claude-code-session-id", b"s1")],
            &[("X-Session-ID", b"s1")],
            &[("x-claude-code-session-id", b"s1"), ("X-Session-Id", b"s1")],
            &[("x-session-id", b"s1"), ("x-session-id", b"s1")],
            &[("x-claude-code-session-id", b""), ("x-session-id", b"s1")],
            &[("x-claude-code-session-id", b"s1"), ("x-session-id", b"  ")],
        ];
        for case in cases {
            assert_eq!(client_session_id(&headers(case)), Ok(Some("s1")), "{case:?}");
        }
    }

    #[test]
    fn missing_or_empty_values_leave_the_session_unset() {
        let cases: [Pairs<'_>; 3] = [
            &[("x-claude-code-agent-id", b"agent-1")],
            &[("x-claude-code-session-id", b"")],
            &[("x-claude-code-session-id", b""), ("x-session-id", b"")],
        ];
        for case in cases {
            assert_eq!(client_session_id(&headers(case)), Ok(None), "{case:?}");
        }
    }

    #[test]
    fn different_values_conflict() {
        let cases: [Pairs<'_>; 3] = [
            &[("x-claude-code-session-id", b"s1"), ("x-session-id", b"s2")],
            &[("x-session-id", b"s1"), ("x-session-id", b"s2")],
            &[("x-claude-code-session-id", b"s1"), ("x-session-id", b"S1")],
        ];
        for case in cases {
            assert_eq!(
                client_session_id(&headers(case)),
                Err(SessionIdError::Conflict),
                "{case:?}"
            );
        }
    }

    #[test]
    fn non_ascii_values_are_rejected() {
        let case: Pairs<'_> = &[("x-session-id", "s\u{e9}".as_bytes())];
        assert_eq!(client_session_id(&headers(case)), Err(SessionIdError::NotVisibleAscii));
    }

    #[test]
    fn values_are_bounded() {
        let longest = "s".repeat(MAX_SESSION_ID_LEN);
        let case: Pairs<'_> = &[("x-session-id", longest.as_bytes())];
        assert_eq!(client_session_id(&headers(case)), Ok(Some(longest.as_str())));
        let too_long = "s".repeat(MAX_SESSION_ID_LEN + 1);
        let case: Pairs<'_> = &[("x-claude-code-session-id", too_long.as_bytes())];
        assert_eq!(client_session_id(&headers(case)), Err(SessionIdError::TooLong));
    }
}
