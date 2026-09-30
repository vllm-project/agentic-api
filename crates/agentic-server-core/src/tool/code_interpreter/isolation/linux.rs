//! Per-call Linux cgroup v2 isolation and authenticated Unix-socket control plane.

use std::fs::{self, DirBuilder};
use std::io::{self, ErrorKind};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};

use crate::config::CodeInterpreterRuntimeConfig;
use crate::tool::ToolError;

use super::super::eryx::run_worker_request;
use super::super::provider::{ExecutionCancellation, ExecutionOutput, ExecutionStatus};
use super::{
    WorkerLimits, WorkerRequest, WorkerResponse, read_frame, worker_request_limit, worker_response_limit, write_frame,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
mod cgroup;

pub use cgroup::prepare;

struct Cgroup {
    path: PathBuf,
}

impl Cgroup {
    fn current_path() -> io::Result<PathBuf> {
        cgroup::current_cgroup()
    }

    fn new(memory_bytes: usize) -> io::Result<Self> {
        // Server startup or the service manager places the gateway in a leaf
        // below a delegated parent. Never migrate a running executor.
        let current = Self::current_path()?;
        let parent = current
            .parent()
            .ok_or_else(|| io::Error::new(ErrorKind::PermissionDenied, "gateway needs a delegated parent cgroup"))?;
        if !cgroup::is_delegated_parent(parent) {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "gateway parent must be delegated with memory and pids controllers, not a systemd slice",
            ));
        }
        let path = parent.join(format!("agentic-eryx-{}", uuid::Uuid::now_v7().simple()));
        fs::create_dir(&path)?;
        let group = Self { path };
        fs::write(group.path.join("memory.max"), memory_bytes.to_string())?;
        fs::write(group.path.join("memory.swap.max"), "0")?;
        fs::write(group.path.join("memory.oom.group"), "1")?;
        fs::write(group.path.join("pids.max"), "64")?;
        if !group.path.join("cgroup.kill").exists() {
            return Err(io::Error::new(ErrorKind::Unsupported, "cgroup.kill is required"));
        }
        Ok(group)
    }

    fn attach(&self, child: &Child, memory_bytes: usize) -> io::Result<()> {
        fs::write(self.path.join("cgroup.procs"), child.id().to_string())?;
        let membership = fs::read_to_string(format!("/proc/{}/cgroup", child.id()))?;
        if cgroup::parse_membership(&membership)? != self.path {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "worker cgroup attachment was not confirmed",
            ));
        }
        let actual = fs::read_to_string(self.path.join("memory.max"))?;
        let effective = actual
            .trim()
            .parse::<usize>()
            .map_err(|_| io::Error::new(ErrorKind::PermissionDenied, "worker memory limit is not finite"))?;
        // The kernel may round a value to its page size. A lower effective
        // limit is safe; a higher one is not.
        if effective == 0 || effective > memory_bytes {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "worker memory limit was not confirmed",
            ));
        }
        let swap = fs::read_to_string(self.path.join("memory.swap.max"))?;
        if swap.trim() != "0" {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "worker swap limit was not confirmed",
            ));
        }
        Ok(())
    }

    fn kill(&self) {
        let _ = fs::write(self.path.join("cgroup.kill"), "1");
    }

    fn oom_killed(&self) -> bool {
        fs::read_to_string(self.path.join("memory.events")).is_ok_and(|events| {
            events.lines().any(|line| {
                line.strip_prefix("oom_kill ")
                    .and_then(|count| count.parse::<u64>().ok())
                    .is_some_and(|count| count > 0)
            })
        })
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        self.kill();
        for _ in 0..10 {
            if fs::remove_dir(&self.path).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        tracing::warn!("code interpreter worker cgroup could not be removed");
    }
}

struct PrivateSocket {
    directory: PathBuf,
    path: PathBuf,
    listener: UnixListener,
}

impl PrivateSocket {
    fn new(temp_dir: &Path) -> io::Result<Self> {
        let directory = temp_dir.join(format!("agentic-eryx-{}", uuid::Uuid::now_v7().simple()));
        DirBuilder::new().mode(0o700).create(&directory)?;
        let path = directory.join("control.sock");
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            directory,
            path,
            listener,
        })
    }

    fn accept_child(&self, child: &mut Child, cancelled: &AtomicBool) -> io::Result<UnixStream> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(io::Error::new(ErrorKind::Interrupted, "worker request was cancelled"));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(ErrorKind::TimedOut, "worker startup timed out"));
            }
            if child.try_wait()?.is_some() {
                return Err(io::Error::new(ErrorKind::BrokenPipe, "worker exited during startup"));
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let credentials =
                        getsockopt(&stream, PeerCredentials).map_err(|error| io::Error::other(error.to_string()))?;
                    if u32::try_from(credentials.pid()).ok() == Some(child.id()) {
                        return Ok(stream);
                    }
                    // Reject a connection from any other process.
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for PrivateSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}

struct WorkerChild {
    child: Child,
    group: Cgroup,
}

impl Drop for WorkerChild {
    fn drop(&mut self) {
        self.group.kill();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn worker_executable() -> io::Result<PathBuf> {
    // The explicit path is for a separately built worker executable in tests
    // and deployments with a renamed server binary. It is operator-owned.
    std::env::var_os("AGENTIC_CODE_INTERPRETER_WORKER_EXECUTABLE")
        .map(PathBuf::from)
        .map_or_else(std::env::current_exe, Ok)
}

pub(in crate::tool::code_interpreter) fn run_isolated(
    config: CodeInterpreterRuntimeConfig,
    temp_dir: &Path,
    code: Option<String>,
    cancellation: Option<Arc<ExecutionCancellation>>,
) -> Result<WorkerResponse, ToolError> {
    run_isolated_inner(config, temp_dir, code, cancellation).map_err(|error| {
        tracing::error!(%error, "code interpreter isolated worker failed");
        if error.kind() == ErrorKind::OutOfMemory {
            ToolError::Execution("code interpreter worker exceeded its memory limit".to_owned())
        } else {
            ToolError::Execution("code interpreter isolated worker failed".to_owned())
        }
    })
}

fn run_isolated_inner(
    config: CodeInterpreterRuntimeConfig,
    temp_dir: &Path,
    code: Option<String>,
    cancellation: Option<Arc<ExecutionCancellation>>,
) -> io::Result<WorkerResponse> {
    let limits = WorkerLimits::from_config(config)?;
    let request = match code {
        Some(code) => WorkerRequest::Run { limits, code },
        None => WorkerRequest::Probe(limits),
    };
    let is_probe = matches!(request, WorkerRequest::Probe(_));
    let group = Cgroup::new(limits.worker_memory_bytes())?;
    let socket = PrivateSocket::new(temp_dir)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    if let Some(cancellation) = cancellation {
        let path = group.path.clone();
        let cancelled_for_callback = Arc::clone(&cancelled);
        cancellation.install(move || {
            cancelled_for_callback.store(true, Ordering::Release);
            let _ = fs::write(path.join("cgroup.kill"), "1");
        });
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(io::Error::new(ErrorKind::Interrupted, "worker request was cancelled"));
    }
    let mut command = Command::new(worker_executable()?);
    command
        .arg(super::WORKER_MARKER)
        .arg(&socket.path)
        .env_clear()
        .env("TMPDIR", temp_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn()?;
    let mut worker = WorkerChild { child, group };
    worker.group.attach(&worker.child, limits.worker_memory_bytes())?;
    if cancelled.load(Ordering::Acquire) {
        return Err(io::Error::new(ErrorKind::Interrupted, "worker request was cancelled"));
    }

    let mut stream = match socket.accept_child(&mut worker.child, &cancelled) {
        Ok(stream) => stream,
        Err(_error) if worker.group.oom_killed() => {
            return Err(io::Error::new(
                ErrorKind::OutOfMemory,
                "worker exceeded its memory cgroup",
            ));
        }
        Err(error) => return Err(error),
    };
    let deadline = config.execution_wall_time.saturating_add(STARTUP_TIMEOUT);
    stream.set_read_timeout(Some(deadline))?;
    stream.set_write_timeout(Some(STARTUP_TIMEOUT))?;
    write_frame(&mut stream, &request, worker_request_limit())?;
    let (response, killed_for_timeout) = read_worker_response(&mut stream, &mut worker, is_probe)?;
    wait_worker_exit(&mut worker, &cancelled, killed_for_timeout)?;
    Ok(response)
}

fn incomplete_limit_output() -> WorkerResponse {
    WorkerResponse::Output(ExecutionOutput {
        status: ExecutionStatus::Incomplete,
        stdout: String::new(),
        stderr: "Code execution exceeded a resource limit.".to_owned(),
    })
}

fn is_worker_read_timeout(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock)
}

fn read_worker_response(
    stream: &mut UnixStream,
    worker: &mut WorkerChild,
    is_probe: bool,
) -> io::Result<(WorkerResponse, bool)> {
    match read_frame(stream, worker_response_limit()) {
        Ok(response) => Ok((response, false)),
        Err(error) if is_worker_read_timeout(&error) => {
            worker.group.kill();
            Ok((incomplete_limit_output(), true))
        }
        Err(error) => {
            // The OOM event can appear after the socket closes, so first let
            // the kernel report the child exit before classifying the error.
            for _ in 0..50 {
                if worker.child.try_wait()?.is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            if worker.group.oom_killed() {
                if is_probe {
                    return Err(io::Error::new(
                        ErrorKind::OutOfMemory,
                        "worker exceeded its memory cgroup",
                    ));
                }
                Ok((incomplete_limit_output(), false))
            } else {
                Err(error)
            }
        }
    }
}

fn wait_worker_exit(worker: &mut WorkerChild, cancelled: &AtomicBool, killed_for_timeout: bool) -> io::Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(exit) = worker.child.try_wait()? {
            if !exit.success() && !worker.group.oom_killed() && !killed_for_timeout {
                return Err(io::Error::other("isolated worker exited unsuccessfully"));
            }
            return Ok(());
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(io::Error::new(ErrorKind::Interrupted, "worker request was cancelled"));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "worker did not exit after returning a result",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn cgroup_memory_is_bounded(expected_limit: usize) -> io::Result<()> {
    let current = Cgroup::current_path()?;
    let memory_limit = fs::read_to_string(current.join("memory.max"))?;
    let memory_limit = memory_limit
        .trim()
        .parse::<usize>()
        .map_err(|_| io::Error::new(ErrorKind::PermissionDenied, "worker memory cgroup is not bounded"))?;
    if memory_limit > expected_limit {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "worker memory cgroup is too large",
        ));
    }
    let swap_limit = fs::read_to_string(current.join("memory.swap.max"))?;
    if swap_limit.trim() != "0" {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "worker memory cgroup permits swap",
        ));
    }
    Ok(())
}

pub(in crate::tool::code_interpreter) fn worker_main(socket_path: &Path) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(STARTUP_TIMEOUT))?;
    let request: WorkerRequest = read_frame(&mut stream, worker_request_limit())?;
    let limits = match &request {
        WorkerRequest::Probe(limits) | WorkerRequest::Run { limits, .. } => *limits,
    };
    cgroup_memory_is_bounded(limits.worker_memory_bytes())?;
    let config = limits.into_config()?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let response = runtime.block_on(run_worker_request(config, request));
    write_frame(&mut stream, &response, worker_response_limit())
}

#[cfg(test)]
pub(in crate::tool::code_interpreter) fn active_worker_cgroups() -> io::Result<Vec<PathBuf>> {
    let current = Cgroup::current_path()?;
    let parent = current
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "gateway cgroup parent missing"))?;
    fs::read_dir(parent)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("agentic-eryx-"))
        .map(|entry| Ok(entry.path()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn socket_read_timeout_is_recognized_as_worker_deadline() {
        let (mut stream, _silent_peer) = UnixStream::pair().expect("worker control socket");
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("bounded socket read");
        let error = read_frame::<WorkerResponse>(&mut stream, worker_response_limit())
            .expect_err("silent peer should time out");
        assert!(is_worker_read_timeout(&error), "unexpected socket error: {error}");
    }

    #[test]
    #[ignore = "requires a delegated cgroup v2 parent and the sleep executable"]
    fn silent_worker_timeout_returns_incomplete_and_reaps_child() {
        const LIMIT: usize = 128 * 1024 * 1024;
        let group = Cgroup::new(LIMIT).expect("delegated memory cgroup");
        let child = Command::new("sleep")
            .arg("30")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("silent child");
        let mut worker = WorkerChild { child, group };
        worker.group.attach(&worker.child, LIMIT).expect("attach silent child");
        let pid = worker.child.id();
        let group_path = worker.group.path.clone();
        let (mut stream, _silent_peer) = UnixStream::pair().expect("worker control socket");
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("bounded socket read");

        let (response, killed_for_timeout) =
            read_worker_response(&mut stream, &mut worker, false).expect("timeout becomes incomplete output");
        assert!(killed_for_timeout);
        assert!(matches!(
            response,
            WorkerResponse::Output(ExecutionOutput {
                status: ExecutionStatus::Incomplete,
                ..
            })
        ));
        wait_worker_exit(&mut worker, &AtomicBool::new(false), killed_for_timeout)
            .expect("cgroup-killed child must exit");
        drop(worker);
        assert!(!group_path.exists(), "worker cgroup was not removed");
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "worker child was not reaped"
        );
    }

    #[test]
    #[ignore = "requires a delegated cgroup v2 parent and python3"]
    fn cgroup_oom_kills_attached_child_and_gateway_survives() {
        const LIMIT: usize = 32 * 1024 * 1024;
        let group = Cgroup::new(LIMIT).expect("delegated memory cgroup");
        let mut command = Command::new("python3");
        command
            .arg("-S")
            .arg("-c")
            .arg("import sys\nsys.stdin.buffer.read(1)\nchunks = []\nfor _ in range(64):\n    chunks.append(bytearray(b'X' * (4 * 1024 * 1024)))")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().expect("allocator child");
        let mut worker = WorkerChild { child, group };
        let pid = worker.child.id();
        let group_path = worker.group.path.clone();
        worker
            .group
            .attach(&worker.child, LIMIT)
            .expect("attach before allocation");
        worker
            .child
            .stdin
            .take()
            .expect("allocator stdin")
            .write_all(b"1")
            .expect("release allocator");
        let deadline = Instant::now() + Duration::from_secs(10);
        let exit = loop {
            if let Some(exit) = worker.child.try_wait().expect("poll allocator") {
                break exit;
            }
            assert!(Instant::now() < deadline, "allocator did not hit cgroup memory limit");
            thread::sleep(Duration::from_millis(20));
        };
        assert!(!exit.success(), "allocator unexpectedly exited normally");
        assert!(worker.group.oom_killed(), "kernel did not report an OOM kill");
        drop(worker);
        assert!(!group_path.exists(), "worker cgroup was not removed");
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "allocator child was not reaped"
        );
    }
}
