//! Establish worker containment before telemetry or Tokio creates threads.

use std::ffi::OsStr;
use std::io;
use std::path::Path;

#[cfg(target_os = "linux")]
use agentic_core::tool::code_interpreter::prepare_embedded_runtime;
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
        prepare_embedded_runtime()?;
        // Other platforms retain the provider's unsupported-platform error.
    }
    Ok(())
}
