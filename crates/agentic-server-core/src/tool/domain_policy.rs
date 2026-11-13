//! Domain policy shared by the gateway's web tools.
//!
//! A `web_search` or `web_fetch` declaration may name `allowed_domains` and
//! `blocked_domains` ([`DomainFilters`]). This module owns the one parser of
//! such an entry ([`validate_domain_entry`]), the [`DomainFilter`] built from
//! its result that decides whether a URL is admitted (web search applies it to
//! results, web fetch to the requested URL and to every redirect hop), and the
//! declaration check that uses the same parser ([`validate_domain_filters`] for
//! `web_fetch`, the Messages adapter for Anthropic's `web_search`, whose entries
//! may carry a path).

use url::Host;

use super::handler::ToolError;
use crate::types::tools::{DomainFilters, WebSearchToolParam};

/// The rule both lists share, as Anthropic documents it for every server tool;
/// the Messages adapter reports it in the same words.
pub(crate) const EXCLUSIVE_LISTS_RULE: &str = "allowed_domains and blocked_domains cannot be used together";

/// Whether a declared entry may name a path after its host.
///
/// Anthropic's web search documents entries such as `example.com/blog` and
/// `example.com/*/articles`; its web fetch and `OpenAI`'s web search document
/// host names only, so their entries stay host-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum EntryPaths {
    /// The entry must be a host name or address and nothing else.
    #[default]
    Refused,
    /// A path may follow the host; `*` may appear once, in the path only.
    Allowed,
}

impl EntryPaths {
    /// The entry rule of a `web_search` declaration's lists: paths only where
    /// the Messages adapter validated Anthropic entries.
    pub(crate) fn of_web_search(config: &WebSearchToolParam) -> Self {
        if config.path_entries {
            Self::Allowed
        } else {
            Self::Refused
        }
    }
}

/// A parsed `allowed_domains` / `blocked_domains` entry: the host as the URL
/// parser normalizes it (lowercase, IDNA) and, for an [`EntryPaths::Allowed`]
/// entry, the path after it, percent-encoded the way a result URL's path is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DomainEntry {
    host: String,
    /// Starts with `/`; `None` when the entry names every path on the host
    /// (no path, `/`, or `/*`).
    path: Option<String>,
}

/// Parses a declared entry, or explains why it cannot match anything. This is
/// the one parser of the entry format: declarations are validated with it and
/// [`DomainFilter`] builds its rules from its result, so the two cannot
/// disagree.
///
/// The entry is read as the authority and path of an `https` URL, so its host
/// and path are normalized exactly as a result URL's are. Anthropic's rule for
/// wildcards is kept: `*` is refused in the host (`*.example.com`,
/// `ex*.com`) and allowed in the path only.
///
/// # Errors
///
/// The reason, worded to follow the entry (`entry "." is not a host name`).
pub(crate) fn validate_domain_entry(entry: &str, paths: EntryPaths) -> Result<DomainEntry, &'static str> {
    let shape_error = match paths {
        EntryPaths::Refused => "must be a host name without a scheme or path",
        EntryPaths::Allowed => "must be a host name, optionally followed by a path, without a scheme",
    };
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return Err("is empty");
    }
    if trimmed.contains("://") || (paths == EntryPaths::Refused && trimmed.contains('/')) {
        return Err(shape_error);
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err("must not contain whitespace");
    }
    let authority = trimmed.split('/').next().unwrap_or_default();
    // The URL parser would read a port, user info, or a `\` separator out of
    // the authority; none of them belongs to a host name.
    let port = authority.rsplit(']').next().is_some_and(|tail| tail.contains(':'));
    if authority.is_empty() || port || authority.contains(['@', '\\', '?', '#']) {
        return Err("is not a host name");
    }
    if authority.contains('*') {
        return Err(match paths {
            EntryPaths::Refused => "is not a host name",
            EntryPaths::Allowed => "must not use a wildcard (*) in the host, only in the path after it",
        });
    }
    let url = url::Url::parse(&format!("https://{trimmed}")).map_err(|_| "is not a host name")?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(shape_error);
    }
    let host = match url.host() {
        Some(Host::Ipv4(_) | Host::Ipv6(_)) => url.host_str().unwrap_or_default().to_owned(),
        Some(Host::Domain(domain)) => {
            // The URL parser accepts characters no host name carries; a label
            // must be letters, digits, and hyphens.
            let domain = domain.trim_end_matches('.');
            let labels_valid = !domain.is_empty()
                && domain.split('.').all(|label| {
                    !label.is_empty() && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                });
            if !labels_valid {
                return Err("is not a host name");
            }
            domain.to_owned()
        }
        None => return Err("is not a host name"),
    };
    let path = url.path();
    if path.matches('*').count() > 1 {
        return Err("may use the wildcard (*) at most once");
    }
    // A bare `/` or `/*` names every path on the host.
    let path = (path != "/" && path != "/*").then(|| normalize_percent_escapes(path));
    Ok(DomainEntry { host, path })
}

/// Validates a declaration's domain lists for a tool that matches on the host
/// only: the lists are mutually exclusive, and every entry is a host name or
/// address. Errors name the tool, the field, and the entry, so the client can
/// fix the declaration.
///
/// # Errors
///
/// [`ToolError::Config`] with the offending field and entry.
pub(crate) fn validate_domain_filters(tool: &str, filters: Option<&DomainFilters>) -> Result<(), ToolError> {
    let allowed = filters
        .and_then(|filters| filters.allowed_domains.as_deref())
        .unwrap_or_default();
    let blocked = filters
        .and_then(|filters| filters.blocked_domains.as_deref())
        .unwrap_or_default();
    if !allowed.is_empty() && !blocked.is_empty() {
        return Err(ToolError::Config(format!("{tool} {EXCLUSIVE_LISTS_RULE}")));
    }
    for (field, entries) in [("allowed_domains", allowed), ("blocked_domains", blocked)] {
        for entry in entries {
            validate_domain_entry(entry, EntryPaths::Refused)
                .map_err(|reason| ToolError::Config(format!("{tool} {field} entry {entry:?} {reason}")))?;
        }
    }
    Ok(())
}

/// Admits URLs against declared allow and block lists.
///
/// A host matches a domain when it equals the domain or ends with `.{domain}`
/// (label boundary), compared case-insensitively after IDNA normalization.
/// Under [`EntryPaths::Allowed`] an entry may also carry a path, as Anthropic's
/// web search documents (`example.com/blog`): the URL path must then start with
/// that path at a segment boundary, so `example.com/blog` matches `/blog` and
/// `/blog/post-1` but not `/blogger`, and a single `*` matches any run of
/// characters (`example.com/*/articles`). Under [`EntryPaths::Refused`] an entry
/// with a path is not a host and matches nothing, as before. A URL without a
/// parseable host cannot be checked, so it is refused whenever any list is
/// active (fail closed). An allowlist whose entries normalize to nothing admits
/// no host rather than every host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DomainFilter {
    include: Vec<DomainEntry>,
    exclude: Vec<DomainEntry>,
    /// An allowlist was declared but none of its entries normalizes to a
    /// domain, so nothing can match it: the filter fails closed instead of
    /// widening to every host.
    deny_all: bool,
}

impl DomainFilter {
    pub(crate) fn new(include: Option<&[String]>, exclude: Option<&[String]>, paths: EntryPaths) -> Self {
        let include_declared = include.is_some_and(|domains| !domains.is_empty());
        let include = normalize_domains(include, paths);
        Self {
            deny_all: include_declared && include.is_empty(),
            include,
            exclude: normalize_domains(exclude, paths),
        }
    }

    /// The filter a host-only declaration's lists describe; no lists is no
    /// filter.
    pub(crate) fn from_filters(filters: Option<&DomainFilters>) -> Self {
        Self::new(
            filters.and_then(|filters| filters.allowed_domains.as_deref()),
            filters.and_then(|filters| filters.blocked_domains.as_deref()),
            EntryPaths::Refused,
        )
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.deny_all && self.include.is_empty() && self.exclude.is_empty()
    }

    pub(crate) fn allows(&self, url: &str) -> bool {
        if self.deny_all {
            return false;
        }
        let Some((host, path)) = url_host_and_path(url) else {
            return self.is_empty();
        };
        let matches = |entry: &DomainEntry| entry.matches(&host, &path);
        (self.include.is_empty() || self.include.iter().any(matches)) && !self.exclude.iter().any(matches)
    }
}

impl DomainEntry {
    fn matches(&self, host: &str, path: &str) -> bool {
        host_matches_domain(host, &self.host)
            && self
                .path
                .as_deref()
                .is_none_or(|pattern| path_matches_prefix(path, pattern))
    }
}

/// Which list an entry sits in, for [`provider_domains`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DomainList {
    Allowed,
    Blocked,
}

/// The entries to pass to a provider's own domain parameter (`include_domains`,
/// `site:`), which documents host names only. An entry with a path sends its
/// host to an allowlist, and the client-side [`DomainFilter`] then narrows the
/// results to the path; it sends nothing to a blocklist, because blocking the
/// whole host there would drop more than was declared. Other entries pass
/// through unchanged. A list left empty is `None`.
pub(crate) fn provider_domains(
    entries: Option<Vec<String>>,
    list: DomainList,
    paths: EntryPaths,
) -> Option<Vec<String>> {
    if paths == EntryPaths::Refused {
        return entries;
    }
    let mut kept: Vec<String> = Vec::new();
    for entry in entries? {
        match validate_domain_entry(&entry, paths) {
            Ok(DomainEntry { host, path: Some(_) }) => {
                if list == DomainList::Allowed && !kept.contains(&host) {
                    kept.push(host);
                }
            }
            _ => kept.push(entry),
        }
    }
    (!kept.is_empty()).then_some(kept)
}

fn normalize_domains(domains: Option<&[String]>, paths: EntryPaths) -> Vec<DomainEntry> {
    domains
        .unwrap_or_default()
        .iter()
        .filter_map(|domain| normalize_domain(domain, paths))
        .collect()
}

/// The rule for one configured entry, parsed by [`validate_domain_entry`].
/// Entries that normalize to nothing (`.`) are dropped. Entries that do not
/// parse are kept verbatim (lowercased) as a host no URL has, so a typo fails
/// closed instead of silently widening an allowlist; declarations that are
/// validated up front (Messages `web_search`, `web_fetch`) never reach this,
/// so it serves the Responses `web_search` lists and model-supplied lists.
fn normalize_domain(domain: &str, paths: EntryPaths) -> Option<DomainEntry> {
    let trimmed = domain.trim();
    if trimmed.trim_end_matches('.').is_empty() {
        return None;
    }
    Some(validate_domain_entry(trimmed, paths).unwrap_or_else(|_| DomainEntry {
        host: trimmed.to_lowercase(),
        path: None,
    }))
}

fn url_host_and_path(url: &str) -> Option<(String, String)> {
    let url = url::Url::parse(url).ok()?;
    let host = url.host()?.to_string().trim_end_matches('.').to_owned();
    Some((host, normalize_percent_escapes(url.path())))
}

/// Uppercases the hex digits of percent escapes, so `%c3%a9` in a result URL
/// and `%C3%A9` from an entry the URL parser encoded compare equal.
fn normalize_percent_escapes(path: &str) -> String {
    let mut normalized = String::with_capacity(path.len());
    let mut escape_digits = 0;
    for character in path.chars() {
        if escape_digits > 0 {
            normalized.push(character.to_ascii_uppercase());
            escape_digits -= 1;
        } else {
            if character == '%' {
                escape_digits = 2;
            }
            normalized.push(character);
        }
    }
    normalized
}

fn host_matches_domain(host: &str, domain: &str) -> bool {
    host == domain || host.strip_suffix(domain).is_some_and(|prefix| prefix.ends_with('.'))
}

/// Whether `path` lies under `pattern`. Without a wildcard the pattern must be
/// a prefix that ends at a segment boundary: `/blog` covers `/blog` and
/// `/blog/post-1`, not `/blogger`. A `*` matches any run of characters,
/// including `/`, and the part after it must likewise end at a boundary; a
/// trailing `*` or `/` matches anything after it.
fn path_matches_prefix(path: &str, pattern: &str) -> bool {
    let ends_at_boundary =
        |rest: &str, matched: &str| rest.is_empty() || rest.starts_with('/') || matched.ends_with('/');
    let Some((before, after)) = pattern.split_once('*') else {
        return path
            .strip_prefix(pattern)
            .is_some_and(|rest| ends_at_boundary(rest, pattern));
    };
    let Some(rest) = path.strip_prefix(before) else {
        return false;
    };
    if after.is_empty() {
        return true;
    }
    rest.match_indices(after)
        .any(|(index, _)| ends_at_boundary(&rest[index + after.len()..], after))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filters(allowed: &[&str], blocked: &[&str]) -> DomainFilters {
        let list = |entries: &[&str]| (!entries.is_empty()).then(|| entries.iter().map(|e| (*e).to_owned()).collect());
        DomainFilters {
            allowed_domains: list(allowed),
            blocked_domains: list(blocked),
        }
    }

    #[test]
    fn domain_entries_must_be_host_names() {
        for accepted in [
            "example.com",
            " Docs.Example.com. ",
            "xn--bcher-kva.example",
            "93.184.216.34",
            "[2001:db8::1]",
        ] {
            assert!(
                validate_domain_entry(accepted, EntryPaths::Refused).is_ok(),
                "{accepted}"
            );
        }
        for (rejected, reason) in [
            ("", "is empty"),
            ("   ", "is empty"),
            (".", "is not a host name"),
            ("...", "is not a host name"),
            ("https://example.com", "must be a host name without a scheme or path"),
            ("example.com/blog", "must be a host name without a scheme or path"),
            ("exa mple.com", "must not contain whitespace"),
            ("example.com:8080", "is not a host name"),
            ("*.example.com", "is not a host name"),
        ] {
            assert_eq!(
                validate_domain_entry(rejected, EntryPaths::Refused),
                Err(reason),
                "{rejected:?}"
            );
        }
    }

    #[test]
    fn declared_lists_are_exclusive_and_name_hosts() {
        assert!(validate_domain_filters("web_fetch", None).is_ok());
        assert!(validate_domain_filters("web_fetch", Some(&filters(&["Example.COM.", "93.184.216.34"], &[]))).is_ok());
        assert!(validate_domain_filters("web_fetch", Some(&filters(&[], &["example.org"]))).is_ok());

        let error = validate_domain_filters("web_fetch", Some(&filters(&["a.com"], &["b.com"]))).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid tool config: web_fetch allowed_domains and blocked_domains cannot be used together"
        );
        let error = validate_domain_filters("web_fetch", Some(&filters(&["."], &[]))).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid tool config: web_fetch allowed_domains entry \".\" is not a host name"
        );
        // The tool name is the caller's: today only web_fetch applies this rule.
        let error = validate_domain_filters("web_search", Some(&filters(&[], &["example.com/blog"]))).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid tool config: web_search blocked_domains entry \"example.com/blog\" must be a host name without a \
             scheme or path"
        );
    }

    #[test]
    fn domain_filter_matches_on_label_boundary_case_insensitively() {
        let filter = DomainFilter::new(Some(&["Example.COM.".to_owned()]), None, EntryPaths::Refused);
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
            EntryPaths::Refused,
        );
        assert!(filter.allows("https://www.example.com/"));
        assert!(!filter.allows("https://internal.example.com/"));
        assert!(!filter.allows("https://a.internal.example.com/"));

        let blocklist_only = DomainFilter::new(None, Some(&["example.com".to_owned()]), EntryPaths::Refused);
        assert!(blocklist_only.allows("https://example.org/"));
        assert!(!blocklist_only.allows("https://sub.example.com/"));
    }

    fn path_filter(include: &[&str], exclude: &[&str]) -> DomainFilter {
        let list = |entries: &[&str]| entries.iter().map(|e| (*e).to_owned()).collect::<Vec<_>>();
        let (include, exclude) = (list(include), list(exclude));
        DomainFilter::new(
            (!include.is_empty()).then_some(include.as_slice()),
            (!exclude.is_empty()).then_some(exclude.as_slice()),
            EntryPaths::Allowed,
        )
    }

    #[test]
    fn web_search_entries_may_carry_a_path_and_wildcards_only_there() {
        let parsed = |entry: &str| validate_domain_entry(entry, EntryPaths::Allowed);
        let entry = |host: &str, path: Option<&str>| DomainEntry {
            host: host.to_owned(),
            path: path.map(str::to_owned),
        };
        assert_eq!(parsed(" Example.COM./Blog "), Ok(entry("example.com", Some("/Blog"))));
        assert_eq!(
            parsed("Bücher.example/café"),
            Ok(entry("xn--bcher-kva.example", Some("/caf%C3%A9")))
        );
        assert_eq!(
            parsed("example.com/*/articles"),
            Ok(entry("example.com", Some("/*/articles")))
        );
        for whole_host in ["example.com", "example.com/", "example.com/*"] {
            assert_eq!(parsed(whole_host), Ok(entry("example.com", None)), "{whole_host}");
        }
        assert_eq!(parsed("[2001:db8::1]/a"), Ok(entry("[2001:db8::1]", Some("/a"))));
        for (rejected, reason) in [
            (
                "*.example.com",
                "must not use a wildcard (*) in the host, only in the path after it",
            ),
            (
                "ex*.com/blog",
                "must not use a wildcard (*) in the host, only in the path after it",
            ),
            ("example.com/*/a/*", "may use the wildcard (*) at most once"),
            (
                "https://example.com/blog",
                "must be a host name, optionally followed by a path, without a scheme",
            ),
            (
                "example.com/blog?page=2",
                "must be a host name, optionally followed by a path, without a scheme",
            ),
            (
                "example.com/blog#top",
                "must be a host name, optionally followed by a path, without a scheme",
            ),
            ("example.com:8080/blog", "is not a host name"),
            ("example.com:443", "is not a host name"),
            ("user@example.com/blog", "is not a host name"),
            ("/blog", "is not a host name"),
            ("./blog", "is not a host name"),
            ("exa mple.com/blog", "must not contain whitespace"),
        ] {
            assert_eq!(parsed(rejected), Err(reason), "{rejected:?}");
        }
    }

    #[test]
    fn domain_filter_entries_with_a_path_match_urls_under_that_path() {
        let allowlist = path_filter(&["Example.COM/blog"], &[]);
        assert!(allowlist.allows("https://example.com/blog"));
        assert!(allowlist.allows("https://example.com/blog/post-1"));
        assert!(allowlist.allows("https://docs.example.com/blog/post-1?page=2"));
        assert!(!allowlist.allows("https://example.com/blogger"));
        assert!(!allowlist.allows("https://example.com/"));
        assert!(!allowlist.allows("https://example.com/news/blog"));
        assert!(!allowlist.allows("https://example.org/blog/post-1"));

        // A blocked path blocks only that path; the rest of the host stays admitted.
        let blocklist = path_filter(&[], &["example.com/blog"]);
        assert!(!blocklist.allows("https://example.com/blog/post-1"));
        assert!(!blocklist.allows("https://www.example.com/blog"));
        assert!(blocklist.allows("https://example.com/blogger"));
        assert!(blocklist.allows("https://example.com/docs"));
        assert!(blocklist.allows("https://example.org/blog/post-1"));

        // A host-only entry and a path entry combine as alternatives.
        let mixed = path_filter(&["example.com/blog", "example.org"], &[]);
        assert!(mixed.allows("https://example.com/blog/a"));
        assert!(mixed.allows("https://example.org/anything"));
        assert!(!mixed.allows("https://example.com/docs"));
    }

    #[test]
    fn domain_filter_path_entries_compare_percent_encoded_paths() {
        let filter = path_filter(&["example.com/café"], &[]);
        assert!(filter.allows("https://example.com/café/menu"));
        assert!(filter.allows("https://example.com/caf%C3%A9/menu"));
        assert!(filter.allows("https://example.com/caf%c3%a9"));
        assert!(!filter.allows("https://example.com/cafe"));
    }

    #[test]
    fn domain_filter_path_wildcard_matches_any_run_up_to_a_segment_boundary() {
        let filter = path_filter(&["example.com/*/articles"], &[]);
        assert!(filter.allows("https://example.com/en/articles"));
        assert!(filter.allows("https://example.com/en/us/articles/42"));
        assert!(!filter.allows("https://example.com/en/articlesx"));
        assert!(!filter.allows("https://example.com/en/posts"));
        assert!(!filter.allows("https://example.com/articles"));

        let trailing = path_filter(&["example.com/blog/*"], &[]);
        assert!(trailing.allows("https://example.com/blog/a"));
        assert!(!trailing.allows("https://example.com/blogger"));

        // A bare or wildcard-only path names the whole host.
        for entry in ["example.com/", "example.com/*"] {
            let filter = path_filter(&[entry], &[]);
            assert!(filter.allows("https://example.com/"), "{entry}");
            assert!(filter.allows("https://docs.example.com/any/path"), "{entry}");
            assert!(!filter.allows("https://example.org/"), "{entry}");
        }
    }

    #[test]
    fn responses_entries_with_a_path_keep_matching_nothing() {
        let allowlist = DomainFilter::new(Some(&["example.com/blog".to_owned()]), None, EntryPaths::Refused);
        assert!(!allowlist.allows("https://example.com/blog/post"));
        let blocklist = DomainFilter::new(None, Some(&["example.com/blog".to_owned()]), EntryPaths::Refused);
        assert!(blocklist.allows("https://example.com/blog/post"));
    }

    #[test]
    fn provider_lists_take_the_host_of_an_allowed_path_and_drop_a_blocked_one() {
        let list = |entries: &[&str]| Some(entries.iter().map(|e| (*e).to_owned()).collect::<Vec<_>>());
        assert_eq!(
            provider_domains(
                list(&["example.com/blog", "Example.com/docs", "example.org"]),
                DomainList::Allowed,
                EntryPaths::Allowed
            ),
            list(&["example.com", "example.org"])
        );
        assert_eq!(
            provider_domains(
                list(&["example.com/private", "spam.example"]),
                DomainList::Blocked,
                EntryPaths::Allowed
            ),
            list(&["spam.example"])
        );
        assert_eq!(
            provider_domains(list(&["example.com/private"]), DomainList::Blocked, EntryPaths::Allowed),
            None
        );
        assert_eq!(
            provider_domains(list(&["example.com/private"]), DomainList::Blocked, EntryPaths::Refused),
            list(&["example.com/private"])
        );
    }

    #[test]
    fn domain_filter_fails_closed_on_unparsable_urls() {
        let unparsable = [
            "not_a_valid_url",
            "not a url",
            "example.com/path",
            "mailto:someone@example.com",
        ];

        let allowlist = DomainFilter::new(Some(&["example.com".to_owned()]), None, EntryPaths::Refused);
        let blocklist = DomainFilter::new(None, Some(&["example.com".to_owned()]), EntryPaths::Refused);
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
        let idna = DomainFilter::new(Some(&["Bücher.example".to_owned()]), None, EntryPaths::Refused);
        assert!(idna.allows("https://shop.bücher.example/"));
        assert!(idna.allows("https://xn--bcher-kva.example/"));

        let invalid = DomainFilter::new(
            Some(&["https://example.com/path".to_owned()]),
            None,
            EntryPaths::Refused,
        );
        assert!(!invalid.is_empty());
        assert!(!invalid.allows("https://example.com/"));
    }

    #[test]
    fn domain_filter_fails_closed_when_an_allowlist_names_no_domain() {
        // "." normalizes to nothing; a declared allowlist must then admit
        // nothing rather than every host.
        let dots = DomainFilter::new(Some(&[".".to_owned(), "...".to_owned()]), None, EntryPaths::Refused);
        assert!(!dots.is_empty());
        assert!(!dots.allows("https://example.com/"));
        assert!(!dots.allows("not a url"));
        // An absent or explicitly empty allowlist is still no filter.
        assert!(DomainFilter::new(Some(&[]), None, EntryPaths::Refused).allows("https://example.com/"));
        assert!(DomainFilter::new(None, Some(&[".".to_owned()]), EntryPaths::Refused).allows("https://example.com/"));
    }

    #[test]
    fn a_declarations_lists_build_the_filter() {
        assert!(DomainFilter::from_filters(None).is_empty());
        let filter = DomainFilter::from_filters(Some(&filters(&["example.com"], &[])));
        assert!(filter.allows("https://docs.example.com/"));
        assert!(!filter.allows("https://example.org/"));
        let filter = DomainFilter::from_filters(Some(&filters(&[], &["example.com"])));
        assert!(!filter.allows("https://docs.example.com/"));
        assert!(filter.allows("https://example.org/"));
    }
}
