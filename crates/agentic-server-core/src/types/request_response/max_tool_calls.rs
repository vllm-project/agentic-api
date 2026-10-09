//! The `max_tool_calls` request parameter.
//!
//! The Responses API accepts an integer from 1 to `i64::MAX` and rejects other
//! values with a typed parameter error (`integer_below_min_value`,
//! `integer_above_max_value`, or `invalid_type`). Deserialization therefore
//! accepts any JSON value and keeps it as [`MaxToolCalls`]; request admission
//! parses it into a [`NonZeroU64`] limit with [`MaxToolCalls::limit`].

use std::fmt;
use std::num::NonZeroU64;

use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Request parameter name reported in validation errors.
pub const MAX_TOOL_CALLS_PARAM: &str = "max_tool_calls";

/// Largest accepted limit, matching the Responses API.
const MAX_LIMIT: u64 = i64::MAX.unsigned_abs();

/// Smallest magnitude of an `f64` integer that no 64-bit signed integer can hold (2^63).
const OUT_OF_RANGE_MAGNITUDE: f64 = 9_223_372_036_854_775_808.0;

/// `max_tool_calls` exactly as the client sent it; `null` is the surrounding `Option`.
#[derive(Debug, Clone, PartialEq)]
pub struct MaxToolCalls(WireValue);

#[derive(Debug, Clone, PartialEq)]
enum WireValue {
    Limit(NonZeroU64),
    BelowMinimum(OutOfRangeInteger),
    AboveMaximum(OutOfRangeInteger),
    String(String),
    Boolean(bool),
    Decimal(f64),
    Object,
    Array,
}

impl MaxToolCalls {
    /// Wraps an already valid limit.
    #[must_use]
    pub const fn new(limit: NonZeroU64) -> Self {
        Self(WireValue::Limit(limit))
    }

    /// Parses the client value into the maximum number of built-in tool calls
    /// one response may process.
    ///
    /// # Errors
    ///
    /// Returns [`MaxToolCallsError`] when the value is not an integer from 1 to `i64::MAX`.
    pub fn limit(&self) -> Result<NonZeroU64, MaxToolCallsError> {
        let received = match &self.0 {
            WireValue::Limit(limit) => return Ok(*limit),
            WireValue::BelowMinimum(value) => return Err(MaxToolCallsError::BelowMinimum(*value)),
            WireValue::AboveMaximum(value) => return Err(MaxToolCallsError::AboveMaximum(*value)),
            WireValue::String(_) => JsonKind::String,
            WireValue::Boolean(_) => JsonKind::Boolean,
            WireValue::Decimal(_) => JsonKind::Decimal,
            WireValue::Object => JsonKind::Object,
            WireValue::Array => JsonKind::Array,
        };
        Err(MaxToolCallsError::InvalidType(received))
    }
}

/// An integer outside the accepted range, as received.
///
/// JSON integers beyond 64 bits arrive as `f64`, so their digits are the
/// nearest `f64` value rather than the exact text the client sent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutOfRangeInteger {
    Signed(i64),
    Unsigned(u64),
    Float(f64),
}

impl fmt::Display for OutOfRangeInteger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Signed(value) => write!(f, "{value}"),
            Self::Unsigned(value) => write!(f, "{value}"),
            Self::Float(value) => write!(f, "{value:.0}"),
        }
    }
}

/// JSON value kinds named by an `invalid_type` error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonKind {
    String,
    Boolean,
    Decimal,
    Object,
    Array,
}

impl fmt::Display for JsonKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::String => "a string",
            Self::Boolean => "a boolean",
            Self::Decimal => "a decimal number",
            Self::Object => "an object",
            Self::Array => "an array",
        })
    }
}

/// An invalid `max_tool_calls` value, reported with the Responses API wording and codes.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum MaxToolCallsError {
    #[error("Invalid 'max_tool_calls': integer below minimum value. Expected a value >= 1, but got {0} instead.")]
    BelowMinimum(OutOfRangeInteger),
    #[error(
        "Invalid 'max_tool_calls': integer above maximum value. Expected a value <= {MAX_LIMIT}, but got {0} instead."
    )]
    AboveMaximum(OutOfRangeInteger),
    #[error("Invalid type for 'max_tool_calls': expected an integer, but got {0} instead.")]
    InvalidType(JsonKind),
}

impl MaxToolCallsError {
    /// Machine-readable error code for the API error envelope.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::BelowMinimum(_) => "integer_below_min_value",
            Self::AboveMaximum(_) => "integer_above_max_value",
            Self::InvalidType(_) => "invalid_type",
        }
    }
}

impl Serialize for OutOfRangeInteger {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Signed(value) => serializer.serialize_i64(*value),
            Self::Unsigned(value) => serializer.serialize_u64(*value),
            Self::Float(value) => serializer.serialize_f64(*value),
        }
    }
}

impl Serialize for MaxToolCalls {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.0 {
            WireValue::Limit(limit) => serializer.serialize_u64(limit.get()),
            WireValue::BelowMinimum(value) | WireValue::AboveMaximum(value) => value.serialize(serializer),
            WireValue::String(value) => serializer.serialize_str(value),
            WireValue::Boolean(value) => serializer.serialize_bool(*value),
            WireValue::Decimal(value) => serializer.serialize_f64(*value),
            WireValue::Object => serializer.serialize_map(Some(0))?.end(),
            WireValue::Array => serializer.serialize_seq(Some(0))?.end(),
        }
    }
}

impl<'de> Deserialize<'de> for MaxToolCalls {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(WireVisitor).map(Self)
    }
}

struct WireVisitor;

impl<'de> Visitor<'de> for WireVisitor {
    type Value = WireValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<WireValue, E> {
        Ok(match NonZeroU64::new(value) {
            None => WireValue::BelowMinimum(OutOfRangeInteger::Unsigned(value)),
            Some(_) if value > MAX_LIMIT => WireValue::AboveMaximum(OutOfRangeInteger::Unsigned(value)),
            Some(limit) => WireValue::Limit(limit),
        })
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<WireValue, E> {
        Ok(u64::try_from(value).ok().and_then(NonZeroU64::new).map_or(
            WireValue::BelowMinimum(OutOfRangeInteger::Signed(value)),
            WireValue::Limit,
        ))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<WireValue, E> {
        // serde_json reports integers beyond 64 bits as `f64`.
        let integer_out_of_range = value.fract() == 0.0 && value.abs() >= OUT_OF_RANGE_MAGNITUDE;
        Ok(match integer_out_of_range {
            true if value > 0.0 => WireValue::AboveMaximum(OutOfRangeInteger::Float(value)),
            true => WireValue::BelowMinimum(OutOfRangeInteger::Float(value)),
            false => WireValue::Decimal(value),
        })
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<WireValue, E> {
        Ok(WireValue::Boolean(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<WireValue, E> {
        Ok(WireValue::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<WireValue, E> {
        Ok(WireValue::String(value))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<WireValue, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(WireValue::Object)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<WireValue, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(WireValue::Array)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: serde_json::Value) -> Result<NonZeroU64, MaxToolCallsError> {
        serde_json::from_value::<MaxToolCalls>(value).unwrap().limit()
    }

    fn parse_text(json: &str) -> Result<NonZeroU64, MaxToolCallsError> {
        serde_json::from_str::<MaxToolCalls>(json).unwrap().limit()
    }

    #[test]
    fn accepts_integers_from_one_to_i64_max() {
        for value in [1_u64, 5, 2_147_483_648, 4_294_967_296, MAX_LIMIT] {
            assert_eq!(parse(serde_json::json!(value)).unwrap().get(), value);
        }
    }

    #[test]
    fn reports_openai_codes_and_messages_for_rejected_values() {
        let cases = [
            ("0", "integer_below_min_value", "but got 0 instead."),
            ("-1", "integer_below_min_value", "but got -1 instead."),
            (
                "-9223372036854775808",
                "integer_below_min_value",
                "but got -9223372036854775808 instead.",
            ),
            (
                "9223372036854775808",
                "integer_above_max_value",
                "but got 9223372036854775808 instead.",
            ),
            (
                "18446744073709551615",
                "integer_above_max_value",
                "but got 18446744073709551615 instead.",
            ),
            (
                "18446744073709551616",
                "integer_above_max_value",
                "but got 18446744073709551616 instead.",
            ),
            (r#""2""#, "invalid_type", "but got a string instead."),
            ("2.5", "invalid_type", "but got a decimal number instead."),
            ("1000.0", "invalid_type", "but got a decimal number instead."),
            ("true", "invalid_type", "but got a boolean instead."),
            (r#"{"a": 1}"#, "invalid_type", "but got an object instead."),
            ("[1]", "invalid_type", "but got an array instead."),
        ];
        for (json, code, message_end) in cases {
            let error = parse_text(json).unwrap_err();
            assert_eq!(error.code(), code, "{json}");
            assert!(error.to_string().ends_with(message_end), "{json}: {error}");
        }
        assert_eq!(
            parse_text("9223372036854775808").unwrap_err().to_string(),
            "Invalid 'max_tool_calls': integer above maximum value. Expected a value <= 9223372036854775807, \
             but got 9223372036854775808 instead."
        );
    }

    #[test]
    fn integers_beyond_64_bits_keep_their_code_but_quote_the_nearest_f64() {
        // -9223372036854775809 is not representable as f64; the code is still exact.
        let error = parse_text("-9223372036854775809").unwrap_err();
        assert_eq!(error.code(), "integer_below_min_value");
        assert!(error.to_string().ends_with("but got -9223372036854775808 instead."));
    }

    #[test]
    fn serialization_preserves_the_client_value() {
        for json in ["3", "0", "-1", r#""2""#, "true", "2.5", "9223372036854775808"] {
            let wire: MaxToolCalls = serde_json::from_str(json).unwrap();
            assert_eq!(serde_json::to_string(&wire).unwrap(), json);
        }
    }
}
