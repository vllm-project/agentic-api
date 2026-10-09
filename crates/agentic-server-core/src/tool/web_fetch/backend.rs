//! The contract between the `web_fetch` handler and the backend that retrieves
//! a page.
//!
//! The handler owns the model-facing policy (argument parsing, URL admission,
//! domain filtering, text extraction, content limits, output shape); a backend
//! only turns an admitted URL into a document. The built-in backend is
//! [`HttpFetchBackend`](super::http::HttpFetchBackend). The trait is
//! crate-private: replacing the retriever (with an extraction service, say) is
//! a change to this module, not to the Messages loop.

use std::future::Future;
use std::pin::Pin;

use reqwest::StatusCode;
use url::Url;

use super::policy::UrlRejection;
use crate::tool::domain_policy::DomainFilter;

/// A page body a backend retrieved, before text extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchedDocument {
    /// The URL that answered, after redirects.
    pub url: Url,
    /// Lowercase media type of the body without parameters, e.g. `text/html`.
    pub media_type: String,
    /// The body decoded to text. HTML is converted to plain text by the handler.
    pub body: String,
    /// Whether the backend cut the body at its download ceiling.
    pub truncated: bool,
}

/// Why a fetch produced no document.
///
/// Each variant keeps the underlying error as its source, so logs and the
/// model-facing message carry the cause. The handler maps variants onto the
/// documented `web_fetch_tool_result_error` codes at one place
/// (`Refusal::from_failure`).
#[derive(Debug, thiserror::Error)]
pub(crate) enum FetchFailure {
    /// The host is, or resolves to, an address the policy refuses.
    #[error("{host} is not a public address")]
    NotPublic { host: String },
    /// The host is outside the declaration's domain filters.
    #[error("{host} is outside the allowed domains")]
    OutsideDomains { host: String },
    /// A URL failed admission; only reached for redirect targets, the handler
    /// admits the requested URL before the backend sees it.
    #[error("{0}")]
    Rejected(#[source] UrlRejection),
    /// A later hop was refused by the address policy, the domain filters, or
    /// URL admission.
    #[error("redirect target refused: {0}")]
    RedirectRefused(#[source] Box<FetchFailure>),
    /// A redirect target is not an absolute HTTP(S) URL the gateway fetches.
    #[error("invalid redirect target: {0}")]
    InvalidRedirect(#[source] UrlRejection),
    /// A `Location` header could not be resolved against the request URL.
    #[error("invalid redirect target")]
    UnparseableRedirect(#[source] url::ParseError),
    /// A redirect status arrived without a `Location` header.
    #[error("HTTP {status} without a Location header")]
    RedirectWithoutLocation { status: StatusCode },
    /// The redirect chain exceeded the configured hop limit.
    #[error("more than {0} redirects")]
    TooManyRedirects(u8),
    /// The URL has no host; unreachable for admitted HTTP(S) URLs.
    #[error("url has no host")]
    NoHost,
    /// Name resolution failed.
    #[error("could not resolve {host}")]
    Dns {
        host: String,
        #[source]
        source: std::io::Error,
    },
    /// Name resolution returned no address.
    #[error("{host} has no address")]
    NoAddress { host: String },
    /// The fetch ran out of its time budget: resolving, connecting, waiting for
    /// the response, or reading the body.
    #[error("timed out")]
    TimedOut,
    /// The request could not be sent or answered.
    #[error("request failed")]
    Request(#[source] reqwest::Error),
    /// The body could not be read.
    #[error("failed to read the body")]
    Body(#[source] reqwest::Error),
    /// The origin answered with a non-success status other than 429.
    #[error("HTTP {0}")]
    Status(StatusCode),
    /// The origin answered HTTP 429.
    #[error("the origin rate-limited the request (HTTP 429)")]
    TooManyRequests,
    /// The body is not text, HTML, or another supported text form.
    #[error("content type {0:?} is not supported")]
    UnsupportedContentType(String),
    /// The HTTP client could not be built.
    #[error("could not build the fetch client")]
    Client(#[source] reqwest::Error),
    /// The handler has no backend (a specification-only handler).
    #[error("no fetch backend is configured")]
    NoBackend,
}

/// A page-retrieval backend behind `web_fetch`.
///
/// `url` has passed [`policy::validate_url`](super::policy::validate_url) and
/// `filter`; a backend that follows redirects must re-apply both to every hop
/// and refuse non-public addresses unless configured otherwise.
pub(crate) trait WebFetchBackend: std::fmt::Debug + Send + Sync {
    fn fetch<'a>(
        &'a self,
        url: &'a Url,
        filter: &'a DomainFilter,
    ) -> Pin<Box<dyn Future<Output = Result<FetchedDocument, FetchFailure>> + Send + 'a>>;
}
