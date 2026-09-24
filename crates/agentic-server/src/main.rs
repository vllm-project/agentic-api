use std::collections::HashMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use agentic_core::DatabaseBackend;
use agentic_core::config::{
    CodeInterpreterRuntimeConfig, Config, DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS,
    DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS, DEFAULT_POSTGRES_IDLE_TIMEOUT_SECONDS,
    DEFAULT_POSTGRES_LOCK_TIMEOUT_SECONDS, DEFAULT_POSTGRES_MAX_CONNECTIONS, DEFAULT_POSTGRES_MAX_LIFETIME_SECONDS,
    DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS, DEFAULT_POSTGRES_STATEMENT_TIMEOUT_SECONDS,
    DEFAULT_SQLITE_JOURNAL_SIZE_LIMIT_BYTES, DEFAULT_SQLITE_MAX_CONNECTIONS, DEFAULT_SQLITE_MMAP_SIZE_BYTES,
    PostgresConfig, SqliteConfig, SqliteTempStore, ToolRuntimeConfig, default_database_url, ensure_agentic_api_home,
    normalize_base_url,
};
use agentic_core::error::Error;
use agentic_server::app::DEFAULT_MAX_REQUEST_BODY_SIZE;
use agentic_server::auth::OidcConfig;
use agentic_server::telemetry::{self, DEFAULT_SHUTDOWN_TIMEOUT, TelemetryConfig, TelemetryError, TelemetryGuard};
use tracing::warn;

mod config_file;
mod responses_config;
mod server;
mod web_search_config;
use responses_config::{generated_responses_file_config, resolve_responses_config};

use config_file::{
    CodeInterpreterFileConfig, FileConfig, McpFileConfig, MessagesGatewayFileConfig, ServerFileConfig, ToolsFileConfig,
};
use server::GatewayOptions;
use web_search_config::{generated_web_search_file_config, resolve_web_search_config};

/// Environment override for the serialized request-size ceiling.
const MAX_REQUEST_BODY_SIZE_ENV: &str = "AGENTIC_MAX_REQUEST_BODY_SIZE_BYTES";

#[derive(Args, Clone)]
struct CommonArgs {
    #[arg(long, env = "OPENAI_API_KEY", hide_env_values = true, global = true)]
    openai_api_key: Option<String>,

    /// OIDC issuer for optional inbound bearer-token authentication.
    #[arg(long, env = "OIDC_ISSUER", global = true)]
    oidc_issuer: Option<String>,

    /// Required bearer-token audience when `OIDC_ISSUER` is configured.
    #[arg(long, env = "OIDC_AUDIENCE", global = true)]
    oidc_audience: Option<String>,

    #[arg(long, env = "GATEWAY_HOST", default_value = "0.0.0.0", global = true)]
    gateway_host: String,

    #[arg(long, env = "GATEWAY_PORT", default_value_t = 9000, global = true)]
    gateway_port: u16,

    #[arg(long, default_value_t = 600.0, global = true)]
    llm_ready_timeout_s: f64,

    #[arg(long, default_value_t = 2.0, global = true)]
    llm_ready_interval_s: f64,

    /// Skip the upstream /health readiness probe. Useful for hosted OpenAI-compatible providers.
    #[arg(long, env = "SKIP_LLM_READY_CHECK", default_value_t = false, global = true)]
    skip_llm_ready_check: bool,

    /// Maximum serialized request size in bytes for HTTP bodies and WebSocket messages.
    /// Covers JSON overhead, replayed history, and base64 attachments; unrelated to the
    /// upstream token context limit. Overrides `AGENTIC_MAX_REQUEST_BODY_SIZE_BYTES` and
    /// `server.max_request_body_size_bytes` in the configuration file.
    #[arg(long, global = true)]
    max_request_body_size_bytes: Option<NonZeroUsize>,

    /// `SQLite` or `PostgreSQL` URL for conversation and response storage.
    /// Defaults to `agentic_api.db` in the Agentic API home directory.
    #[arg(
        long,
        visible_alias = "database-url",
        env = "DATABASE_URL",
        hide_env_values = true,
        global = true
    )]
    db_url: Option<String>,
}

fn oidc_config_from_values(
    issuer: Option<&str>,
    audience: Option<&str>,
) -> Result<Option<OidcConfig>, server::ServerError> {
    match (issuer, audience) {
        (None, None) => Ok(None),
        (Some(issuer), Some(audience)) => Ok(Some(OidcConfig::new(issuer, audience)?)),
        _ => Err(Error::Config("OIDC_ISSUER and OIDC_AUDIENCE must be configured together".to_owned()).into()),
    }
}

#[derive(Parser)]
#[command(
    name = "agentic-server",
    about = "Stateful API gateway for vLLM Responses API",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Base URL for the standalone OpenAI-compatible inference server.
    #[arg(long, env = "LLM_API_BASE")]
    llm_api_base: Option<String>,

    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Subcommand)]
enum Commands {
    /// Spawn vLLM and run the gateway in the foreground
    Serve {
        /// Model name or path
        model: String,

        /// vLLM server port
        #[arg(long, default_value_t = 8000)]
        port: u16,

        /// Additional arguments passed through to vLLM
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        llm_args: Vec<String>,
    },
}

fn parse_env_u64(name: &str, default: u64) -> Result<u64, Error> {
    parse_env_u64_value(name, std::env::var(name), default)
}

fn parse_env_u64_value(name: &str, value: Result<String, std::env::VarError>, default: u64) -> Result<u64, Error> {
    match value {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|e| Error::Config(format!("{name} must be an unsigned integer: {e}"))),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(Error::Config(format!("failed to read {name}: {e}"))),
    }
}

fn parse_env_u32(name: &str, default: u32) -> Result<u32, Error> {
    parse_env_u32_value(name, std::env::var(name), default)
}

fn parse_env_u32_value(name: &str, value: Result<String, std::env::VarError>, default: u32) -> Result<u32, Error> {
    match value {
        Ok(value) => {
            let parsed = value
                .parse::<u32>()
                .map_err(|e| Error::Config(format!("{name} must be a positive integer: {e}")))?;
            if parsed == 0 {
                return Err(Error::Config(format!("{name} must be greater than 0")));
            }
            Ok(parsed)
        }
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(Error::Config(format!("failed to read {name}: {e}"))),
    }
}

fn parse_env_nonzero_usize(name: &str, default: NonZeroUsize) -> Result<NonZeroUsize, Error> {
    parse_env_nonzero_usize_value(name, std::env::var(name), default)
}

fn parse_env_nonzero_usize_value(
    name: &str,
    value: Result<String, std::env::VarError>,
    default: NonZeroUsize,
) -> Result<NonZeroUsize, Error> {
    match value {
        Ok(value) => value
            .parse::<NonZeroUsize>()
            .map_err(|error| Error::Config(format!("{name} must be a positive integer: {error}"))),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(Error::Config(format!("failed to read {name}: {error}"))),
    }
}

fn parse_env_nonzero_u64_value(
    name: &str,
    value: Result<String, std::env::VarError>,
    default: NonZeroU64,
) -> Result<NonZeroU64, Error> {
    match value {
        Ok(value) => value
            .parse::<NonZeroU64>()
            .map_err(|error| Error::Config(format!("{name} must be a positive integer: {error}"))),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(Error::Config(format!("failed to read {name}: {error}"))),
    }
}

fn parse_env_bool_value(name: &str, value: Result<String, std::env::VarError>, default: bool) -> Result<bool, Error> {
    match value {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            _ => Err(Error::Config(format!("{name} must be true, false, 1, or 0"))),
        },
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(Error::Config(format!("failed to read {name}: {error}"))),
    }
}

/// The process-environment inputs that can override code-interpreter settings.
///
/// Keeping the raw values together lets the configuration builder use the
/// exact production precedence while tests exercise that path without changing
/// process-global environment variables in parallel.
struct CodeInterpreterEnvironmentValues {
    enabled: Result<String, std::env::VarError>,
    max_source_bytes: Result<String, std::env::VarError>,
    execution_wall_time_seconds: Result<String, std::env::VarError>,
    max_fuel: Result<String, std::env::VarError>,
    max_guest_memory_bytes: Result<String, std::env::VarError>,
    max_stdout_bytes: Result<String, std::env::VarError>,
    max_stderr_bytes: Result<String, std::env::VarError>,
    max_concurrent_guests: Result<String, std::env::VarError>,
    max_aggregate_guest_memory_bytes: Result<String, std::env::VarError>,
}

impl CodeInterpreterEnvironmentValues {
    fn from_process() -> Self {
        Self {
            enabled: std::env::var("AGENTIC_CODE_INTERPRETER_ENABLED"),
            max_source_bytes: std::env::var("AGENTIC_CODE_INTERPRETER_MAX_SOURCE_BYTES"),
            execution_wall_time_seconds: std::env::var("AGENTIC_CODE_INTERPRETER_EXECUTION_WALL_TIME_SECONDS"),
            max_fuel: std::env::var("AGENTIC_CODE_INTERPRETER_MAX_FUEL"),
            max_guest_memory_bytes: std::env::var("AGENTIC_CODE_INTERPRETER_MAX_GUEST_MEMORY_BYTES"),
            max_stdout_bytes: std::env::var("AGENTIC_CODE_INTERPRETER_MAX_STDOUT_BYTES"),
            max_stderr_bytes: std::env::var("AGENTIC_CODE_INTERPRETER_MAX_STDERR_BYTES"),
            max_concurrent_guests: std::env::var("AGENTIC_CODE_INTERPRETER_MAX_CONCURRENT_GUESTS"),
            max_aggregate_guest_memory_bytes: std::env::var(
                "AGENTIC_CODE_INTERPRETER_MAX_AGGREGATE_GUEST_MEMORY_BYTES",
            ),
        }
    }

    #[cfg(test)]
    fn not_present() -> Self {
        Self {
            enabled: Err(std::env::VarError::NotPresent),
            max_source_bytes: Err(std::env::VarError::NotPresent),
            execution_wall_time_seconds: Err(std::env::VarError::NotPresent),
            max_fuel: Err(std::env::VarError::NotPresent),
            max_guest_memory_bytes: Err(std::env::VarError::NotPresent),
            max_stdout_bytes: Err(std::env::VarError::NotPresent),
            max_stderr_bytes: Err(std::env::VarError::NotPresent),
            max_concurrent_guests: Err(std::env::VarError::NotPresent),
            max_aggregate_guest_memory_bytes: Err(std::env::VarError::NotPresent),
        }
    }
}

/// Resolves the request-size ceiling as CLI argument > environment variable >
/// configuration file > default.
///
/// An explicit CLI argument short-circuits the lower-priority sources, so a
/// stale or malformed `AGENTIC_MAX_REQUEST_BODY_SIZE_BYTES` inherited from the
/// environment cannot block startup when the operator names a valid value.
fn resolve_max_request_body_size(cli: Option<NonZeroUsize>, file: Option<NonZeroUsize>) -> Result<NonZeroUsize, Error> {
    resolve_max_request_body_size_value(cli, file, std::env::var(MAX_REQUEST_BODY_SIZE_ENV))
}

fn resolve_max_request_body_size_value(
    cli: Option<NonZeroUsize>,
    file: Option<NonZeroUsize>,
    value: Result<String, std::env::VarError>,
) -> Result<NonZeroUsize, Error> {
    if let Some(cli) = cli {
        return Ok(cli);
    }
    parse_env_nonzero_usize_value(
        MAX_REQUEST_BODY_SIZE_ENV,
        value,
        file.unwrap_or(DEFAULT_MAX_REQUEST_BODY_SIZE),
    )
}

fn parse_env_duration(name: &str, default_seconds: u64) -> Result<Duration, Error> {
    parse_env_duration_value(name, std::env::var(name), default_seconds)
}

fn parse_env_duration_value(
    name: &str,
    value: Result<String, std::env::VarError>,
    default_seconds: u64,
) -> Result<Duration, Error> {
    let seconds = parse_env_u64_value(name, value, default_seconds)?;
    if seconds == 0 {
        return Err(Error::Config(format!("{name} must be greater than 0")));
    }
    Ok(Duration::from_secs(seconds))
}

fn parse_env_optional_duration(name: &str, default_seconds: u64) -> Result<Option<Duration>, Error> {
    parse_env_optional_duration_value(name, std::env::var(name), default_seconds)
}

fn parse_env_optional_duration_value(
    name: &str,
    value: Result<String, std::env::VarError>,
    default_seconds: u64,
) -> Result<Option<Duration>, Error> {
    let seconds = parse_env_u64_value(name, value, default_seconds)?;
    Ok((seconds > 0).then(|| Duration::from_secs(seconds)))
}

fn parse_env_temp_store() -> Result<SqliteTempStore, Error> {
    parse_env_temp_store_value(std::env::var("SQLITE_TEMP_STORE"))
}

fn parse_env_temp_store_value(value: Result<String, std::env::VarError>) -> Result<SqliteTempStore, Error> {
    match value {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "default" | "0" => Ok(SqliteTempStore::Default),
            "file" | "1" => Ok(SqliteTempStore::File),
            "memory" | "2" => Ok(SqliteTempStore::Memory),
            _ => Err(Error::Config(
                "SQLITE_TEMP_STORE must be one of default, file, memory, 0, 1, or 2".to_owned(),
            )),
        },
        Err(std::env::VarError::NotPresent) => Ok(SqliteTempStore::default()),
        Err(e) => Err(Error::Config(format!("failed to read SQLITE_TEMP_STORE: {e}"))),
    }
}

fn sqlite_config_from_env() -> Result<SqliteConfig, Error> {
    Ok(SqliteConfig {
        max_connections: parse_env_u32("SQLITE_MAX_CONNECTIONS", DEFAULT_SQLITE_MAX_CONNECTIONS)?,
        journal_size_limit_bytes: parse_env_u64(
            "SQLITE_JOURNAL_SIZE_LIMIT_BYTES",
            DEFAULT_SQLITE_JOURNAL_SIZE_LIMIT_BYTES,
        )?,
        temp_store: parse_env_temp_store()?,
        mmap_size_bytes: parse_env_u64("SQLITE_MMAP_SIZE_BYTES", DEFAULT_SQLITE_MMAP_SIZE_BYTES)?,
    })
}

fn postgres_config_from_env() -> Result<PostgresConfig, Error> {
    Ok(PostgresConfig {
        max_connections: parse_env_u32("POSTGRES_MAX_CONNECTIONS", DEFAULT_POSTGRES_MAX_CONNECTIONS)?,
        acquire_timeout: parse_env_duration(
            "POSTGRES_ACQUIRE_TIMEOUT_SECONDS",
            DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS,
        )?,
        lock_timeout: parse_env_duration("POSTGRES_LOCK_TIMEOUT_SECONDS", DEFAULT_POSTGRES_LOCK_TIMEOUT_SECONDS)?,
        migration_timeout: parse_env_duration(
            "POSTGRES_MIGRATION_TIMEOUT_SECONDS",
            DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS,
        )?,
        statement_timeout: parse_env_duration(
            "POSTGRES_STATEMENT_TIMEOUT_SECONDS",
            DEFAULT_POSTGRES_STATEMENT_TIMEOUT_SECONDS,
        )?,
        idle_timeout: parse_env_optional_duration(
            "POSTGRES_IDLE_TIMEOUT_SECONDS",
            DEFAULT_POSTGRES_IDLE_TIMEOUT_SECONDS,
        )?,
        max_lifetime: parse_env_optional_duration(
            "POSTGRES_MAX_LIFETIME_SECONDS",
            DEFAULT_POSTGRES_MAX_LIFETIME_SECONDS,
        )?,
    })
}

fn database_configs_from_env(database_url: &str) -> Result<(PostgresConfig, SqliteConfig), Error> {
    let backend = DatabaseBackend::from_url(database_url)
        .map_err(|error| Error::Config(format!("invalid DATABASE_URL: {error}")))?;
    match backend {
        DatabaseBackend::Postgres => Ok((postgres_config_from_env()?, SqliteConfig::default())),
        DatabaseBackend::Sqlite => Ok((PostgresConfig::default(), sqlite_config_from_env()?)),
        DatabaseBackend::Other => Ok((PostgresConfig::default(), SqliteConfig::default())),
    }
}

/// Resolve operator-owned code-interpreter limits with environment precedence,
/// then config-file values, then disabled defaults. Availability still depends
/// on the Cargo feature and successful embedded-runtime initialization.
fn code_interpreter_config_from_operator_values(
    file: &CodeInterpreterFileConfig,
    values: CodeInterpreterEnvironmentValues,
) -> Result<CodeInterpreterRuntimeConfig, Error> {
    let defaults = file.with_defaults();
    let config = CodeInterpreterRuntimeConfig {
        enabled: parse_env_bool_value("AGENTIC_CODE_INTERPRETER_ENABLED", values.enabled, defaults.enabled)?,
        max_source_bytes: parse_env_nonzero_usize_value(
            "AGENTIC_CODE_INTERPRETER_MAX_SOURCE_BYTES",
            values.max_source_bytes,
            defaults.max_source_bytes,
        )?,
        execution_wall_time: parse_env_duration_value(
            "AGENTIC_CODE_INTERPRETER_EXECUTION_WALL_TIME_SECONDS",
            values.execution_wall_time_seconds,
            defaults.execution_wall_time.as_secs(),
        )?,
        max_fuel: parse_env_nonzero_u64_value("AGENTIC_CODE_INTERPRETER_MAX_FUEL", values.max_fuel, defaults.max_fuel)?,
        max_guest_memory_bytes: parse_env_nonzero_usize_value(
            "AGENTIC_CODE_INTERPRETER_MAX_GUEST_MEMORY_BYTES",
            values.max_guest_memory_bytes,
            defaults.max_guest_memory_bytes,
        )?,
        max_stdout_bytes: parse_env_nonzero_usize_value(
            "AGENTIC_CODE_INTERPRETER_MAX_STDOUT_BYTES",
            values.max_stdout_bytes,
            defaults.max_stdout_bytes,
        )?,
        max_stderr_bytes: parse_env_nonzero_usize_value(
            "AGENTIC_CODE_INTERPRETER_MAX_STDERR_BYTES",
            values.max_stderr_bytes,
            defaults.max_stderr_bytes,
        )?,
        max_concurrent_guests: parse_env_nonzero_usize_value(
            "AGENTIC_CODE_INTERPRETER_MAX_CONCURRENT_GUESTS",
            values.max_concurrent_guests,
            defaults.max_concurrent_guests,
        )?,
        max_aggregate_guest_memory_bytes: parse_env_nonzero_usize_value(
            "AGENTIC_CODE_INTERPRETER_MAX_AGGREGATE_GUEST_MEMORY_BYTES",
            values.max_aggregate_guest_memory_bytes,
            defaults.max_aggregate_guest_memory_bytes,
        )?,
    };
    config
        .validate()
        .map_err(|error| Error::Config(format!("invalid code interpreter configuration: {error}")))?;
    Ok(config)
}

fn build_config(llm_api_base: String, common: &CommonArgs, file: &FileConfig) -> Result<Config, Error> {
    let db_url = common
        .db_url
        .clone()
        .or_else(|| file.database_url.clone())
        .map_or_else(default_database_url, Ok)?;
    let (postgres, sqlite) = database_configs_from_env(&db_url)?;
    let web_search = resolve_web_search_config(&file.web_search, environment_value)?;
    let mcp_allowed_hosts = environment_value("AGENTIC_MCP_ALLOWED_HOSTS")
        .map_or_else(|| file.mcp.allowed_hosts.clone(), |value| parse_comma_separated(&value));
    let max_concurrent_gateway_calls_default = file
        .tools
        .max_concurrent_gateway_calls
        .unwrap_or(DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS);
    let max_concurrent_gateway_calls = parse_env_nonzero_usize(
        "AGENTIC_MAX_CONCURRENT_GATEWAY_CALLS",
        max_concurrent_gateway_calls_default,
    )?;
    let code_interpreter = code_interpreter_config_from_operator_values(
        &file.code_interpreter,
        CodeInterpreterEnvironmentValues::from_process(),
    )?;
    let responses_config = resolve_responses_config(&file.responses)?;
    Ok(Config {
        llm_api_base,
        openai_api_key: common.openai_api_key.clone(),
        llm_ready_timeout_s: common.llm_ready_timeout_s,
        llm_ready_interval_s: common.llm_ready_interval_s,
        skip_llm_ready_check: common.skip_llm_ready_check,
        db_url: Some(db_url),
        postgres,
        sqlite,
        tools: ToolRuntimeConfig {
            web_search,
            mcp_servers: file.mcp_servers.clone(),
            mcp_allowed_hosts,
            messages_gateway_tool_aliases: file.messages_gateway.tool_aliases.clone(),
            code_interpreter,
            max_concurrent_gateway_calls,
        },
        responses: responses_config,
    })
}

fn gateway_options<'a>(
    common: &'a CommonArgs,
    file: &FileConfig,
    oidc: Option<OidcConfig>,
) -> Result<GatewayOptions<'a>, Error> {
    Ok(GatewayOptions {
        model_capabilities: file.model_capabilities(),
        host: &common.gateway_host,
        port: common.gateway_port,
        max_request_body_size: resolve_max_request_body_size(
            common.max_request_body_size_bytes,
            file.server.max_request_body_size_bytes,
        )?,
        oidc,
    })
}

fn generated_file_config(llm_api_base: String) -> FileConfig {
    FileConfig {
        llm_api_base: Some(llm_api_base),
        web_search: generated_web_search_file_config(environment_value),
        mcp: McpFileConfig {
            allowed_hosts: environment_value("AGENTIC_MCP_ALLOWED_HOSTS")
                .map_or_else(Vec::new, |value| parse_comma_separated(&value)),
        },
        server: ServerFileConfig {
            max_request_body_size_bytes: environment_value(MAX_REQUEST_BODY_SIZE_ENV)
                .and_then(|value| value.parse::<NonZeroUsize>().ok()),
        },
        tools: ToolsFileConfig {
            max_concurrent_gateway_calls: environment_value("AGENTIC_MAX_CONCURRENT_GATEWAY_CALLS")
                .and_then(|value| value.parse::<NonZeroUsize>().ok()),
        },
        code_interpreter: CodeInterpreterFileConfig::default(),
        messages_gateway: MessagesGatewayFileConfig {
            tool_aliases: environment_value("MESSAGES_GATEWAY_TOOL_ALIASES"),
        },
        responses: generated_responses_file_config(),
        mcp_servers: HashMap::new(),
        ..FileConfig::default()
    }
}

fn environment_value(name: &str) -> Option<String> {
    clean_value(std::env::var(name).ok())
}

fn clean_value(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn parse_comma_separated(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Upper bound for stopping the runtime once the gateway has drained. Request
/// tasks the drain deadline abandoned are dropped here, which finalizes their
/// spans and metrics; only in-flight blocking work can hold this up.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

fn main() -> Result<(), server::ServerError> {
    // Parse first so `--help`/`--version` never build exporters.
    let cli = Cli::parse();
    // Providers and the subscriber are created outside the runtime so the
    // guard outlives every task.
    let telemetry_config = TelemetryConfig::from_env().map_err(TelemetryError::from)?;
    let telemetry = telemetry::init(&telemetry_config)?;
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(run(cli));
    // Stop the runtime before flushing telemetry: connection tasks that outlived
    // the gateway drain are dropped now, so their final measurements land in
    // providers that are still accepting them.
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    shutdown_telemetry(telemetry);
    result
}

/// Flush exported telemetry after the runtime has stopped; failures are
/// logged rather than surfaced because the gateway result is what matters.
fn shutdown_telemetry(telemetry: TelemetryGuard) {
    if let Err(error) = telemetry.shutdown_blocking(DEFAULT_SHUTDOWN_TIMEOUT) {
        warn!(%error, "telemetry shutdown incomplete");
    }
}

async fn run(cli: Cli) -> Result<(), server::ServerError> {
    let Cli {
        command,
        llm_api_base,
        common,
    } = cli;
    let agentic_home = ensure_agentic_api_home()?;
    let loaded_file_config = FileConfig::load(&agentic_home)?;
    let config_file_missing = loaded_file_config.is_none();
    let mut file_config = loaded_file_config.unwrap_or_default();
    let oidc_config = oidc_config_from_values(common.oidc_issuer.as_deref(), common.oidc_audience.as_deref())?;

    match command {
        None => {
            let base = llm_api_base
                .or_else(|| file_config.llm_api_base.clone())
                .ok_or_else(|| {
                Error::Config(
                    "standalone mode requires llm_api_base in config.toml, LLM_API_BASE, or --llm-api-base; use `agentic-server serve <model>` for integrated mode"
                        .to_owned(),
                )
            })?;
            if config_file_missing {
                file_config = generated_file_config(base.clone()).create_or_load(&agentic_home)?;
            }
            let config = build_config(normalize_base_url(&base), &common, &file_config)?;
            let gateway = gateway_options(&common, &file_config, oidc_config)?;
            server::run(config, gateway).await
        }
        Some(Commands::Serve { model, port, llm_args }) => {
            if llm_api_base.is_some() {
                return Err(Error::Config(
                    "--llm-api-base is only valid in standalone mode; remove it when using `serve`".to_owned(),
                )
                .into());
            }
            if config_file_missing {
                file_config =
                    generated_file_config(format!("http://127.0.0.1:{port}")).create_or_load(&agentic_home)?;
            }
            let config = build_config(
                normalize_base_url(&format!("http://127.0.0.1:{port}")),
                &common,
                &file_config,
            )?;
            let mut args = vec!["--model".to_owned(), model];
            args.push("--port".to_owned());
            args.push(port.to_string());
            args.extend(llm_args);
            let gateway = gateway_options(&common, &file_config, oidc_config)?;
            server::run_with_llm(config, gateway, args).await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU64, NonZeroUsize};
    use std::time::Duration;

    use clap::{CommandFactory, Parser};

    use super::config_file::FileConfig;
    use super::{
        Cli, CodeInterpreterEnvironmentValues, Commands, code_interpreter_config_from_operator_values,
        database_configs_from_env, oidc_config_from_values, parse_env_bool_value, parse_env_duration_value,
        parse_env_nonzero_u64_value, parse_env_nonzero_usize_value, parse_env_optional_duration_value,
        parse_env_temp_store_value, parse_env_u32_value, parse_env_u64_value, resolve_max_request_body_size_value,
    };
    use agentic_core::config::{
        DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS, DEFAULT_POSTGRES_IDLE_TIMEOUT_SECONDS,
        DEFAULT_POSTGRES_LOCK_TIMEOUT_SECONDS, DEFAULT_POSTGRES_MAX_CONNECTIONS,
        DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS, DEFAULT_POSTGRES_STATEMENT_TIMEOUT_SECONDS,
        DEFAULT_SQLITE_MAX_CONNECTIONS, SqliteTempStore,
    };
    use agentic_server::app::DEFAULT_MAX_REQUEST_BODY_SIZE;

    #[test]
    fn serve_uses_common_args_before_subcommand() {
        let cli = Cli::parse_from(["agentic-server", "--llm-ready-timeout-s", "0.1", "serve", "model-a"]);
        assert!((cli.common.llm_ready_timeout_s - 0.1).abs() < f64::EPSILON);
        assert!(matches!(cli.command, Some(Commands::Serve { .. })));
    }

    #[test]
    fn serve_uses_common_args_after_subcommand() {
        let cli = Cli::parse_from(["agentic-server", "serve", "--llm-ready-timeout-s", "0.1", "model-a"]);
        assert!((cli.common.llm_ready_timeout_s - 0.1).abs() < f64::EPSILON);
        assert!(matches!(cli.command, Some(Commands::Serve { .. })));
    }

    #[test]
    fn skip_llm_ready_check_can_be_set_from_cli() {
        let cli = Cli::parse_from([
            "agentic-server",
            "--llm-api-base",
            "http://localhost:8000",
            "--skip-llm-ready-check",
        ]);
        assert!(cli.common.skip_llm_ready_check);
    }

    #[test]
    fn standalone_base_url_uses_llm_api_base_flag() {
        let cli = Cli::parse_from(["agentic-server", "--llm-api-base", "http://localhost:8000"]);

        assert_eq!(cli.llm_api_base.as_deref(), Some("http://localhost:8000"));
    }

    #[test]
    fn oidc_configuration_requires_issuer_and_audience_together() {
        assert!(oidc_config_from_values(None, None).expect("disabled OIDC").is_none());
        assert!(oidc_config_from_values(Some("https://issuer.example"), None).is_err());
        assert!(oidc_config_from_values(None, Some("agentic-api")).is_err());
        assert!(
            oidc_config_from_values(Some("https://issuer.example"), Some("agentic-api"))
                .expect("complete OIDC configuration")
                .is_some()
        );
    }

    #[test]
    fn container_runtime_options_are_bound_to_environment_variables() {
        let command = Cli::command();

        for (argument, expected_env) in [
            ("llm_api_base", "LLM_API_BASE"),
            ("gateway_host", "GATEWAY_HOST"),
            ("gateway_port", "GATEWAY_PORT"),
            ("oidc_issuer", "OIDC_ISSUER"),
            ("oidc_audience", "OIDC_AUDIENCE"),
        ] {
            let env = command
                .get_arguments()
                .find(|arg| arg.get_id() == argument)
                .and_then(clap::Arg::get_env)
                .unwrap_or_else(|| panic!("{argument} must be configurable through {expected_env}"));

            assert_eq!(env, expected_env);
        }
    }

    #[test]
    fn sqlite_tuning_is_env_only_not_cli() {
        let mut help = Vec::new();
        Cli::command().write_long_help(&mut help).expect("render help");
        let help = String::from_utf8(help).expect("help is utf8");

        assert!(!help.contains("--sqlite-journal-size-limit-bytes"));
        assert!(!help.contains("--sqlite-max-connections"));
        assert!(!help.contains("--sqlite-temp-store"));
        assert!(!help.contains("--sqlite-mmap-size-bytes"));

        assert!(
            Cli::try_parse_from([
                "agentic-server",
                "--llm-api-base",
                "http://localhost:8000",
                "--sqlite-temp-store",
                "memory",
            ])
            .is_err()
        );
    }

    #[test]
    fn sqlite_tuning_env_parser_uses_defaults_and_rejects_invalid_values() {
        assert_eq!(
            parse_env_u32_value(
                "SQLITE_MAX_CONNECTIONS",
                Err(std::env::VarError::NotPresent),
                DEFAULT_SQLITE_MAX_CONNECTIONS
            )
            .expect("default value"),
            DEFAULT_SQLITE_MAX_CONNECTIONS
        );
        assert_eq!(
            parse_env_u32_value(
                "SQLITE_MAX_CONNECTIONS",
                Ok("6".to_owned()),
                DEFAULT_SQLITE_MAX_CONNECTIONS
            )
            .expect("parsed value"),
            6
        );
        assert!(
            parse_env_u32_value(
                "SQLITE_MAX_CONNECTIONS",
                Ok("0".to_owned()),
                DEFAULT_SQLITE_MAX_CONNECTIONS
            )
            .is_err()
        );
        assert!(
            parse_env_u32_value(
                "SQLITE_MAX_CONNECTIONS",
                Ok("not-a-number".to_owned()),
                DEFAULT_SQLITE_MAX_CONNECTIONS
            )
            .is_err()
        );

        assert_eq!(
            parse_env_u64_value("SQLITE_MMAP_SIZE_BYTES", Err(std::env::VarError::NotPresent), 1_024)
                .expect("default value"),
            1_024
        );
        assert_eq!(
            parse_env_u64_value("SQLITE_MMAP_SIZE_BYTES", Ok("4096".to_owned()), 1_024).expect("parsed value"),
            4_096
        );
        assert!(parse_env_u64_value("SQLITE_MMAP_SIZE_BYTES", Ok("not-a-number".to_owned()), 1_024).is_err());

        assert_eq!(
            parse_env_temp_store_value(Err(std::env::VarError::NotPresent)).expect("default temp store"),
            SqliteTempStore::Memory
        );
        assert_eq!(
            parse_env_temp_store_value(Ok("file".to_owned())).expect("file temp store"),
            SqliteTempStore::File
        );
        assert_eq!(
            parse_env_temp_store_value(Ok("2".to_owned())).expect("memory temp store"),
            SqliteTempStore::Memory
        );
        assert!(parse_env_temp_store_value(Ok("invalid".to_owned())).is_err());
    }

    #[test]
    fn gateway_concurrency_env_parser_requires_a_nonzero_value() {
        let default = agentic_core::config::DEFAULT_MAX_CONCURRENT_GATEWAY_CALLS;
        assert_eq!(
            parse_env_nonzero_usize_value(
                "AGENTIC_MAX_CONCURRENT_GATEWAY_CALLS",
                Err(std::env::VarError::NotPresent),
                default,
            )
            .expect("default value"),
            default
        );
        assert_eq!(
            parse_env_nonzero_usize_value("AGENTIC_MAX_CONCURRENT_GATEWAY_CALLS", Ok("3".to_owned()), default,)
                .expect("positive value")
                .get(),
            3
        );
        assert!(
            parse_env_nonzero_usize_value("AGENTIC_MAX_CONCURRENT_GATEWAY_CALLS", Ok("0".to_owned()), default,)
                .is_err()
        );
    }

    #[test]
    fn code_interpreter_environment_parsers_are_strict_and_default_disabled() {
        assert!(
            !parse_env_bool_value(
                "AGENTIC_CODE_INTERPRETER_ENABLED",
                Err(std::env::VarError::NotPresent),
                false,
            )
            .expect("missing enablement uses the disabled default")
        );
        assert!(
            parse_env_bool_value("AGENTIC_CODE_INTERPRETER_ENABLED", Ok("true".to_owned()), false,)
                .expect("true is accepted")
        );
        assert!(
            parse_env_bool_value(
                "AGENTIC_CODE_INTERPRETER_ENABLED",
                Ok("not-a-boolean".to_owned()),
                false,
            )
            .is_err()
        );

        let default_fuel = NonZeroU64::new(10).expect("nonzero test default");
        assert_eq!(
            parse_env_nonzero_u64_value("AGENTIC_CODE_INTERPRETER_MAX_FUEL", Ok("25".to_owned()), default_fuel,)
                .expect("positive fuel limit"),
            NonZeroU64::new(25).expect("nonzero test value")
        );
        assert!(
            parse_env_nonzero_u64_value("AGENTIC_CODE_INTERPRETER_MAX_FUEL", Ok("0".to_owned()), default_fuel,)
                .is_err()
        );
    }

    #[test]
    fn code_interpreter_operator_config_prefers_environment_over_file() {
        let file: FileConfig = toml::from_str(concat!(
            "[code_interpreter]\n",
            "enabled = false\n",
            "max_source_bytes = 313\n",
            "max_stdout_bytes = 64\n"
        ))
        .expect("valid code-interpreter file configuration");
        let mut environment = CodeInterpreterEnvironmentValues::not_present();
        environment.enabled = Ok("true".to_owned());
        environment.max_stdout_bytes = Ok("512".to_owned());
        let config = code_interpreter_config_from_operator_values(&file.code_interpreter, environment)
            .expect("valid operator configuration");

        assert!(config.enabled, "environment overrides the file enablement");
        assert_eq!(config.max_source_bytes.get(), 313, "file value is retained");
        assert_eq!(
            config.max_stdout_bytes.get(),
            512,
            "environment overrides the file output budget"
        );
    }

    #[test]
    fn postgres_timeout_parser_uses_defaults_and_allows_disabling_recycling() {
        assert_eq!(
            parse_env_duration_value(
                "POSTGRES_ACQUIRE_TIMEOUT_SECONDS",
                Err(std::env::VarError::NotPresent),
                DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS,
            )
            .expect("default acquire timeout"),
            Duration::from_secs(DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS)
        );
        assert_eq!(
            parse_env_duration_value(
                "POSTGRES_ACQUIRE_TIMEOUT_SECONDS",
                Ok("9".to_owned()),
                DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS,
            )
            .expect("explicit acquire timeout"),
            Duration::from_secs(9)
        );
        assert!(
            parse_env_duration_value(
                "POSTGRES_ACQUIRE_TIMEOUT_SECONDS",
                Ok("0".to_owned()),
                DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS,
            )
            .is_err()
        );
        assert_eq!(
            parse_env_optional_duration_value(
                "POSTGRES_IDLE_TIMEOUT_SECONDS",
                Ok("0".to_owned()),
                DEFAULT_POSTGRES_IDLE_TIMEOUT_SECONDS,
            )
            .expect("disabled idle timeout"),
            None
        );
        assert_eq!(
            parse_env_optional_duration_value(
                "POSTGRES_IDLE_TIMEOUT_SECONDS",
                Ok("45".to_owned()),
                DEFAULT_POSTGRES_IDLE_TIMEOUT_SECONDS,
            )
            .expect("explicit idle timeout"),
            Some(Duration::from_secs(45))
        );
        assert_eq!(
            parse_env_duration_value(
                "POSTGRES_LOCK_TIMEOUT_SECONDS",
                Err(std::env::VarError::NotPresent),
                DEFAULT_POSTGRES_LOCK_TIMEOUT_SECONDS,
            )
            .expect("default lock timeout"),
            Duration::from_secs(DEFAULT_POSTGRES_LOCK_TIMEOUT_SECONDS)
        );
        assert_eq!(
            parse_env_duration_value(
                "POSTGRES_MIGRATION_TIMEOUT_SECONDS",
                Err(std::env::VarError::NotPresent),
                DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS,
            )
            .expect("default migration timeout"),
            Duration::from_secs(DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS)
        );
        assert_eq!(
            parse_env_duration_value(
                "POSTGRES_STATEMENT_TIMEOUT_SECONDS",
                Err(std::env::VarError::NotPresent),
                DEFAULT_POSTGRES_STATEMENT_TIMEOUT_SECONDS,
            )
            .expect("default statement timeout"),
            Duration::from_secs(DEFAULT_POSTGRES_STATEMENT_TIMEOUT_SECONDS)
        );
        assert!(
            parse_env_u32_value(
                "POSTGRES_MAX_CONNECTIONS",
                Ok("0".to_owned()),
                DEFAULT_POSTGRES_MAX_CONNECTIONS,
            )
            .is_err()
        );
    }

    #[test]
    fn max_request_body_size_is_configurable_from_cli_env_and_file() {
        let cli = NonZeroUsize::new(4_096);
        let file = NonZeroUsize::new(2_048);
        let missing = || Err(std::env::VarError::NotPresent);

        assert_eq!(
            resolve_max_request_body_size_value(None, None, missing()).expect("default value"),
            DEFAULT_MAX_REQUEST_BODY_SIZE
        );
        assert_eq!(
            resolve_max_request_body_size_value(None, file, missing()).expect("file value"),
            file.expect("nonzero")
        );
        assert_eq!(
            resolve_max_request_body_size_value(None, file, Ok("8192".to_owned()))
                .expect("environment overrides the file")
                .get(),
            8_192
        );
        assert_eq!(
            resolve_max_request_body_size_value(cli, file, Ok("8192".to_owned()))
                .expect("CLI overrides the environment")
                .get(),
            4_096
        );
    }

    #[test]
    fn max_request_body_size_rejects_invalid_environment_overrides() {
        let cli = NonZeroUsize::new(4_096);

        assert!(resolve_max_request_body_size_value(None, None, Ok("0".to_owned())).is_err());
        assert!(resolve_max_request_body_size_value(None, None, Ok("-1".to_owned())).is_err());
        assert!(resolve_max_request_body_size_value(None, None, Ok("not-a-number".to_owned())).is_err());

        // An explicit CLI argument outranks the environment, so a stale or malformed
        // inherited value cannot block startup.
        assert_eq!(
            resolve_max_request_body_size_value(cli, None, Ok("0".to_owned()))
                .expect("CLI argument overrides a malformed environment value")
                .get(),
            4_096
        );
        assert_eq!(
            resolve_max_request_body_size_value(cli, None, Ok("not-a-number".to_owned()))
                .expect("CLI argument overrides an unparsable environment value")
                .get(),
            4_096
        );
    }

    #[test]
    fn max_request_body_size_argument_is_global() {
        let before = Cli::parse_from([
            "agentic-server",
            "--max-request-body-size-bytes",
            "4096",
            "serve",
            "model-a",
        ]);
        let after = Cli::parse_from([
            "agentic-server",
            "serve",
            "model-a",
            "--max-request-body-size-bytes",
            "4096",
        ]);

        assert_eq!(
            before.common.max_request_body_size_bytes.map(NonZeroUsize::get),
            Some(4_096)
        );
        assert_eq!(
            after.common.max_request_body_size_bytes.map(NonZeroUsize::get),
            Some(4_096)
        );
        assert!(
            Cli::try_parse_from([
                "agentic-server",
                "--llm-api-base",
                "http://localhost:8000",
                "--max-request-body-size-bytes",
                "0",
            ])
            .is_err()
        );
    }

    #[test]
    fn database_config_rejects_an_invalid_url() {
        let error = database_configs_from_env("not a database URL").expect_err("invalid URL must be rejected");
        assert!(error.to_string().contains("invalid DATABASE_URL"));
    }
}
