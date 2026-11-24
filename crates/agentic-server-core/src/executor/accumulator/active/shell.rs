//! Streaming shell-command order, buffers, and retained-byte accounting.

use super::{Budget, invalid};
use crate::events::EventPayload;
use crate::events::types::ShellCommandUpdate;
use crate::executor::error::ExecutorResult;
use crate::executor::response_budget::{RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedAccount, RetainedSize};
use crate::types::io::{ApplyDone, ShellCall};

#[derive(Clone)]
pub(in crate::executor::accumulator) struct ShellCallState {
    pub(in crate::executor::accumulator) item: ShellCall,
    /// Per-command completion flags once the first command event arrives.
    command_stream: Option<Vec<bool>>,
    /// Text of the command currently streaming; moved into the item on done.
    pub(super) command: String,
}

impl ShellCallState {
    pub(super) fn new(item: ShellCall) -> Self {
        Self {
            item,
            command_stream: None,
            command: String::new(),
        }
    }

    pub(in crate::executor::accumulator) fn has_unfinished_commands(&self) -> bool {
        self.command_stream
            .as_deref()
            .is_some_and(|done| done.iter().any(|complete| !complete))
    }

    pub(in crate::executor::accumulator) fn tracks_commands(&self) -> bool {
        self.command_stream.is_some()
    }

    pub(super) fn apply(
        &mut self,
        payload: &EventPayload,
        account: &mut RetainedAccount,
        budget: Budget<'_>,
    ) -> ExecutorResult<()> {
        let EventPayload::ShellCallCommand {
            command_index, update, ..
        } = payload
        else {
            return Ok(());
        };
        let index = *command_index as usize;
        let done = self.command_stream.as_deref().unwrap_or_default();
        match update {
            ShellCommandUpdate::Added(command) => {
                if index != done.len() || self.item.action.commands.len() != done.len() || done.last() == Some(&false) {
                    return Err(invalid("shell command added out of order"));
                }
                account.charge(budget, RETAINED_CONTAINER_OVERHEAD_BYTES + command.len())?;
                self.item.action.commands.push(String::new());
                self.command.clone_from(command);
                self.command_stream.get_or_insert_with(Vec::new).push(false);
                Ok(())
            }
            ShellCommandUpdate::Delta(delta) => {
                if done.get(index) != Some(&false) {
                    return Err(invalid("shell command delta has no active command"));
                }
                account.charge(budget, delta.len())?;
                self.command.push_str(delta);
                Ok(())
            }
            ShellCommandUpdate::Done(command) => {
                if done.get(index) != Some(&false) || self.command != *command {
                    return Err(invalid(
                        "shell command done is repeated or contradicts streamed command",
                    ));
                }
                account.grow(
                    budget,
                    self,
                    |state| state.item.action.commands[index].len() + state.command.len(),
                    |state| {
                        state.item.apply_done(payload, &mut state.command);
                        if let Some(done) = state.command_stream.as_mut() {
                            done[index] = true;
                        }
                    },
                )
            }
        }
    }
}

impl RetainedSize for ShellCallState {
    fn retained_bytes(&self) -> usize {
        self.item.retained_bytes() + self.command.len()
    }
}
