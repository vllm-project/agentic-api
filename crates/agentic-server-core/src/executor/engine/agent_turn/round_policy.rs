//! The tool-loop policy that turns one round's calls into a [`RoundDecision`].

use super::RoundDecision;
use crate::executor::gateway::GatewayCallResult;
use crate::tool::ToolRegistry;
use crate::types::io::OutputItem;

/// The client-owned calls one round produced. A synchronous call ends the response so the
/// client can return its output; an async call lets the model continue without it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct ClientCalls {
    synchronous: bool,
    asynchronous: bool,
}

impl ClientCalls {
    pub(super) fn of(output_items: &[OutputItem], registry: &ToolRegistry) -> Self {
        output_items
            .iter()
            .filter(|item| item.requires_client_action(registry))
            .fold(Self::default(), |calls, item| {
                if item.is_async_call() {
                    Self {
                        asynchronous: true,
                        ..calls
                    }
                } else {
                    Self {
                        synchronous: true,
                        ..calls
                    }
                }
            })
    }
}

/// Consecutive async-only rounds allowed to continue. After an async call the model needs one more
/// round to answer; a second allows it to start another async job first. A model that keeps
/// calling async tools instead of answering stops here rather than spending the round budget.
const MAX_CONSECUTIVE_ASYNC_CONTINUATIONS: usize = 2;

/// How many rounds in a row the loop has continued only because of async calls.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct AsyncContinuations(usize);

/// Classify one turn's output into a [`RoundDecision`].
///
/// Order matters: synchronous client calls take precedence (they must be handed
/// back even when gateway or async calls are also present in the same turn), as
/// `OpenAI` ends the response there. Gateway tools that ran continue the loop,
/// unless this was the last permitted round, in which case the budget is
/// exhausted and the turn is `Incomplete`. A turn whose only client calls are
/// async also continues, with the calls pending, so the model can keep working
/// as an `OpenAI` model does; on the last permitted round, or after
/// [`MAX_CONSECUTIVE_ASYNC_CONTINUATIONS`] such rounds in a row, it is `Done`, since
/// no work is left unrecorded. A turn with no tool work is `Done`.
///
/// `round` is zero-based; `max_rounds` is the total budget. `continuations` counts
/// the consecutive async-only continuations and is updated for the next round.
pub(super) fn classify_round(
    client_calls: ClientCalls,
    gateway_results: &[GatewayCallResult],
    round: usize,
    max_rounds: usize,
    continuations: &mut AsyncContinuations,
) -> RoundDecision {
    let last_round = round + 1 >= max_rounds;
    let previous = std::mem::take(&mut continuations.0);
    if client_calls.synchronous {
        RoundDecision::RequiresClientAction
    } else if !gateway_results.is_empty() {
        if last_round {
            RoundDecision::Incomplete(format!("gateway tool execution exceeded {max_rounds} rounds"))
        } else {
            RoundDecision::Continue
        }
    } else if client_calls.asynchronous && !last_round && previous < MAX_CONSECUTIVE_ASYNC_CONTINUATIONS {
        continuations.0 = previous + 1;
        RoundDecision::Continue
    } else {
        RoundDecision::Done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::io::InputItem;

    const NONE: ClientCalls = ClientCalls {
        synchronous: false,
        asynchronous: false,
    };
    const SYNC: ClientCalls = ClientCalls {
        synchronous: true,
        asynchronous: false,
    };
    const ASYNC: ClientCalls = ClientCalls {
        synchronous: false,
        asynchronous: true,
    };
    const BOTH: ClientCalls = ClientCalls {
        synchronous: true,
        asynchronous: true,
    };

    fn gateway_results() -> [GatewayCallResult; 1] {
        [GatewayCallResult {
            item_index: 0,
            input_item: InputItem::Unknown,
            public_output: None,
            omitted: false,
        }]
    }

    #[test]
    fn round_policy_keeps_client_handoff_precedence_and_uses_the_turns_limit() {
        let results = gateway_results();
        assert!(matches!(
            classify_round(SYNC, &results, 0, 1, &mut AsyncContinuations::default()),
            RoundDecision::RequiresClientAction
        ));
        assert!(matches!(
            classify_round(NONE, &[], 0, 1, &mut AsyncContinuations::default()),
            RoundDecision::Done
        ));
        assert!(matches!(
            classify_round(NONE, &results, 0, 2, &mut AsyncContinuations::default()),
            RoundDecision::Continue
        ));
        assert!(matches!(
            classify_round(NONE, &results, 1, 2, &mut AsyncContinuations::default()),
            RoundDecision::Incomplete(_)
        ));
        // The legacy ten-round policy is supplied by the single-agent adapter,
        // not hardcoded into shared turn execution or used as a tree budget.
        assert!(matches!(
            classify_round(NONE, &results, 10, 12, &mut AsyncContinuations::default()),
            RoundDecision::Continue
        ));
    }

    /// Async-only client calls keep the response going (the model answers with the calls
    /// pending); a synchronous call still hands back, as in the recorded `OpenAI` responses.
    #[test]
    fn round_policy_continues_after_async_calls_unless_a_synchronous_call_needs_the_client() {
        let results = gateway_results();
        assert!(matches!(
            classify_round(ASYNC, &[], 0, 2, &mut AsyncContinuations::default()),
            RoundDecision::Continue
        ));
        assert!(matches!(
            classify_round(ASYNC, &results, 0, 2, &mut AsyncContinuations::default()),
            RoundDecision::Continue
        ));
        assert!(matches!(
            classify_round(BOTH, &[], 0, 2, &mut AsyncContinuations::default()),
            RoundDecision::RequiresClientAction
        ));
        // On the last round an async-only turn completes: its calls are already public.
        assert!(matches!(
            classify_round(ASYNC, &[], 1, 2, &mut AsyncContinuations::default()),
            RoundDecision::Done
        ));
        assert!(matches!(
            classify_round(ASYNC, &results, 1, 2, &mut AsyncContinuations::default()),
            RoundDecision::Incomplete(_)
        ));
    }

    /// A model that keeps calling async tools instead of answering stops after two continuations
    /// in a row; any other round resets the count.
    #[test]
    fn consecutive_async_only_continuations_are_capped() {
        let mut continuations = AsyncContinuations::default();
        let decisions: Vec<bool> = (0..3)
            .map(|round| {
                matches!(
                    classify_round(ASYNC, &[], round, 10, &mut continuations),
                    RoundDecision::Continue
                )
            })
            .collect();
        assert_eq!(decisions, [true, true, false]);

        let mut continuations = AsyncContinuations::default();
        classify_round(ASYNC, &[], 0, 10, &mut continuations);
        classify_round(NONE, &gateway_results(), 1, 10, &mut continuations);
        assert!(matches!(
            classify_round(ASYNC, &[], 2, 10, &mut continuations),
            RoundDecision::Continue
        ));
    }
}
