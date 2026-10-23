//! Client-carried multi-agent checkpoints. No request state is retained by the gateway.

use std::sync::OnceLock;

use base64::Engine;
use ring::rand::{SecureRandom, SystemRandom};

use super::{CheckpointLimits, ValidatedTreeCheckpoint, collaboration::TranscriptSealer};
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::{
    agent_tree::StoredTreeSnapshot,
    io::{AgentAttribution, CompactionItem, InOutItem, InputItem, OutputItem},
};
use crate::utils::{common::serialized_size_up_to, uuid7_str};

const DOMAIN: &[u8] = b"agentic-api/multi-agent/replay/v1";
const KEY_ENV: &str = "AGENTIC_MULTI_AGENT_REPLAY_KEY";

impl ValidatedTreeCheckpoint {
    pub(in crate::executor) fn replay_item(&self, max_bytes: usize) -> ExecutorResult<OutputItem> {
        if serialized_size_up_to(&self.snapshot, max_bytes)
            .map_err(ExecutorError::JsonError)?
            .is_none()
        {
            return Err(invalid("multi-agent replay state exceeds the retained-byte limit"));
        }
        let bytes = serde_json::to_vec(&self.snapshot).map_err(ExecutorError::JsonError)?;
        Ok(OutputItem::Compaction(CompactionItem {
            id: Some(uuid7_str("cmp_")),
            agent: Some(AgentAttribution {
                agent_name: "/root".into(),
            }),
            encrypted_content: replay_sealer()?.seal_bytes(&bytes, DOMAIN)?,
        }))
    }

    pub(in crate::executor) fn from_replay(input: &[InOutItem], max_bytes: usize) -> ExecutorResult<Option<Self>> {
        for (index, item) in input.iter().enumerate().rev() {
            let compaction = match item {
                InOutItem::Input(InputItem::Compaction(item)) | InOutItem::Output(OutputItem::Compaction(item))
                    if item.agent.is_some() || item.encrypted_content.starts_with("enc_") =>
                {
                    item
                }
                _ => continue,
            };
            // The response-boundary compaction contains the whole tree, including
            // pending client calls. Per-agent model summaries cannot restore it.
            if compaction
                .agent
                .as_ref()
                .is_some_and(|agent| agent.agent_name != "/root")
            {
                return Err(invalid("multi-agent replay requires the final root compaction item"));
            }
            let encoded = &compaction.encrypted_content;
            // Bound base64 decoding and JSON allocation before inspecting the authenticated snapshot.
            if encoded.len() > max_bytes.saturating_mul(4) / 3 + 128 {
                return Err(invalid("multi-agent replay state exceeds the retained-byte limit"));
            }
            let bytes = replay_sealer()?.open_bytes(encoded, DOMAIN)?;
            if bytes.len() > max_bytes {
                return Err(invalid("multi-agent replay state exceeds the retained-byte limit"));
            }
            let snapshot: StoredTreeSnapshot =
                serde_json::from_slice(&bytes).map_err(|_| invalid("invalid multi-agent replay state"))?;
            let mut checkpoint = Self::from_stored(snapshot, &CheckpointLimits::for_response(max_bytes))?;
            // A compaction supersedes its prefix. Keeping that prefix for display
            // or pruning it entirely must produce the same continuation.
            checkpoint.replayed_items = index + 1;
            return Ok(Some(checkpoint));
        }
        Ok(None)
    }
}

/// A deployment key survives requests, not request histories. Configure the same
/// key across replicas and restarts; the default is scoped to this process.
fn replay_sealer() -> ExecutorResult<TranscriptSealer> {
    static KEY: OnceLock<Result<[u8; 32], &'static str>> = OnceLock::new();
    let key = KEY
        .get_or_init(|| match std::env::var(KEY_ENV) {
            Ok(value) => base64::engine::general_purpose::STANDARD
                .decode(value)
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or("AGENTIC_MULTI_AGENT_REPLAY_KEY must be a base64-encoded 32-byte key"),
            Err(std::env::VarError::NotPresent) => {
                let mut key = [0; 32];
                SystemRandom::new()
                    .fill(&mut key)
                    .map_err(|_| "could not generate multi-agent replay key")?;
                Ok(key)
            }
            Err(std::env::VarError::NotUnicode(_)) => Err("AGENTIC_MULTI_AGENT_REPLAY_KEY must be valid base64"),
        })
        .as_ref()
        .map_err(|message| ExecutorError::StreamError((*message).into()))?;
    TranscriptSealer::with_key(key)
}

fn invalid(message: &str) -> ExecutorError {
    ExecutorError::InvalidRequest(message.into())
}
