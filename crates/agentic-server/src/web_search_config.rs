//! Deployment configuration for the selected web-search provider.

use std::num::NonZeroUsize;

use agentic_core::config::{WebSearchProviderConfig, WebSearchProviderKind};
use agentic_core::error::Error;

use crate::config_file::WebSearchFileConfig;

/// Environment override for the `web_search` backend (`you`, `brave`, or `tavily`).
const WEB_SEARCH_PROVIDER_ENV: &str = "AGENTIC_WEB_SEARCH_PROVIDER";
/// Provider-neutral environment override for the `web_search` endpoint.
const WEB_SEARCH_BASE_URL_ENV: &str = "AGENTIC_WEB_SEARCH_BASE_URL";
/// Legacy You.com endpoint override, honored only when You.com is selected.
const YOU_API_BASE_URL_ENV: &str = "YOU_API_BASE_URL";
/// Environment override for the concurrent-query ceiling of one batched search.
const WEB_SEARCH_MAX_CONCURRENT_QUERIES_ENV: &str = "AGENTIC_WEB_SEARCH_MAX_CONCURRENT_QUERIES";

/// Resolves the `web_search` provider settings as environment variable >
/// configuration file > provider default.
///
/// The provider comes from `AGENTIC_WEB_SEARCH_PROVIDER` or `[web_search]
/// provider`. The API key is read from the variable named by `api_key_env`,
/// else the provider's conventional variable. The endpoint prefers
/// `AGENTIC_WEB_SEARCH_BASE_URL`, then `YOU_API_BASE_URL` (You.com only), then
/// the file, then the provider default. `max_concurrent_queries` is left unset
/// so the provider's own default applies.
pub(crate) fn resolve_web_search_config(
    file: &WebSearchFileConfig,
    env: impl Fn(&str) -> Option<String>,
) -> Result<WebSearchProviderConfig, Error> {
    let provider = match env(WEB_SEARCH_PROVIDER_ENV) {
        Some(value) => value
            .parse::<WebSearchProviderKind>()
            .map_err(|error| Error::Config(format!("{WEB_SEARCH_PROVIDER_ENV}: {error}")))?,
        None => file.provider.unwrap_or_default(),
    };
    let api_key_env = file
        .api_key_env
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| provider.default_api_key_env());
    let api_key = env(api_key_env);
    let base_url = env(WEB_SEARCH_BASE_URL_ENV)
        .or_else(|| provider.is_you().then(|| env(YOU_API_BASE_URL_ENV)).flatten())
        .or_else(|| file.base_url.clone())
        .or_else(|| provider.default_base_url().map(str::to_owned));
    let max_concurrent_queries = match env(WEB_SEARCH_MAX_CONCURRENT_QUERIES_ENV) {
        Some(value) => Some(value.parse::<NonZeroUsize>().map_err(|error| {
            Error::Config(format!(
                "{WEB_SEARCH_MAX_CONCURRENT_QUERIES_ENV} must be a positive integer: {error}"
            ))
        })?),
        None => file.max_concurrent_queries,
    };
    Ok(WebSearchProviderConfig::new(api_key, base_url)
        .with_provider(provider)
        .with_max_concurrent_queries(max_concurrent_queries))
}

/// Seeds `[web_search]` in a generated configuration file from the current
/// environment. Credentials stay unpinned so changing providers selects the
/// corresponding default key variable; a malformed
/// provider value is ignored here and rejected at startup by
/// [`resolve_web_search_config`].
pub(crate) fn generated_web_search_file_config(env: impl Fn(&str) -> Option<String>) -> WebSearchFileConfig {
    let provider = env(WEB_SEARCH_PROVIDER_ENV)
        .and_then(|value| value.parse::<WebSearchProviderKind>().ok())
        .unwrap_or_default();
    WebSearchFileConfig {
        provider: Some(provider),
        base_url: env(WEB_SEARCH_BASE_URL_ENV)
            .or_else(|| provider.is_you().then(|| env(YOU_API_BASE_URL_ENV)).flatten()),
        api_key_env: None,
        max_concurrent_queries: env(WEB_SEARCH_MAX_CONCURRENT_QUERIES_ENV)
            .and_then(|value| value.parse::<NonZeroUsize>().ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Environment lookup over a fixed set of variables, mirroring `environment_value`.
    fn env_from<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        }
    }

    #[test]
    fn web_search_config_defaults_to_you_with_legacy_variables() {
        let file = WebSearchFileConfig {
            base_url: Some("https://file.example".to_owned()),
            ..WebSearchFileConfig::default()
        };
        let config = resolve_web_search_config(
            &file,
            env_from(&[
                ("YOU_API_KEY", "you-secret"),
                ("YOU_API_BASE_URL", "https://you.example"),
            ]),
        )
        .expect("resolve you.com");
        assert_eq!(config.provider, WebSearchProviderKind::You);
        assert_eq!(config.api_key.as_deref(), Some("you-secret"));
        assert_eq!(config.base_url.as_deref(), Some("https://you.example"));
        assert_eq!(config.max_concurrent_queries, None);

        // Without any endpoint variable the file value is used; You.com has no default.
        let config = resolve_web_search_config(&file, env_from(&[])).expect("resolve from file");
        assert_eq!(config.base_url.as_deref(), Some("https://file.example"));
        assert_eq!(config.api_key, None);
        let config = resolve_web_search_config(&WebSearchFileConfig::default(), env_from(&[])).expect("resolve empty");
        assert_eq!(config.base_url, None);
    }

    #[test]
    fn web_search_config_selects_brave_from_environment_or_file() {
        let config = resolve_web_search_config(
            &WebSearchFileConfig::default(),
            env_from(&[
                ("AGENTIC_WEB_SEARCH_PROVIDER", "Brave"),
                ("BRAVE_API_KEY", "brave-secret"),
                ("YOU_API_KEY", "you-secret"),
                ("YOU_API_BASE_URL", "https://you.example"),
            ]),
        )
        .expect("resolve brave");
        assert_eq!(config.provider, WebSearchProviderKind::Brave);
        assert_eq!(config.api_key.as_deref(), Some("brave-secret"));
        assert_eq!(
            config.base_url.as_deref(),
            Some("https://api.search.brave.com"),
            "YOU_API_BASE_URL must not leak into the Brave endpoint"
        );

        let file = WebSearchFileConfig {
            provider: Some(WebSearchProviderKind::Brave),
            api_key_env: Some("MY_BRAVE_KEY".to_owned()),
            max_concurrent_queries: NonZeroUsize::new(2),
            ..WebSearchFileConfig::default()
        };
        let config = resolve_web_search_config(&file, env_from(&[("MY_BRAVE_KEY", "custom-secret")]))
            .expect("resolve brave from file");
        assert_eq!(config.provider, WebSearchProviderKind::Brave);
        assert_eq!(config.api_key.as_deref(), Some("custom-secret"));
        assert_eq!(config.max_concurrent_queries.map(NonZeroUsize::get), Some(2));

        // The environment variable wins over the file for the provider itself.
        let config = resolve_web_search_config(&file, env_from(&[("AGENTIC_WEB_SEARCH_PROVIDER", "you")]))
            .expect("resolve override");
        assert_eq!(config.provider, WebSearchProviderKind::You);
    }

    #[test]
    fn web_search_config_selects_tavily_from_environment_or_file() {
        let config = resolve_web_search_config(
            &WebSearchFileConfig::default(),
            env_from(&[
                ("AGENTIC_WEB_SEARCH_PROVIDER", "Tavily"),
                ("TAVILY_API_KEY", "tvly-secret"),
                ("BRAVE_API_KEY", "brave-secret"),
                ("YOU_API_KEY", "you-secret"),
                ("YOU_API_BASE_URL", "https://you.example"),
            ]),
        )
        .expect("resolve tavily");
        assert_eq!(config.provider, WebSearchProviderKind::Tavily);
        assert_eq!(config.api_key.as_deref(), Some("tvly-secret"));
        assert_eq!(
            config.base_url.as_deref(),
            Some("https://api.tavily.com"),
            "YOU_API_BASE_URL must not leak into the Tavily endpoint"
        );
        assert_eq!(
            config.max_concurrent_queries, None,
            "Tavily inherits the gateway ceiling"
        );

        let file = WebSearchFileConfig {
            provider: Some(WebSearchProviderKind::Tavily),
            api_key_env: Some("MY_TAVILY_KEY".to_owned()),
            base_url: Some("https://tavily.example".to_owned()),
            max_concurrent_queries: NonZeroUsize::new(3),
        };
        let config = resolve_web_search_config(&file, env_from(&[("MY_TAVILY_KEY", "custom-secret")]))
            .expect("resolve tavily from file");
        assert_eq!(config.provider, WebSearchProviderKind::Tavily);
        assert_eq!(config.api_key.as_deref(), Some("custom-secret"));
        assert_eq!(config.base_url.as_deref(), Some("https://tavily.example"));
        assert_eq!(config.max_concurrent_queries.map(NonZeroUsize::get), Some(3));
    }

    #[test]
    fn web_search_config_applies_environment_precedence_for_endpoint_and_concurrency() {
        let file = WebSearchFileConfig {
            base_url: Some("https://file.example".to_owned()),
            max_concurrent_queries: NonZeroUsize::new(2),
            ..WebSearchFileConfig::default()
        };
        let config = resolve_web_search_config(
            &file,
            env_from(&[
                ("AGENTIC_WEB_SEARCH_BASE_URL", "https://generic.example"),
                ("YOU_API_BASE_URL", "https://legacy.example"),
                ("AGENTIC_WEB_SEARCH_MAX_CONCURRENT_QUERIES", "4"),
            ]),
        )
        .expect("resolve overrides");
        assert_eq!(config.base_url.as_deref(), Some("https://generic.example"));
        assert_eq!(config.max_concurrent_queries.map(NonZeroUsize::get), Some(4));

        let config = resolve_web_search_config(&file, env_from(&[("YOU_API_BASE_URL", "https://legacy.example")]))
            .expect("legacy endpoint");
        assert_eq!(config.base_url.as_deref(), Some("https://legacy.example"));
        assert_eq!(config.max_concurrent_queries.map(NonZeroUsize::get), Some(2));
    }

    #[test]
    fn web_search_config_rejects_invalid_environment_values() {
        let error = resolve_web_search_config(
            &WebSearchFileConfig::default(),
            env_from(&[("AGENTIC_WEB_SEARCH_PROVIDER", "bing")]),
        )
        .expect_err("unknown provider");
        assert_eq!(
            error.to_string(),
            "AGENTIC_WEB_SEARCH_PROVIDER: unknown web_search provider \"bing\"; expected one of: you, brave, tavily"
        );

        let error = resolve_web_search_config(
            &WebSearchFileConfig::default(),
            env_from(&[("AGENTIC_WEB_SEARCH_MAX_CONCURRENT_QUERIES", "0")]),
        )
        .expect_err("zero concurrency");
        assert!(
            error
                .to_string()
                .starts_with("AGENTIC_WEB_SEARCH_MAX_CONCURRENT_QUERIES must be a positive integer"),
            "{error}"
        );
    }

    #[test]
    fn generated_web_search_config_documents_the_selected_provider() {
        let generated = generated_web_search_file_config(env_from(&[("YOU_API_BASE_URL", "https://you.example")]));
        assert_eq!(generated.provider, Some(WebSearchProviderKind::You));
        assert_eq!(generated.api_key_env, None);
        assert_eq!(generated.base_url.as_deref(), Some("https://you.example"));
        assert_eq!(generated.max_concurrent_queries, None);

        let generated = generated_web_search_file_config(env_from(&[
            ("AGENTIC_WEB_SEARCH_PROVIDER", "brave"),
            ("YOU_API_BASE_URL", "https://you.example"),
            ("AGENTIC_WEB_SEARCH_MAX_CONCURRENT_QUERIES", "3"),
        ]));
        assert_eq!(generated.provider, Some(WebSearchProviderKind::Brave));
        assert_eq!(generated.api_key_env, None);
        assert_eq!(
            generated.base_url, None,
            "the Brave default endpoint is not pinned into the file"
        );
        assert_eq!(generated.max_concurrent_queries.map(NonZeroUsize::get), Some(3));

        let generated = generated_web_search_file_config(env_from(&[("AGENTIC_WEB_SEARCH_PROVIDER", "bing")]));
        assert_eq!(generated.provider, Some(WebSearchProviderKind::You));
    }

    #[test]
    fn generated_web_search_config_can_switch_provider_without_pinning_credentials() {
        for (initial, next, key) in [
            ("you", "brave", "BRAVE_API_KEY"),
            ("brave", "you", "YOU_API_KEY"),
            ("brave", "tavily", "TAVILY_API_KEY"),
            ("tavily", "you", "YOU_API_KEY"),
        ] {
            let generated = generated_web_search_file_config(env_from(&[("AGENTIC_WEB_SEARCH_PROVIDER", initial)]));
            let config = resolve_web_search_config(
                &generated,
                env_from(&[("AGENTIC_WEB_SEARCH_PROVIDER", next), (key, "selected-secret")]),
            )
            .expect("resolve switched provider");
            assert_eq!(config.api_key.as_deref(), Some("selected-secret"));
        }
    }
}
