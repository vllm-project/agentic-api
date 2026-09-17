//! Turn-level `usage` accounting for the Messages gateway tool loops.
//!
//! Each gateway round is one upstream `/v1/messages` inference with its own
//! `usage`, but the client sees one logical message. Reporting only the final
//! round would drop the tokens spent by every hidden tool round, so both loops
//! fold each round's counters into these totals and write them into the
//! terminal `usage` object: the returned message's `usage` (JSON) or the final
//! `message_delta.usage` (SSE), which Anthropic defines as cumulative.
//!
//! A round's counters are the last values it reported. For SSE that is
//! `message_start.usage` overlaid by each `message_delta.usage`, so a repeated
//! cumulative delta never double-counts and an upstream whose deltas carry only
//! `output_tokens` still contributes its `input_tokens`. Only the documented
//! integer counters are summed. A counter absent from every round stays absent —
//! a missing value is never reported as zero — and every other `usage` field
//! passes through from the final round untouched. A single-round turn is
//! returned exactly as the upstream sent it.

use serde_json::Value;

/// The Anthropic `usage` counters that add across inference rounds.
const COUNTERS: [&str; 4] = [
    "input_tokens",
    "output_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
];

type Counters = [Option<u64>; COUNTERS.len()];

/// Saturating per-counter totals across the rounds committed so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct MessagesUsageTotals {
    /// Totals of every committed round.
    committed: Counters,
    /// Rounds committed so far.
    rounds: usize,
    /// The counters the current round reported most recently.
    round: Counters,
}

impl MessagesUsageTotals {
    /// Overlay one `usage` object onto the current round. Anything that is not a
    /// JSON object, and any counter that is not a non-negative integer, is
    /// ignored rather than guessed at.
    pub(super) fn observe(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage.and_then(Value::as_object) else {
            return;
        };
        for (current, name) in self.round.iter_mut().zip(COUNTERS) {
            if let Some(value) = usage.get(name).and_then(Value::as_u64) {
                *current = Some(value);
            }
        }
    }

    /// Add the current round to the totals and start the next one.
    pub(super) fn commit(&mut self) {
        for (total, value) in self.committed.iter_mut().zip(std::mem::take(&mut self.round)) {
            if let Some(value) = value {
                *total = Some(total.unwrap_or(0).saturating_add(value));
            }
        }
        self.rounds += 1;
    }

    /// Record one completed round from its final `usage` object.
    pub(super) fn record(&mut self, usage: Option<&Value>) {
        self.observe(usage);
        self.commit();
    }

    /// Fold the final round in and, when earlier rounds were hidden, rewrite
    /// `usage` as the turn total. A single round passes through untouched.
    pub(super) fn finish(&mut self, usage: Option<&mut Value>) {
        let Some(usage) = usage else {
            return;
        };
        self.record(Some(&*usage));
        if self.rounds < 2 {
            return;
        }
        if let Some(usage) = usage.as_object_mut() {
            for (total, name) in self.committed.iter().zip(COUNTERS) {
                if let Some(total) = total {
                    usage.insert(name.to_owned(), Value::from(*total));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn recorded(rounds: &[Value]) -> MessagesUsageTotals {
        let mut totals = MessagesUsageTotals::default();
        for usage in rounds {
            totals.record(Some(usage));
        }
        totals
    }

    #[test]
    fn sums_each_counter_reported_by_any_round() {
        let mut usage = json!({
            "input_tokens": 20, "output_tokens": 6, "service_tier": "standard", "cache_creation_input_tokens": 7
        });
        recorded(&[json!({"input_tokens": 10, "output_tokens": 4, "cache_read_input_tokens": 128})])
            .finish(Some(&mut usage));
        assert_eq!(
            usage,
            json!({
                "input_tokens": 30, "output_tokens": 10, "service_tier": "standard",
                "cache_creation_input_tokens": 7, "cache_read_input_tokens": 128
            })
        );
        assert_eq!(
            usage.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "input_tokens",
                "output_tokens",
                "service_tier",
                "cache_creation_input_tokens",
                "cache_read_input_tokens"
            ],
            "the final round's field order is kept; new counters follow"
        );
    }

    #[test]
    fn a_single_round_is_unchanged() {
        let original = json!({"output_tokens": 15, "server_tool_use": {"web_search_requests": 1}});
        let mut usage = original.clone();
        let mut totals = MessagesUsageTotals::default();
        totals.observe(Some(&json!({"input_tokens": 25, "output_tokens": 1})));
        totals.finish(Some(&mut usage));
        assert_eq!(usage, original);
    }

    #[test]
    fn message_start_supplies_counters_the_deltas_omit() {
        let mut totals = MessagesUsageTotals::default();
        totals.observe(Some(&json!({"input_tokens": 25, "output_tokens": 1})));
        totals.observe(Some(&json!({"output_tokens": 3})));
        totals.observe(Some(&json!({"output_tokens": 15})));
        totals.commit();
        totals.observe(Some(&json!({"input_tokens": 40, "output_tokens": 1})));
        let mut usage = json!({"output_tokens": 9});
        totals.finish(Some(&mut usage));
        assert_eq!(
            usage,
            json!({"output_tokens": 24, "input_tokens": 65}),
            "cumulative deltas count once"
        );
    }

    #[test]
    fn missing_counters_are_never_reported_as_zero() {
        let mut usage = json!({"output_tokens": 3});
        recorded(&[json!({"output_tokens": 9}), json!({})]).finish(Some(&mut usage));
        assert_eq!(usage, json!({"output_tokens": 12}));
    }

    #[test]
    fn ignores_values_that_are_not_counters() {
        let mut usage = json!({"input_tokens": 2, "output_tokens": "many"});
        recorded(&[
            json!({"input_tokens": -4, "output_tokens": 1.5}),
            json!(["input_tokens", 7]),
            Value::Null,
            json!({"input_tokens": 3}),
        ])
        .finish(Some(&mut usage));
        assert_eq!(usage, json!({"input_tokens": 5, "output_tokens": "many"}));
    }

    #[test]
    fn totals_saturate_instead_of_overflowing() {
        let mut usage = json!({"input_tokens": 1});
        recorded(&[json!({"input_tokens": u64::MAX})]).finish(Some(&mut usage));
        assert_eq!(usage["input_tokens"], json!(u64::MAX));
    }

    #[test]
    fn a_final_round_without_a_usage_object_is_left_alone() {
        let mut totals = recorded(&[json!({"input_tokens": 10})]);
        let mut usage = json!("opaque");
        totals.finish(Some(&mut usage));
        assert_eq!(usage, json!("opaque"));
        totals.finish(None);
        assert_eq!(totals.committed, [Some(10), None, None, None]);
    }
}
