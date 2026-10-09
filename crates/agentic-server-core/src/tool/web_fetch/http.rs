//! The built-in fetch backend: a bounded HTTP GET whose redirects are
//! re-validated hop by hop.
//!
//! Every hop resolves the host through [`policy::resolve_addrs`] and pins the
//! connection to the addresses that passed, so a name cannot re-resolve to a
//! non-public address after the check. Redirects are not followed by the HTTP
//! client; each `Location` is parsed, admitted, and domain-filtered like the
//! original URL. One absolute deadline covers name resolution, every request,
//! and the body read. The body is read up to the configured ceiling and
//! decoded with the charset the response declares.
//!
//! Under the default public-only policy the client ignores the proxies the
//! environment configures and connects directly, so the address check and the
//! pin apply to the real destination; a proxy would resolve the name itself. A
//! backend allowed to reach private networks honours the environment proxies
//! like the rest of the gateway.

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::time::{Duration, Instant};

use futures::StreamExt;
use http::header::{ACCEPT, CONTENT_TYPE, LOCATION};
use reqwest::StatusCode;
use reqwest::redirect::Policy;
use url::{Host, Url};

use super::WebFetchErrorCode;
use super::backend::{FetchFailure, FetchedDocument, WebFetchBackend};
use super::extract::{sniff_html_charset, truncate_to_char_boundary};
use super::policy::{UrlRejection, is_public_ip, resolve_addrs, validate_url};
use crate::config::WebFetchConfig;
use crate::tool::domain_policy::DomainFilter;

const USER_AGENT: &str = concat!("agentic-api/", env!("CARGO_PKG_VERSION"));
const ACCEPT_VALUE: &str =
    "text/html, application/xhtml+xml, text/plain;q=0.9, text/*;q=0.8, application/json;q=0.5, */*;q=0.1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MEDIA_TYPE: &str = "text/plain";
/// Longest media type honoured; a longer `Content-Type` is not a media type.
const MAX_MEDIA_TYPE_LEN: usize = 128;

/// The default backend, configured once per gateway.
#[derive(Debug, Clone)]
pub(crate) struct HttpFetchBackend {
    config: WebFetchConfig,
    /// Which resolved addresses may be contacted: every address when the
    /// operator allowed private networks, otherwise [`is_public_ip`].
    admit: fn(IpAddr) -> bool,
}

/// The address policy of a backend that may reach private networks.
fn admit_any(_ip: IpAddr) -> bool {
    true
}

impl HttpFetchBackend {
    pub(crate) const fn new(config: WebFetchConfig) -> Self {
        let admit: fn(IpAddr) -> bool = if config.allow_private_networks {
            admit_any
        } else {
            is_public_ip
        };
        Self { config, admit }
    }

    /// A backend with `admit` as its address policy, for tests whose origin
    /// the real policy would refuse.
    #[cfg(test)]
    const fn with_address_policy(config: WebFetchConfig, admit: fn(IpAddr) -> bool) -> Self {
        Self { config, admit }
    }

    /// Whether the client may use the proxies the environment configures. Only
    /// a backend allowed to reach private networks does: a proxy resolves the
    /// name itself, which would put the address check and the pin out of reach.
    const fn honours_proxies(&self) -> bool {
        self.config.allow_private_networks
    }

    async fn fetch_following_redirects(
        &self,
        mut url: Url,
        filter: &DomainFilter,
    ) -> Result<FetchedDocument, FetchFailure> {
        let deadline = Instant::now() + self.config.timeout;
        for hop in 0..=self.config.max_redirects {
            if !filter.allows(url.as_str()) {
                let host = url.host_str().unwrap_or_default().to_owned();
                return Err(refused_on(hop, FetchFailure::OutsideDomains { host }));
            }
            let response = self
                .request(&url, deadline)
                .await
                .map_err(|failure| refused_on(hop, failure))?;
            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or(FetchFailure::RedirectWithoutLocation { status })?;
                let target = url.join(location).map_err(FetchFailure::UnparseableRedirect)?;
                url = validate_url(target.as_str()).map_err(redirect_rejection)?;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS {
                return Err(FetchFailure::TooManyRequests);
            }
            if !status.is_success() {
                return Err(FetchFailure::Status(status));
            }
            return self.read_document(url, response, deadline).await;
        }
        Err(FetchFailure::TooManyRedirects(self.config.max_redirects))
    }

    /// Send one GET for `url` within `deadline`, with the connection pinned to
    /// the admitted addresses. Resolution counts against the same deadline.
    async fn request(&self, url: &Url, deadline: Instant) -> Result<reqwest::Response, FetchFailure> {
        let host = url.host().ok_or(FetchFailure::NoHost)?;
        let port = url.port_or_known_default().unwrap_or(80);
        let addrs = tokio::time::timeout_at(deadline.into(), resolve_addrs(&host, port, self.admit))
            .await
            .map_err(|_| FetchFailure::TimedOut)??;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(FetchFailure::TimedOut);
        }
        let mut builder = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(remaining)
            .connect_timeout(CONNECT_TIMEOUT.min(remaining))
            .user_agent(USER_AGENT);
        if !self.honours_proxies() {
            builder = builder.no_proxy();
        }
        if let Host::Domain(name) = host {
            builder = builder.resolve_to_addrs(name, &addrs);
        }
        let client = builder.build().map_err(FetchFailure::Client)?;
        let sent = tokio::time::timeout_at(
            deadline.into(),
            client.get(url.clone()).header(ACCEPT, ACCEPT_VALUE).send(),
        )
        .await
        .map_err(|_| FetchFailure::TimedOut)?;
        sent.map_err(|error| {
            if error.is_timeout() {
                FetchFailure::TimedOut
            } else {
                FetchFailure::Request(error)
            }
        })
    }

    async fn read_document(
        &self,
        url: Url,
        response: reqwest::Response,
        deadline: Instant,
    ) -> Result<FetchedDocument, FetchFailure> {
        let header = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let (mut media_type, charset) = parse_content_type(header.as_deref());
        if !is_supported_media_type(&media_type) {
            let shown = truncate_to_char_boundary(&media_type, MAX_MEDIA_TYPE_LEN).to_owned();
            return Err(FetchFailure::UnsupportedContentType(shown));
        }
        let (bytes, truncated) = read_bounded(response, self.config.max_response_bytes.get(), deadline).await?;
        if header.is_none() && looks_like_html(&bytes) {
            "text/html".clone_into(&mut media_type);
        }
        let body = decode_body(&bytes, charset.as_deref(), media_type == "text/html");
        Ok(FetchedDocument {
            url,
            media_type,
            body,
            truncated,
        })
    }
}

impl WebFetchBackend for HttpFetchBackend {
    fn fetch<'a>(
        &'a self,
        url: &'a Url,
        filter: &'a DomainFilter,
    ) -> Pin<Box<dyn Future<Output = Result<FetchedDocument, FetchFailure>> + Send + 'a>> {
        Box::pin(self.fetch_following_redirects(url.clone(), filter))
    }
}

/// A policy refusal on a later hop is reported as a refused redirect target,
/// so the model can tell the hop from the URL it asked for.
fn refused_on(hop: u8, failure: FetchFailure) -> FetchFailure {
    match failure {
        FetchFailure::NotPublic { .. } | FetchFailure::OutsideDomains { .. } if hop > 0 => {
            FetchFailure::RedirectRefused(Box::new(failure))
        }
        failure => failure,
    }
}

/// A redirect target that failed URL admission: a refusal (credentials) keeps
/// its code, anything else makes the page not accessible.
fn redirect_rejection(rejection: UrlRejection) -> FetchFailure {
    match rejection.code() {
        WebFetchErrorCode::UrlNotAllowed => FetchFailure::RedirectRefused(Box::new(FetchFailure::Rejected(rejection))),
        _ => FetchFailure::InvalidRedirect(rejection),
    }
}

/// Split a `Content-Type` header into its lowercase media type and charset.
/// A missing header counts as plain text.
fn parse_content_type(header: Option<&str>) -> (String, Option<String>) {
    let Some(header) = header else {
        return (DEFAULT_MEDIA_TYPE.to_owned(), None);
    };
    let mut parts = header.split(';');
    let media_type = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
    let charset = parts.find_map(|parameter| {
        let (key, value) = parameter.split_once('=')?;
        key.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_ascii_lowercase())
    });
    let media_type = if media_type.is_empty() {
        DEFAULT_MEDIA_TYPE.to_owned()
    } else {
        media_type
    };
    (media_type, charset.filter(|value| !value.is_empty()))
}

/// Text, HTML, and the structured text forms a page may answer with. PDF and
/// binary types are refused with `unsupported_content_type`.
fn is_supported_media_type(media_type: &str) -> bool {
    media_type.len() <= MAX_MEDIA_TYPE_LEN
        && (media_type.starts_with("text/")
            || matches!(
                media_type,
                "application/xhtml+xml" | "application/xml" | "application/json" | "application/ld+json"
            ))
}

/// Whether a body served without a `Content-Type` is an HTML document.
fn looks_like_html(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
    let head = head.trim_start();
    head.starts_with("<!doctype html") || head.starts_with("<html")
}

/// Read a body up to `limit` bytes, stopping at the deadline; a longer body is
/// cut and reported as truncated rather than failed.
async fn read_bounded(
    response: reqwest::Response,
    limit: usize,
    deadline: Instant,
) -> Result<(Vec<u8>, bool), FetchFailure> {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        let next = tokio::time::timeout_at(deadline.into(), stream.next())
            .await
            .map_err(|_| FetchFailure::TimedOut)?;
        let Some(chunk) = next else {
            return Ok((body, false));
        };
        let chunk = chunk.map_err(FetchFailure::Body)?;
        let room = limit.saturating_sub(body.len());
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
}

/// Decode a body with its declared charset, an HTML `<meta>` charset, or UTF-8.
fn decode_body(bytes: &[u8], charset: Option<&str>, html: bool) -> String {
    let encoding = charset
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .or_else(|| {
            html.then(|| sniff_html_charset(bytes))
                .flatten()
                .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        })
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(bytes);
    text.into_owned()
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use tokio::net::TcpListener;

    use super::*;

    /// `127.0.0.2` stands in for an internal address that the test origin on
    /// `127.0.0.1` can redirect to; nothing listens there.
    const STAND_IN_INTERNAL: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 2);

    fn admit_all_but_the_stand_in(ip: IpAddr) -> bool {
        ip != IpAddr::V4(STAND_IN_INTERNAL)
    }

    /// Redirects every request to the stand-in internal address, counting them.
    async fn hop(State((hits, port)): State<(Arc<AtomicUsize>, u16)>) -> Response {
        hits.fetch_add(1, Ordering::SeqCst);
        (
            StatusCode::FOUND,
            [("location", format!("http://{STAND_IN_INTERNAL}:{port}/internal"))],
        )
            .into_response()
    }

    /// Answers after a delay longer than any deadline a test configures.
    async fn slow() -> &'static str {
        tokio::time::sleep(Duration::from_secs(2)).await;
        "late"
    }

    /// A loopback origin; returns its request counter and port.
    async fn origin() -> (Arc<AtomicUsize>, u16) {
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/hop", get(hop))
            .route("/slow", get(slow))
            .with_state((Arc::clone(&hits), port));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (hits, port)
    }

    #[tokio::test]
    async fn an_internal_address_is_refused_directly_and_through_a_redirect() {
        let (hits, port) = origin().await;
        let backend = HttpFetchBackend::with_address_policy(WebFetchConfig::default(), admit_all_but_the_stand_in);
        let filter = DomainFilter::new(None, None);

        let direct = Url::parse(&format!("http://{STAND_IN_INTERNAL}:{port}/internal")).unwrap();
        let error = backend.fetch(&direct, &filter).await.unwrap_err();
        assert!(
            matches!(&error, FetchFailure::NotPublic { host } if host == "127.0.0.2"),
            "{error:?}"
        );
        assert_eq!(error.to_string(), "127.0.0.2 is not a public address");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "refused before any connection");

        let through_redirect = Url::parse(&format!("http://127.0.0.1:{port}/hop")).unwrap();
        let error = backend.fetch(&through_redirect, &filter).await.unwrap_err();
        assert!(
            matches!(
                &error,
                FetchFailure::RedirectRefused(inner)
                    if matches!(inner.as_ref(), FetchFailure::NotPublic { host } if host == "127.0.0.2")
            ),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            "redirect target refused: 127.0.0.2 is not a public address"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1, "only the first hop was requested");
    }

    #[tokio::test]
    async fn the_deadline_bounds_a_slow_origin() {
        let (_hits, port) = origin().await;
        let config = WebFetchConfig::default()
            .with_allow_private_networks(true)
            .with_timeout(Duration::from_millis(50));
        let backend = HttpFetchBackend::new(config);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/slow")).unwrap();
        let started = Instant::now();
        let error = backend.fetch(&url, &DomainFilter::new(None, None)).await.unwrap_err();
        assert!(matches!(error, FetchFailure::TimedOut), "{error:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the fetch stopped at its deadline, not when the origin answered: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_default_policy_follows_the_private_network_switch() {
        let strict = HttpFetchBackend::new(WebFetchConfig::default());
        assert!(!(strict.admit)(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!((strict.admit)(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!strict.honours_proxies(), "public-only fetches connect directly");
        let open = HttpFetchBackend::new(WebFetchConfig::default().with_allow_private_networks(true));
        assert!((open.admit)(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(open.honours_proxies());
    }

    #[test]
    fn content_type_parsing_lowercases_and_extracts_charset() {
        assert_eq!(
            parse_content_type(Some("Text/HTML; Charset=\"ISO-8859-1\"")),
            ("text/html".to_owned(), Some("iso-8859-1".to_owned()))
        );
        assert_eq!(
            parse_content_type(Some("application/json;charset=utf-8; boundary=x")),
            ("application/json".to_owned(), Some("utf-8".to_owned()))
        );
        assert_eq!(parse_content_type(Some("text/plain")), ("text/plain".to_owned(), None));
        assert_eq!(parse_content_type(Some("; charset=")), ("text/plain".to_owned(), None));
        assert_eq!(parse_content_type(None), ("text/plain".to_owned(), None));
    }

    #[test]
    fn supported_media_types_are_text_forms_only() {
        for supported in [
            "text/html",
            "text/plain",
            "text/markdown",
            "application/xhtml+xml",
            "application/json",
        ] {
            assert!(is_supported_media_type(supported), "{supported}");
        }
        for refused in ["application/pdf", "image/png", "application/octet-stream", "audio/mpeg"] {
            assert!(!is_supported_media_type(refused), "{refused}");
        }
        assert!(
            !is_supported_media_type(&format!("text/{}", "x".repeat(MAX_MEDIA_TYPE_LEN))),
            "an overlong media type is not a media type"
        );
    }

    #[test]
    fn bodies_without_a_content_type_are_sniffed_for_html() {
        assert!(looks_like_html(b"  <!DOCTYPE html><html>"));
        assert!(looks_like_html(b"<HTML lang=en>"));
        assert!(!looks_like_html(b"{\"json\": true}"));
        assert!(!looks_like_html(b"plain text with <b>markup</b> later"));
    }

    #[test]
    fn body_decoding_honors_declared_sniffed_and_default_charsets() {
        let latin1 = b"caf\xe9";
        assert_eq!(decode_body(latin1, Some("iso-8859-1"), false), "café");
        assert_eq!(decode_body(latin1, Some("latin1"), false), "café");
        let sniffed = b"<meta charset=\"windows-1252\"><p>caf\xe9</p>";
        assert_eq!(
            decode_body(sniffed, None, true),
            "<meta charset=\"windows-1252\"><p>café</p>"
        );
        assert_eq!(
            decode_body(sniffed, None, false),
            "<meta charset=\"windows-1252\"><p>caf\u{fffd}</p>"
        );
        assert_eq!(decode_body("naïve".as_bytes(), Some("not-a-charset"), false), "naïve");
    }

    #[test]
    fn redirect_targets_keep_a_refusal_and_make_the_rest_inaccessible() {
        let refused = redirect_rejection(UrlRejection::Credentials);
        assert!(matches!(
            &refused,
            FetchFailure::RedirectRefused(inner) if matches!(inner.as_ref(), FetchFailure::Rejected(UrlRejection::Credentials))
        ));
        assert_eq!(refused.to_string(), "redirect target refused: url carries credentials");

        let invalid = redirect_rejection(UrlRejection::Scheme("ftp".to_owned()));
        assert!(matches!(
            invalid,
            FetchFailure::InvalidRedirect(UrlRejection::Scheme(_))
        ));
        assert_eq!(
            invalid.to_string(),
            "invalid redirect target: url scheme \"ftp\" is not http or https"
        );
        assert!(matches!(
            redirect_rejection(UrlRejection::TooLong),
            FetchFailure::InvalidRedirect(UrlRejection::TooLong)
        ));

        let on_first_hop = refused_on(0, FetchFailure::NotPublic { host: "h".to_owned() });
        assert!(matches!(on_first_hop, FetchFailure::NotPublic { .. }));
        let on_later_hop = refused_on(2, FetchFailure::OutsideDomains { host: "h".to_owned() });
        assert_eq!(
            on_later_hop.to_string(),
            "redirect target refused: h is outside the allowed domains"
        );
        let unrelated = refused_on(2, FetchFailure::TimedOut);
        assert!(matches!(unrelated, FetchFailure::TimedOut));
    }
}
