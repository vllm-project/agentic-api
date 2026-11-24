//! Domain policy shared by the gateway's web tools.
//!
//! A `web_search` or `web_fetch` declaration may name `allowed_domains` and
//! `blocked_domains` ([`DomainFilters`]). This module owns the [`DomainFilter`]
//! that decides whether a URL's host is admitted — web search applies it to
//! results, web fetch to the requested URL and to every redirect hop — and the
//! host-name rule a `web_fetch` entry must satisfy to match anything
//! ([`validate_domain_entry`], [`validate_domain_filters`]). `web_search` keeps
//! its lenient entry rule in the Messages adapter, because Anthropic's web
//! search accepts subpath entries the host-name rule would refuse.

use url::Host;

use super::handler::ToolError;
use crate::types::tools::DomainFilters;

/// The rule both lists share, as Anthropic documents it for every server tool;
/// the Messages adapter reports it in the same words.
pub(crate) const EXCLUSIVE_LISTS_RULE: &str = "allowed_domains and blocked_domains cannot be used together";

/// Why a declared `allowed_domains` / `blocked_domains` entry cannot match a
/// host. The filter matches on the host only, so an entry must be a host name
/// or address: no scheme, no path, nothing that normalizes to nothing.
///
/// # Errors
///
/// The reason, worded to follow the entry (`entry "." is not a host name`).
pub(crate) fn validate_domain_entry(entry: &str) -> Result<(), &'static str> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return Err("is empty");
    }
    if trimmed.contains("://") || trimmed.contains('/') {
        return Err("must be a host name without a scheme or path");
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err("must not contain whitespace");
    }
    let host = trimmed.trim_end_matches('.');
    // `Host::parse` applies IDNA and lowercasing but admits characters such as
    // `*` that no host carries; a label must be letters, digits, and hyphens.
    match Host::parse(host) {
        Ok(Host::Ipv4(_) | Host::Ipv6(_)) => Ok(()),
        Ok(Host::Domain(domain))
            if domain.split('.').all(|label| {
                !label.is_empty() && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            }) =>
        {
            Ok(())
        }
        _ => Err("is not a host name"),
    }
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
            validate_domain_entry(entry)
                .map_err(|reason| ToolError::Config(format!("{tool} {field} entry {entry:?} {reason}")))?;
        }
    }
    Ok(())
}

/// Admits URLs by host against declared allow and block lists.
///
/// A host matches a domain when it equals the domain or ends with `.{domain}`
/// (label boundary), compared case-insensitively after IDNA normalization. A
/// URL without a parseable host cannot be checked, so it is refused whenever
/// any list is active (fail closed). An allowlist whose entries normalize to
/// nothing admits no host rather than every host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DomainFilter {
    include: Vec<String>,
    exclude: Vec<String>,
    /// An allowlist was declared but none of its entries normalizes to a
    /// domain, so nothing can match it: the filter fails closed instead of
    /// widening to every host.
    deny_all: bool,
}

impl DomainFilter {
    pub(crate) fn new(include: Option<&[String]>, exclude: Option<&[String]>) -> Self {
        let include_declared = include.is_some_and(|domains| !domains.is_empty());
        let include = normalize_domains(include);
        Self {
            deny_all: include_declared && include.is_empty(),
            include,
            exclude: normalize_domains(exclude),
        }
    }

    /// The filter a declaration's lists describe; no lists is no filter.
    pub(crate) fn from_filters(filters: Option<&DomainFilters>) -> Self {
        Self::new(
            filters.and_then(|filters| filters.allowed_domains.as_deref()),
            filters.and_then(|filters| filters.blocked_domains.as_deref()),
        )
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.deny_all && self.include.is_empty() && self.exclude.is_empty()
    }

    pub(crate) fn allows(&self, url: &str) -> bool {
        if self.deny_all {
            return false;
        }
        let Some(host) = url_host(url) else {
            return self.is_empty();
        };
        let matches = |domain: &String| host_matches_domain(&host, domain);
        (self.include.is_empty() || self.include.iter().any(matches)) && !self.exclude.iter().any(matches)
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
    Some(Host::parse(trimmed).map_or_else(|_| trimmed.to_ascii_lowercase(), |host| host.to_string()))
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
            assert_eq!(validate_domain_entry(accepted), Ok(()), "{accepted}");
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
            assert_eq!(validate_domain_entry(rejected), Err(reason), "{rejected:?}");
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
    fn domain_filter_fails_closed_when_an_allowlist_names_no_domain() {
        // "." normalizes to nothing; a declared allowlist must then admit
        // nothing rather than every host.
        let dots = DomainFilter::new(Some(&[".".to_owned(), "...".to_owned()]), None);
        assert!(!dots.is_empty());
        assert!(!dots.allows("https://example.com/"));
        assert!(!dots.allows("not a url"));
        // An absent or explicitly empty allowlist is still no filter.
        assert!(DomainFilter::new(Some(&[]), None).allows("https://example.com/"));
        assert!(DomainFilter::new(None, Some(&[".".to_owned()])).allows("https://example.com/"));
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
