//! Model-facing `web_search` arguments and provider-neutral argument helpers.
//!
//! Everything here is independent of a concrete search backend: parsing and
//! validating what the model sent, the typed [`Freshness`] filter, the result
//! count and domain lists every provider request starts from
//! ([`SearchScope`]), the log of arguments a provider drops, and result
//! retention under the shared [`DomainFilter`] for providers without
//! server-side domain filtering.

use std::fmt;
use std::str::FromStr;

use chrono::NaiveDate;
use serde::Deserialize;

use super::WebSearchResult;
use crate::config::WebSearchProviderKind;
use crate::tool::domain_policy::DomainFilter;
use crate::tool::handler::ToolError;
use crate::types::tools::{WebSearchContextSize, WebSearchToolParam};

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

/// How many provider searches a `web_search` call asks for: the unit a search
/// budget counts. Zero when the arguments do not parse, because the handler
/// then fails before any provider request.
pub(crate) fn requested_searches(arguments: &str) -> usize {
    WebSearchArguments::from_json(arguments).map_or(0, |arguments| arguments.queries().len())
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
            _ => match parse_freshness_range(value) {
                Some(Self::Range { from, to }) if from > to => Err(ToolError::Config(format!(
                    "web_search freshness range starts after it ends: {} is after {}",
                    from.format(DATE_FORMAT),
                    to.format(DATE_FORMAT)
                ))),
                Some(range) => Ok(range),
                None => Err(ToolError::Config(
                    "web_search freshness must be day, week, month, year, or YYYY-MM-DDtoYYYY-MM-DD".to_owned(),
                )),
            },
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

/// The result count and domain lists a provider request is built from, derived
/// the same way for every provider.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SearchScope {
    pub(crate) count: Option<u8>,
    pub(crate) include_domains: Option<Vec<String>>,
    pub(crate) exclude_domains: Option<Vec<String>>,
}

impl SearchScope {
    /// `count` falls back to the default of the tool's `search_context_size`,
    /// must be within 1..=100, and is then clamped to `max_count`: the model
    /// cannot know provider limits, so a larger request is clamped rather than
    /// rejected. Domain filters in the tool configuration win over the model's
    /// lists, and an allowlist cannot be combined with a blocklist or
    /// `boost_domains`.
    pub(crate) fn from_args_and_config(
        args: &WebSearchArguments,
        config: &WebSearchToolParam,
        max_count: Option<u8>,
    ) -> Result<Self, ToolError> {
        let count = args
            .count
            .or_else(|| {
                config
                    .search_context_size
                    .map(WebSearchContextSize::default_count)
                    .map(u16::from)
            })
            .map(validate_count)
            .transpose()?
            .map(|count| clamp_count(count, max_count));
        let config_domains = config
            .filters
            .as_ref()
            .and_then(|filters| clean_vec(filters.allowed_domains.as_deref()));
        let config_blocked_domains = config
            .filters
            .as_ref()
            .and_then(|filters| clean_vec(filters.blocked_domains.as_deref()));
        let include_domains = config_domains.or_else(|| args.include_domains.clone());
        let exclude_domains = config_blocked_domains.or_else(|| args.exclude_domains.clone());
        if include_domains.is_some() && (exclude_domains.is_some() || args.boost_domains.is_some()) {
            return Err(ToolError::Config(
                "include_domains cannot be combined with exclude_domains or boost_domains".to_owned(),
            ));
        }
        Ok(Self {
            count,
            include_domains,
            exclude_domains,
        })
    }

    /// The client-side filter for these domain lists.
    pub(crate) fn domain_filter(&self) -> DomainFilter {
        DomainFilter::new(self.include_domains.as_deref(), self.exclude_domains.as_deref())
    }
}

fn clamp_count(count: u8, max_count: Option<u8>) -> u8 {
    match max_count {
        Some(max) if count > max => {
            tracing::debug!(
                requested = count,
                max,
                "clamped web_search count to the provider maximum"
            );
            max
        }
        _ => count,
    }
}

/// A `web_search` argument that some providers have no parameter for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchArgument {
    Country,
    Language,
    Safesearch,
    Freshness,
    Livecrawl,
    LivecrawlFormats,
    CrawlTimeout,
    BoostDomains,
}

impl SearchArgument {
    const fn name(self) -> &'static str {
        match self {
            Self::Country => "country",
            Self::Language => "language",
            Self::Safesearch => "safesearch",
            Self::Freshness => "freshness",
            Self::Livecrawl => "livecrawl",
            Self::LivecrawlFormats => "livecrawl_formats",
            Self::CrawlTimeout => "crawl_timeout",
            Self::BoostDomains => "boost_domains",
        }
    }

    /// Whether the model or, for `country`, the tool's `user_location` set it.
    fn is_set(self, args: &WebSearchArguments, config: &WebSearchToolParam) -> bool {
        match self {
            Self::Country => {
                args.country.is_some()
                    || config
                        .user_location
                        .as_ref()
                        .is_some_and(|location| clean_string(location.country.as_deref()).is_some())
            }
            Self::Language => args.language.is_some(),
            Self::Safesearch => args.safesearch.is_some(),
            Self::Freshness => args.freshness.is_some(),
            Self::Livecrawl => args.livecrawl.is_some(),
            Self::LivecrawlFormats => args.livecrawl_formats.is_some(),
            Self::CrawlTimeout => args.crawl_timeout.is_some(),
            Self::BoostDomains => args.boost_domains.is_some(),
        }
    }
}

/// The `unsupported` arguments that are set, in the order given.
fn ignored_arguments(
    args: &WebSearchArguments,
    config: &WebSearchToolParam,
    unsupported: &[SearchArgument],
) -> Vec<&'static str> {
    unsupported
        .iter()
        .filter(|argument| argument.is_set(args, config))
        .map(|argument| argument.name())
        .collect()
}

/// Logs the `unsupported` arguments the provider drops because it has no
/// parameter for them.
pub(crate) fn log_ignored_arguments(
    provider: WebSearchProviderKind,
    args: &WebSearchArguments,
    config: &WebSearchToolParam,
    unsupported: &[SearchArgument],
) {
    let ignored = ignored_arguments(args, config, unsupported);
    if !ignored.is_empty() {
        tracing::debug!(arguments = ?ignored, "ignored web_search arguments without a {provider} equivalent");
    }
}

/// Drops the results a declaration's domain filter refuses. A result without a
/// parseable URL is dropped whenever a filter is active (fail closed); with no
/// filter the results are untouched.
pub(crate) fn retain_allowed_results(filter: &DomainFilter, results: &mut Vec<WebSearchResult>) {
    if !filter.is_empty() {
        results.retain(|result| filter.allows(&result.url));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tools::{DomainFilters, WebSearchUserLocation};

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
    fn requested_searches_counts_what_the_handler_would_run() {
        assert_eq!(requested_searches(r#"{"query":"one"}"#), 1);
        assert_eq!(requested_searches(r#"{"queries":["a","b","c"]}"#), 3);
        // `queries` wins over `query`, and blank entries are never searched.
        assert_eq!(requested_searches(r#"{"query":"potato","queries":[" a ","","b"]}"#), 2);
        assert_eq!(requested_searches(r#"{"query":"potato","queries":["  "]}"#), 1);
        // Arguments that do not parse never reach a provider.
        for rejected in [
            r#"{"query":"  "}"#,
            r#"{"queries":["1","2","3","4","5","6"]}"#,
            r#"{"query":"q","freshness":"never"}"#,
            "{not json",
        ] {
            assert!(WebSearchArguments::from_json(rejected).is_err(), "{rejected}");
            assert_eq!(requested_searches(rejected), 0, "{rejected}");
        }
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
    fn freshness_rejects_a_range_that_ends_before_it_starts() {
        let error = "2024-02-29to2024-01-05"
            .parse::<Freshness>()
            .expect_err("reversed range");
        assert_eq!(
            error.to_string(),
            "invalid tool config: web_search freshness range starts after it ends: 2024-02-29 is after 2024-01-05"
        );
        let error = WebSearchArguments::from_json(r#"{"query":"q","freshness":"2024-02-29to2024-01-05"}"#).unwrap_err();
        assert!(error.to_string().contains("starts after it ends"));

        // A single-day range is inclusive on both ends and stays valid.
        let day = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        assert_eq!(
            "2024-02-29to2024-02-29".parse::<Freshness>().unwrap(),
            Freshness::Range { from: day, to: day }
        );
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

    fn scope(json: &str, config: &WebSearchToolParam, max_count: Option<u8>) -> Result<SearchScope, ToolError> {
        SearchScope::from_args_and_config(&WebSearchArguments::from_json(json).unwrap(), config, max_count)
    }

    #[test]
    fn scope_count_defaults_from_context_size_then_validates_and_clamps() {
        let default = WebSearchToolParam::default();
        assert_eq!(
            scope(r#"{"query":"q"}"#, &default, Some(20)).unwrap(),
            SearchScope::default()
        );
        assert_eq!(
            scope(r#"{"query":"q","count":50}"#, &default, Some(20)).unwrap().count,
            Some(20)
        );
        assert_eq!(
            scope(r#"{"query":"q","count":20}"#, &default, Some(20)).unwrap().count,
            Some(20)
        );
        assert_eq!(
            scope(r#"{"query":"q","count":100}"#, &default, None).unwrap().count,
            Some(100)
        );

        let high = WebSearchToolParam {
            search_context_size: Some(WebSearchContextSize::High),
            ..WebSearchToolParam::default()
        };
        assert_eq!(
            scope(r#"{"query":"q"}"#, &high, Some(20)).unwrap().count,
            Some(WebSearchContextSize::High.default_count())
        );
        assert_eq!(
            scope(r#"{"query":"q","count":4}"#, &high, Some(20)).unwrap().count,
            Some(4)
        );

        for count in [0, 101] {
            let error = scope(&format!(r#"{{"query":"q","count":{count}}}"#), &default, Some(20)).unwrap_err();
            assert_eq!(
                error.to_string(),
                "invalid tool config: web_search count must be between 1 and 100"
            );
        }
    }

    #[test]
    fn scope_prefers_tool_config_domains_and_rejects_conflicting_lists() {
        let config = WebSearchToolParam {
            filters: Some(DomainFilters {
                allowed_domains: Some(vec![" rust-lang.org ".to_owned(), String::new()]),
                blocked_domains: Some(vec!["  ".to_owned()]),
            }),
            ..WebSearchToolParam::default()
        };
        let error = scope(
            r#"{"query":"q","include_domains":["example.com"],"exclude_domains":["spam.example"]}"#,
            &config,
            None,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid tool config: include_domains cannot be combined with exclude_domains or boost_domains",
            "a blank config blocklist falls back to the model's list"
        );

        let scoped = scope(r#"{"query":"q","include_domains":["example.com"]}"#, &config, None).unwrap();
        assert_eq!(scoped.include_domains, Some(vec!["rust-lang.org".to_owned()]));
        assert_eq!(scoped.exclude_domains, None);
        let filter = scoped.domain_filter();
        assert!(filter.allows("https://doc.rust-lang.org/book"));
        assert!(!filter.allows("https://example.com/"));
    }

    #[test]
    fn scope_rejects_an_allowlist_with_boost_domains() {
        let error = scope(
            r#"{"query":"q","include_domains":["a.com"],"boost_domains":["b.com"]}"#,
            &WebSearchToolParam::default(),
            None,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid tool config: include_domains cannot be combined with exclude_domains or boost_domains"
        );

        let scoped = scope(
            r#"{"query":"q","exclude_domains":["b.com"]}"#,
            &WebSearchToolParam::default(),
            None,
        )
        .unwrap();
        assert_eq!(scoped.exclude_domains, Some(vec!["b.com".to_owned()]));
        assert!(!scoped.domain_filter().allows("https://b.com/x"));
    }

    #[test]
    fn ignored_arguments_lists_only_the_set_ones_in_the_given_order() {
        use SearchArgument::{
            BoostDomains, Country, CrawlTimeout, Freshness, Language, Livecrawl, LivecrawlFormats, Safesearch,
        };

        let all = [
            Country,
            Language,
            Safesearch,
            Freshness,
            Livecrawl,
            LivecrawlFormats,
            CrawlTimeout,
            BoostDomains,
        ];
        let args = WebSearchArguments::from_json(
            r#"{"query":"q","country":"us","language":"en","safesearch":"off","freshness":"day","livecrawl":"web",
                "livecrawl_formats":["markdown"],"crawl_timeout":5,"boost_domains":["a.com"]}"#,
        )
        .unwrap();
        let config = WebSearchToolParam::default();
        assert_eq!(
            ignored_arguments(&args, &config, &all),
            [
                "country",
                "language",
                "safesearch",
                "freshness",
                "livecrawl",
                "livecrawl_formats",
                "crawl_timeout",
                "boost_domains"
            ]
        );
        assert_eq!(
            ignored_arguments(&args, &config, &[BoostDomains, Country]),
            ["boost_domains", "country"]
        );

        let bare = WebSearchArguments::from_json(r#"{"query":"q"}"#).unwrap();
        assert!(ignored_arguments(&bare, &config, &all).is_empty());

        let located = WebSearchToolParam {
            user_location: Some(WebSearchUserLocation {
                country: Some("gb".to_owned()),
                ..WebSearchUserLocation::default()
            }),
            ..WebSearchToolParam::default()
        };
        assert_eq!(ignored_arguments(&bare, &located, &all), ["country"]);
        let blank = WebSearchToolParam {
            user_location: Some(WebSearchUserLocation {
                country: Some("  ".to_owned()),
                ..WebSearchUserLocation::default()
            }),
            ..WebSearchToolParam::default()
        };
        assert!(ignored_arguments(&bare, &blank, &all).is_empty());
    }

    #[test]
    fn retaining_results_is_a_no_op_without_a_filter_and_fails_closed_with_one() {
        let mut results = vec![result("https://a.example/"), result("not a url")];
        retain_allowed_results(&DomainFilter::default(), &mut results);
        assert_eq!(results.len(), 2);

        retain_allowed_results(&DomainFilter::new(None, Some(&["b.example".to_owned()])), &mut results);
        assert_eq!(results.len(), 1, "blocklist drops the uncheckable result");
        assert_eq!(results[0].url, "https://a.example/");

        retain_allowed_results(&DomainFilter::new(Some(&["a.example".to_owned()]), None), &mut results);
        assert_eq!(results.len(), 1);
    }
}
