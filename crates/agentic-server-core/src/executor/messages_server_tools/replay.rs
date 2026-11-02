//! Gateway-made server tool blocks turned back into the rounds the model saw.
//!
//! The client returns an assistant turn as one message: model content with each
//! gateway call shown as a `server_tool_use` block followed, after the round's
//! last block, by its result blocks. The upstream saw that turn as rounds — an
//! assistant message ending in `tool_use` blocks, then a user message of
//! `tool_result`s. Splitting the message at the end of each run of result
//! blocks restores those rounds; the ids and the projected `tool_result` texts
//! are the ones the live rounds used. Blocks the gateway did not make pass
//! through unchanged.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::sealing::MessagesResultKey;
use super::{ReplayFailure, replayed_tool_result, upstream_tool_use_id};
use crate::executor::{ExecutorError, ExecutorResult};
use crate::types::messages::{ContentBlock, GatewayToolResult, ServerToolResultBlock, ServerToolUseBlock};

/// A content block the gateway made.
enum GatewayBlock {
    Call,
    Result,
}

fn gateway_block(block: &Value) -> Option<GatewayBlock> {
    let made_here = |field: &str| {
        block
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|id| upstream_tool_use_id(id).is_some())
    };
    match block.get("type").and_then(Value::as_str) {
        Some("server_tool_use") if made_here("id") => Some(GatewayBlock::Call),
        Some("web_search_tool_result" | "web_fetch_tool_result") if made_here("tool_use_id") => {
            Some(GatewayBlock::Result)
        }
        _ => None,
    }
}

fn carries_gateway_blocks(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("assistant")
        && message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| content.iter().any(|block| gateway_block(block).is_some()))
}

/// An assistant message carrying gateway-made blocks, split into its other
/// fields and its content; any other message is handed back.
fn gateway_turn(message: Value) -> Result<(Map<String, Value>, Vec<Value>), Value> {
    if !carries_gateway_blocks(&message) {
        return Err(message);
    }
    match message {
        Value::Object(mut fields) => match fields.remove("content") {
            Some(Value::Array(content)) => Ok((fields, content)),
            content => {
                if let Some(content) = content {
                    fields.insert("content".to_owned(), content);
                }
                Err(Value::Object(fields))
            }
        },
        message => Err(message),
    }
}

fn invalid(message: String) -> ExecutorError {
    ExecutorError::InvalidRequest(message)
}

/// Replace the gateway-made `server_tool_use` and result blocks in a request's
/// `messages` with the `tool_use` / `tool_result` rounds the upstream saw when
/// the gateway ran those calls. Returns whether the request changed.
///
/// # Errors
/// Returns [`ExecutorError::InvalidRequest`] when a gateway-made block is
/// malformed, unpaired, or repeated, or when its sealed search content was
/// modified or was not issued by this gateway.
pub fn replay_server_tool_blocks(request: &mut Value, key: &MessagesResultKey) -> ExecutorResult<bool> {
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return Ok(false);
    };
    if !messages.iter().any(carries_gateway_blocks) {
        return Ok(false);
    }
    let mut replay = Replay {
        key,
        messages: Vec::with_capacity(messages.len() + 2),
        ends_with_results: false,
        seen: HashSet::new(),
    };
    for message in std::mem::take(messages) {
        match gateway_turn(message) {
            Ok((fields, content)) => replay.assistant_turn(fields, content)?,
            Err(message) => replay.push(message),
        }
    }
    *messages = replay.messages;
    Ok(true)
}

struct Replay<'a> {
    key: &'a MessagesResultKey,
    messages: Vec<Value>,
    /// Whether the last message is a user message of replayed results.
    ends_with_results: bool,
    /// Every gateway call id seen so far in the request.
    seen: HashSet<String>,
}

/// One assistant message being split into rounds.
struct Round {
    /// The message's fields other than `content`, kept on every round.
    fields: Map<String, Value>,
    content: Vec<Value>,
    results: Vec<GatewayToolResult>,
}

impl Replay<'_> {
    /// Add a message the gateway did not change. A user message right after
    /// replayed results joins them, after the results, so roles still alternate.
    fn push(&mut self, message: Value) {
        if self.ends_with_results
            && message.get("role").and_then(Value::as_str) == Some("user")
            && let Some(Value::Array(results)) = self.messages.last_mut().and_then(|last| last.get_mut("content"))
        {
            match message.get("content") {
                Some(Value::Array(blocks)) => results.extend(blocks.iter().cloned()),
                Some(Value::String(text)) => {
                    results.push(serde_json::json!({"type": "text", "text": text}));
                }
                _ => {}
            }
        } else {
            self.messages.push(message);
        }
        self.ends_with_results = false;
    }

    fn assistant_turn(&mut self, fields: Map<String, Value>, content: Vec<Value>) -> ExecutorResult<()> {
        let mut round = Round {
            fields,
            content: Vec::new(),
            results: Vec::new(),
        };
        let mut open_calls: HashMap<String, ServerToolUseBlock> = HashMap::new();
        for block in content {
            match gateway_block(&block) {
                Some(GatewayBlock::Call) => {
                    self.end_round(&mut round)?;
                    let call = self.call(block)?;
                    round.content.push(tool_use(&call)?);
                    open_calls.insert(call.id.clone(), call);
                }
                Some(GatewayBlock::Result) => {
                    let kind = block.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
                    let result: ServerToolResultBlock = serde_json::from_value(block)
                        .map_err(|error| invalid(format!("invalid gateway {kind} block: {error}")))?;
                    let id = result.tool_use_id();
                    let call = open_calls.remove(id).ok_or_else(|| {
                        invalid(format!(
                            "{} block for `{id}` does not follow its server_tool_use block in the same message",
                            result.block_type()
                        ))
                    })?;
                    if call.name != result.tool_name() {
                        return Err(invalid(format!(
                            "{} block for `{id}` answers a `{}` call",
                            result.block_type(),
                            call.name
                        )));
                    }
                    let upstream_id = upstream_tool_use_id(&call.id).unwrap_or_default();
                    let replayed =
                        replayed_tool_result(upstream_id, &call.input, &result, self.key).map_err(|failure| {
                            match failure {
                                ReplayFailure::Tampered => invalid(format!(
                                    "{} block for `{id}` was modified or was not issued by this gateway",
                                    result.block_type()
                                )),
                                ReplayFailure::InvalidInput => {
                                    invalid(format!("server_tool_use `{id}` no longer carries the search it ran"))
                                }
                            }
                        })?;
                    round.results.push(replayed);
                }
                None => {
                    self.end_round(&mut round)?;
                    round.content.push(block);
                }
            }
        }
        if let Some(call) = open_calls.values().next() {
            return Err(invalid(format!(
                "`{name}` tool use with id `{id}` was found without a corresponding `{name}_tool_result` block",
                name = call.name,
                id = call.id
            )));
        }
        self.end_round(&mut round)?;
        if !round.content.is_empty() {
            self.push_assistant(&round.fields, std::mem::take(&mut round.content));
        }
        Ok(())
    }

    fn call(&mut self, block: Value) -> ExecutorResult<ServerToolUseBlock> {
        let call: ServerToolUseBlock = serde_json::from_value(block)
            .map_err(|error| invalid(format!("invalid gateway server_tool_use block: {error}")))?;
        if !matches!(call.name.as_str(), "web_search" | "web_fetch") {
            return Err(invalid(format!(
                "server_tool_use `{}` names `{}`, which the gateway does not run",
                call.id, call.name
            )));
        }
        if !self.seen.insert(call.id.clone()) {
            return Err(invalid(format!(
                "server_tool_use id `{}` appears more than once",
                call.id
            )));
        }
        Ok(call)
    }

    /// Close the current round once its results are in: the assistant content
    /// so far, then a user message of its results.
    fn end_round(&mut self, round: &mut Round) -> ExecutorResult<()> {
        if round.results.is_empty() {
            return Ok(());
        }
        self.push_assistant(&round.fields, std::mem::take(&mut round.content));
        let results = serde_json::to_value(std::mem::take(&mut round.results)).map_err(ExecutorError::JsonError)?;
        let mut user = Map::new();
        user.insert("role".to_owned(), Value::String("user".to_owned()));
        user.insert("content".to_owned(), results);
        self.messages.push(Value::Object(user));
        self.ends_with_results = true;
        Ok(())
    }

    fn push_assistant(&mut self, fields: &Map<String, Value>, content: Vec<Value>) {
        let mut message = fields.clone();
        message.insert("content".to_owned(), Value::Array(content));
        self.messages.push(Value::Object(message));
        self.ends_with_results = false;
    }
}

/// The `tool_use` block the upstream produced for a gateway call.
fn tool_use(call: &ServerToolUseBlock) -> ExecutorResult<Value> {
    serde_json::to_value(ContentBlock::ToolUse {
        id: upstream_tool_use_id(&call.id).unwrap_or_default().to_owned(),
        name: call.name.clone(),
        input: call.input.clone(),
    })
    .map_err(ExecutorError::JsonError)
}
