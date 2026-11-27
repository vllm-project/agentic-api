//! Gateway process selection and supervision shared by serve and harness sessions.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
#[cfg(unix)]
use std::time::Duration;

use agentic_core::error::Error;
#[cfg(unix)]
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use tokio::process::{Child, Command};

use crate::agentic_cli::{CommonOptions, SourceOptions};
use crate::server::shutdown_signal;

// Allow the gateway's 8s drain, native frontend/engine shutdown, and telemetry flush.
#[cfg(unix)]
const SERVER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(35);

#[must_use]
pub fn server_args(source: &SourceOptions, common: &CommonOptions) -> Vec<OsString> {
    let mut args = Vec::new();
    if let Some(upstream) = &source.upstream {
        args.extend([OsString::from("--llm-api-base"), OsString::from(upstream)]);
    } else if let Some(model) = &source.model {
        args.extend([OsString::from("--model"), OsString::from(model)]);
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
pub fn server_binary_path(current_exe: &Path, source: &SourceOptions) -> std::path::PathBuf {
    current_exe.with_file_name(if source.upstream.is_some() {
        "agentic-server"
    } else {
        "agentic-server-native"
    })
}

pub(super) fn server_command(
    current_exe: &Path,
    source: &SourceOptions,
    common: &CommonOptions,
) -> Result<Command, Error> {
    let path = server_binary_path(current_exe, source);
    if !path.is_file() {
        return Err(Error::Config(format!(
            "{} not found; build agentic-server and agentic-server-native together",
            path.display()
        )));
    }
    let mut command = Command::new(path);
    command
        .args(server_args(source, common))
        .kill_on_drop(true)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if source.upstream.is_none() {
        command.env_remove("LLM_API_BASE");
    }
    Ok(command)
}

/// Run one gateway in the foreground and let it drain its owned engine on termination.
///
/// # Errors
/// Returns a configuration, spawn, signal-registration, or process-wait failure.
pub async fn run_server(
    current_exe: &Path,
    source: &SourceOptions,
    common: &CommonOptions,
) -> Result<ExitStatus, Error> {
    let shutdown = shutdown_signal()?;
    let mut child = server_command(current_exe, source, common)?.spawn()?;
    tokio::select! {
        status = child.wait() => Ok(status?),
        () = shutdown => Ok(stop_server(&mut child).await?),
    }
}

pub(super) async fn stop_server(child: &mut Child) -> io::Result<ExitStatus> {
    if let Some(status) = child.try_wait()? {
        return Ok(status);
    }
    #[cfg(unix)]
    {
        let pid = child.id().and_then(|pid| i32::try_from(pid).ok()).map(Pid::from_raw);
        if let Some(pid) = pid {
            if let Err(error) = kill(pid, Signal::SIGTERM) {
                tracing::warn!(%error, "could not request gateway shutdown");
            }
            if let Ok(result) = tokio::time::timeout(SERVER_SHUTDOWN_TIMEOUT, child.wait()).await {
                return result;
            }
            tracing::warn!("gateway shutdown timed out; stopping process");
        }
    }
    child.kill().await?;
    child.wait().await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    async fn launcher_waits_for_gateway_cleanup_before_reaping_it() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("cleaned-up");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(r#"trap 'printf stopped > "$1"; exit 0' TERM; printf 'ready\n'; while :; do sleep 1; done"#)
            .arg("fake-gateway")
            .arg(&marker)
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .await
            .unwrap();
        assert_eq!(ready, "ready\n");
        assert!(stop_server(&mut child).await.unwrap().success());
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "stopped");
    }
}
