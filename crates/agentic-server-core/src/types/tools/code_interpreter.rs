//! Typed model-call arguments for the gateway-executed code interpreter.
//!
//! This module deliberately has no dependency on the tool registry or a
//! runtime implementation. The execution layer receives this already-checked
//! shape rather than deserializing model JSON itself.

use std::num::NonZeroUsize;

use serde::Deserialize;

/// The closed argument shape accepted by the model-visible `code_interpreter`
/// function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeInterpreterCallArguments {
    /// Python source code to execute.
    code: String,
}

/// Errors returned while parsing a model call for the code interpreter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CodeInterpreterCallArgumentsError {
    /// The call did not contain syntactically valid JSON.
    #[error("code_interpreter arguments must be valid JSON")]
    MalformedJson,
    /// The call did not encode the closed `{"code": "..."}` JSON shape.
    #[error("code_interpreter arguments must contain exactly one string field 'code'")]
    InvalidShape,
    /// The source string exceeded the caller-provided byte limit.
    #[error("code_interpreter source is {actual} bytes, exceeding the {limit}-byte limit")]
    SourceTooLarge {
        /// Actual UTF-8 byte length of the source string.
        actual: usize,
        /// Caller-provided maximum UTF-8 byte length.
        limit: usize,
    },
}

/// Private deserialization shape used only by the validating constructor.
///
/// Keeping this separate prevents callers from using serde to construct a
/// `CodeInterpreterCallArguments` value without supplying a source limit.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCodeInterpreterCallArguments {
    code: String,
}

impl CodeInterpreterCallArguments {
    /// Parse one model function-call argument string without initializing a guest.
    ///
    /// `max_source_bytes` is nonzero by type, so callers cannot accidentally
    /// configure an unusable zero-byte execution budget.
    ///
    /// # Errors
    ///
    /// Returns [`CodeInterpreterCallArgumentsError::MalformedJson`] for malformed
    /// JSON and [`CodeInterpreterCallArgumentsError::InvalidShape`] for missing,
    /// non-string, or unknown fields. Returns
    /// [`CodeInterpreterCallArgumentsError::SourceTooLarge`] when the UTF-8
    /// source string exceeds `max_source_bytes`.
    pub fn from_json(
        arguments: &str,
        max_source_bytes: NonZeroUsize,
    ) -> Result<Self, CodeInterpreterCallArgumentsError> {
        let parsed = serde_json::from_str::<RawCodeInterpreterCallArguments>(arguments).map_err(|error| {
            if error.is_syntax() || error.is_eof() {
                CodeInterpreterCallArgumentsError::MalformedJson
            } else {
                CodeInterpreterCallArgumentsError::InvalidShape
            }
        })?;
        let actual = parsed.code.len();
        let limit = max_source_bytes.get();
        if actual > limit {
            return Err(CodeInterpreterCallArgumentsError::SourceTooLarge { actual, limit });
        }
        Ok(Self { code: parsed.code })
    }

    /// Borrow the validated Python source code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Consume the validated arguments and return their Python source code.
    #[must_use]
    pub fn into_code(self) -> String {
        self.code
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_interpreter_call_arguments_accept_only_code() {
        let arguments = CodeInterpreterCallArguments::from_json(
            r#"{"code":"print(1 + 1)"}"#,
            NonZeroUsize::new(64).expect("nonzero limit"),
        )
        .expect("closed arguments parse");

        assert_eq!(arguments.code(), "print(1 + 1)");
    }

    #[test]
    fn code_interpreter_call_arguments_reject_malformed_missing_nonstring_and_extra_fields() {
        for (arguments, expected) in [
            (
                r#"{"code":"unterminated}"#,
                CodeInterpreterCallArgumentsError::MalformedJson,
            ),
            ("{}", CodeInterpreterCallArgumentsError::InvalidShape),
            (r#"{"code":42}"#, CodeInterpreterCallArgumentsError::InvalidShape),
            (
                r#"{"code":"print(1)","result":"structured"}"#,
                CodeInterpreterCallArgumentsError::InvalidShape,
            ),
        ] {
            let error =
                CodeInterpreterCallArguments::from_json(arguments, NonZeroUsize::new(64).expect("nonzero limit"))
                    .expect_err("invalid model arguments must not construct a validated value");
            assert_eq!(error, expected);
        }
    }

    #[test]
    fn code_interpreter_call_arguments_reject_source_over_the_caller_limit() {
        let error =
            CodeInterpreterCallArguments::from_json(r#"{"code":"ππ"}"#, NonZeroUsize::new(3).expect("nonzero limit"))
                .expect_err("four UTF-8 source bytes exceed the limit");

        assert!(matches!(
            error,
            CodeInterpreterCallArgumentsError::SourceTooLarge { actual: 4, limit: 3 }
        ));
    }
}
