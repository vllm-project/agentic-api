//! Eryx implementation of the code interpreter provider.

use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eryx::{CancellationToken, Error as EryxError, OutputHandler, ResourceLimits, Sandbox};

use crate::config::{CodeInterpreterRuntimeConfig, agentic_api_home};
use crate::tool::ToolError;

use super::isolation::{self, WorkerRequest, WorkerResponse};
use super::provider::{CodeInterpreterProvider, ExecutionCancellation, ExecutionOutput, ExecutionStatus};

const TRUNCATION_MARKER: &str = "\n[output truncated]";

#[derive(Debug)]
struct OutputState {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_limit: NonZeroUsize,
    stderr_limit: NonZeroUsize,
    stdout_exceeded: bool,
    stderr_exceeded: bool,
    cancellation: Option<CancellationToken>,
}

impl OutputState {
    fn new(stdout_limit: NonZeroUsize, stderr_limit: NonZeroUsize) -> Self {
        Self {
            stdout: Vec::with_capacity(stdout_limit.get().min(8 * 1024)),
            stderr: Vec::with_capacity(stderr_limit.get().min(8 * 1024)),
            stdout_limit,
            stderr_limit,
            stdout_exceeded: false,
            stderr_exceeded: false,
            cancellation: None,
        }
    }

    fn install_cancellation(&mut self, cancellation: CancellationToken) {
        if self.stdout_exceeded || self.stderr_exceeded {
            cancellation.cancel();
        }
        self.cancellation = Some(cancellation);
    }

    fn append(&mut self, chunk: &[u8], is_stderr: bool) {
        let exceeded = if is_stderr {
            append_bounded(&mut self.stderr, chunk, self.stderr_limit)
        } else {
            append_bounded(&mut self.stdout, chunk, self.stdout_limit)
        };
        if exceeded {
            if is_stderr {
                self.stderr_exceeded = true;
            } else {
                self.stdout_exceeded = true;
            }
            if let Some(cancellation) = &self.cancellation {
                cancellation.cancel();
            }
        }
    }

    fn take_output(&mut self) -> (String, String, bool) {
        add_truncation_marker(&mut self.stdout, self.stdout_limit, self.stdout_exceeded);
        add_truncation_marker(&mut self.stderr, self.stderr_limit, self.stderr_exceeded);
        let exceeded = self.stdout_exceeded || self.stderr_exceeded;
        (
            String::from_utf8_lossy(&std::mem::take(&mut self.stdout)).into_owned(),
            String::from_utf8_lossy(&std::mem::take(&mut self.stderr)).into_owned(),
            exceeded,
        )
    }
}

fn append_bounded(buffer: &mut Vec<u8>, chunk: &[u8], limit: NonZeroUsize) -> bool {
    let remaining = limit.get().saturating_sub(buffer.len());
    buffer.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    chunk.len() > remaining
}

fn add_truncation_marker(buffer: &mut Vec<u8>, limit: NonZeroUsize, exceeded: bool) {
    if !exceeded {
        return;
    }
    let marker = TRUNCATION_MARKER.as_bytes();
    let marker_len = marker.len().min(limit.get());
    buffer.truncate(limit.get().saturating_sub(marker_len));
    buffer.extend_from_slice(&marker[..marker_len]);
}

#[derive(Clone)]
struct BoundedOutputHandler(Arc<Mutex<OutputState>>);

#[async_trait]
impl OutputHandler for BoundedOutputHandler {
    async fn on_output(&self, chunk: &[u8]) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .append(chunk, false);
    }

    async fn on_stderr(&self, chunk: &[u8]) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .append(chunk, true);
    }
}

#[derive(Debug)]
pub(super) struct EryxProvider {
    config: CodeInterpreterRuntimeConfig,
    temp_dir: PathBuf,
}

impl EryxProvider {
    pub(super) fn new(config: CodeInterpreterRuntimeConfig) -> Result<Self, ToolError> {
        let temp_dir = match std::env::var_os("TMPDIR").filter(|value| !value.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => agentic_api_home()
                .map_err(|error| ToolError::Config(error.to_string()))?
                .join("tmp"),
        };
        ensure_private_temp_directory(&temp_dir)?;
        Ok(Self { config, temp_dir })
    }
}

impl CodeInterpreterProvider for EryxProvider {
    fn check_ready(&self) -> Result<(), ToolError> {
        // Exercise runtime initialization inside a memory-limited worker.
        match isolation::run_isolated(self.config, &self.temp_dir, None, None) {
            Ok(WorkerResponse::Ready) => Ok(()),
            Ok(_) => Err(ToolError::Config(
                "code interpreter isolated worker failed readiness".to_owned(),
            )),
            Err(error) => Err(ToolError::Config(error.to_string())),
        }
    }

    fn max_concurrency(&self) -> NonZeroUsize {
        self.config.max_concurrent_guests
    }

    fn execute(
        &self,
        code: String,
        cancellation: Arc<ExecutionCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<ExecutionOutput, ToolError>> + Send + '_>> {
        let config = self.config;
        let temp_dir = self.temp_dir.clone();
        Box::pin(async move {
            let response = tokio::task::spawn_blocking(move || {
                isolation::run_isolated(config, &temp_dir, Some(code), Some(cancellation))
            })
            .await
            .map_err(|_| ToolError::Execution("code interpreter worker supervisor failed".to_owned()))??;
            match response {
                WorkerResponse::Output(output) => Ok(output),
                WorkerResponse::Ready | WorkerResponse::Failed => Err(ToolError::Execution(
                    "code interpreter isolated worker failed".to_owned(),
                )),
            }
        })
    }
}

pub(super) async fn run_worker_request(config: CodeInterpreterRuntimeConfig, request: WorkerRequest) -> WorkerResponse {
    match request {
        WorkerRequest::Probe(_) => {
            // Readiness must prove that the runtime executes within the hard
            // worker memory limit, not merely that its lazy builder succeeds.
            match supervise(config, "pass".to_owned(), Arc::new(ExecutionCancellation::default())).await {
                Ok(output) if matches!(output.status, ExecutionStatus::Completed) => WorkerResponse::Ready,
                _ => WorkerResponse::Failed,
            }
        }
        WorkerRequest::Run { code, .. } => {
            match supervise(config, code, Arc::new(ExecutionCancellation::default())).await {
                Ok(output) => WorkerResponse::Output(output),
                Err(_) => WorkerResponse::Failed,
            }
        }
    }
}

async fn supervise(
    config: CodeInterpreterRuntimeConfig,
    code: String,
    cancellation: Arc<ExecutionCancellation>,
) -> Result<ExecutionOutput, ToolError> {
    let output_state = Arc::new(Mutex::new(OutputState::new(
        config.max_stdout_bytes,
        config.max_stderr_bytes,
    )));
    let handler = BoundedOutputHandler(Arc::clone(&output_state));
    let sandbox = tokio::task::spawn_blocking(move || build_sandbox(config, Some(handler)))
        .await
        .map_err(|_| ToolError::Execution("code interpreter initialization task failed".to_owned()))?
        .map_err(|error| map_initialization_error(&error))?;

    let handle = sandbox.execute_cancellable(&code);
    let handle_cancellation = handle.cancellation_token();
    cancellation.install({
        let handle_cancellation = handle_cancellation.clone();
        move || handle_cancellation.cancel()
    });
    output_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .install_cancellation(handle_cancellation);
    let result = handle.wait().await;
    let (stdout, mut stderr, output_exceeded) = output_state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take_output();

    let status = match result {
        // Crossing an output budget cancels the guest, so a nominal success
        // and the resulting cancellation both mean the run was cut short.
        Ok(_) | Err(EryxError::Cancelled) if output_exceeded => ExecutionStatus::Incomplete,
        Ok(_) => ExecutionStatus::Completed,
        Err(EryxError::PythonException(_)) => {
            if stderr.is_empty() {
                "Python execution raised an exception.".clone_into(&mut stderr);
            }
            ExecutionStatus::Failed
        }
        Err(EryxError::Timeout(_) | EryxError::FuelExhausted { .. } | EryxError::ResourceLimit(_)) => {
            if stderr.is_empty() {
                "Code execution exceeded a resource limit.".clone_into(&mut stderr);
            }
            ExecutionStatus::Incomplete
        }
        Err(_) => return Err(ToolError::Execution("code interpreter runtime failed".to_owned())),
    };
    Ok(ExecutionOutput { status, stdout, stderr })
}

fn build_sandbox(
    config: CodeInterpreterRuntimeConfig,
    output_handler: Option<BoundedOutputHandler>,
) -> Result<Sandbox, EryxError> {
    let memory = u64::try_from(config.max_guest_memory_bytes.get())
        .map_err(|_| EryxError::Initialization("guest memory limit is too large".to_owned()))?;
    let limits = ResourceLimits::default()
        .with_execution_timeout(config.execution_wall_time)
        .with_max_memory_bytes(memory)
        .with_max_fuel(config.max_fuel.get());
    let builder = Sandbox::embedded()
        .with_trace_collection(false)
        .with_resource_limits(limits)
        .with_result_variable(format!("__agentic_private_result_{}", uuid::Uuid::now_v7().simple()));
    match output_handler {
        Some(handler) => builder.with_output_handler(handler).build(),
        None => builder.build(),
    }
}

fn ensure_private_temp_directory(temp_dir: &Path) -> Result<(), ToolError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        use std::path::Component;

        if !temp_dir.is_absolute() || temp_dir.components().any(|part| matches!(part, Component::ParentDir)) {
            return Err(ToolError::Config(
                "code interpreter TMPDIR must be an absolute path without '..'".to_owned(),
            ));
        }
        let path: PathBuf = temp_dir.components().collect();
        #[cfg(target_os = "linux")]
        let effective_uid = nix::unistd::geteuid().as_raw();
        let ancestors: Vec<_> = path.ancestors().collect();
        for (index, component_path) in ancestors.iter().rev().enumerate() {
            if let Err(error) = std::fs::symlink_metadata(component_path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(ToolError::Config(
                        "code interpreter TMPDIR cannot be inspected".to_owned(),
                    ));
                }
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700);
                if let Err(error) = builder.create(component_path)
                    && error.kind() != std::io::ErrorKind::AlreadyExists
                {
                    return Err(ToolError::Config("code interpreter TMPDIR is not writable".to_owned()));
                }
            }
            let metadata = std::fs::symlink_metadata(component_path)
                .map_err(|_| ToolError::Config("code interpreter TMPDIR cannot be inspected".to_owned()))?;
            if metadata.file_type().is_symlink() {
                return Err(ToolError::Config(
                    "code interpreter TMPDIR path must not contain symlinks".to_owned(),
                ));
            }
            if index + 1 == ancestors.len() {
                #[cfg(target_os = "linux")]
                validate_private_temp_directory(&metadata, Some(effective_uid))?;
                #[cfg(not(target_os = "linux"))]
                validate_private_temp_directory(&metadata, None)?;
            } else {
                #[cfg(target_os = "linux")]
                validate_trusted_temp_parent(&metadata, effective_uid)?;
            }
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(temp_dir)
        .map_err(|_| ToolError::Config("code interpreter TMPDIR is not writable".to_owned()))?;
    Ok(())
}

#[cfg(unix)]
fn validate_private_temp_directory(metadata: &std::fs::Metadata, effective_uid: Option<u32>) -> Result<(), ToolError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if !metadata.is_dir()
        || effective_uid.is_some_and(|uid| metadata.uid() != uid)
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(ToolError::Config(
            "code interpreter requires an operator-owned TMPDIR with mode 0700".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_trusted_temp_parent(metadata: &std::fs::Metadata, effective_uid: u32) -> Result<(), ToolError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let mode = metadata.permissions().mode();
    if !metadata.is_dir()
        || (metadata.uid() != effective_uid && metadata.uid() != 0)
        || (mode & 0o022 != 0 && mode & 0o1000 == 0)
    {
        return Err(ToolError::Config(
            "code interpreter TMPDIR parent is not trusted".to_owned(),
        ));
    }
    Ok(())
}

/// Record the operator-actionable cause and return a fixed public message.
///
/// Eryx initialization errors can embed host paths from `$TMPDIR` or the
/// embedded-asset cache, so the mapped error stays fixed for callers while
/// the preserved source is written only to the server log.
fn map_initialization_error(error: &EryxError) -> ToolError {
    tracing::error!(error = %error, "code interpreter embedded runtime failed to initialize");
    ToolError::Config("code interpreter embedded runtime failed to initialize".to_owned())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::num::NonZeroU64;
    use std::time::Duration;

    use super::*;
    use crate::tool::code_interpreter::CodeInterpreterExecutor;

    fn test_config() -> CodeInterpreterRuntimeConfig {
        CodeInterpreterRuntimeConfig {
            enabled: true,
            execution_wall_time: Duration::from_secs(10),
            max_fuel: NonZeroU64::new(10_000_000_000).expect("nonzero"),
            max_stdout_bytes: NonZeroUsize::new(128).expect("nonzero"),
            max_stderr_bytes: NonZeroUsize::new(128).expect("nonzero"),
            max_concurrent_guests: NonZeroUsize::new(1).expect("nonzero"),
            max_aggregate_guest_memory_bytes: NonZeroUsize::new(128 * 1024 * 1024).expect("nonzero"),
            ..CodeInterpreterRuntimeConfig::default()
        }
    }

    #[test]
    fn private_temp_directory_requires_owner_and_rejects_symlink() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

        let root = std::env::temp_dir().join(format!("agentic-eryx-tmpdir-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&root).expect("create test root");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).expect("set test root permissions");
        let private = root.join("private");
        std::fs::create_dir(&private).expect("create private directory");
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))
            .expect("set private directory permissions");

        ensure_private_temp_directory(&private).expect("owned private directory is valid");
        let metadata = std::fs::symlink_metadata(&private).expect("private directory metadata");
        let other_uid = metadata.uid().wrapping_add(1);
        assert!(validate_private_temp_directory(&metadata, Some(other_uid)).is_err());
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o750))
            .expect("set overly broad permissions");
        assert!(ensure_private_temp_directory(&private).is_err());
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))
            .expect("restore private permissions");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777))
            .expect("make parent writable without sticky bit");
        assert!(ensure_private_temp_directory(&private).is_err());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o1777)).expect("make parent sticky");
        ensure_private_temp_directory(&private).expect("sticky parent protects the private directory");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).expect("restore test root permissions");
        let fresh_parent = root.join("fresh");
        let fresh_private = fresh_parent.join("private");
        ensure_private_temp_directory(&fresh_private).expect("create missing private path");
        for directory in [&fresh_parent, &fresh_private] {
            let metadata = std::fs::symlink_metadata(directory).expect("created directory metadata");
            assert_eq!(metadata.uid(), nix::unistd::geteuid().as_raw());
            assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        }

        let link = root.join("link");
        symlink(&private, &link).expect("create symlink to private directory");
        assert!(ensure_private_temp_directory(&link).is_err());
        let nested = private.join("nested");
        std::fs::create_dir(&nested).expect("create nested private directory");
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o700))
            .expect("set nested private permissions");
        assert!(ensure_private_temp_directory(&link.join("nested")).is_err());

        std::fs::remove_dir(nested).expect("remove nested private directory");
        std::fs::remove_file(link).expect("remove symlink");
        std::fs::remove_dir(private).expect("remove private directory");
        std::fs::remove_dir(fresh_private).expect("remove created private directory");
        std::fs::remove_dir(fresh_parent).expect("remove created parent directory");
        std::fs::remove_dir(root).expect("remove test root");
    }

    fn require_worker_executable() {
        let path = std::env::var_os("AGENTIC_CODE_INTERPRETER_WORKER_EXECUTABLE")
            .expect("set AGENTIC_CODE_INTERPRETER_WORKER_EXECUTABLE to the built server binary");
        assert!(std::path::Path::new(&path).is_file(), "worker executable must exist");
    }

    #[tokio::test]
    #[ignore = "requires a delegated cgroup and a built agentic-server worker executable"]
    async fn embedded_runtime_executes_python_and_classifies_failures_and_limits() {
        require_worker_executable();
        let executor = CodeInterpreterExecutor::from_config(test_config()).expect("embedded runtime starts");

        let success = executor
            .execute_call(r#"{"code":"print(6 * 7)"}"#)
            .await
            .expect("successful execution");
        assert!(matches!(success.status, ExecutionStatus::Completed));
        assert_eq!(success.stdout.trim(), "42");

        let exception = executor
            .execute_call(r#"{"code":"raise ValueError('private detail')"}"#)
            .await
            .expect("script failure is a typed tool output");
        assert!(matches!(exception.status, ExecutionStatus::Failed));
        assert!(!exception.stderr.contains("private detail"));

        let filesystem = executor
        .execute_call(
            r#"{"code":"import json\nprint('stdlib-imported')\ntry:\n    with open('/data/agentic-write-probe', 'w', encoding='utf-8') as handle:\n        handle.write('probe')\n    print('data-writable')\nexcept OSError:\n    print('data-unavailable')\ntry:\n    with open(json.__file__, 'a', encoding='utf-8') as handle:\n        handle.write('probe')\n    print('stdlib-writable')\nexcept (OSError, TypeError):\n    print('stdlib-read-only')"}"#,
        )
        .await
        .expect("filesystem policy is a typed tool output");
        assert!(matches!(filesystem.status, ExecutionStatus::Completed));
        assert!(filesystem.stdout.contains("stdlib-imported"));
        assert!(filesystem.stdout.contains("data-unavailable"));
        assert!(filesystem.stdout.contains("stdlib-read-only"));
        assert!(!filesystem.stdout.contains("-writable"));

        let oversized = executor
            .execute_call(r#"{"code":"print('x' * 10000)"}"#)
            .await
            .expect("output limit is a typed tool output");
        assert!(matches!(oversized.status, ExecutionStatus::Incomplete));
        assert!(oversized.stdout.len() <= test_config().max_stdout_bytes.get());
        assert!(oversized.stdout.contains("[output truncated]"));
    }

    #[tokio::test]
    #[ignore = "requires a delegated cgroup and a built agentic-server worker executable"]
    async fn isolated_worker_contains_raw_descriptor_output_and_recovers_from_timeout() {
        require_worker_executable();
        let config = test_config();
        let executor = CodeInterpreterExecutor::from_config(config).expect("isolated worker starts");
        let raw = executor
            .execute_call(
                r#"{"code":"import os\nos.write(1, b'x' * 1000000)\nos.write(2, b'y' * 1000000)\nprint('safe')"}"#,
            )
            .await
            .expect("raw descriptor writes remain inside worker");
        assert!(matches!(raw.status, ExecutionStatus::Completed));
        assert_eq!(raw.stdout.trim(), "safe");
        assert!(raw.stderr.is_empty());
        assert!(raw.stdout.len() <= config.max_stdout_bytes.get());
        assert!(raw.stderr.len() <= config.max_stderr_bytes.get());
        assert!(!raw.stdout.contains("xxxxxxxx"));
        assert!(!raw.stderr.contains("yyyyyyyy"));

        let timeout_config = CodeInterpreterRuntimeConfig {
            execution_wall_time: Duration::from_secs(2),
            ..test_config()
        };
        let timeout_executor = CodeInterpreterExecutor::from_config(timeout_config).expect("timeout worker starts");
        let timeout = timeout_executor
            .execute_call(r#"{"code":"while True: pass"}"#)
            .await
            .expect("timeout produces typed result");
        assert!(matches!(timeout.status, ExecutionStatus::Incomplete));
        let after = timeout_executor
            .execute_call(r#"{"code":"print('still-ready')"}"#)
            .await
            .expect("worker permit released after timeout");
        assert_eq!(after.stdout.trim(), "still-ready");
    }

    #[tokio::test]
    #[ignore = "requires a delegated cgroup and a built agentic-server worker executable"]
    async fn cancellation_kills_and_reaps_worker_before_releasing_capacity() {
        require_worker_executable();
        let executor = Arc::new(CodeInterpreterExecutor::from_config(test_config()).expect("worker starts"));
        let before = isolation::active_worker_cgroups().expect("list worker cgroups");
        let running = Arc::clone(&executor);
        let task = tokio::spawn(async move { running.execute_call(r#"{"code":"while True: pass"}"#).await });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let worker_group = loop {
            let groups = isolation::active_worker_cgroups().expect("list active worker cgroups");
            if let Some(path) = groups.into_iter().find(|path| !before.contains(path)) {
                if std::fs::read_to_string(path.join("cgroup.procs")).is_ok_and(|pids| !pids.trim().is_empty()) {
                    break path;
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "worker did not start");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let worker_pid = std::fs::read_to_string(worker_group.join("cgroup.procs"))
            .expect("worker PID")
            .trim()
            .to_owned();
        task.abort();
        let _ = task.await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if !worker_group.exists() {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "worker cgroup was not removed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !std::path::Path::new(&format!("/proc/{worker_pid}")).exists(),
            "worker was not reaped"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match executor.execute_call(r#"{"code":"print('reaped')"}"#).await {
                Ok(result) => {
                    assert_eq!(result.stdout.trim(), "reaped");
                    break;
                }
                Err(error) if error.to_string().contains("capacity") && tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("worker capacity did not recover: {error}"),
            }
        }
    }
}
