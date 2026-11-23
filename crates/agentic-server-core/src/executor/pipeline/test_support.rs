//! Test entry point to the lenient ingestion round the engine runs for live upstream streams.

use super::RoundIngestion;
use crate::events::{SSEEventType, SseLine};
use crate::executor::accumulator::Validation;
use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::executor::translate::TranslationContext;
use crate::types::request_response::ResponsePayload;

/// Pushes complete SSE lines through one lenient round that must reach a terminal response event, then finishes it.
///
/// The lenient end-of-stream policy completes an unfinished stream, so a `completed` status alone does not show that
/// the stream's terminal event was ingested; this requires it.
pub(in crate::executor) fn ingest_lenient<I>(lines: I, model: &str) -> ExecutorResult<ResponsePayload>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let (round, terminal) = push_lenient(lines)?;
    if !terminal {
        return Err(ExecutorError::StreamError(
            "lenient round ended without a terminal response event".to_owned(),
        ));
    }
    round.finish(model, None)
}

/// Pushes complete SSE lines through one lenient round and applies its end-of-stream policy, terminal event or not.
pub(in crate::executor) fn ingest_lenient_to_eof<I>(lines: I, model: &str) -> ExecutorResult<ResponsePayload>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    push_lenient(lines)?.0.finish(model, None)
}

fn push_lenient<I>(lines: I) -> ExecutorResult<(RoundIngestion, bool)>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let mut round = RoundIngestion::new(Validation::Lenient, TranslationContext::default(), None);
    let mut terminal = false;
    for line in lines {
        let translation = round.push(SseLine::parse(line.as_ref()))?;
        terminal |= translation.frames.iter().any(|frame| {
            matches!(
                frame.event_type,
                SSEEventType::ResponseCompleted | SSEEventType::ResponseIncomplete | SSEEventType::ResponseFailed
            )
        });
    }
    Ok((round, terminal))
}
