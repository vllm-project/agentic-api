//! Buffered HTTP/SSE ingestion convenience APIs.

use super::ResponseAccumulator;
use crate::events::SseLine;
use crate::executor::error::ExecutorResult;
use crate::utils::uuid7_str;

impl ResponseAccumulator {
    /// Processes pre-collected raw SSE lines synchronously.
    ///
    /// Useful when lines have already been buffered (e.g. replaying a recorded stream).
    /// Prefer [`Self::from_stream`] for live async streams. Malformed data frames are skipped for compatibility.
    ///
    /// # Errors
    ///
    /// Returns an error when repeated authoritative output-item content conflicts.
    pub fn from_sse_lines(
        lines: impl IntoIterator<Item = String>,
        conversation_id: Option<&str>,
    ) -> ExecutorResult<Self> {
        let mut acc = Self::new(uuid7_str("resp_"), conversation_id.map(str::to_string));
        for line in lines {
            let _ = acc.process_line(SseLine::parse(&line))?;
        }
        acc.finalize_all()?;
        Ok(acc)
    }
}
