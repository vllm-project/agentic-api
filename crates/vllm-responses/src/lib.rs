//! Typed, transport-independent inference events for the Responses API.
//!
//! This crate deliberately contains no HTTP, SSE, storage, tool execution, or
//! engine implementation details. Adapters translate their native output into
//! [`InferenceEvent`] values; the Responses runtime owns public lifecycle and
//! delivery semantics.

#![forbid(unsafe_code)]

/// A stable output-item position in one response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct OutputIndex(pub u32);

/// Token accounting supplied with a terminal inference event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct InferenceUsage {
    /// Tokens consumed by the prompt.
    pub input_tokens: u64,
    /// Tokens produced by generation.
    pub output_tokens: u64,
    /// Tokens included in the visible or retained reasoning output.
    pub reasoning_tokens: u64,
    /// Prompt tokens served from the model's cache.
    pub cached_tokens: u64,
}

impl InferenceUsage {
    /// Creates usage with no prompt-cache hit.
    #[must_use]
    pub const fn new(input_tokens: u64, output_tokens: u64, reasoning_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cached_tokens: 0,
        }
    }

    /// Records prompt tokens served from the model's cache.
    #[must_use]
    pub const fn with_cached_tokens(mut self, cached_tokens: u64) -> Self {
        self.cached_tokens = cached_tokens;
        self
    }
}

/// A terminal upstream failure with a stable machine-readable category when available.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InferenceFailure {
    /// Adapter- or upstream-defined error category.
    pub code: Option<String>,
    /// A safe diagnostic for the Responses runtime to map to its public error contract.
    pub message: String,
}

impl InferenceFailure {
    /// Creates a failure without an adapter-specific category.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
        }
    }

    /// Adds an adapter- or upstream-defined error category.
    #[must_use]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
}

/// Semantic output from a model inference adapter.
///
/// Every event is independent of HTTP framing and JSON wire shape. The caller
/// assigns the public response ID before starting inference; lifecycle events
/// therefore do not carry a second, provider-derived response identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InferenceEvent {
    /// The adapter accepted the request and prompt preparation completed.
    Started,
    /// Generation is active after the response-created lifecycle event.
    InProgress,
    /// A text output item began.
    TextStarted {
        /// Stable item identity for subsequent deltas.
        item_id: String,
        /// Position in the response output sequence.
        output_index: OutputIndex,
    },
    /// Incremental text for an active message item.
    TextDelta {
        /// Item identity supplied by [`Self::TextStarted`].
        item_id: String,
        /// Position in the response output sequence.
        output_index: OutputIndex,
        /// Newly generated text.
        delta: String,
    },
    /// A text output item completed.
    TextCompleted {
        /// Stable item identity.
        item_id: String,
        /// Position in the response output sequence.
        output_index: OutputIndex,
        /// Complete authoritative text.
        text: String,
    },
    /// A reasoning output item began.
    ReasoningStarted {
        /// Stable item identity for subsequent deltas.
        item_id: String,
        /// Position in the response output sequence.
        output_index: OutputIndex,
    },
    /// Incremental reasoning text for an active reasoning item.
    ReasoningTextDelta {
        /// Stable item identity.
        item_id: String,
        /// Position in the response output sequence.
        output_index: OutputIndex,
        /// Newly generated reasoning text.
        delta: String,
    },
    /// A reasoning output item completed.
    ReasoningCompleted {
        /// Stable item identity.
        item_id: String,
        /// Position in the response output sequence.
        output_index: OutputIndex,
        /// Complete authoritative reasoning text.
        text: String,
    },
    /// A function-call output item began.
    FunctionCallStarted {
        /// Stable item identity for subsequent events.
        item_id: String,
        /// Position in the response output sequence.
        output_index: OutputIndex,
        /// Stable function-call identity.
        call_id: String,
        /// Declared function name.
        name: String,
    },
    /// Incremental arguments for an active function call.
    FunctionCallArgumentsDelta {
        /// Stable item identity.
        item_id: String,
        /// Position in the response output sequence.
        output_index: OutputIndex,
        /// Newly generated function-call arguments.
        delta: String,
    },
    /// A function-call output item completed.
    FunctionCallCompleted {
        /// Stable item identity.
        item_id: String,
        /// Position in the response output sequence.
        output_index: OutputIndex,
        /// Stable function-call identity.
        call_id: String,
        /// Declared function name.
        name: String,
        /// Complete authoritative function-call arguments.
        arguments: String,
    },
    /// Generation completed normally.
    Completed {
        /// Optional usage reported by the adapter.
        usage: Option<InferenceUsage>,
    },
    /// Generation stopped before normal completion.
    ///
    /// Every active output item must emit its corresponding `*Completed` event
    /// before this event. The incomplete state is response-level only; adapters
    /// must not rely on it to complete or discard partial output.
    Incomplete {
        /// Optional usage reported by the adapter.
        usage: Option<InferenceUsage>,
        /// Adapter-provided incomplete reason.
        reason: Option<String>,
    },
    /// Generation failed.
    ///
    /// Every active output item must emit its corresponding `*Completed` event
    /// before this event. The failure state is response-level only.
    Failed {
        /// Optional usage reported before failure.
        usage: Option<InferenceUsage>,
        /// Typed failure details.
        failure: InferenceFailure,
    },
}
