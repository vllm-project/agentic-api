use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct InputTokenDetails {
    pub cached_tokens: i64,
    /// Tokens written to the prompt cache, when reported by the upstream service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OutputTokenDetails {
    pub reasoning_tokens: i64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ResponseUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub total_tokens: i64,
    #[serde(default)]
    pub input_tokens_details: InputTokenDetails,
    #[serde(default)]
    pub output_tokens_details: OutputTokenDetails,
}

impl ResponseUsage {
    /// Add usage from another completed operation, saturating each counter on overflow.
    ///
    /// Callers must account for each operation once; this method does not deduplicate
    /// streaming snapshots or distinguish inference, compaction, and agent turns.
    /// Keep the reported total independent of the input/output counters rather than
    /// recomputing it, matching the executor's existing aggregation behavior.
    /// Optional counters remain absent if neither operation reports them; otherwise
    /// only the reported values are summed.
    #[must_use]
    pub fn saturating_add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
            input_tokens_details: InputTokenDetails {
                cached_tokens: self
                    .input_tokens_details
                    .cached_tokens
                    .saturating_add(other.input_tokens_details.cached_tokens),
                cache_write_tokens: match (
                    self.input_tokens_details.cache_write_tokens,
                    other.input_tokens_details.cache_write_tokens,
                ) {
                    (Some(left), Some(right)) => Some(left.saturating_add(right)),
                    (Some(value), None) | (None, Some(value)) => Some(value),
                    (None, None) => None,
                },
            },
            output_tokens_details: OutputTokenDetails {
                reasoning_tokens: self
                    .output_tokens_details
                    .reasoning_tokens
                    .saturating_add(other.output_tokens_details.reasoning_tokens),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_usage_details_and_preserves_reported_totals() {
        let first: ResponseUsage = serde_json::from_value(serde_json::json!({
            "input_tokens": 10,
            "output_tokens": 4,
            "total_tokens": 15,
            "input_tokens_details": {"cached_tokens": 3, "cache_write_tokens": 2},
            "output_tokens_details": {"reasoning_tokens": 2}
        }))
        .unwrap();
        let second: ResponseUsage = serde_json::from_value(serde_json::json!({
            "input_tokens": 6,
            "output_tokens": 3,
            "total_tokens": 9,
            "input_tokens_details": {"cached_tokens": 2, "cache_write_tokens": 1},
            "output_tokens_details": {"reasoning_tokens": 1}
        }))
        .unwrap();

        assert_eq!(
            serde_json::to_value(first.saturating_add(second)).unwrap(),
            serde_json::json!({
                "input_tokens": 16,
                "output_tokens": 7,
                "total_tokens": 24,
                "input_tokens_details": {"cached_tokens": 5, "cache_write_tokens": 3},
                "output_tokens_details": {"reasoning_tokens": 3}
            })
        );
    }

    #[test]
    fn saturates_all_counters_instead_of_wrapping() {
        fn usage(value: i64) -> ResponseUsage {
            ResponseUsage {
                input_tokens: value,
                output_tokens: value,
                total_tokens: value,
                input_tokens_details: InputTokenDetails {
                    cached_tokens: value,
                    cache_write_tokens: Some(value),
                },
                output_tokens_details: OutputTokenDetails {
                    reasoning_tokens: value,
                },
            }
        }

        for (limit, increment) in [(i64::MAX, 1), (i64::MIN, -1)] {
            assert_eq!(
                serde_json::to_value(usage(limit).saturating_add(usage(increment))).unwrap(),
                serde_json::to_value(usage(limit)).unwrap()
            );
        }
    }

    #[test]
    fn cache_write_tokens_preserve_omission_and_explicit_zero() {
        for count in [None, Some(0), Some(7)] {
            let mut wire = serde_json::json!({"cached_tokens": 0});
            if let Some(count) = count {
                wire["cache_write_tokens"] = serde_json::json!(count);
            }
            let details: InputTokenDetails = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(details.cache_write_tokens, count);
            assert_eq!(serde_json::to_value(details).unwrap(), wire);
        }
    }

    #[test]
    fn cache_write_totals_preserve_missing_reports() {
        fn usage(cache_write_tokens: Option<i64>) -> ResponseUsage {
            ResponseUsage {
                input_tokens_details: InputTokenDetails {
                    cache_write_tokens,
                    ..Default::default()
                },
                ..Default::default()
            }
        }

        for (left, right, expected) in [
            (None, None, None),
            (None, Some(0), Some(0)),
            (Some(0), None, Some(0)),
            (None, Some(7), Some(7)),
            (Some(7), None, Some(7)),
        ] {
            let total = usage(left).saturating_add(usage(right));
            assert_eq!(total.input_tokens_details.cache_write_tokens, expected);
        }
    }
}
