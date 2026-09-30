//! Establish worker containment before telemetry or Tokio creates threads.

use std::ffi::OsStr;
use std::io;
use std::path::Path;

use agentic_core::tool::code_interpreter::run_embedded_worker;

use crate::config_file::FileConfig;
use crate::server::ServerError;
use crate::{CodeInterpreterEnvironmentValues, code_interpreter_config_from_operator_values};

pub(super) fn run_worker() -> Option<io::Result<()>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(OsStr::new("--agentic-code-interpreter-worker")) {
        return None;
    }
    Some((|| {
        let socket = args
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "worker socket path is required"))?;
        if args.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unexpected worker argument",
            ));
        }
        run_embedded_worker(Path::new(&socket))
    })())
}

pub(super) fn prepare(file: Option<&FileConfig>) -> Result<(), ServerError> {
    let defaults = FileConfig::default();
    let config = code_interpreter_config_from_operator_values(
        &file.unwrap_or(&defaults).code_interpreter,
        CodeInterpreterEnvironmentValues::from_process(),
    )?;
    if config.enabled {
        #[cfg(target_os = "linux")]
        linux::prepare()?;
        // Other platforms retain the provider's unsupported-platform error.
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{self, OpenOptions};
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::path::{Component, Path, PathBuf};
    use std::process::{self, Command};

    const CGROUP_ROOT: &str = "/sys/fs/cgroup";
    const DELEGATION_ATTEMPT: &str = "_AGENTIC_CODE_INTERPRETER_DELEGATION";

    pub(super) fn prepare() -> io::Result<()> {
        let current = current_cgroup()?;
        if current.parent().is_some_and(parent_ready) || prepare_delegated_scope(&current)? {
            return Ok(());
        }
        if std::env::var_os(DELEGATION_ATTEMPT).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "code interpreter needs a delegated cgroup v2 with memory and pids controllers; \
                 automatic systemd delegation did not provide it",
            ));
        }
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

    fn current_cgroup() -> io::Result<PathBuf> {
        let content = fs::read_to_string("/proc/self/cgroup")?;
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
        if !parent.starts_with(CGROUP_ROOT) || !has_controllers(&parent.join("cgroup.subtree_control")) {
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
        if scope == Path::new(CGROUP_ROOT) || !has_controllers(&scope.join("cgroup.controllers")) {
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
}
