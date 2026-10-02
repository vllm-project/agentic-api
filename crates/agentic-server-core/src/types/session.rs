//! Tenant-scoped session identity for Messages requests.
//!
//! A client names its session with an opaque id. The gateway forwards a value
//! derived from that id and the owning tenant, so equal client ids from two
//! tenants never name the same upstream session, and a client cannot choose
//! another tenant's value. The derived id is a soft routing and retention hint;
//! it is not KV-cache block identity and does not prove residency.

use std::fmt::Write as _;

use ring::digest;

/// Mixed into every derivation so a later format change yields new ids.
const DERIVATION_TAG: &str = "agentic-api/session-id/v1";

/// The namespace that owns a client session id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTenant<'a> {
    /// No verified identity; the deployment's default tenant.
    Default,
    /// A verified OIDC principal.
    Principal { issuer: &'a str, subject: &'a str },
}

/// A tenant-scoped session id: 64 lowercase hex digits of a SHA-256 digest.
///
/// Each input field is length-prefixed, so no choice of client id can make two
/// different (tenant, id) pairs hash the same input.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ScopedSessionId(String);

impl ScopedSessionId {
    #[must_use]
    pub fn new(tenant: SessionTenant<'_>, client_session_id: &str) -> Self {
        let mut context = digest::Context::new(&digest::SHA256);
        let mut field = |value: &str| {
            context.update(&(value.len() as u64).to_be_bytes());
            context.update(value.as_bytes());
        };
        field(DERIVATION_TAG);
        match tenant {
            SessionTenant::Default => field("default"),
            SessionTenant::Principal { issuer, subject } => {
                field("oidc");
                field(issuer);
                field(subject);
            }
        }
        field(client_session_id);
        let digest = context.finish();
        let mut hex = String::with_capacity(digest.as_ref().len() * 2);
        for byte in digest.as_ref() {
            let _ = write!(hex, "{byte:02x}");
        }
        Self(hex)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{ScopedSessionId, SessionTenant};

    const CLIENT_ID: &str = "3f1c9a52-8d0e-4b6a-9c1e-2f7d4e5a6b70";

    fn principal<'a>(issuer: &'a str, subject: &'a str) -> SessionTenant<'a> {
        SessionTenant::Principal { issuer, subject }
    }

    #[test]
    fn derivation_matches_known_answers() {
        // Independent reference: SHA-256 over each field as a big-endian u64
        // byte length followed by its UTF-8 bytes.
        assert_eq!(
            ScopedSessionId::new(SessionTenant::Default, CLIENT_ID).as_str(),
            "433772640b8f1bb27b06c00492e81b62353217e16a8473637e20679166e67e34"
        );
        assert_eq!(
            ScopedSessionId::new(principal("https://issuer.example", "user-a"), CLIENT_ID).as_str(),
            "7fb6895e930bef3992b0a757f90d837b8d98cdb9d4e7c588d1523a6051e6ad60"
        );
    }

    #[test]
    fn same_tenant_and_id_give_the_same_value() {
        let tenant = principal("https://issuer.example", "user-a");
        assert_eq!(
            ScopedSessionId::new(tenant, CLIENT_ID),
            ScopedSessionId::new(tenant, CLIENT_ID)
        );
    }

    #[test]
    fn tenants_never_share_a_session() {
        let ids = [
            ScopedSessionId::new(SessionTenant::Default, CLIENT_ID),
            ScopedSessionId::new(principal("https://issuer.example", "user-a"), CLIENT_ID),
            ScopedSessionId::new(principal("https://issuer.example", "user-b"), CLIENT_ID),
            ScopedSessionId::new(principal("https://other.example", "user-a"), CLIENT_ID),
        ];
        for (i, left) in ids.iter().enumerate() {
            for right in &ids[i + 1..] {
                assert_ne!(left, right);
            }
        }
    }

    #[test]
    fn field_boundaries_are_unambiguous() {
        assert_ne!(
            ScopedSessionId::new(principal("a", "bc"), "d"),
            ScopedSessionId::new(principal("ab", "c"), "d")
        );
        assert_ne!(
            ScopedSessionId::new(principal("a", "b"), "cd"),
            ScopedSessionId::new(principal("a", "bc"), "d")
        );
    }

    #[test]
    fn value_is_fixed_length_lowercase_hex() {
        let long = "x".repeat(8192);
        for id in [
            ScopedSessionId::new(SessionTenant::Default, ""),
            ScopedSessionId::new(SessionTenant::Default, &long),
        ] {
            assert_eq!(id.as_str().len(), 64);
            assert!(id.as_str().bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        }
    }
}
