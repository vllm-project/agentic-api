use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

use super::config_file::CodeInterpreterFileConfig;
use agentic_core::DatabaseBackend;
use agentic_core::config::{
    CodeInterpreterRuntimeConfig, DEFAULT_POSTGRES_ACQUIRE_TIMEOUT_SECONDS, DEFAULT_POSTGRES_IDLE_TIMEOUT_SECONDS,
    DEFAULT_POSTGRES_LOCK_TIMEOUT_SECONDS, DEFAULT_POSTGRES_MAX_CONNECTIONS, DEFAULT_POSTGRES_MAX_LIFETIME_SECONDS,
    DEFAULT_POSTGRES_MIGRATION_TIMEOUT_SECONDS, DEFAULT_POSTGRES_STATEMENT_TIMEOUT_SECONDS,
    DEFAULT_SQLITE_JOURNAL_SIZE_LIMIT_BYTES, DEFAULT_SQLITE_MAX_CONNECTIONS, DEFAULT_SQLITE_MMAP_SIZE_BYTES,
    PostgresConfig, SqliteConfig, SqliteTempStore,
};
use agentic_core::error::Error;

pub(super) fn parse_env_u64(name: &str, default: u64) -> Result<u64, Error> {
    parse_env_u64_value(name, std::env::var(name), default)
}

pub(super) fn parse_env_u64_value(
    name: &str,
    value: Result<String, std::env::VarError>,
    default: u64,
) -> Result<u64, Error> {
    match value {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|e| Error::Config(format!("{name} must be an unsigned integer: {e}"))),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(Error::Config(format!("failed to read {name}: {e}"))),
    }
}

pub(super) fn parse_env_u32(name: &str, default: u32) -> Result<u32, Error> {
    parse_env_u32_value(name, std::env::var(name), default)
}

pub(super) fn parse_env_u32_value(
    name: &str,
    value: Result<String, std::env::VarError>,
    default: u32,
) -> Result<u32, Error> {
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

pub(super) fn parse_env_nonzero_usize(name: &str, default: NonZeroUsize) -> Result<NonZeroUsize, Error> {
    parse_env_nonzero_usize_value(name, std::env::var(name), default)
}

pub(super) fn parse_env_nonzero_usize_value(
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

pub(super) fn parse_env_nonzero_u64_value(
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

pub(super) fn parse_env_bool_value(
    name: &str,
    value: Result<String, std::env::VarError>,
    default: bool,
) -> Result<bool, Error> {
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
pub(super) struct CodeInterpreterEnvironmentValues {
    pub(super) enabled: Result<String, std::env::VarError>,
    pub(super) max_source_bytes: Result<String, std::env::VarError>,
    pub(super) execution_wall_time_seconds: Result<String, std::env::VarError>,
    pub(super) max_fuel: Result<String, std::env::VarError>,
    pub(super) max_guest_memory_bytes: Result<String, std::env::VarError>,
    pub(super) max_stdout_bytes: Result<String, std::env::VarError>,
    pub(super) max_stderr_bytes: Result<String, std::env::VarError>,
    pub(super) max_concurrent_guests: Result<String, std::env::VarError>,
    pub(super) max_aggregate_guest_memory_bytes: Result<String, std::env::VarError>,
    pub(super) max_worker_memory_bytes: Result<String, std::env::VarError>,
    pub(super) max_aggregate_worker_memory_bytes: Result<String, std::env::VarError>,
}

impl CodeInterpreterEnvironmentValues {
    pub(super) fn from_process() -> Self {
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
            max_worker_memory_bytes: std::env::var("AGENTIC_CODE_INTERPRETER_MAX_WORKER_MEMORY_BYTES"),
            max_aggregate_worker_memory_bytes: std::env::var(
                "AGENTIC_CODE_INTERPRETER_MAX_AGGREGATE_WORKER_MEMORY_BYTES",
            ),
        }
    }

    #[cfg(test)]
    pub(super) fn not_present() -> Self {
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
            max_worker_memory_bytes: Err(std::env::VarError::NotPresent),
            max_aggregate_worker_memory_bytes: Err(std::env::VarError::NotPresent),
        }
    }
}

pub(super) fn parse_env_duration(name: &str, default_seconds: u64) -> Result<Duration, Error> {
    parse_env_duration_value(name, std::env::var(name), default_seconds)
}

pub(super) fn parse_env_duration_value(
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

pub(super) fn parse_env_optional_duration(name: &str, default_seconds: u64) -> Result<Option<Duration>, Error> {
    parse_env_optional_duration_value(name, std::env::var(name), default_seconds)
}

pub(super) fn parse_env_optional_duration_value(
    name: &str,
    value: Result<String, std::env::VarError>,
    default_seconds: u64,
) -> Result<Option<Duration>, Error> {
    let seconds = parse_env_u64_value(name, value, default_seconds)?;
    Ok((seconds > 0).then(|| Duration::from_secs(seconds)))
}

pub(super) fn parse_env_temp_store() -> Result<SqliteTempStore, Error> {
    parse_env_temp_store_value(std::env::var("SQLITE_TEMP_STORE"))
}

pub(super) fn parse_env_temp_store_value(value: Result<String, std::env::VarError>) -> Result<SqliteTempStore, Error> {
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

pub(super) fn sqlite_config_from_env() -> Result<SqliteConfig, Error> {
    Ok(SqliteConfig {
        max_connections: parse_env_u32("SQLITE_MAX_CONNECTIONS", DEFAULT_SQLITE_MAX_CONNECTIONS)?,
        journal_size_limit_bytes: parse_env_u64(
            "SQLITE_JOURNAL_SIZE_LIMIT_BYTES",
            DEFAULT_SQLITE_JOURNAL_SIZE_LIMIT_BYTES,
        )?,
        temp_store: parse_env_temp_store()?,
        mmap_size_bytes: parse_env_u64("SQLITE_MMAP_SIZE_BYTES", DEFAULT_SQLITE_MMAP_SIZE_BYTES)?,
        recreate_outdated_schema: false,
    })
}

pub(super) fn postgres_config_from_env() -> Result<PostgresConfig, Error> {
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

pub(super) fn database_configs_from_env(database_url: &str) -> Result<(PostgresConfig, SqliteConfig), Error> {
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
pub(super) fn code_interpreter_config_from_operator_values(
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
        max_worker_memory_bytes: parse_env_nonzero_usize_value(
            "AGENTIC_CODE_INTERPRETER_MAX_WORKER_MEMORY_BYTES",
            values.max_worker_memory_bytes,
            defaults.max_worker_memory_bytes,
        )?,
        max_aggregate_worker_memory_bytes: parse_env_nonzero_usize_value(
            "AGENTIC_CODE_INTERPRETER_MAX_AGGREGATE_WORKER_MEMORY_BYTES",
            values.max_aggregate_worker_memory_bytes,
            defaults.max_aggregate_worker_memory_bytes,
        )?,
    };
    config
        .validate()
        .map_err(|error| Error::Config(format!("invalid code interpreter configuration: {error}")))?;
    Ok(config)
}
