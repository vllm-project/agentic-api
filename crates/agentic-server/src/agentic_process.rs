mod catalog;
mod harness;

pub use catalog::resolve_model;
#[cfg(test)]
use catalog::{CATALOG_MODEL_GRACE, CatalogBudget, MAX_CATALOG_BYTES, catalog_selection};
use catalog::{CodexModelSelection, resolve_codex_selection};
#[cfg(test)]
use harness::{build_harness_command, harness_launch_args};
use harness::{harness_environment, prepare_attached_harness_environment, spawn_harness, validate_claude_passthrough};

use std::{ffi::OsString, path::Path, time::Duration};

use agentic_core::error::Error;
use reqwest::Client;
use tokio::time::{Instant, sleep};

use crate::{
    agentic_cli::{CommonOptions, Harness, HarnessOptions, SourceOptions},
    agentic_output::redact_url,
};

/// Reasoning effort passed to Claude Code unless `AGENTIC_CLAUDE_EFFORT` overrides it.
///
/// Qwen chat templates served by vLLM accept `low`, `medium`, and `xhigh`; Claude Code's
/// default of `high` is rejected by the template, so the CLI always pins a compatible value.
pub const DEFAULT_CLAUDE_EFFORT: &str = "medium";
const CLAUDE_EFFORT_ENV: &str = "AGENTIC_CLAUDE_EFFORT";
const PLACEHOLDER_MODEL: &str = "agentic-api";
/// Readiness budget for a gateway the launcher did not start.
const ATTACHED_GATEWAY_TIMEOUT: Duration = Duration::from_secs(30);
/// Readiness poll interval for a gateway the launcher did not start.
const ATTACHED_GATEWAY_INTERVAL: Duration = Duration::from_millis(250);

#[must_use]
pub fn server_args(source: &SourceOptions, common: &CommonOptions) -> Vec<OsString> {
    let mut args = Vec::new();
    if let Some(upstream) = &source.upstream {
        args.extend([OsString::from("--llm-api-base"), OsString::from(upstream)]);
    } else if let Some(model) = &source.model {
        args.extend([OsString::from("serve"), OsString::from(model)]);
        args.extend([OsString::from("--port"), OsString::from(source.llm_port.to_string())]);
    }
    args.extend([
        OsString::from("--gateway-host"),
        OsString::from(&common.gateway_host),
        OsString::from("--gateway-port"),
        OsString::from(common.gateway_port.to_string()),
        OsString::from("--db-url"),
        OsString::from(&common.database_url),
        OsString::from("--llm-ready-timeout-s"),
        OsString::from(common.llm_ready_timeout_s.to_string()),
        OsString::from("--llm-ready-interval-s"),
        OsString::from(common.llm_ready_interval_s.to_string()),
    ]);
    if let Some(max_request_body_size_bytes) = common.max_request_body_size_bytes {
        args.extend([
            OsString::from("--max-request-body-size-bytes"),
            OsString::from(max_request_body_size_bytes.to_string()),
        ]);
    }
    if let Some(api_key) = &common.api_key {
        args.extend([OsString::from("--openai-api-key"), OsString::from(api_key)]);
    }
    if common.skip_llm_ready_check {
        args.push(OsString::from("--skip-llm-ready-check"));
    }
    args
}

#[must_use]
pub fn server_binary_path(current_exe: &Path) -> std::path::PathBuf {
    current_exe.with_file_name("agentic-server")
}

#[must_use]
pub fn claude_effort() -> String {
    std::env::var(CLAUDE_EFFORT_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_CLAUDE_EFFORT.to_owned())
}

/// Wait until the gateway is live and, unless skipped, its upstream is ready.
///
/// # Errors
///
/// Returns a configuration error when the timeout expires.
pub async fn wait_for_gateway(
    client: &Client,
    gateway_url: &str,
    timeout: Duration,
    interval: Duration,
    skip_llm_ready_check: bool,
) -> Result<(), Error> {
    let deadline = Instant::now() + timeout;
    let health_url = format!("{}/health", gateway_url.trim_end_matches('/'));
    let ready_url = format!("{}/ready", gateway_url.trim_end_matches('/'));
    let ready = tokio::time::timeout_at(deadline, async {
        while Instant::now() < deadline {
            let health_ok = client
                .get(&health_url)
                .send()
                .await
                .is_ok_and(|response| response.status().is_success());
            let ready_ok = skip_llm_ready_check
                || client
                    .get(&ready_url)
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success());
            if health_ok && ready_ok {
                // A completed future can win timeout polling after the deadline.
                return Instant::now() < deadline;
            }
            sleep(interval).await;
        }
        false
    })
    .await
    .unwrap_or(false);

    if ready {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "gateway did not become ready at {}",
            redact_url(gateway_url)
        )))
    }
}

/// The model a harness will run, with the metadata that harness needs to configure it.
#[derive(Debug)]
enum HarnessModel {
    Codex(CodexModelSelection),
    Claude(String),
}

/// Resolve the model each harness will run.
///
/// Codex reads its model and capabilities from the gateway catalog; Claude Code keeps using the
/// upstream model listing, which needs no capability metadata.
///
/// # Errors
///
/// Returns a configuration error when no model can be resolved.
async fn resolve_harness_model(
    client: &Client,
    harness: Harness,
    gateway_url: &str,
    options: &HarnessOptions,
) -> Result<HarnessModel, Error> {
    match harness {
        Harness::Codex => Ok(HarnessModel::Codex(
            resolve_codex_selection(
                client,
                gateway_url,
                options.source.model.as_deref(),
                options.common.api_key.as_deref(),
                Duration::from_secs_f64(options.common.llm_ready_timeout_s),
                Duration::from_secs_f64(options.common.llm_ready_interval_s),
            )
            .await?,
        )),
        Harness::Claude => Ok(HarnessModel::Claude(
            resolve_model(client, &options.source, options.common.api_key.as_deref()).await?,
        )),
    }
}

/// Run one gateway-plus-harness session and return the harness exit status.
///
/// # Errors
///
/// Returns an error when a child cannot start or readiness fails.
pub async fn run_session(
    current_exe: &Path,
    harness: crate::agentic_cli::Harness,
    options: crate::agentic_cli::HarnessOptions,
) -> Result<std::process::ExitStatus, Error> {
    if matches!(harness, crate::agentic_cli::Harness::Claude) {
        validate_claude_passthrough(&options.harness_args)?;
    }
    let gateway_url = format!("http://{}:{}", options.common.gateway_host, options.common.gateway_port);
    let session_root = create_session_root("agentic-api-session")?;
    let claude_state_root = harness_state_root(harness, session_root.path())?;

    let mut server = start_server(current_exe, &options)?;

    let client = match gateway_client() {
        Ok(client) => client,
        Err(error) => {
            cleanup(&mut server, session_root.path()).await;
            return Err(error);
        }
    };
    if let Err(error) = wait_for_gateway(
        &client,
        &gateway_url,
        Duration::from_secs_f64(options.common.llm_ready_timeout_s),
        Duration::from_secs_f64(options.common.llm_ready_interval_s),
        options.common.skip_llm_ready_check,
    )
    .await
    {
        cleanup(&mut server, session_root.path()).await;
        return Err(error);
    }

    let harness_model = match resolve_harness_model(&client, harness, &gateway_url, &options).await {
        Ok(harness_model) => harness_model,
        Err(error) => {
            cleanup(&mut server, session_root.path()).await;
            return Err(error);
        }
    };
    let harness_env = match harness_environment(
        &harness_model,
        &gateway_url,
        &options,
        session_root.path(),
        &claude_state_root,
    ) {
        Ok(environment) => environment,
        Err(error) => {
            cleanup(&mut server, session_root.path()).await;
            return Err(error);
        }
    };
    if !options.common.quiet {
        println!("{}", harness_env.summary);
    }

    let mut harness_child = match spawn_harness(harness, options.common.yolo, &options.harness_args, &harness_env) {
        Ok(child) => child,
        Err(error) => {
            cleanup(&mut server, session_root.path()).await;
            return Err(error);
        }
    };

    let harness_status = tokio::select! {
        status = harness_child.wait() => status?,
        signal = tokio::signal::ctrl_c() => {
            signal?;
            let _ = harness_child.kill().await;
            harness_child.wait().await?
        }
    };
    cleanup(&mut server, session_root.path()).await;
    Ok(harness_status)
}

fn start_server(
    current_exe: &Path,
    options: &crate::agentic_cli::HarnessOptions,
) -> Result<tokio::process::Child, Error> {
    let server_path = server_binary_path(current_exe);
    if !server_path.is_file() {
        return Err(Error::Config(format!(
            "agentic-server binary not found beside {}; run cargo build -p agentic-server --bins first",
            current_exe.display()
        )));
    }
    let mut server = tokio::process::Command::new(server_path);
    server.kill_on_drop(true);
    server.args(server_args(&options.source, &options.common));
    server.stdout(std::process::Stdio::inherit());
    server.stderr(std::process::Stdio::inherit());
    Ok(server.spawn()?)
}

/// Launch a coding harness against an already-running Agentic API gateway.
///
/// # Errors
///
/// Returns an error when the gateway is not ready, configuration cannot be written, or the harness cannot start.
pub async fn run_attached_harness(
    harness: crate::agentic_cli::Harness,
    options: crate::agentic_cli::AttachedHarnessOptions,
) -> Result<std::process::ExitStatus, Error> {
    if matches!(harness, crate::agentic_cli::Harness::Claude) {
        validate_claude_passthrough(&options.harness_args)?;
    }
    let session_root = create_session_root("agentic-api-harness")?;
    let claude_state_root = harness_state_root(harness, session_root.path())?;

    let result = async {
        let client = gateway_client()?;
        wait_for_gateway(
            &client,
            &options.gateway_url,
            ATTACHED_GATEWAY_TIMEOUT,
            ATTACHED_GATEWAY_INTERVAL,
            false,
        )
        .await?;
        let harness_model = match harness {
            Harness::Codex => HarnessModel::Codex(
                resolve_codex_selection(
                    &client,
                    &options.gateway_url,
                    Some(&options.model),
                    options.api_key.as_deref(),
                    ATTACHED_GATEWAY_TIMEOUT,
                    ATTACHED_GATEWAY_INTERVAL,
                )
                .await?,
            ),
            Harness::Claude => HarnessModel::Claude(options.model.clone()),
        };
        let mut harness_env = prepare_attached_harness_environment(
            &harness_model,
            session_root.path(),
            &claude_state_root,
            &options.gateway_url,
            options.api_key.as_deref(),
        )?;
        if matches!(harness, Harness::Claude) {
            harness_env
                .environment
                .insert("CLAUDE_CODE_EFFORT_LEVEL".to_owned(), claude_effort());
        }
        if !options.quiet {
            println!("{}", harness_env.summary);
        }
        let mut child = spawn_harness(harness, options.yolo, &options.harness_args, &harness_env)?;
        let status = tokio::select! {
            status = child.wait() => status?,
            signal = tokio::signal::ctrl_c() => {
                signal?;
                let _ = child.kill().await;
                child.wait().await?
            }
        };
        Ok(status)
    }
    .await;
    let _ = tokio::fs::remove_dir_all(session_root.path()).await;
    result
}

async fn cleanup(server: &mut tokio::process::Child, session_root: &Path) {
    let _ = server.kill().await;
    let _ = server.wait().await;
    let _ = tokio::fs::remove_dir_all(session_root).await;
}

struct SessionRoot {
    directory: tempfile::TempDir,
}

impl SessionRoot {
    fn path(&self) -> &Path {
        self.directory.path()
    }
}

fn create_session_root(prefix: &str) -> Result<SessionRoot, Error> {
    let prefix = format!("{prefix}-");
    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    Ok(SessionRoot {
        directory: builder.tempdir()?,
    })
}

fn harness_state_root(
    harness: crate::agentic_cli::Harness,
    temporary_root: &Path,
) -> Result<std::path::PathBuf, Error> {
    match harness {
        crate::agentic_cli::Harness::Claude => Ok(agentic_core::config::agentic_api_home()?
            .join("harnesses")
            .join("claude")),
        crate::agentic_cli::Harness::Codex => Ok(temporary_root.to_owned()),
    }
}

fn gateway_client() -> Result<Client, Error> {
    Client::builder()
        .timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(Error::HttpClient)
}

#[cfg(test)]
mod tests;
