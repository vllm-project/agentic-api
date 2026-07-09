use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to build HTTP client")]
    HttpClient(#[source] reqwest::Error),

    #[error("LLM not ready within {timeout_s:.0}s at {url}")]
    LlmTimeout { url: String, timeout_s: f64 },

    #[error("LLM subprocess exited before becoming ready: {status}")]
    LlmProcessExited { status: String },

    #[error(transparent)]
    Io(#[from] io::Error),

    #[error("invalid header value")]
    InvalidHeader(#[from] reqwest::header::InvalidHeaderValue),

    #[error("{0}")]
    Config(String),
}
