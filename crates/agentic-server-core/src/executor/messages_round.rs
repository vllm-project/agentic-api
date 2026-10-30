//! Pure, shared orchestration policy for a completed Messages inference round.
//! Transport and ingestion establish completion before asking for a decision;
//! budget admission, execution, history and client delivery keep their owners.

use super::messages_context::MessagesRequestContext;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MessagesRoundDecision {
    ContinueGateway,
    ReturnClient,
    Finish,
}

impl MessagesRoundDecision {
    /// Preserve unknown or truncated stops. Only vLLM's `end_turn` on a client
    /// call needs repairing to tell the client to supply a `tool_result`.
    pub(super) fn visible_stop_reason(self, original: Option<&str>) -> Option<&str> {
        match self {
            Self::ReturnClient if original == Some("end_turn") => Some("tool_use"),
            Self::ContinueGateway | Self::ReturnClient | Self::Finish => original,
        }
    }
}

pub(super) fn decide_round<'a>(
    ctx: &MessagesRequestContext,
    stop_reason: Option<&str>,
    has_client_call: bool,
    gateway_names: impl Iterator<Item = &'a str>,
) -> MessagesRoundDecision {
    if has_client_call {
        return MessagesRoundDecision::ReturnClient;
    }
    let mut gateway_names = gateway_names.peekable();
    if gateway_names.peek().is_some() && ctx.is_tool_call_stop(stop_reason, gateway_names) {
        MessagesRoundDecision::ContinueGateway
    } else {
        MessagesRoundDecision::Finish
    }
}
