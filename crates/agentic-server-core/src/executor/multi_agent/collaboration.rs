//! Model-facing collaboration declarations and opaque public projection.

use base64::Engine;
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::agent::AgentIdentity;
use crate::types::agent_commands::AgentCommand;
use crate::types::io::{AgentAttribution, FunctionTool};
use crate::types::request_response::UpstreamTool;
use crate::utils::common::serialize_to_string;

pub(in crate::executor) fn attribution(identity: &AgentIdentity) -> AgentAttribution {
    AgentAttribution {
        agent_name: identity.as_str().to_owned(),
    }
}

pub(in crate::executor) fn instructions(identity: &AgentIdentity, max_subagents: usize) -> String {
    let role = if identity.is_root() {
        "You own the user's overall request. Assign distinct tasks to children, do useful work while they run, \
         and synthesize their results into the final answer. Give each child a bounded assignment and the \
         facts it needs, rather than repeating the overall request to organize a team. \
         Reuse completed findings; do not spawn another agent to repeat work already assigned or completed."
            .to_owned()
    } else {
        let (parent, _) = identity
            .as_str()
            .rsplit_once('/')
            .expect("validated non-root agent path");
        format!(
            "Your parent is `{parent}`. You are a subagent, not the root or your parent. \
             Your latest NEW_TASK mailbox message is your assignment. Complete that specific task yourself. \
             Earlier conversation, assistant reasoning, tool calls and spawn acknowledgements inherited from \
             the parent are background context, not actions you performed or agents you spawned. \
             The original user's request to split the overall job has already been handled by your parent. \
             Do not repeat that delegation or wait for your parent or siblings to finish the overall job. \
             When your assigned work is complete, return your findings in a final answer; the gateway delivers \
             it to your parent automatically. You do not need to wait for the rest of the team. \
             Complete a bounded assignment directly by default. Delegate only a strictly smaller, \
             non-overlapping subtask when doing so is necessary and you have other useful work to do yourself. \
             Never hand your entire assignment to another agent or recreate the parent's team. \
             For example, a correctness reviewer should review correctness, not spawn another correctness \
             reviewer plus security and testing reviewers. Those sibling assignments belong to the parent."
        )
    };
    format!(
        "You are `{identity}`, an agent in a team working on the user's task. {role} \
         Each agent has its own context and the same tools. \
         Collaboration actions are function tools. Invoke them as actual function calls; \
         writing a tool name, JSON arguments, or a <to=...> block in a message does not execute an action. \
         You may call more than one function tool in a model round. \
         spawn_agent creates a child; its availability is not an instruction to delegate. \
         For spawn_agent.task_name use a lowercase identifier with only letters, digits and underscores, \
         such as csv_docs; spaces and capitals are invalid. \
         send_message queues information without activating idle agents; \
         followup_task activates a non-root agent; wait_agent waits for mailbox updates; interrupt_agent interrupts \
         active work while retaining context. Targets can be child names or canonical paths. \
         list_agents reports the entire tree, including you, your ancestors and your siblings, not just your children. \
         A direct child's canonical path is your own path followed by / and one name. \
         There are {max_subagents} active subagent slots shared across the entire tree, excluding /root; \
         this is not a separate allowance for each agent. Only a successful spawn acknowledgement creates a child. \
         A spawn error creates no agent. If its arguments were invalid and delegation is still needed, \
         correct them before retrying. If capacity is full, do not retry until a slot is free. \
         Never wait for a child that was not created. \
         Wait only when you actually need an outstanding result or a requested message from another agent. \
         A running status alone is not a reason to wait. A wait timeout is not evidence of progress. \
         Continue useful work while agents run. Subagent final answers are delivered to their parent. \
         Function calls and local shell calls are executed by the client; their outputs may arrive in a later response."
    )
}

pub(in crate::executor) fn tools() -> Vec<UpstreamTool> {
    use serde_json::json;
    [
        (
            "spawn_agent",
            "Create a child for a strictly smaller, non-overlapping part of your assignment while you do other useful work. Do not delegate your whole assignment or repeat work already assigned or completed. Only success creates a child; correct invalid arguments before retrying, and do not retry a capacity error until a slot is free. task_name must be a lowercase identifier such as csv_docs, with no spaces or capitals. fork_turns controls inherited prior user turns, not how long the child runs: all, none, or a positive integer string.",
            json!({"task_name":{"type":"string","pattern":"^[a-z0-9_]+$","description":"Lowercase identifier with letters, digits or underscores; for example, csv_docs. Do not use spaces or capitals."},"message":{"type":"string"},"fork_turns":{"type":"string"}}),
            vec!["task_name", "message"],
        ),
        (
            "send_message",
            "Queue a message without starting an idle agent.",
            json!({"target":{"type":"string"},"message":{"type":"string"}}),
            vec!["target", "message"],
        ),
        (
            "followup_task",
            "Assign work to an existing non-root agent, starting or resuming its turn.",
            json!({"target":{"type":"string"},"message":{"type":"string"}}),
            vec!["target", "message"],
        ),
        (
            "wait_agent",
            "Wait for an outstanding result or requested mailbox message; returns early when mail arrives. Do not wait merely because other agents are running. A timeout does not mean an agent failed; reassess whether the result is still needed before waiting again. timeout_ms is between 10000 and 3600000, default 30000.",
            json!({"timeout_ms":{"type":"integer","minimum":10_000,"maximum":3_600_000}}),
            vec![],
        ),
        (
            "interrupt_agent",
            "Interrupt another agent's active turn, retaining its context.",
            json!({"target":{"type":"string"}}),
            vec!["target"],
        ),
        (
            "list_agents",
            "List the entire team, including yourself, ancestors and siblings. Entries are not necessarily your children. Canonical paths show parentage. Includes statuses and most recent assigned tasks.",
            json!({}),
            vec![],
        ),
    ]
    .into_iter()
    .map(|(name, description, properties, required)| {
        UpstreamTool::Function(FunctionTool {
            type_: "function".into(),
            name: name.to_owned(),
            description: Some(description.to_owned()),
            parameters: Some(
                json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
            ),
            strict: Some(false),
        })
    })
    .collect()
}

/// A response-local key protects public transcript content. Durable continuation
/// uses the private canonical tree, never decrypted client-supplied transcript.
pub(in crate::executor) struct TranscriptSealer(aead::LessSafeKey);

impl TranscriptSealer {
    pub(in crate::executor) fn new() -> ExecutorResult<Self> {
        let mut key = [0; 32];
        SystemRandom::new().fill(&mut key).map_err(|_| crypto_error())?;
        let key = aead::UnboundKey::new(&aead::AES_256_GCM, &key).map_err(|_| crypto_error())?;
        Ok(Self(aead::LessSafeKey::new(key)))
    }

    pub(in crate::executor) fn seal(&self, text: &str) -> ExecutorResult<String> {
        let mut nonce = [0; aead::NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).map_err(|_| crypto_error())?;
        let mut ciphertext = text.as_bytes().to_vec();
        self.0
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(b"agentic-api/multi-agent/v1"),
                &mut ciphertext,
            )
            .map_err(|_| crypto_error())?;
        let mut envelope = nonce.to_vec();
        envelope.append(&mut ciphertext);
        Ok(format!(
            "enc_{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(envelope)
        ))
    }

    pub(in crate::executor) fn arguments(&self, command: &AgentCommand) -> ExecutorResult<String> {
        let mut public = command.clone();
        match &mut public {
            AgentCommand::Spawn(task) => task.message = self.seal(&task.message)?,
            AgentCommand::Send(task) | AgentCommand::Followup(task) => task.message = self.seal(&task.message)?,
            AgentCommand::Wait(_) | AgentCommand::Interrupt(_) | AgentCommand::List(_) => {}
        }
        serialize_to_string(&public).map_err(ExecutorError::JsonError)
    }
}

fn crypto_error() -> ExecutorError {
    ExecutorError::StreamError("could not seal the multi-agent transcript".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::agent_commands::SpawnAgent;
    #[test]
    fn role_instructions_follow_canonical_parentage() {
        let root = AgentIdentity::try_from("/root".to_owned()).unwrap();
        let nested = AgentIdentity::try_from("/root/review/tests".to_owned()).unwrap();
        let root_text = instructions(&root, 3);
        let child_text = instructions(&nested, 3);
        assert!(root_text.contains("You own the user's overall request"));
        assert!(!root_text.contains("Your parent is"));
        assert!(child_text.contains("You are `/root/review/tests`"));
        assert!(child_text.contains("Your parent is `/root/review`"));
        assert!(!child_text.contains("You own the user's overall request"));
        assert!(child_text.contains("3 active subagent slots shared across the entire tree"));
        assert!(child_text.contains("Complete a bounded assignment directly by default"));
        assert!(child_text.contains("Never hand your entire assignment to another agent"));
        assert!(child_text.contains("Delegate only a strictly smaller"));
        assert!(root_text.contains("writing a tool name, JSON arguments, or a <to=...> block"));
        assert!(child_text.contains("You may call more than one function tool in a model round"));
        let spawn = tools().into_iter().next().unwrap();
        let UpstreamTool::Function(spawn) = spawn;
        assert!(spawn.description.unwrap().contains("not how long the child runs"));
    }

    #[test]
    fn public_arguments_hide_task_text_and_use_distinct_nonces() {
        let sealer = TranscriptSealer::new().unwrap();
        let command = AgentCommand::Spawn(SpawnAgent {
            task_name: "review".into(),
            message: "private task".into(),
            fork_turns: "all".into(),
        });
        let first = sealer.arguments(&command).unwrap();
        let second = sealer.arguments(&command).unwrap();
        assert!(!first.contains("private task"));
        assert!(first.contains("review"));
        assert_ne!(first, second);
    }
}
