//! HTTP and direct model inference, below shared request orchestration.

pub mod direct;
pub mod http;

use futures::Stream;
use std::pin::Pin;

pub(crate) use http::UpstreamHeaders;
pub(super) use http::{fetch_response_json, response_lines, send_request};

/// SSE stream of raw lines sent to the client (`data: …\n\n` per event).
pub type BoxStream = Pin<Box<dyn Stream<Item = String> + Send>>;

/// Wire-format marker signalling end-of-stream to the client.
pub(super) const DONE_MARKER: &str = "data: [DONE]\n\n";
