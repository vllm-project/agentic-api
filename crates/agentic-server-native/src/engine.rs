//! Own the engine process group even after the launcher has exited and been reaped.

use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use tokio::process::{Child, Command};
use vllm_managed_engine::ManagedEngineConfig;

/// vLLM owns command construction; this composition root owns process lifetime.
pub(super) struct Engine {
    child: Child,
    group: Option<Pid>,
}

impl Engine {
    pub(super) fn spawn(config: &ManagedEngineConfig) -> io::Result<Self> {
        let mut command = Command::from(config.to_command());
        command
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .process_group(0);
        let child = command.spawn()?;
        let group = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .map(Pid::from_raw)
            .ok_or_else(|| io::Error::other("spawned engine has no valid process group"))?;
        Ok(Self {
            child,
            group: Some(group),
        })
    }

    pub(super) async fn wait_for_exit(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    pub(super) async fn shutdown(mut self, timeout: Duration) -> io::Result<()> {
        if self.child.try_wait()?.is_none() {
            // Let the launcher drain workers before killing any remaining group members.
            if let Some(pid) = self.group {
                ignore_exited(kill(pid, Signal::SIGTERM))?;
            }
            let _ = tokio::time::timeout(timeout, self.child.wait()).await;
        }
        self.kill_group()?;
        self.child.wait().await?;
        Ok(())
    }

    fn kill_group(&mut self) -> io::Result<()> {
        if let Some(group) = self.group {
            ignore_exited(killpg(group, Signal::SIGKILL))?;
            self.group = None;
        }
        Ok(())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Err(error) = self.kill_group() {
            tracing::warn!(%error, "could not stop the native engine process group");
        }
    }
}

fn ignore_exited(result: Result<(), Errno>) -> io::Result<()> {
    match result {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(error) => Err(error.into()),
    }
}
