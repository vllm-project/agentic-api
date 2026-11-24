//! Shared cgroup membership, delegation validation, and pre-runtime setup.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub(super) const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const DELEGATION_ATTEMPT: &str = "_AGENTIC_CODE_INTERPRETER_DELEGATION";

pub fn prepare() -> io::Result<()> {
    let current = current_cgroup()?;
    // Set up the occupied unit first. Its writable app.slice parent is not a
    // delegation boundary and must never receive worker cgroups.
    if prepare_delegated_scope(&current)? {
        return Ok(());
    }
    if std::env::var_os(DELEGATION_ATTEMPT).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "code interpreter needs a delegated cgroup v2 with memory and pids controllers; \
             automatic systemd delegation did not provide it",
        ));
    }
    if !is_systemd_unit(&current) && current.parent().is_some_and(parent_ready) {
        return Ok(());
    }
    ensure_user_manager()?;
    eprintln!("Code interpreter: requesting a delegated systemd user scope for worker limits");
    let error = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "--property=Delegate=yes", "--"])
        .arg(std::env::current_exe()?)
        .args(std::env::args_os().skip(1))
        .env(DELEGATION_ATTEMPT, "1")
        .exec();
    Err(io::Error::new(
        error.kind(),
        format!(
            "cannot request code interpreter cgroup delegation: {error}; \
             start a systemd user manager or run in a service with Delegate=yes"
        ),
    ))
}

pub(super) fn current_cgroup() -> io::Result<PathBuf> {
    parse_membership(&fs::read_to_string("/proc/self/cgroup")?)
}

pub(super) fn parse_membership(content: &str) -> io::Result<PathBuf> {
    let relative = content
        .lines()
        .find_map(|line| line.strip_prefix("0::/"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "code interpreter requires cgroup v2"))?;
    if Path::new(relative)
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid cgroup v2 membership",
        ));
    }
    Ok(Path::new(CGROUP_ROOT).join(relative))
}

fn has_controllers(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|controls| {
        ["memory", "pids"]
            .iter()
            .all(|required| controls.split_whitespace().any(|control| control == *required))
    })
}

/// Reuse service-manager or test-runner setup without moving the process.
fn parent_ready(parent: &Path) -> bool {
    if !is_delegated_parent(parent) {
        return false;
    }
    let probe = parent.join(format!("agentic-startup-{}", process::id()));
    if fs::create_dir(&probe).is_err() {
        return false;
    }
    let ready = probe.join("memory.max").exists() && probe.join("pids.max").exists();
    fs::remove_dir(probe).is_ok() && ready
}

/// Only move this single process out of a delegated scope before threads start.
fn prepare_delegated_scope(scope: &Path) -> io::Result<bool> {
    // Prepared leaves belong to their parent delegation; do not nest another
    // gateway leaf when starting through the test wrapper or a service wrapper.
    if !is_systemd_unit(scope) || !has_controllers(&scope.join("cgroup.controllers")) {
        return Ok(false);
    }
    let pid = process::id().to_string();
    if fs::read_to_string(scope.join("cgroup.procs"))?.trim() != pid {
        return Ok(false);
    }
    if OpenOptions::new().write(true).open(scope.join("cgroup.procs")).is_err()
        || OpenOptions::new()
            .write(true)
            .open(scope.join("cgroup.subtree_control"))
            .is_err()
    {
        return Ok(false);
    }
    let leaf = scope.join(format!("gateway-{pid}"));
    fs::create_dir(&leaf)?;
    if let Err(error) = fs::write(leaf.join("cgroup.procs"), &pid) {
        let _ = fs::remove_dir(&leaf);
        return Err(error);
    }
    if !fs::read_to_string(scope.join("cgroup.procs"))?.trim().is_empty() {
        return Err(io::Error::other("other processes occupy the delegated cgroup parent"));
    }
    fs::write(scope.join("cgroup.subtree_control"), "+memory +pids")?;
    Ok(true)
}

fn is_systemd_unit(path: &Path) -> bool {
    matches!(path.extension().and_then(OsStr::to_str), Some("scope" | "service"))
}

pub(super) fn is_delegated_parent(parent: &Path) -> bool {
    parent != Path::new(CGROUP_ROOT)
        && parent.starts_with(CGROUP_ROOT)
        && parent.extension() != Some(OsStr::new("slice"))
        && has_controllers(&parent.join("cgroup.subtree_control"))
}

/// Check the actual user-manager connection, not merely environment presence.
/// Bound the probe so a stale or unresponsive D-Bus address cannot stall startup.
fn ensure_user_manager() -> io::Result<()> {
    let mut probe = Command::new("systemctl")
        .args(["--user", "--no-pager", "--no-ask-password", "show-environment"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| delegation_unavailable(&error.to_string()))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match probe.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => return Err(delegation_unavailable("systemd user manager is unreachable")),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            result => {
                let _ = probe.kill();
                let _ = probe.wait();
                return Err(delegation_unavailable(&match result {
                    Err(error) => error.to_string(),
                    _ => "systemd user manager probe timed out".to_owned(),
                }));
            }
        }
    }
}

fn delegation_unavailable(reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "code interpreter cgroup delegation unavailable: {reason}; \
            configure the gateway service with Delegate=yes, use an active systemd user session \
            (or enable lingering for that user), or disable code_interpreter.enabled"
        ),
    )
}
