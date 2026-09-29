use super::{RETAINED_CONTAINER_OVERHEAD_BYTES, RetainedSize, opt_len, sum_retained};
use crate::types::client_calls::ClientToolOutput;
use crate::types::io::{
    CustomToolCallOutputMessage, FunctionToolResultMessage, InputFileContent, InputImageContent, InputTextContent,
    ShellCallOutputContent, ShellCallOutputMessage, ToolCallOutput, ToolOutputContent, ToolSearchOutputMessage,
};
use crate::utils::common::serialized_size_up_to;

impl RetainedSize for InputTextContent {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + self.text.len()
            + self
                .extra
                .iter()
                .map(|(key, value)| key.len() + value.retained_bytes())
                .sum::<usize>()
    }
}

impl RetainedSize for InputImageContent {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + opt_len(self.file_id.as_ref())
            + opt_len(self.image_url.as_ref())
            + opt_len(self.detail.as_ref())
            + self
                .extra
                .iter()
                .map(|(key, value)| key.len() + value.retained_bytes())
                .sum::<usize>()
    }
}

impl RetainedSize for InputFileContent {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + opt_len(self.file_data.as_ref())
            + opt_len(self.file_id.as_ref())
            + opt_len(self.file_url.as_ref())
            + opt_len(self.filename.as_ref())
            + opt_len(self.detail.as_ref())
            + self
                .extra
                .iter()
                .map(|(key, value)| key.len() + value.retained_bytes())
                .sum::<usize>()
    }
}

impl RetainedSize for ToolOutputContent {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::InputText(content) => content.retained_bytes(),
            Self::InputImage(content) => content.retained_bytes(),
            Self::InputFile(content) => content.retained_bytes(),
        }
    }
}

impl RetainedSize for ToolCallOutput {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + match self {
                Self::Text(text) => text.len(),
                Self::Content(parts) => sum_retained(parts),
            }
    }
}

impl RetainedSize for FunctionToolResultMessage {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES + self.call_id.len() + self.output.retained_bytes()
    }
}

impl RetainedSize for ShellCallOutputContent {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + self.stdout.len()
            + self.stderr.len()
            + self
                .extra
                .iter()
                .map(|(key, value)| key.len() + value.retained_bytes())
                .sum::<usize>()
    }
}

impl RetainedSize for ShellCallOutputMessage {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + opt_len(self.id.as_ref())
            + self.call_id.len()
            + sum_retained(&self.output)
            + self
                .extra
                .iter()
                .map(|(key, value)| key.len() + value.retained_bytes())
                .sum::<usize>()
    }
}

impl RetainedSize for CustomToolCallOutputMessage {
    fn retained_bytes(&self) -> usize {
        RETAINED_CONTAINER_OVERHEAD_BYTES
            + self.call_id.len()
            + opt_len(self.name.as_ref())
            + self.output.retained_bytes()
    }
}

impl RetainedSize for ToolSearchOutputMessage {
    fn retained_bytes(&self) -> usize {
        // Count the catalog without allocating a second JSON representation.
        serialized_size_up_to(self, usize::MAX)
            .ok()
            .flatten()
            .unwrap_or(usize::MAX)
            .saturating_add(RETAINED_CONTAINER_OVERHEAD_BYTES)
    }
}

impl RetainedSize for ClientToolOutput {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Function(output) => output.retained_bytes(),
            Self::Shell(output) => output.retained_bytes(),
            Self::Custom(output) => output.retained_bytes(),
            Self::ToolSearch(output) => output.retained_bytes(),
        }
    }
}
