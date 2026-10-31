// Fixture preparation through the production HTTP/SSE transport, not another parser.

#![cfg(test)]

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;

use super::super::fixtures::{WorkloadIdentity, named_workloads, recorded_wire_rounds};
use super::http_server::HttpServer;
use crate::executor::error::{ExecutorError, ExecutorResult};

const MAX_FRAMED_BYTES: usize = 16 * 1024 * 1024;
const CHUNK_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct FramedWorkload {
    pub identity: WorkloadIdentity,
    pub lines: Vec<String>,
}

pub(super) async fn named_inputs() -> ExecutorResult<Vec<FramedWorkload>> {
    let mut prepared = Vec::new();
    for workload in named_workloads() {
        let lines = frame_wire(vec![Bytes::from(workload.wire)], false, CHUNK_TIMEOUT).await?;
        assert_eq!(
            lines.len(),
            workload.identity.expected_framed_lines,
            "{}",
            workload.identity.id
        );
        prepared.push(FramedWorkload {
            identity: workload.identity,
            lines,
        });
    }
    Ok(prepared)
}

pub(super) async fn recorded_inputs() -> ExecutorResult<Vec<Vec<String>>> {
    let mut rounds = Vec::new();
    for wire in recorded_wire_rounds() {
        rounds.push(frame_wire(vec![Bytes::from(wire)], false, CHUNK_TIMEOUT).await?);
    }
    Ok(rounds)
}

async fn frame_wire(chunks: Vec<Bytes>, keep_open: bool, chunk_timeout: Duration) -> ExecutorResult<Vec<String>> {
    let server = HttpServer::start(chunks, keep_open, Duration::ZERO).await?;
    let result = async {
        // This is a synthetic local POST envelope, not a recorded model request.
        // Only the captured response bytes and native transport are under test.
        let mut framed = server.stream(chunk_timeout);
        let mut lines = Vec::new();
        let mut bytes = 0usize;
        while let Some(line) = framed.next().await {
            let line = line?;
            bytes = bytes
                .checked_add(line.len())
                .ok_or_else(|| ExecutorError::StreamError("framed fixture byte count overflow".to_owned()))?;
            if lines.len() >= super::MAX_LINES || bytes > MAX_FRAMED_BYTES {
                return Err(ExecutorError::StreamError(
                    "framed fixture exceeds its retention bound".to_owned(),
                ));
            }
            lines.push(line);
        }
        Ok(lines)
    }
    .await;
    let cleanup = server.stop().await;
    // A fixture-server cleanup failure is a harness failure, even when an
    // upstream error was expected. Do not let that expected error mask it.
    cleanup?;
    result
}

#[tokio::test]
async fn native_transport_filters_metadata_without_trimming_data_values() {
    // Hand-authored transport unit input, not a captured model cassette.
    let wire = b"event: ignored\nid: 7\n: comment\n\ndata:{\"a\":1}\n\ndata:  {\"b\":2}\n\n";
    let lines = frame_wire(vec![Bytes::from_static(wire)], false, CHUNK_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(lines, ["data: {\"a\":1}", "data:  {\"b\":2}"]);
}

#[tokio::test]
async fn native_done_terminates_a_nonclosing_body_and_server_join_is_observed() {
    for marker in ["data:[DONE]", "data: [DONE]"] {
        let wire = format!("data:{{\"a\":1}}\n\n{marker}\n\ndata: never ingest this\n\n");
        let lines = frame_wire(vec![Bytes::from(wire)], true, CHUNK_TIMEOUT).await.unwrap();
        assert_eq!(lines, ["data: {\"a\":1}"]);
    }
}

#[tokio::test]
async fn native_chunk_timeout_remains_the_primary_error_after_server_cleanup() {
    let error = frame_wire(
        vec![Bytes::from_static(b"data: {}\n\n")],
        true,
        Duration::from_millis(50),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("chunk timeout"));
}

#[tokio::test]
async fn fragmented_recorded_wire_has_identical_native_framing() {
    let whole = recorded_inputs().await.unwrap();
    assert_eq!(whole.iter().map(Vec::len).collect::<Vec<_>>(), [118, 14, 24, 20]);
    for (wire, expected) in recorded_wire_rounds().into_iter().zip(whole) {
        let chunks = Bytes::from(wire).chunks(17).map(Bytes::copy_from_slice).collect();
        let fragmented = frame_wire(chunks, false, CHUNK_TIMEOUT).await.unwrap();
        assert_eq!(fragmented, expected);
    }
}

#[tokio::test]
async fn named_workloads_keep_identity_and_native_framing_across_fragmentation() {
    let prepared = named_inputs().await.unwrap();
    let workloads = named_workloads();
    assert_eq!(prepared.len(), workloads.len());
    for (workload, expected) in workloads.into_iter().zip(prepared) {
        assert_eq!(workload.identity, expected.identity);
        // Keep the existing mock backend's 32768-chunk bound for the long wire.
        // Both sizes split transport fields/UTF-8; no semantic event is altered.
        let chunk_bytes = if workload.identity.extended_gate { 257 } else { 17 };
        let chunks = Bytes::from(workload.wire)
            .chunks(chunk_bytes)
            .map(Bytes::copy_from_slice)
            .collect();
        assert_eq!(
            frame_wire(chunks, false, CHUNK_TIMEOUT).await.unwrap(),
            expected.lines,
            "{}",
            expected.identity.id
        );
    }
}
