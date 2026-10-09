//! SearXNG endpoint validation and the redirect-free client every request
//! goes through.
//!
//! Split out of the parent module to keep it under the production-line limit
//! (#319); nothing here is specific to query shaping or response parsing.

use crate::error::Error;
use crate::tool::handler::ToolError;

/// Operator-facing fix for a missing SearXNG endpoint, shared by the startup
/// check in `agentic-server` and the execution-time fallback in the parent module.
pub const SEARXNG_BASE_URL_HINT: &str = "SearXNG requires a base URL; set AGENTIC_WEB_SEARCH_BASE_URL or [web_search] base_url \
     (for example http://searxng:8080)";

const SEARCH_PATH: &str = "search";

/// Checks that a configured SearXNG endpoint can be addressed: an absolute
/// `http`/`https` URL with a host and no query or fragment, because the
/// provider appends `/search` to its path. A blank or missing value is
/// reported with [`SEARXNG_BASE_URL_HINT`]. Shared by the `agentic-server`
/// startup check and the provider so both reject the same inputs.
///
/// # Errors
///
/// Returns [`Error::Config`] with the operator-facing message when the value
/// is blank, not an absolute `http(s)` URL with a host, or carries a query or
/// fragment.
pub fn validate_searxng_base_url(value: Option<&str>) -> Result<(), Error> {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    let Some(value) = value else {
        return Err(Error::Config(SEARXNG_BASE_URL_HINT.to_owned()));
    };
    parse_base_url(value).map(drop).map_err(Error::Config)
}

/// Parses a non-blank endpoint, producing the operator-facing message on failure.
fn parse_base_url(value: &str) -> Result<url::Url, String> {
    match url::Url::parse(value) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.has_host() => {
            if url.query().is_some() || url.fragment().is_some() {
                return Err(format!(
                    "SearXNG base URL {value:?} must not contain a query or fragment; the gateway appends /search to \
                     its path"
                ));
            }
            Ok(url)
        }
        _ => Err(format!(
            "SearXNG base URL {value:?} must be an absolute http(s) URL such as http://searxng:8080"
        )),
    }
}

/// Builds the `/search` endpoint under the configured base path, so a
/// sub-path mount such as `http://host/searxng` resolves to
/// `http://host/searxng/search`.
pub(super) fn search_endpoint(base_url: &str) -> Result<url::Url, ToolError> {
    let mut url = parse_base_url(base_url).map_err(ToolError::Config)?;
    let path = format!("{}/{SEARCH_PATH}", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url)
}

/// Builds the HTTP client used for every SearXNG request, with redirects
/// disabled.
///
/// SearXNG answers an external "bang" query (`!!g rust`, `!ddg rust`, any of
/// its 13,000+ pinned bangs) with a redirect to the named external engine
/// *before* it looks at `format=json`. The gateway's shared client follows
/// redirects by default, so without this override the gateway itself would
/// send the model's query — written by the model, requested by the user, or
/// injected through content the model read — to whichever external engine
/// the bang names, straight past the configured, trusted SearXNG instance.
/// Restricting that instance to internal engines does not help: bangs never
/// consult the enabled-engines list. A redirect is therefore always rejected
/// by the parent module's status handling rather than followed.
///
/// This provider does not share the gateway's client, so it also does not
/// share that client's connection pool; that is immaterial for the small,
/// steady request volume a single self-hosted instance sees. Building a
/// dedicated client here can only fail the way `reqwest::Client::new()` can
/// (TLS backend / resolver initialization), an environment invariant every
/// other unconditional `reqwest::Client::new()` call in this gateway already
/// relies on.
pub(super) fn redirect_free_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest TLS backend initialization is an environment invariant, not a SearXNG-specific condition")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_endpoint_appends_search_under_the_base_path() {
        assert_eq!(
            search_endpoint("http://searxng:8080").unwrap().as_str(),
            "http://searxng:8080/search"
        );
        assert_eq!(
            search_endpoint("https://search.internal/searxng").unwrap().as_str(),
            "https://search.internal/searxng/search"
        );
        assert_eq!(
            search_endpoint("http://[::1]:8080/a/b").unwrap().as_str(),
            "http://[::1]:8080/a/b/search"
        );
        for invalid in [
            "searxng:8080",
            "ftp://searxng",
            "http://",
            "http://host?x=y",
            "http://host/#top",
        ] {
            let error = search_endpoint(invalid).expect_err(invalid).to_string();
            assert!(error.starts_with("invalid tool config: SearXNG base URL"), "{error}");
        }
    }

    #[test]
    fn validate_base_url_shares_the_provider_rules() {
        assert!(validate_searxng_base_url(Some(" http://searxng:8080/ ")).is_ok());
        assert_eq!(
            validate_searxng_base_url(None).unwrap_err().to_string(),
            SEARXNG_BASE_URL_HINT
        );
        assert_eq!(
            validate_searxng_base_url(Some("  ")).unwrap_err().to_string(),
            SEARXNG_BASE_URL_HINT
        );
        assert_eq!(
            validate_searxng_base_url(Some("http://host?x=y"))
                .unwrap_err()
                .to_string(),
            "SearXNG base URL \"http://host?x=y\" must not contain a query or fragment; the gateway appends /search \
             to its path"
        );
        assert_eq!(
            validate_searxng_base_url(Some("/searxng")).unwrap_err().to_string(),
            "SearXNG base URL \"/searxng\" must be an absolute http(s) URL such as http://searxng:8080"
        );
    }
}
