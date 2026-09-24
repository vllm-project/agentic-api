use std::collections::HashMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::tool::McpServerEntry;

pub const AGENTIC_API_HOME_ENV: &str = "AGENTIC_API_HOME";
pub const CONFIG_FILE_NAME: &str = "config.toml";
pub const DATABASE_FILE_NAME: &str = "agentic_api.db";

pub const DEFAULT_POSTGRES_MAX_CONNECTIONS: u32 = 10;
pub const DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS: u64 = 30;
pub const DEFAULT_POSTGRES_IDLE_TIMEOUT_SECONDS: u64 = 600;
pub const DEFAULT_POSTGRES_LOCK_TIMEOUT_SECONDS: u64 = 5;
pub const DEFAULT_POSTGRES_MAX_LIFETIME_SECONDS: u64 = 1_800;
pub const DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS: u64 = 300;
pub const DEFAULT_POSTGRES_STATEMENT_TIMEOUT_SECONDS: u64 = 30;
pub const DEFAULT_SQLITE_MAX_CONNECTIONS: u32 = 4;
pub const DEFAULT_SQLITE_JOURNAL_SIZE_LIMIT_BYTES: u64 = 6_144_000;
pub const DEFAULT_SQLITE_MMAP_SIZE_BYTES: u64 = 268_435_456;
pub const DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS: NonZeroUsize = NonZeroUsize::new(5).expect("default is nonzero");
/// Brave Search's free plan allows roughly one request per second.
pub const DEFAULT_BRAVE_MAX_CONCURRENT_QUERIES: NonZeroUsize = NonZeroUsize::new(1).expect("default is nonzero");

pub const DEFAULT_MAX_RETAINED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_MAX_UPSTREAM_JSON_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_UPSTREAM_SSE_LINE_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_STREAM_EVENT_BYTES: usize = 16 * 1024 * 1024;

pub const MIN_WIRE_HEADROOM_BYTES: usize = 64 * 1024;

pub const MAX_RETAINED_RESPONSE_BYTES_ENV: &str = "AGENTIC_MAX_RETAINED_RESPONSE_BYTES";
pub const MAX_UPSTREAM_JSON_BYTES_ENV: &str = "AGENTIC_MAX_UPSTREAM_JSON_BYTES";
pub const MAX_UPSTREAM_SSE_LINE_BYTES_ENV: &str = "AGENTIC_MAX_UPSTREAM_SSE_LINE_BYTES";
pub const MAX_STREAM_EVENT_BYTES_ENV: &str = "AGENTIC_MAX_STREAM_EVENT_BYTES";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponsesConfig {
    pub max_retained_bytes: usize,
    pub max_upstream_json_bytes: usize,
    pub max_upstream_sse_line_bytes: usize,
    pub max_stream_event_bytes: usize,
}

impl Default for ResponsesConfig {
    fn default() -> Self {
        Self {
            max_retained_bytes: DEFAULT_MAX_RETAINED_RESPONSE_BYTES,
            max_upstream_json_bytes: DEFAULT_MAX_UPSTREAM_JSON_BYTES,
            max_upstream_sse_line_bytes: DEFAULT_MAX_UPSTREAM_SSE_LINE_BYTES,
            max_stream_event_bytes: DEFAULT_MAX_STREAM_EVENT_BYTES,
        }
    }
}

impl ResponsesConfig {
    /// Validates internal consistency between configured limits.
    ///
    /// The wire limits (`max_stream_event_bytes`, `max_upstream_sse_line_bytes`,
    /// and `max_upstream_json_bytes`) must exceed `max_retained_bytes` by proportional
    /// wire headroom (`max(MIN_WIRE_HEADROOM_BYTES, max_retained_bytes / 4)`) to account for
    /// JSON serialization overhead, escaping, and message envelopes.
    ///
    /// # Errors
    /// Returns [`Error::Config`] when streaming delivery, upstream line, or JSON limits
    /// cannot admit the retained response plus wire headroom.
    pub fn validate(&self) -> Result<(), Error> {
        let headroom = MIN_WIRE_HEADROOM_BYTES.max(self.max_retained_bytes / 4);
        let required = self.max_retained_bytes.saturating_add(headroom);
        if self.max_stream_event_bytes < required {
            return Err(Error::Config(format!(
                "max_stream_event_bytes ({}) cannot be smaller than max_retained_bytes ({}) plus headroom ({})",
                self.max_stream_event_bytes, self.max_retained_bytes, required
            )));
        }
        if self.max_upstream_sse_line_bytes < required {
            return Err(Error::Config(format!(
                "max_upstream_sse_line_bytes ({}) cannot be smaller than max_retained_bytes ({}) plus headroom ({})",
                self.max_upstream_sse_line_bytes, self.max_retained_bytes, required
            )));
        }
        if self.max_upstream_json_bytes < required {
            return Err(Error::Config(format!(
                "max_upstream_json_bytes ({}) cannot be smaller than max_retained_bytes ({}) plus headroom ({})",
                self.max_upstream_json_bytes, self.max_retained_bytes, required
            )));
        }
        Ok(())
    }
}

/// Conservative operator-owned defaults for the opt-in code interpreter.
///
/// These values do not make a code interpreter available: registration remains
/// fail-closed until the feature, operator opt-in, and runtime readiness checks
/// all succeed.
pub const DEFAULT_CODE_INTERPRETER_MAX_SOURCE_BYTES: NonZeroUsize =
    NonZeroUsize::new(64 * 1024).expect("default is nonzero");
pub const DEFAULT_CODE_INTERPRETER_EXECUTION_WALL_TIME: Duration = Duration::from_secs(10);
pub const DEFAULT_CODE_INTERPRETER_MAX_FUEL: NonZeroU64 = NonZeroU64::new(10_000_000_000).expect("default is nonzero");
pub const DEFAULT_CODE_INTERPRETER_MAX_GUEST_MEMORY_BYTES: NonZeroUsize =
    NonZeroUsize::new(128 * 1024 * 1024).expect("default is nonzero");
pub const DEFAULT_CODE_INTERPRETER_MAX_STDOUT_BYTES: NonZeroUsize =
    NonZeroUsize::new(64 * 1024).expect("default is nonzero");
pub const DEFAULT_CODE_INTERPRETER_MAX_STDERR_BYTES: NonZeroUsize =
    NonZeroUsize::new(64 * 1024).expect("default is nonzero");
pub const DEFAULT_CODE_INTERPRETER_MAX_CONCURRENT_GUESTS: NonZeroUsize =
    NonZeroUsize::new(2).expect("default is nonzero");
pub const DEFAULT_CODE_INTERPRETER_MAX_AGGREGATE_GUEST_MEMORY_BYTES: NonZeroUsize =
    NonZeroUsize::new(256 * 1024 * 1024).expect("default is nonzero");

/// Operator-owned runtime limits for the gateway-executed code interpreter.
///
/// This is not a client request setting. The default remains disabled, and a
/// feature-enabled build continues rejecting declarations until it has a
/// verified runtime implementation. Nonzero numeric types prevent unusable
/// zero-byte, zero-fuel, and zero-capacity configurations at construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeInterpreterRuntimeConfig {
    /// Whether an operator intends to enable the runtime once it is available.
    pub enabled: bool,
    /// Largest accepted UTF-8 source string before guest creation.
    pub max_source_bytes: NonZeroUsize,
    /// Maximum time allotted to guest execution after initialization.
    pub execution_wall_time: Duration,
    /// Maximum WASM execution fuel allotted to one guest.
    pub max_fuel: NonZeroU64,
    /// Maximum guest memory reservation for one execution.
    pub max_guest_memory_bytes: NonZeroUsize,
    /// Maximum stdout bytes retained from Eryx's normal output handler.
    pub max_stdout_bytes: NonZeroUsize,
    /// Maximum stderr bytes retained from Eryx's normal output handler.
    pub max_stderr_bytes: NonZeroUsize,
    /// Process-wide maximum number of simultaneously admitted guests.
    pub max_concurrent_guests: NonZeroUsize,
    /// Process-wide maximum sum of admitted guest memory reservations.
    pub max_aggregate_guest_memory_bytes: NonZeroUsize,
}

impl Default for CodeInterpreterRuntimeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_source_bytes: DEFAULT_CODE_INTERPRETER_MAX_SOURCE_BYTES,
            execution_wall_time: DEFAULT_CODE_INTERPRETER_EXECUTION_WALL_TIME,
            max_fuel: DEFAULT_CODE_INTERPRETER_MAX_FUEL,
            max_guest_memory_bytes: DEFAULT_CODE_INTERPRETER_MAX_GUEST_MEMORY_BYTES,
            max_stdout_bytes: DEFAULT_CODE_INTERPRETER_MAX_STDOUT_BYTES,
            max_stderr_bytes: DEFAULT_CODE_INTERPRETER_MAX_STDERR_BYTES,
            max_concurrent_guests: DEFAULT_CODE_INTERPRETER_MAX_CONCURRENT_GUESTS,
            max_aggregate_guest_memory_bytes: DEFAULT_CODE_INTERPRETER_MAX_AGGREGATE_GUEST_MEMORY_BYTES,
        }
    }
}

impl CodeInterpreterRuntimeConfig {
    /// Validate cross-field invariants before a runtime or admission controller
    /// is constructed.
    ///
    /// # Errors
    ///
    /// Returns an error if a single guest reservation could never fit in the
    /// configured process-wide memory budget or the wall-time limit is zero.
    pub fn validate(self) -> Result<(), CodeInterpreterRuntimeConfigError> {
        if self.execution_wall_time.is_zero() {
            return Err(CodeInterpreterRuntimeConfigError::ZeroWallTime);
        }
        if self.max_guest_memory_bytes.get() > self.max_aggregate_guest_memory_bytes.get() {
            return Err(CodeInterpreterRuntimeConfigError::GuestMemoryExceedsAggregate {
                guest_memory_bytes: self.max_guest_memory_bytes.get(),
                aggregate_memory_bytes: self.max_aggregate_guest_memory_bytes.get(),
            });
        }
        if self.max_concurrent_guests.get() > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(CodeInterpreterRuntimeConfigError::ConcurrencyExceedsSemaphore {
                configured: self.max_concurrent_guests.get(),
                maximum: tokio::sync::Semaphore::MAX_PERMITS,
            });
        }
        Ok(())
    }
}

/// Cross-field configuration errors for [`CodeInterpreterRuntimeConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CodeInterpreterRuntimeConfigError {
    /// A duration constructed programmatically had no execution time.
    #[error("code interpreter execution_wall_time must be greater than zero")]
    ZeroWallTime,
    /// A single guest could never acquire the configured aggregate reservation.
    #[error(
        "code interpreter max_guest_memory_bytes ({guest_memory_bytes}) exceeds max_aggregate_guest_memory_bytes ({aggregate_memory_bytes})"
    )]
    GuestMemoryExceedsAggregate {
        /// Per-guest reservation in bytes.
        guest_memory_bytes: usize,
        /// Process-wide aggregate reservation in bytes.
        aggregate_memory_bytes: usize,
    },
    /// Tokio cannot construct a semaphore larger than its platform limit.
    #[error("code interpreter max_concurrent_guests ({configured}) exceeds the supported maximum ({maximum})")]
    ConcurrencyExceedsSemaphore {
        /// Operator-provided concurrency.
        configured: usize,
        /// Tokio's maximum supported permit count.
        maximum: usize,
    },
}

#[cfg(test)]
mod code_interpreter_config_tests {
    use super::*;

    #[test]
    fn code_interpreter_rejects_a_concurrency_value_that_would_panic_semaphore_construction() {
        let config = CodeInterpreterRuntimeConfig {
            max_concurrent_guests: NonZeroUsize::new(tokio::sync::Semaphore::MAX_PERMITS + 1).expect("nonzero"),
            ..CodeInterpreterRuntimeConfig::default()
        };

        assert!(matches!(
            config.validate(),
            Err(CodeInterpreterRuntimeConfigError::ConcurrencyExceedsSemaphore { .. })
        ));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PostgresConfig {
    pub max_connections: u32,
    pub acquire_timeout: Duration,
    pub lock_timeout: Duration,
    pub migration_timeout: Duration,
    pub statement_timeout: Duration,
    pub idle_timeout: Option<Duration>,
    pub max_lifetime: Option<Duration>,
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_POSTGRES_MAX_CONNECTIONS,
            acquire_timeout: Duration::from_secs(DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS),
            lock_timeout: Duration::from_secs(DEFAULT_POSTGRES_LOCK_TIMEOUT_SECONDS),
            migration_timeout: Duration::from_secs(DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS),
            statement_timeout: Duration::from_secs(DEFAULT_POSTGRES_STATEMENT_TIMEOUT_SECONDS),
            idle_timeout: Some(Duration::from_secs(DEFAULT_POSTGRES_IDLE_TIMEOUT_SECONDS)),
            max_lifetime: Some(Duration::from_secs(DEFAULT_POSTGRES_MAX_LIFETIME_SECONDS)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SqliteTempStore {
    Default,
    File,
    #[default]
    Memory,
}

impl SqliteTempStore {
    #[must_use]
    pub fn as_pragma_value(self) -> &'static str {
        match self {
            Self::Default => "DEFAULT",
            Self::File => "FILE",
            Self::Memory => "MEMORY",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqliteConfig {
    pub max_connections: u32,
    pub journal_size_limit_bytes: u64,
    pub temp_store: SqliteTempStore,
    pub mmap_size_bytes: u64,
}

impl Default for SqliteConfig {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_SQLITE_MAX_CONNECTIONS,
            journal_size_limit_bytes: DEFAULT_SQLITE_JOURNAL_SIZE_LIMIT_BYTES,
            temp_store: SqliteTempStore::default(),
            mmap_size_bytes: DEFAULT_SQLITE_MMAP_SIZE_BYTES,
        }
    }
}

/// Backend that serves the gateway-owned `web_search` tool.
///
/// Selected through [`WebSearchProviderConfig::provider`]; `you` is the
/// default so existing deployments are unchanged. The enum is non-exhaustive
/// so downstream crates keep a fallback arm when a new variant lands (#291).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WebSearchProviderKind {
    #[default]
    You,
    Brave,
}

impl WebSearchProviderKind {
    /// Every selectable provider, in the order operator-facing messages list them.
    pub const ALL: [Self; 2] = [Self::You, Self::Brave];

    /// Environment variable that conventionally carries this provider's API key.
    #[must_use]
    pub const fn default_api_key_env(self) -> &'static str {
        match self {
            Self::You => "YOU_API_KEY",
            Self::Brave => "BRAVE_API_KEY",
        }
    }

    /// Endpoint used when neither the environment nor the configuration file
    /// sets one. You.com has no default so a deployment that fails today keeps
    /// failing the same way (#291 Q2).
    #[must_use]
    pub const fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::You => None,
            Self::Brave => Some("https://api.search.brave.com"),
        }
    }

    /// Provider-imposed default ceiling on concurrent search requests. `None`
    /// inherits the gateway-wide limit. Brave's free plan allows roughly one
    /// request per second, so it defaults to serial queries.
    #[must_use]
    pub const fn default_max_concurrent_queries(self) -> Option<NonZeroUsize> {
        match self {
            Self::You => None,
            Self::Brave => Some(DEFAULT_BRAVE_MAX_CONCURRENT_QUERIES),
        }
    }

    /// Human-readable provider name used in operator-facing messages.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::You => "You.com",
            Self::Brave => "Brave Search",
        }
    }

    /// Configuration label (`you`, `brave`) matching the serialized form.
    #[must_use]
    pub const fn config_name(self) -> &'static str {
        match self {
            Self::You => "you",
            Self::Brave => "brave",
        }
    }

    /// Whether this is the default provider whose model-facing output must stay
    /// byte-identical to earlier releases.
    #[must_use]
    pub const fn is_you(&self) -> bool {
        matches!(self, Self::You)
    }
}

impl std::str::FromStr for WebSearchProviderKind {
    type Err = Error;

    /// Parses a configuration or environment value case-insensitively.
    fn from_str(value: &str) -> Result<Self, Error> {
        let trimmed = value.trim();
        Self::ALL
            .into_iter()
            .find(|kind| kind.config_name().eq_ignore_ascii_case(trimmed))
            .ok_or_else(|| {
                let expected = Self::ALL
                    .iter()
                    .map(|kind| kind.config_name())
                    .collect::<Vec<_>>()
                    .join(", ");
                Error::Config(format!(
                    "unknown web_search provider {trimmed:?}; expected one of: {expected}"
                ))
            })
    }
}

impl std::fmt::Display for WebSearchProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Selection and credentials for the gateway-owned `web_search` provider.
///
/// Construct with [`WebSearchProviderConfig::new`] and the `with_*` builders;
/// the struct is non-exhaustive so adding a provider setting is not a
/// breaking change for downstream crates.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct WebSearchProviderConfig {
    pub provider: WebSearchProviderKind,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    /// Operator override for the provider's concurrent-query ceiling. `None`
    /// uses [`WebSearchProviderKind::default_max_concurrent_queries`].
    pub max_concurrent_queries: Option<NonZeroUsize>,
}

impl WebSearchProviderConfig {
    /// Builds a You.com config from the credential and endpoint the deployment resolved.
    #[must_use]
    pub const fn new(api_key: Option<String>, base_url: Option<String>) -> Self {
        Self {
            provider: WebSearchProviderKind::You,
            api_key,
            base_url,
            max_concurrent_queries: None,
        }
    }

    /// Selects the provider the credential and endpoint belong to.
    #[must_use]
    pub const fn with_provider(mut self, provider: WebSearchProviderKind) -> Self {
        self.provider = provider;
        self
    }

    /// Overrides the provider's default concurrent-query ceiling.
    #[must_use]
    pub const fn with_max_concurrent_queries(mut self, max_concurrent_queries: Option<NonZeroUsize>) -> Self {
        self.max_concurrent_queries = max_concurrent_queries;
        self
    }
}

impl std::fmt::Debug for WebSearchProviderConfig {
    /// Redacts `api_key` so debug-printing any enclosing config never logs the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSearchProviderConfig")
            .field("provider", &self.provider)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("base_url", &self.base_url)
            .field("max_concurrent_queries", &self.max_concurrent_queries)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct ToolRuntimeConfig {
    pub web_search: WebSearchProviderConfig,
    pub mcp_servers: HashMap<String, McpServerEntry>,
    pub mcp_allowed_hosts: Vec<String>,
    pub messages_gateway_tool_aliases: Option<String>,
    /// Operator-only limits for the opt-in code interpreter. A default config
    /// is disabled and does not make the tool available.
    pub code_interpreter: CodeInterpreterRuntimeConfig,
    /// Upper bound on gateway-owned tool calls executing concurrently within one
    /// round. A sliding window admits another call as one finishes. Handlers with
    /// nested outbound work also use this value as their provider-level concurrency
    /// ceiling; individual handlers may further serialize calls to the same tool
    /// name. The nonzero type prevents constructing a scheduler window that can
    /// never be polled.
    pub max_concurrent_gateway_calls: NonZeroUsize,
}

impl Default for ToolRuntimeConfig {
    fn default() -> Self {
        Self {
            web_search: WebSearchProviderConfig::default(),
            mcp_servers: HashMap::default(),
            mcp_allowed_hosts: Vec::default(),
            messages_gateway_tool_aliases: None,
            code_interpreter: CodeInterpreterRuntimeConfig::default(),
            max_concurrent_gateway_calls: DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub llm_api_base: String,
    pub openai_api_key: Option<String>,
    pub llm_ready_timeout_s: f64,
    pub llm_ready_interval_s: f64,
    pub skip_llm_ready_check: bool,
    /// Database URL for conversation and response storage.
    /// `None` uses the local database in the Agentic API home directory.
    pub db_url: Option<String>,
    pub postgres: PostgresConfig,
    pub sqlite: SqliteConfig,
    pub tools: ToolRuntimeConfig,
    pub responses: ResponsesConfig,
}

/// Resolves the directory used for user configuration and local state.
///
/// `AGENTIC_API_HOME` takes precedence over the default `~/.agentic-api`.
/// The returned path is absolute, but it is not created by this function.
///
/// # Errors
///
/// Returns a configuration error when the home directory cannot be found or
/// `AGENTIC_API_HOME` is not an absolute path.
pub fn agentic_api_home() -> Result<PathBuf, Error> {
    let configured = std::env::var_os(AGENTIC_API_HOME_ENV).filter(|value| !value.is_empty());
    resolve_agentic_api_home(configured.map(PathBuf::from), dirs::home_dir())
}

fn resolve_agentic_api_home(configured: Option<PathBuf>, user_home: Option<PathBuf>) -> Result<PathBuf, Error> {
    if let Some(path) = configured {
        if !path.is_absolute() {
            return Err(Error::Config(format!(
                "{AGENTIC_API_HOME_ENV} must be an absolute path: {}",
                path.display()
            )));
        }
        return Ok(path);
    }

    let user_home = user_home.ok_or_else(|| Error::Config("could not determine the user home directory".to_owned()))?;
    if !user_home.is_absolute() {
        return Err(Error::Config(format!(
            "user home directory must be an absolute path: {}",
            user_home.display()
        )));
    }
    Ok(user_home.join(".agentic-api"))
}

/// Resolves and creates the Agentic API home directory.
///
/// # Errors
///
/// Returns an error when the path cannot be resolved or created, or when an
/// existing path is not a directory.
pub fn ensure_agentic_api_home() -> Result<PathBuf, Error> {
    let path = agentic_api_home()?;
    std::fs::create_dir_all(&path).map_err(|error| {
        Error::Config(format!(
            "failed to create Agentic API home directory {}: {error}",
            path.display()
        ))
    })?;
    if !path.is_dir() {
        return Err(Error::Config(format!(
            "Agentic API home path is not a directory: {}",
            path.display()
        )));
    }
    Ok(path)
}

/// Returns the default `SQLite` URL inside the Agentic API home directory.
///
/// # Errors
///
/// Returns an error when the home directory cannot be resolved or created.
pub fn default_database_url() -> Result<String, Error> {
    default_database_url_in(&ensure_agentic_api_home()?)
}

fn default_database_url_in(home: &Path) -> Result<String, Error> {
    const SQLITE_PATH_ENCODE_SET: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'<')
        .add(b'>')
        .add(b'?')
        .add(b'%')
        .add(b'`')
        .add(b'{')
        .add(b'}');

    let path = home.join(DATABASE_FILE_NAME);
    let path = path
        .to_str()
        .ok_or_else(|| Error::Config(format!("default database path is not valid UTF-8: {}", path.display())))?;
    #[cfg(windows)]
    let path = path.replace('\\', "/");
    let encoded = percent_encoding::utf8_percent_encode(path, SQLITE_PATH_ENCODE_SET);
    Ok(format!("sqlite://{encoded}"))
}

#[must_use]
pub fn normalize_base_url(url: &str) -> String {
    let mut s = url.trim_end_matches('/').to_owned();
    if s.ends_with("/v1") {
        s.truncate(s.len() - 3);
        s = s.trim_end_matches('/').to_owned();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn web_search_provider_config_debug_redacts_api_key() {
        let config = WebSearchProviderConfig::new(
            Some("super-secret-key".to_owned()),
            Some("https://api.example".to_owned()),
        );
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret-key"));
        assert!(rendered.contains(r#"api_key: Some("<redacted>")"#));
        assert!(rendered.contains(r#"base_url: Some("https://api.example")"#));

        let tools = ToolRuntimeConfig {
            web_search: config,
            ..ToolRuntimeConfig::default()
        };
        assert!(!format!("{tools:?}").contains("super-secret-key"));
        assert_eq!(
            format!("{:?}", WebSearchProviderConfig::default()),
            "WebSearchProviderConfig { provider: You, api_key: None, base_url: None, max_concurrent_queries: None }"
        );
    }

    #[test]
    fn web_search_provider_config_builders_select_provider_and_ceiling() {
        let config = WebSearchProviderConfig::new(Some("k".to_owned()), None)
            .with_provider(WebSearchProviderKind::Brave)
            .with_max_concurrent_queries(NonZeroUsize::new(3));
        assert_eq!(config.provider, WebSearchProviderKind::Brave);
        assert_eq!(config.api_key.as_deref(), Some("k"));
        assert_eq!(config.max_concurrent_queries, NonZeroUsize::new(3));
        assert_eq!(
            WebSearchProviderConfig::new(None, None).provider,
            WebSearchProviderKind::You
        );
    }

    #[test]
    fn web_search_provider_kind_labels() {
        assert_eq!(WebSearchProviderKind::You.to_string(), "You.com");
        assert_eq!(WebSearchProviderKind::You.default_api_key_env(), "YOU_API_KEY");
        assert_eq!(WebSearchProviderKind::You.default_base_url(), None);
        assert_eq!(WebSearchProviderKind::You.default_max_concurrent_queries(), None);
        assert!(WebSearchProviderKind::You.is_you());
        assert_eq!(WebSearchProviderKind::default(), WebSearchProviderKind::You);

        assert_eq!(WebSearchProviderKind::Brave.to_string(), "Brave Search");
        assert_eq!(WebSearchProviderKind::Brave.default_api_key_env(), "BRAVE_API_KEY");
        assert_eq!(
            WebSearchProviderKind::Brave.default_base_url(),
            Some("https://api.search.brave.com")
        );
        assert_eq!(
            WebSearchProviderKind::Brave.default_max_concurrent_queries(),
            NonZeroUsize::new(1)
        );
        assert!(!WebSearchProviderKind::Brave.is_you());
    }

    #[test]
    fn web_search_provider_kind_parses_case_insensitively_and_serializes_snake_case() {
        for value in ["brave", "Brave", " BRAVE "] {
            assert_eq!(
                value.parse::<WebSearchProviderKind>().unwrap(),
                WebSearchProviderKind::Brave
            );
        }
        assert_eq!(
            "you".parse::<WebSearchProviderKind>().unwrap(),
            WebSearchProviderKind::You
        );
        let error = "bing".parse::<WebSearchProviderKind>().unwrap_err();
        assert_eq!(
            error.to_string(),
            "unknown web_search provider \"bing\"; expected one of: you, brave"
        );

        assert_eq!(
            serde_json::to_string(&WebSearchProviderKind::Brave).unwrap(),
            "\"brave\""
        );
        assert_eq!(
            serde_json::from_str::<WebSearchProviderKind>("\"you\"").unwrap(),
            WebSearchProviderKind::You
        );
        for kind in WebSearchProviderKind::ALL {
            assert_eq!(kind.config_name().parse::<WebSearchProviderKind>().unwrap(), kind);
        }
    }

    #[test]
    fn strip_trailing_v1() {
        assert_eq!(normalize_base_url("http://host:8000/v1"), "http://host:8000");
        assert_eq!(normalize_base_url("http://host:8000/v1/"), "http://host:8000");
    }

    #[test]
    fn no_v1_unchanged() {
        assert_eq!(normalize_base_url("http://host:8000"), "http://host:8000");
        assert_eq!(normalize_base_url("http://host:8000/"), "http://host:8000");
    }

    #[test]
    fn home_override_takes_precedence() {
        let configured = if cfg!(windows) {
            PathBuf::from(r"C:\agentic-home")
        } else {
            PathBuf::from("/tmp/agentic-home")
        };
        let resolved = resolve_agentic_api_home(Some(configured.clone()), Some(PathBuf::from("/ignored")))
            .expect("absolute configured home");
        assert_eq!(resolved, configured);
    }

    #[test]
    fn default_home_is_hidden_directory() {
        let user_home = if cfg!(windows) {
            PathBuf::from(r"C:\Users\agentic")
        } else {
            PathBuf::from("/home/agentic")
        };
        let resolved = resolve_agentic_api_home(None, Some(user_home.clone())).expect("user home");
        assert_eq!(resolved, user_home.join(".agentic-api"));
    }

    #[test]
    fn relative_home_override_is_rejected() {
        let error = resolve_agentic_api_home(Some(PathBuf::from("relative")), Some(PathBuf::from("/home/agentic")))
            .expect_err("relative override must fail");
        assert!(error.to_string().contains("must be an absolute path"));
    }

    #[test]
    fn database_url_uses_home_directory() {
        let home = if cfg!(windows) {
            PathBuf::from(r"C:\Users\agentic\.agentic-api")
        } else {
            PathBuf::from("/home/agentic/.agentic-api")
        };
        let url = default_database_url_in(&home).expect("database URL");
        assert!(url.starts_with("sqlite://"));
        assert!(url.ends_with("/.agentic-api/agentic_api.db"));
    }

    #[test]
    fn database_url_encodes_url_delimiters_in_home_path() {
        let home = if cfg!(windows) {
            PathBuf::from(r"C:\Users\agentic api\state?#%")
        } else {
            PathBuf::from("/home/agentic api/state?#%")
        };
        let url = default_database_url_in(&home).expect("database URL");
        assert!(url.contains("agentic%20api"));
        assert!(url.contains("state%3F%23%25"));
    }

    #[test]
    fn responses_config_validation() {
        let valid = ResponsesConfig {
            max_retained_bytes: 1024 * 1024,
            max_upstream_json_bytes: 2 * 1024 * 1024,
            max_upstream_sse_line_bytes: 2 * 1024 * 1024,
            max_stream_event_bytes: 2 * 1024 * 1024,
        };
        assert!(valid.validate().is_ok());

        let mut invalid_stream = valid;
        invalid_stream.max_stream_event_bytes = 1024 * 1024;
        assert!(invalid_stream.validate().is_err());

        let mut invalid_sse = valid;
        invalid_sse.max_upstream_sse_line_bytes = 1024 * 1024;
        assert!(invalid_sse.validate().is_err());

        let mut invalid_json = valid;
        invalid_json.max_upstream_json_bytes = 1024 * 1024;
        assert!(invalid_json.validate().is_err());

        // For 4 MiB retained, 64 KiB is not enough headroom (requires 25% = 1 MiB)
        let borderline = ResponsesConfig {
            max_retained_bytes: 4 * 1024 * 1024,
            max_upstream_json_bytes: 4 * 1024 * 1024 + 64 * 1024,
            max_upstream_sse_line_bytes: 5 * 1024 * 1024,
            max_stream_event_bytes: 5 * 1024 * 1024,
        };
        assert!(borderline.validate().is_err());
    }
}
