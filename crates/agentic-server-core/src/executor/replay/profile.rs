//! I/O-free compatibility checks. Passing them does not bypass availability gating.

pub(super) mod parameters;

use sha2::{Digest, Sha256};

use super::upstream_identity;
use crate::types::io::{InputItem, ReasoningStatus, ResponsesInput};
use crate::types::reasoning_profile::OpaqueReasoningProfile;
use crate::types::reasoning_replay::{
    ReasoningProvenance, ReasoningReplayError, ReasoningReplayIdentity, ReasoningReplayPolicy, ReasoningSource,
};
use crate::types::upstream_identity::UpstreamModelId;

/// Binds the configured candidate contract to the exact effective credential.
/// Only a fixed-size digest is retained; construction performs no I/O.
#[derive(Debug)]
pub(super) struct OpaqueReplayTarget {
    profile: OpaqueReasoningProfile,
    identity: ReasoningReplayIdentity,
}

impl OpaqueReplayTarget {
    pub(super) fn new(
        profile: OpaqueReasoningProfile,
        endpoint: &str,
        requested_model: &str,
        auth: Option<&str>,
    ) -> Result<Self, ReasoningReplayError> {
        profile.validate_target(endpoint, requested_model)?;
        let auth = auth
            .filter(|token| !token.trim().is_empty())
            .ok_or(ReasoningReplayError::MissingCredential)?;
        let observation = upstream_identity(
            ReasoningReplayPolicy::OpaqueResponses,
            endpoint,
            Some(auth),
            requested_model,
            Some(profile.model()),
        );
        // Deliberately incompatible with v1 observational fingerprints, even if
        // every old routing component matches. Bind the opaque wire contract too.
        let mut digest = Sha256::new();
        digest.update(b"agentic-api/reasoning-replay/profile-identity/v1\0");
        digest.update(Sha256::digest(profile.identity_domain()));
        digest.update(observation.digest());
        Ok(Self {
            profile,
            identity: ReasoningReplayIdentity::from_digest(digest.finalize().into()),
        })
    }

    /// Observe only a successfully ingested round with exact terminal evidence.
    pub(super) fn observed_identity(
        &self,
        reported_model: Option<&UpstreamModelId>,
    ) -> Result<ReasoningReplayIdentity, ReasoningReplayError> {
        if reported_model.map(UpstreamModelId::as_str) != Some(self.profile.model()) {
            return Err(ReasoningReplayError::ReportedModelMismatch);
        }
        Ok(self.identity)
    }

    /// Inspect canonical input before any projection can discard opaque state or
    /// replace a local compaction checkpoint. Never mutate, reorder, or clone it.
    pub(super) fn validate_input(&self, input: &ResponsesInput) -> Result<(), ReasoningReplayError> {
        let ResponsesInput::Items(items) = input else {
            return Ok(());
        };
        for item in items {
            match item {
                InputItem::Compaction(_) | InputItem::CompactionTrigger => {
                    return Err(ReasoningReplayError::UnsupportedCompaction);
                }
                InputItem::Reasoning(reasoning) => {
                    match reasoning.replay_provenance {
                        Some(ReasoningProvenance::V1 {
                            source: ReasoningSource::Upstream { policy, identity },
                        }) => {
                            if policy != ReasoningReplayPolicy::OpaqueResponses || identity != self.identity {
                                return Err(ReasoningReplayError::IncompatibleProvenance);
                            }
                        }
                        None
                        | Some(ReasoningProvenance::V1 {
                            source: ReasoningSource::ClientSubmitted {},
                        }) => return Err(ReasoningReplayError::UnknownProvenance),
                    }
                    if reasoning
                        .encrypted_content
                        .as_ref()
                        .is_none_or(|state| state.as_str().is_empty())
                    {
                        return Err(ReasoningReplayError::MissingOpaqueState);
                    }
                    if reasoning.status == Some(ReasoningStatus::InProgress) {
                        return Err(ReasoningReplayError::UnfinishedReasoning);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
