//! Model-facing `web_search` arguments and provider-neutral argument helpers.
//!
//! Everything here is independent of a concrete search backend: parsing and
//! validating what the model sent, the typed [`Freshness`] filter, and the
//! [`DomainFilter`] post-filter for providers without server-side domain
//! filtering.

use std::fmt;
use std::str::FromStr;

use chrono::NaiveDate;
use serde::Deserialize;

use super::WebSearchResult;
use crate::tool::handler::ToolError;

pub(crate) const MAX_WEB_SEARCH_QUERIES: usize = 5;

/// Validated, whitespace-normalized arguments the model passed to `web_search`.
///
/// Built through [`WebSearchArguments::from_json`]; every string field is
/// trimmed and empty values collapse to `None`, so providers can consume the
/// fields without re-cleaning them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WebSearchArguments {
    queries: Vec<String>,
    pub(crate) count: Option<u16>,
    pub(crate) freshness: Option<Freshness>,
    pub(crate) country: Option<String>,
    pub(crate) language: Option<String>,
    pub(crate) safesearch: Option<String>,
    pub(crate) livecrawl: Option<String>,
    pub(crate) livecrawl_formats: Option<Vec<String>>,
    pub(crate) crawl_timeout: Option<u16>,
    pub(crate) include_domains: Option<Vec<String>>,
    pub(crate) exclude_domains: Option<Vec<String>>,
    pub(crate) boost_domains: Option<Vec<String>>,
}

/// Wire shape of the model's arguments before validation.
#[derive(Debug, Deserialize)]
struct RawWebSearchArguments {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    queries: Option<Vec<String>>,
    count: Option<u16>,
    freshness: Option<String>,
    country: Option<String>,
    language: Option<String>,
    safesearch: Option<String>,
    livecrawl: Option<String>,
    livecrawl_formats: Option<Vec<String>>,
    crawl_timeout: Option<u16>,
    include_domains: Option<Vec<String>>,
    exclude_domains: Option<Vec<String>>,
    boost_domains: Option<Vec<String>>,
}

impl WebSearchArguments {
    pub(crate) fn from_json(arguments: &str) -> Result<Self, ToolError> {
        let raw = serde_json::from_str::<RawWebSearchArguments>(arguments)
            .map_err(|e| ToolError::Config(format!("web_search arguments must be valid JSON: {e}")))?;
        raw.try_into()
    }

    /// The non-empty list of queries to run; `queries` wins over `query`.
    pub(crate) fn queries(&self) -> &[String] {
        &self.queries
    }
}

impl TryFrom<RawWebSearchArguments> for WebSearchArguments {
    type Error = ToolError;

    fn try_from(raw: RawWebSearchArguments) -> Result<Self, ToolError> {
        let queries = clean_vec(raw.queries.as_deref())
            .or_else(|| clean_string(raw.query.as_deref()).map(|query| vec![query]))
            .unwrap_or_default();
        if queries.is_empty() {
            return Err(ToolError::Config(
                "web_search requires a non-empty query or queries".to_owned(),
            ));
        }
        if queries.len() > MAX_WEB_SEARCH_QUERIES {
            return Err(ToolError::Config(format!(
                "web_search accepts at most {MAX_WEB_SEARCH_QUERIES} queries per call"
            )));
        }
        let freshness = clean_string(raw.freshness.as_deref())
            .map(|value| value.parse::<Freshness>())
            .transpose()?;
        Ok(Self {
            queries,
            count: raw.count,
            freshness,
            country: clean_string(raw.country.as_deref()),
            language: clean_string(raw.language.as_deref()),
            safesearch: clean_string(raw.safesearch.as_deref()),
            livecrawl: clean_string(raw.livecrawl.as_deref()),
            livecrawl_formats: clean_vec(raw.livecrawl_formats.as_deref()),
            crawl_timeout: raw.crawl_timeout,
            include_domains: clean_vec(raw.include_domains.as_deref()),
            exclude_domains: clean_vec(raw.exclude_domains.as_deref()),
            boost_domains: clean_vec(raw.boost_domains.as_deref()),
        })
    }
}

/// Recency filter accepted by `web_search`.
///
/// The wire format is `day`, `week`, `month`, `year`, or an inclusive
/// `YYYY-MM-DDtoYYYY-MM-DD` range. [`Display`](fmt::Display) renders exactly
/// that format, so parsing and rendering round-trip byte-for-byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Freshness {
    Day,
    Week,
    Month,
    Year,
    Range { from: NaiveDate, to: NaiveDate },
}

impl FromStr for Freshness {
    type Err = ToolError;

    fn from_str(value: &str) -> Result<Self, ToolError> {
        match value {
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            "month" => Ok(Self::Month),
            "year" => Ok(Self::Year),
            _ => parse_freshness_range(value).ok_or_else(|| {
                ToolError::Config(
                    "web_search freshness must be day, week, month, year, or YYYY-MM-DDtoYYYY-MM-DD".to_owned(),
                )
            }),
        }
    }
}

impl fmt::Display for Freshness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Day => f.write_str("day"),
            Self::Week => f.write_str("week"),
            Self::Month => f.write_str("month"),
            Self::Year => f.write_str("year"),
            Self::Range { from, to } => write!(f, "{}to{}", from.format(DATE_FORMAT), to.format(DATE_FORMAT)),
        }
    }
}

const DATE_FORMAT: &str = "%Y-%m-%d";

fn parse_freshness_range(value: &str) -> Option<Freshness> {
    let (from, to) = value.split_once("to")?;
    Some(Freshness::Range {
        from: parse_date(from)?,
        to: parse_date(to)?,
    })
}

/// Strict zero-padded `YYYY-MM-DD`, so the accepted set equals the rendered set.
fn parse_date(value: &str) -> Option<NaiveDate> {
    let bytes = value.as_bytes();
    let shaped = bytes.len() == 10
        && bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 4 | 7) {
                *byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        });
    if !shaped {
        return None;
    }
    NaiveDate::parse_from_str(value, DATE_FORMAT).ok()
}

pub(crate) fn validate_count(count: u16) -> Result<u8, ToolError> {
    if (1..=100).contains(&count) {
        Ok(u8::try_from(count).expect("validated web_search count must fit in u8"))
    } else {
        Err(ToolError::Config(
            "web_search count must be between 1 and 100".to_owned(),
        ))
    }
}

pub(crate) fn clean_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

pub(crate) fn clean_vec(values: Option<&[String]>) -> Option<Vec<String>> {
    let cleaned: Vec<String> = values
        .unwrap_or_default()
        .iter()
        .filter_map(|value| clean_string(Some(value.as_str())))
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Provider-neutral domain post-filter for providers without server-side
/// `include_domains` / `exclude_domains` support.
///
/// A host matches a domain when it equals the domain or ends with `.{domain}`
/// (label boundary), compared case-insensitively after IDNA normalization. A
/// URL without a parseable host cannot be checked, so it is rejected whenever
/// any allowlist or blocklist is active (fail closed). You.com filters
/// server-side, so this is not applied on that path; Brave Search applies it
/// to every result section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DomainFilter {
    include: Vec<String>,
    exclude: Vec<String>,
}

impl DomainFilter {
    pub(crate) fn new(include: Option<&[String]>, exclude: Option<&[String]>) -> Self {
        Self {
            include: normalize_domains(include),
            exclude: normalize_domains(exclude),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    pub(crate) fn allows(&self, url: &str) -> bool {
        let Some(host) = url_host(url) else {
            return self.is_empty();
        };
        let matches = |domain: &String| host_matches_domain(&host, domain);
        (self.include.is_empty() || self.include.iter().any(matches)) && !self.exclude.iter().any(matches)
    }

    pub(crate) fn retain(&self, results: &mut Vec<WebSearchResult>) {
        if !self.is_empty() {
            results.retain(|result| self.allows(&result.url));
        }
    }
}

fn normalize_domains(domains: Option<&[String]>) -> Vec<String> {
    domains
        .unwrap_or_default()
        .iter()
        .filter_map(|domain| normalize_domain(domain))
        .collect()
}

/// Lowercases and IDNA-normalizes a configured domain. Entries that are not a
/// valid host are kept verbatim (lowercased) so a typo fails closed instead of
/// silently widening an allowlist.
fn normalize_domain(domain: &str) -> Option<String> {
    let trimmed = domain.trim().trim_end_matches('.');
    if trimmed.is_empty() {
        return None;
    }
    Some(url::Host::parse(trimmed).map_or_else(|_| trimmed.to_ascii_lowercase(), |host| host.to_string()))
}

fn url_host(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .host()
        .map(|host| host.to_string().trim_end_matches('.').to_owned())
}

fn host_matches_domain(host: &str, domain: &str) -> bool {
    host == domain || host.strip_suffix(domain).is_some_and(|prefix| prefix.ends_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(url: &str) -> WebSearchResult {
        WebSearchResult {
            url: url.to_owned(),
            ..WebSearchResult::default()
        }
    }

    #[test]
    fn arguments_prefer_cleaned_queries_over_query() {
        let args = WebSearchArguments::from_json(r#"{"query":" potato ","queries":[" a ","","b"]}"#).unwrap();
        assert_eq!(args.queries(), ["a", "b"]);

        let args = WebSearchArguments::from_json(r#"{"query":" potato ","queries":["  "]}"#).unwrap();
        assert_eq!(args.queries(), ["potato"]);
    }

    #[test]
    fn arguments_reject_missing_and_oversized_queries() {
        let error = WebSearchArguments::from_json(r#"{"query":"  "}"#).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid tool config: web_search requires a non-empty query or queries"
        );

        let error = WebSearchArguments::from_json(r#"{"queries":["1","2","3","4","5","6"]}"#).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("invalid tool config: web_search accepts at most {MAX_WEB_SEARCH_QUERIES} queries per call")
        );

        let error = WebSearchArguments::from_json("{not json").unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("invalid tool config: web_search arguments must be valid JSON: ")
        );
    }

    #[test]
    fn arguments_normalize_strings_and_lists() {
        let args = WebSearchArguments::from_json(
            r#"{"query":"q","country":" us ","language":"","include_domains":[" example.com ",""],"exclude_domains":[]}"#,
        )
        .unwrap();
        assert_eq!(args.country.as_deref(), Some("us"));
        assert_eq!(args.language, None);
        assert_eq!(args.include_domains, Some(vec!["example.com".to_owned()]));
        assert_eq!(args.exclude_domains, None);
        assert_eq!(args.freshness, None);
    }

    #[test]
    fn freshness_round_trips_every_accepted_form() {
        for value in ["day", "week", "month", "year", "2024-01-05to2024-02-29"] {
            let parsed: Freshness = value.parse().expect(value);
            assert_eq!(parsed.to_string(), value);
        }
        assert_eq!(
            "2024-01-05to2024-02-29".parse::<Freshness>().unwrap(),
            Freshness::Range {
                from: NaiveDate::from_ymd_opt(2024, 1, 5).unwrap(),
                to: NaiveDate::from_ymd_opt(2024, 2, 29).unwrap(),
            }
        );
    }

    #[test]
    fn freshness_rejects_unknown_and_malformed_values() {
        for value in [
            "Day",
            "pd",
            "2024-1-5to2024-02-01",
            "2024-01-05to2023-02-30",
            "2024-01-05 to 2024-02-01",
            "2024-01-05",
            "",
        ] {
            let error = value.parse::<Freshness>().expect_err(value);
            assert_eq!(
                error.to_string(),
                "invalid tool config: web_search freshness must be day, week, month, year, or YYYY-MM-DDtoYYYY-MM-DD"
            );
        }
    }

    #[test]
    fn arguments_parse_and_blank_freshness() {
        let args = WebSearchArguments::from_json(r#"{"query":"q","freshness":" week "}"#).unwrap();
        assert_eq!(args.freshness, Some(Freshness::Week));

        let args = WebSearchArguments::from_json(r#"{"query":"q","freshness":"  "}"#).unwrap();
        assert_eq!(args.freshness, None);

        let error = WebSearchArguments::from_json(r#"{"query":"q","freshness":"recent"}"#).unwrap_err();
        assert!(error.to_string().contains("web_search freshness must be"));
    }

    #[test]
    fn validate_count_enforces_inclusive_bounds() {
        assert_eq!(validate_count(1).unwrap(), 1);
        assert_eq!(validate_count(100).unwrap(), 100);
        for count in [0, 101, u16::MAX] {
            assert_eq!(
                validate_count(count).unwrap_err().to_string(),
                "invalid tool config: web_search count must be between 1 and 100"
            );
        }
    }

    #[test]
    fn domain_filter_matches_on_label_boundary_case_insensitively() {
        let filter = DomainFilter::new(Some(&["Example.COM.".to_owned()]), None);
        assert!(filter.allows("https://example.com/a"));
        assert!(filter.allows("https://EXAMPLE.com/a"));
        assert!(filter.allows("https://docs.example.com./a"));
        assert!(!filter.allows("https://notexample.com/a"));
        assert!(!filter.allows("https://example.com.evil.net/a"));
        assert!(!filter.allows("https://example.org/a"));
    }

    #[test]
    fn domain_filter_excludes_after_including() {
        let filter = DomainFilter::new(
            Some(&["example.com".to_owned()]),
            Some(&["internal.example.com".to_owned()]),
        );
        assert!(filter.allows("https://www.example.com/"));
        assert!(!filter.allows("https://internal.example.com/"));
        assert!(!filter.allows("https://a.internal.example.com/"));

        let blocklist_only = DomainFilter::new(None, Some(&["example.com".to_owned()]));
        assert!(blocklist_only.allows("https://example.org/"));
        assert!(!blocklist_only.allows("https://sub.example.com/"));
    }

    #[test]
    fn domain_filter_fails_closed_on_unparsable_urls() {
        let unparsable = [
            "not_a_valid_url",
            "not a url",
            "example.com/path",
            "mailto:someone@example.com",
        ];

        let allowlist = DomainFilter::new(Some(&["example.com".to_owned()]), None);
        let blocklist = DomainFilter::new(None, Some(&["example.com".to_owned()]));
        for url in unparsable {
            assert!(!allowlist.allows(url), "allowlist must reject {url:?}");
            assert!(!blocklist.allows(url), "blocklist must reject {url:?}");
        }

        let unfiltered = DomainFilter::default();
        for url in unparsable {
            assert!(unfiltered.allows(url), "no active filter must pass {url:?}");
        }
    }

    #[test]
    fn domain_filter_normalizes_idna_and_fails_closed_on_invalid_entries() {
        let idna = DomainFilter::new(Some(&["Bücher.example".to_owned()]), None);
        assert!(idna.allows("https://shop.bücher.example/"));
        assert!(idna.allows("https://xn--bcher-kva.example/"));

        let invalid = DomainFilter::new(Some(&["https://example.com/path".to_owned()]), None);
        assert!(!invalid.is_empty());
        assert!(!invalid.allows("https://example.com/"));
    }

    #[test]
    fn domain_filter_retain_is_a_no_op_when_empty() {
        let mut results = vec![result("https://a.example/"), result("not a url")];
        DomainFilter::default().retain(&mut results);
        assert_eq!(results.len(), 2);

        DomainFilter::new(None, Some(&["b.example".to_owned()])).retain(&mut results);
        assert_eq!(results.len(), 1, "blocklist drops the uncheckable result");
        assert_eq!(results[0].url, "https://a.example/");

        DomainFilter::new(Some(&["a.example".to_owned()]), None).retain(&mut results);
        assert_eq!(results.len(), 1);
    }
}
