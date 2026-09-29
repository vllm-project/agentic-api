//! Linux cgroup v2 worker boundary for Eryx. Guest stdout/stderr never carry control data.

use std::io::{self, Read, Write};
use std::num::{NonZeroU64, NonZeroUsize};
#[cfg(not(target_os = "linux"))]
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{
    CODE_INTERPRETER_MAX_WORKER_RETAINED_OUTPUT_BYTES, CODE_INTERPRETER_MAX_WORKER_SOURCE_BYTES,
    CodeInterpreterRuntimeConfig,
};

use super::provider::ExecutionOutput;

pub(super) const WORKER_MARKER: &str = "--agentic-code-interpreter-worker";
// 512 KiB of UTF-8 source can expand sixfold as JSON \u00xx escapes.
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub(super) enum WorkerRequest {
    Probe(WorkerLimits),
    Run { limits: WorkerLimits, code: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) enum WorkerResponse {
    Ready,
    Output(ExecutionOutput),
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(super) struct WorkerLimits {
    wall_time_millis: u64,
    fuel: u64,
    guest_memory_bytes: usize,
    stdout_bytes: usize,
    stderr_bytes: usize,
    worker_memory_bytes: usize,
}

impl WorkerLimits {
    pub(super) fn from_config(config: CodeInterpreterRuntimeConfig) -> io::Result<Self> {
        let wall_time_millis = u64::try_from(config.execution_wall_time.as_millis())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "wall time is too large"))?;
        if config.max_source_bytes.get() > CODE_INTERPRETER_MAX_WORKER_SOURCE_BYTES
            || config
                .max_stdout_bytes
                .get()
                .saturating_add(config.max_stderr_bytes.get())
                > CODE_INTERPRETER_MAX_WORKER_RETAINED_OUTPUT_BYTES
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "worker IPC limits are too large",
            ));
        }
        Ok(Self {
            wall_time_millis,
            fuel: config.max_fuel.get(),
            guest_memory_bytes: config.max_guest_memory_bytes.get(),
            stdout_bytes: config.max_stdout_bytes.get(),
            stderr_bytes: config.max_stderr_bytes.get(),
            worker_memory_bytes: config.max_worker_memory_bytes.get(),
        })
    }

    pub(super) fn into_config(self) -> io::Result<CodeInterpreterRuntimeConfig> {
        fn nonzero(value: usize) -> io::Result<NonZeroUsize> {
            NonZeroUsize::new(value).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "zero worker limit"))
        }
        let fuel =
            NonZeroU64::new(self.fuel).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "zero worker fuel"))?;
        let config = CodeInterpreterRuntimeConfig {
            execution_wall_time: std::time::Duration::from_millis(self.wall_time_millis),
            max_fuel: fuel,
            max_guest_memory_bytes: nonzero(self.guest_memory_bytes)?,
            max_stdout_bytes: nonzero(self.stdout_bytes)?,
            max_stderr_bytes: nonzero(self.stderr_bytes)?,
            max_worker_memory_bytes: nonzero(self.worker_memory_bytes)?,
            max_aggregate_guest_memory_bytes: nonzero(self.guest_memory_bytes)?,
            max_aggregate_worker_memory_bytes: nonzero(self.worker_memory_bytes)?,
            ..CodeInterpreterRuntimeConfig::default()
        };
        config
            .validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(config)
    }

    pub(super) fn worker_memory_bytes(self) -> usize {
        self.worker_memory_bytes
    }
}

pub(super) fn write_frame<T: Serialize>(stream: &mut impl Write, value: &T, max_bytes: usize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "worker IPC frame exceeds limit",
        ));
    }
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "worker IPC frame exceeds protocol size"))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()
}

pub(super) fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut impl Read, max_bytes: usize) -> io::Result<T> {
    let mut len = [0_u8; 4];
    stream.read_exact(&mut len)?;
    let len = usize::try_from(u32::from_be_bytes(len))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid worker IPC frame length"))?;
    if len > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "worker IPC frame exceeds limit",
        ));
    }
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(target_os = "linux")]
mod linux;

#[cfg(all(test, target_os = "linux"))]
pub(super) use linux::active_worker_cgroups;
#[cfg(target_os = "linux")]
pub(super) use linux::{run_isolated, worker_main};

#[cfg(not(target_os = "linux"))]
pub(super) fn run_isolated(
    _config: CodeInterpreterRuntimeConfig,
    _code: Option<String>,
    _cancellation: Option<std::sync::Arc<super::provider::ExecutionCancellation>>,
) -> Result<WorkerResponse, crate::tool::ToolError> {
    Err(crate::tool::ToolError::Config(
        "code interpreter workers require Linux cgroup v2".to_owned(),
    ))
}

#[cfg(not(target_os = "linux"))]
pub(super) fn worker_main(_socket_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "code interpreter workers require Linux cgroup v2",
    ))
}

pub(super) fn worker_request_limit() -> usize {
    MAX_REQUEST_BYTES
}

pub(super) fn worker_response_limit() -> usize {
    MAX_RESPONSE_BYTES
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn rejects_oversized_ipc_before_allocating_frame() {
        let mut oversized = Cursor::new(u32::MAX.to_be_bytes());
        let error = read_frame::<WorkerResponse>(&mut oversized, worker_response_limit())
            .expect_err("oversized frame must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn maximum_source_fits_even_after_json_escape_expansion() {
        let config = CodeInterpreterRuntimeConfig {
            max_source_bytes: NonZeroUsize::new(CODE_INTERPRETER_MAX_WORKER_SOURCE_BYTES).expect("nonzero"),
            ..CodeInterpreterRuntimeConfig::default()
        };
        let limits = WorkerLimits::from_config(config).expect("test limits");
        let request = WorkerRequest::Run {
            limits,
            code: "\0".repeat(config.max_source_bytes.get()),
        };
        let mut frame = Vec::new();
        write_frame(&mut frame, &request, worker_request_limit()).expect("bounded worst-case source");
        assert!(frame.len() <= worker_request_limit() + 4);
        let parsed: WorkerRequest = read_frame(&mut Cursor::new(frame), worker_request_limit()).expect("typed request");
        assert!(matches!(parsed, WorkerRequest::Run { code, .. } if code.len() == config.max_source_bytes.get()));
    }
}
