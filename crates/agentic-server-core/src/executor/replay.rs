//! Server-owned provenance at the inference/continuation boundary.
//!
//! No parsing, output-item assembly, delivery, or opaque replay happens here.

mod profile;

use sha2::{Digest, Sha256};

use super::error::ExecutorResult;
use super::request::{ExecutionContext, RequestContext};
use crate::types::io::{InputItem, OutputItem, ResponsesInput};
use crate::types::reasoning_replay::{
    ReasoningProvenance, ReasoningReplayError, ReasoningReplayIdentity, ReasoningReplayPolicy,
};
use crate::types::upstream_identity::UpstreamModelId;
use crate::types::{RequestPayload, ResponsePayload};
use profile::OpaqueReplayTarget;

fn validate_availability(exec_ctx: &ExecutionContext) -> ExecutorResult<()> {
    // Only the core unit-test binary can install this loopback transport. Profile,
    // routing, credential and provenance checks still run at their normal boundaries.
    #[cfg(test)]
    if exec_ctx
        .opaque_replay_fixture
        .as_ref()
        .is_some_and(super::inference::transport::ResponsesTransport::is_replay_fixture)
    {
        return Ok(());
    }
    exec_ctx.responses_config.validate_reasoning_replay()?;
    Ok(())
}

/// Reject profile/routing mistakes before loading history. No model-name heuristics.
pub(super) fn validate_rehydration_request(
    exec_ctx: &ExecutionContext,
    request: &RequestPayload,
) -> ExecutorResult<()> {
    validate_profile_request(exec_ctx, request)?;
    validate_availability(exec_ctx)
}

fn validate_profile_request(exec_ctx: &ExecutionContext, request: &RequestPayload) -> ExecutorResult<()> {
    let config = &exec_ctx.responses_config;
    config
        .reasoning_replay_policy
        .validate_profile(config.reasoning_replay_profile)?;
    if let Some(profile) = config.reasoning_replay_profile {
        profile.validate_target(&exec_ctx.responses_url(), &request.model)?;
        if request
            .context_management
            .as_ref()
            .is_some_and(|entries| !entries.is_empty())
            || request.input.contains_compaction()
            || request.input.has_compaction_trigger()
        {
            return Err(ReasoningReplayError::UnsupportedCompaction.into());
        }
        profile::parameters::validate(profile, request)?;
    }
    Ok(())
}

/// Recheck resolved canonical history before tools and before every inference round.
/// The final availability gate is intentional: compatible is not yet qualified.
pub(super) fn preflight_inference(
    exec_ctx: &ExecutionContext,
    request: &RequestPayload,
    auth: Option<&str>,
) -> ExecutorResult<()> {
    validate_profile_request(exec_ctx, request)?;
    if let Some(profile) = exec_ctx.responses_config.reasoning_replay_profile {
        OpaqueReplayTarget::new(profile, &exec_ctx.responses_url(), &request.model, auth)?
            .validate_input(&request.input)?;
    }
    validate_availability(exec_ctx)
}

pub(super) fn validate_initial_input(
    exec_ctx: &ExecutionContext,
    request: &RequestPayload,
    auth: Option<&str>,
) -> ExecutorResult<()> {
    preflight_inference(exec_ctx, request, auth)?;
    if exec_ctx.responses_config.reasoning_replay_policy == ReasoningReplayPolicy::VllmPlaintext
        && !request.input.has_compaction_trigger()
    {
        super::rehydrate::validate_reasoning_for_vllm(&request.input)?;
    }
    Ok(())
}

/// Only the default vLLM adapter mutates its initial model-context copy.
pub(super) fn prepare_initial_reasoning(
    input: &mut ResponsesInput,
    policy: ReasoningReplayPolicy,
    round: usize,
    compacted: bool,
) -> ExecutorResult<()> {
    if policy == ReasoningReplayPolicy::VllmPlaintext && round == 0 && !compacted {
        return super::rehydrate::prepare_reasoning_for_vllm(input);
    }
    Ok(())
}

/// Bind exact routing inputs and both model identities without retaining secrets.
/// Nested, fixed-width hashes make field boundaries unambiguous without allocations.
/// Only opaque replay binds the credential; plaintext replay survives key rotation.
pub(super) fn upstream_identity(
    policy: ReasoningReplayPolicy,
    endpoint: &str,
    auth: Option<&str>,
    requested_model: &str,
    reported_model: Option<&str>,
) -> ReasoningReplayIdentity {
    let mut digest = Sha256::new();
    digest.update(b"agentic-api/reasoning-replay/identity/v1\0");
    digest.update([match policy {
        ReasoningReplayPolicy::VllmPlaintext => 0,
        ReasoningReplayPolicy::OpaqueResponses => 1,
    }]);
    digest.update(Sha256::digest(endpoint.as_bytes()));
    if policy == ReasoningReplayPolicy::OpaqueResponses {
        digest.update([u8::from(auth.is_some())]);
        digest.update(Sha256::digest(auth.unwrap_or_default().as_bytes()));
    }
    digest.update(Sha256::digest(requested_model.as_bytes()));
    digest.update([u8::from(reported_model.is_some())]);
    digest.update(Sha256::digest(reported_model.unwrap_or_default().as_bytes()));
    ReasoningReplayIdentity::from_digest(digest.finalize().into())
}

/// Ingestion has finished; annotate only items observed from this upstream round.
pub(super) fn record_round_provenance(
    payload: &mut ResponsePayload,
    reported_model: Option<&UpstreamModelId>,
    exec_ctx: &ExecutionContext,
    request: &RequestContext,
    auth: Option<&str>,
) -> ExecutorResult<()> {
    if exec_ctx.responses_config.reasoning_replay_profile.is_none()
        && !payload
            .output
            .iter()
            .any(|item| matches!(item, OutputItem::Reasoning(_)))
    {
        return Ok(());
    }
    let policy = exec_ctx.responses_config.reasoning_replay_policy;
    policy.validate_profile(exec_ctx.responses_config.reasoning_replay_profile)?;
    let identity = match exec_ctx.responses_config.reasoning_replay_profile {
        Some(profile) => OpaqueReplayTarget::new(
            profile,
            &exec_ctx.responses_url(),
            &request.enriched_request.model,
            auth,
        )?
        .observed_identity(reported_model)?,
        None => upstream_identity(
            policy,
            &exec_ctx.responses_url(),
            auth,
            &request.enriched_request.model,
            reported_model.map(UpstreamModelId::as_str),
        ),
    };
    record_upstream_provenance(&mut payload.output, policy, identity);
    Ok(())
}

/// Apply the round's server-owned observation after successful ingestion.
pub(super) fn record_upstream_provenance(
    output: &mut [OutputItem],
    policy: ReasoningReplayPolicy,
    identity: ReasoningReplayIdentity,
) {
    for item in output {
        if let OutputItem::Reasoning(reasoning) = item {
            reasoning.replay_provenance = Some(ReasoningProvenance::upstream(policy, identity));
        }
    }
}

/// Incoming public items never inherit an internal provenance claim.
pub(super) fn mark_client_input(input: &mut ResponsesInput) {
    if let ResponsesInput::Items(items) = input {
        mark_client_items(items);
    }
}

/// Canonical new inputs can also return from a serialized split-execution context.
pub(super) fn mark_client_items(items: &mut [InputItem]) {
    for item in items {
        if let InputItem::Reasoning(reasoning) = item {
            reasoning.replay_provenance = Some(ReasoningProvenance::client_submitted());
        }
    }
}

/// Split-execution output was not observed by this gateway's inference transport.
pub(super) fn mark_external_output(output: &mut [OutputItem]) {
    for item in output {
        if let OutputItem::Reasoning(reasoning) = item {
            reasoning.replay_provenance = Some(ReasoningProvenance::client_submitted());
        }
    }
}

#[cfg(test)]
mod tests;
