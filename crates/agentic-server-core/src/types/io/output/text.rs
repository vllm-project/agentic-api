//! Constructors for assistant output text content.

use super::OutputTextContent;

impl OutputTextContent {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            annotations: Some(vec![]),
            logprobs: Vec::new(),
        }
    }
}
