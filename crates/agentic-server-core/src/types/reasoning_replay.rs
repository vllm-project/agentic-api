//! Server-owned reasoning replay policy and versioned, non-wire provenance.
//!
//! Provenance records an observation, not permission to replay opaque state.
//! It is persisted separately from public items and cannot be supplied over HTTP/WS.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::reasoning_profile::{OpaqueReasoningProfile, OpaqueReplayRequestField};

/// Maximum serialized provenance per stored reasoning item, including JSON overhead.
pub const MAX_REASONING_PROVENANCE_BYTES: usize = 512;

/// Reasoning projection selected by the server, never by a request's model name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningReplayPolicy {
    /// Existing vLLM plaintext projection; opaque-only continuations are rejected.
    #[default]
    VllmPlaintext,
    /// Reserved for qualified opaque Responses replay. Not executable yet.
    OpaqueResponses,
}

impl ReasoningReplayPolicy {
    /// Check policy/profile consistency without authorizing execution.
    ///
    /// # Errors
    /// Opaque replay requires an explicit profile; vLLM must not have one.
    pub fn validate_profile(self, profile: Option<OpaqueReasoningProfile>) -> Result<(), ReasoningReplayError> {
        match (self, profile) {
            (Self::VllmPlaintext, None) | (Self::OpaqueResponses, Some(_)) => Ok(()),
            (Self::VllmPlaintext, Some(_)) => Err(ReasoningReplayError::UnexpectedProfile),
            (Self::OpaqueResponses, None) => Err(ReasoningReplayError::MissingProfile),
        }
    }

    /// Validate availability before storage, tool discovery, or upstream inference.
    ///
    /// # Errors
    /// Returns an error for an unqualified replay policy. Defining a policy or
    /// retaining provenance does not enable its execution.
    pub fn validate(self) -> Result<(), ReasoningReplayError> {
        match self {
            Self::VllmPlaintext => Ok(()),
            Self::OpaqueResponses => Err(ReasoningReplayError::OpaqueNotEnabled),
        }
    }
}

/// Replay failures deliberately contain neither credentials nor opaque state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ReasoningReplayError {
    #[error("opaque reasoning replay is not enabled; provider qualification is incomplete")]
    OpaqueNotEnabled,
    #[error("vllm_plaintext must not configure an opaque reasoning replay profile")]
    UnexpectedProfile,
    #[error("opaque_responses requires an explicit reasoning replay profile")]
    MissingProfile,
    #[error("configured Responses endpoint does not match the reasoning replay profile")]
    EndpointMismatch,
    #[error("model must match the exact snapshot pinned by the reasoning replay profile; aliases are unsupported")]
    ModelMismatch,
    #[error("opaque reasoning replay requires a nonempty effective upstream bearer credential")]
    MissingCredential,
    #[error("local compaction is unsupported by the opaque reasoning replay profile")]
    UnsupportedCompaction,
    #[error("request parameter `{0}` is unsupported by the opaque reasoning replay profile")]
    UnsupportedParameter(OpaqueReplayRequestField),
    #[error("opaque reasoning replay request contains an unsupported or duplicate wire field")]
    UnsupportedWireField,
    #[error("reasoning replay requires gateway-observed provenance; manual and legacy opaque state is unsupported")]
    UnknownProvenance,
    #[error("reasoning provenance is incompatible with the selected profile, endpoint, model, or credential")]
    IncompatibleProvenance,
    #[error("reasoning item has no nonempty opaque state supported by the selected replay profile")]
    MissingOpaqueState,
    #[error("an in-progress reasoning item cannot be replayed by the opaque profile")]
    UnfinishedReasoning,
    #[error("upstream must consistently report the exact model snapshot pinned by the reasoning replay profile")]
    ReportedModelMismatch,
}

/// Fixed-size fingerprint of the effective upstream routing and model identity.
///
/// The executor binds policy, endpoint, requested model, and optional model reported
/// consistently by explicit upstream terminal metadata. Opaque replay additionally
/// binds the effective bearer credential; plaintext replay does not.
/// This is not an authentication credential or a cross-provider compatibility claim.
/// No original identity components are retained here.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReasoningReplayIdentity([u8; 32]);

impl ReasoningReplayIdentity {
    pub(crate) fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    pub(crate) fn digest(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for ReasoningReplayIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReasoningReplayIdentity(<redacted>)")
    }
}

/// Where this particular reasoning item entered canonical history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReasoningSource {
    /// Manually supplied input or externally committed output, not gateway-observed.
    /// Successful inference must not upgrade this to `Upstream`.
    ClientSubmitted {},
    /// Observed through this gateway's inference path under the recorded policy.
    Upstream {
        policy: ReasoningReplayPolicy,
        identity: ReasoningReplayIdentity,
    },
}

/// Versioned per-item provenance stored outside the public Responses schema.
///
/// SQL NULL on legacy items means unknown provenance, not client-submitted or
/// provider-issued state. Unknown versions and fields must fail closed on load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "version", deny_unknown_fields)]
pub enum ReasoningProvenance {
    #[serde(rename = "1")]
    V1 { source: ReasoningSource },
}

impl ReasoningProvenance {
    #[must_use]
    pub const fn client_submitted() -> Self {
        Self::V1 {
            source: ReasoningSource::ClientSubmitted {},
        }
    }

    pub(crate) fn upstream(policy: ReasoningReplayPolicy, identity: ReasoningReplayIdentity) -> Self {
        Self::V1 {
            source: ReasoningSource::Upstream { policy, identity },
        }
    }
}

/// Reserve fixed inline provenance space even before the engine records its origin.
/// This charge also covers the optional discriminant for legacy/missing provenance.
pub const REASONING_PROVENANCE_RETAINED_BYTES: usize = std::mem::size_of::<Option<ReasoningProvenance>>();
