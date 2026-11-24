//! Validate a summarization response before committing its compacted context.

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::io::OutputItem;
use crate::types::request_response::ResponsePayload;
use crate::utils::common::serialize_to_string;

fn response_output_text(output: &[OutputItem]) -> Option<String> {
    let text = output
        .iter()
        .filter_map(|item| match item {
            OutputItem::Message(message) => Some(message),
            _ => None,
        })
        .flat_map(|message| message.content.iter())
        .map(|content| content.text().trim())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

pub(super) fn completed_summary_text(response: &ResponsePayload) -> ExecutorResult<String> {
    if response.status != "completed" || response.error.is_some() {
        let details = response
            .error
            .as_ref()
            .and_then(|error| serialize_to_string(error).ok())
            .or_else(|| {
                response
                    .incomplete_details
                    .as_ref()
                    .and_then(|details| details.reason.clone())
            })
            .unwrap_or_else(|| "upstream returned no failure details".to_owned());
        return Err(ExecutorError::CompactionFailed {
            status: response.status.clone(),
            details,
        });
    }
    response_output_text(&response.output).ok_or_else(|| ExecutorError::CompactionFailed {
        status: response.status.clone(),
        details: "upstream returned no summary text".to_owned(),
    })
}
