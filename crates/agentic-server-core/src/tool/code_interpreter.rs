//! Gateway-executed Python code interpreter backed by Eryx.

use crate::types::io::FunctionTool;
use crate::types::tools::{CodeInterpreterExecution, CodeInterpreterToolParam, ResponsesTool};

use super::{ToolError, ToolHandler, ToolType};

pub(crate) const CODE_INTERPRETER_FUNCTION_NAME: &str = "code_interpreter";

/// Declaration handler used by request validation and normalization.
#[derive(Debug)]
pub struct CodeInterpreterHandler;

impl CodeInterpreterHandler {
    pub(crate) fn validate_declarations(tools: &[ResponsesTool]) -> Result<(), ToolError> {
        let declarations = tools
            .iter()
            .filter(|tool| matches!(tool, ResponsesTool::CodeInterpreter(_)))
            .count();
        if declarations > 1 {
            return Err(ToolError::Config(
                "code_interpreter may be declared only once".to_owned(),
            ));
        }
        for tool in tools {
            let conflicts = match tool {
                ResponsesTool::Function(function) => function.name.as_str() == CODE_INTERPRETER_FUNCTION_NAME,
                ResponsesTool::Custom(custom) => custom.name.as_str() == CODE_INTERPRETER_FUNCTION_NAME,
                _ => false,
            };
            if conflicts {
                return Err(ToolError::Config(format!(
                    "fixed model-visible tool name '{CODE_INTERPRETER_FUNCTION_NAME}' conflicts with another declaration"
                )));
            }
        }
        Ok(())
    }

    #[must_use]
    fn function_tool() -> FunctionTool {
        FunctionTool {
            type_: "function".to_owned(),
            name: CODE_INTERPRETER_FUNCTION_NAME.to_owned(),
            description: Some(
                "Execute Python in a fresh, network-isolated sandbox. Use print(...) to return text.".to_owned(),
            ),
            parameters: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "Python source to execute. Use print(...) to emit the answer."
                    }
                },
                "required": ["code"],
                "additionalProperties": false
            })),
            strict: Some(true),
        }
    }
}

impl ToolHandler for CodeInterpreterHandler {
    type ToolParams = CodeInterpreterToolParam;

    fn tool_type(&self) -> ToolType {
        ToolType::CodeInterpreter
    }

    fn validate(&self, params: &Self::ToolParams) -> Result<(), ToolError> {
        if params.execution != CodeInterpreterExecution::Gateway {
            return Err(ToolError::Config(
                "code_interpreter supports only execution='gateway'".to_owned(),
            ));
        }
        Ok(())
    }

    fn normalize(&self, _params: &Self::ToolParams) -> Vec<FunctionTool> {
        vec![Self::function_tool()]
    }
}

#[cfg(feature = "embedded-code-interpreter")]
mod embedded {
    use std::future::Future;
    use std::num::NonZeroUsize;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use eryx::{CancellationToken, Error as EryxError, OutputHandler, ResourceLimits, Sandbox};
    use serde::{Deserialize, Serialize};
    use tokio::sync::{Semaphore, oneshot};

    use crate::config::CodeInterpreterRuntimeConfig;
    use crate::types::io::{
        CodeInterpreterCall, CodeInterpreterCallOutput, CodeInterpreterCallStatus, FunctionToolCall, GatewayCallStatus,
        OutputItem,
    };
    use crate::types::tools::CodeInterpreterCallArguments;

    use super::{CodeInterpreterHandler, CodeInterpreterToolParam, FunctionTool, ToolError, ToolHandler, ToolType};
    use crate::tool::{GatewayExecutor, GatewayToolEventPlan, ToolOutput};

    const TRUNCATION_MARKER: &str = "\n[output truncated]";

    #[derive(Debug, Clone, Copy, Deserialize, Serialize)]
    #[serde(rename_all = "snake_case")]
    enum ExecutionStatus {
        Completed,
        Failed,
        Incomplete,
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct ExecutionOutput {
        status: ExecutionStatus,
        stdout: String,
        stderr: String,
    }

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

    #[derive(Debug, Default)]
    struct ExecutionCancellation {
        token: Option<CancellationToken>,
        cancelled: bool,
    }

    impl ExecutionCancellation {
        fn install(&mut self, token: CancellationToken) {
            if self.cancelled {
                token.cancel();
            }
            self.token = Some(token);
        }

        fn cancel(&mut self) {
            self.cancelled = true;
            if let Some(token) = &self.token {
                token.cancel();
            }
        }
    }

    struct CancelOnDrop {
        cancellation: Arc<Mutex<ExecutionCancellation>>,
        armed: bool,
    }

    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            if self.armed {
                self.cancellation
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .cancel();
            }
        }
    }

    /// Shared executor with process-wide admission across requests.
    #[derive(Debug)]
    pub(crate) struct EryxCodeInterpreterExecutor {
        config: CodeInterpreterRuntimeConfig,
        guest_permits: Arc<Semaphore>,
    }

    impl EryxCodeInterpreterExecutor {
        pub(crate) fn from_config(config: CodeInterpreterRuntimeConfig) -> Result<Self, ToolError> {
            config
                .validate()
                .map_err(|error| ToolError::Config(error.to_string()))?;
            ensure_private_temp_directory(std::env::temp_dir().as_path())?;
            let aggregate_slots = config.max_aggregate_guest_memory_bytes.get() / config.max_guest_memory_bytes.get();
            let guest_slots =
                NonZeroUsize::new(config.max_concurrent_guests.get().min(aggregate_slots)).ok_or_else(|| {
                    ToolError::Config("code interpreter aggregate memory budget cannot admit one guest".to_owned())
                })?;

            // Fail startup before registration if the operator did not provide
            // an Eryx 0.8-compatible precompiled runtime.
            build_sandbox(config, None).map_err(|error| map_initialization_error(&error))?;
            Ok(Self {
                config,
                guest_permits: Arc::new(Semaphore::new(guest_slots.get())),
            })
        }

        async fn execute_call(&self, arguments: &str) -> Result<ExecutionOutput, ToolError> {
            let arguments = CodeInterpreterCallArguments::from_json(arguments, self.config.max_source_bytes)
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            let permit = Arc::clone(&self.guest_permits)
                .try_acquire_owned()
                .map_err(|_| ToolError::Execution("code interpreter capacity is temporarily unavailable".to_owned()))?;
            let config = self.config;
            let code = arguments.into_code();
            let (sender, receiver) = oneshot::channel();
            let cancellation = Arc::new(Mutex::new(ExecutionCancellation::default()));
            let supervisor_cancellation = Arc::clone(&cancellation);

            // The supervisor owns the permit through terminal guest shutdown,
            // even if an outer request or scheduler future is dropped.
            tokio::spawn(async move {
                let _permit = permit;
                let output = supervise(config, code, supervisor_cancellation).await;
                let _receiver_closed = sender.send(output);
            });
            let mut cancel_on_drop = CancelOnDrop {
                cancellation,
                armed: true,
            };
            let output = receiver
                .await
                .map_err(|_| ToolError::Execution("code interpreter supervisor stopped unexpectedly".to_owned()))?;
            cancel_on_drop.armed = false;
            output
        }
    }

    impl ToolHandler for EryxCodeInterpreterExecutor {
        type ToolParams = CodeInterpreterToolParam;

        fn tool_type(&self) -> ToolType {
            ToolType::CodeInterpreter
        }

        fn validate(&self, params: &Self::ToolParams) -> Result<(), ToolError> {
            CodeInterpreterHandler.validate(params)
        }

        fn normalize(&self, params: &Self::ToolParams) -> Vec<FunctionTool> {
            CodeInterpreterHandler.normalize(params)
        }
    }

    impl GatewayExecutor for EryxCodeInterpreterExecutor {
        type ExecutionParams = CodeInterpreterToolParam;

        fn execute(
            &self,
            call_id: &str,
            _tool_name: &str,
            arguments: &str,
            _params: &Self::ExecutionParams,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
            let call_id = call_id.to_owned();
            let arguments = arguments.to_owned();
            Box::pin(async move {
                let output = serde_json::to_string(&self.execute_call(&arguments).await?)
                    .map_err(|error| ToolError::Execution(format!("failed to serialize code output: {error}")))?;
                Ok(ToolOutput { call_id, output })
            })
        }

        fn supports_parallel_execution(&self) -> bool {
            true
        }

        fn plan_gateway_events(
            &self,
            call: &FunctionToolCall,
            _params: &Self::ExecutionParams,
        ) -> GatewayToolEventPlan {
            GatewayToolEventPlan::new(Some(self.output_item(
                call,
                CodeInterpreterCallStatus::InProgress,
                None,
            )))
        }

        fn public_output(
            &self,
            call: &FunctionToolCall,
            output: &ToolOutput,
            status: GatewayCallStatus,
            _params: &Self::ExecutionParams,
        ) -> Option<OutputItem> {
            let parsed = serde_json::from_str::<ExecutionOutput>(&output.output).ok();
            let (status, outputs) = match (status, parsed) {
                (GatewayCallStatus::Completed, Some(execution)) => {
                    let status = match execution.status {
                        ExecutionStatus::Completed => CodeInterpreterCallStatus::Completed,
                        ExecutionStatus::Failed => CodeInterpreterCallStatus::Failed,
                        ExecutionStatus::Incomplete => CodeInterpreterCallStatus::Incomplete,
                    };
                    let outputs = [execution.stdout, execution.stderr]
                        .into_iter()
                        .filter(|logs| !logs.is_empty())
                        .map(CodeInterpreterCallOutput::logs)
                        .collect();
                    (status, outputs)
                }
                _ => (
                    CodeInterpreterCallStatus::Failed,
                    vec![CodeInterpreterCallOutput::logs("Code execution failed.".to_owned())],
                ),
            };
            Some(self.output_item(call, status, Some(outputs)))
        }
    }

    impl EryxCodeInterpreterExecutor {
        fn output_item(
            &self,
            call: &FunctionToolCall,
            status: CodeInterpreterCallStatus,
            outputs: Option<Vec<CodeInterpreterCallOutput>>,
        ) -> OutputItem {
            let code = CodeInterpreterCallArguments::from_json(&call.arguments, self.config.max_source_bytes)
                .map(CodeInterpreterCallArguments::into_code)
                .unwrap_or_default();
            OutputItem::CodeInterpreterCall(CodeInterpreterCall {
                id: call.id.clone(),
                container_id: format!("cntr_{}", call.id),
                code,
                status,
                outputs,
            })
        }
    }

    async fn supervise(
        config: CodeInterpreterRuntimeConfig,
        code: String,
        cancellation: Arc<Mutex<ExecutionCancellation>>,
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
        cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .install(handle_cancellation.clone());
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
        std::fs::create_dir_all(temp_dir)
            .map_err(|_| ToolError::Config("code interpreter TMPDIR is not writable".to_owned()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(temp_dir)
                .map_err(|_| ToolError::Config("code interpreter TMPDIR cannot be inspected".to_owned()))?
                .permissions()
                .mode()
                & 0o777;
            if mode != 0o700 {
                return Err(ToolError::Config(
                    "code interpreter requires an operator-owned TMPDIR with mode 0700".to_owned(),
                ));
            }
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

    #[cfg(test)]
    mod tests {
        use std::num::NonZeroU64;
        use std::time::Duration;

        use super::*;

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

        #[tokio::test]
        async fn embedded_runtime_executes_python_and_classifies_failures_and_limits() {
            let executor = EryxCodeInterpreterExecutor::from_config(test_config()).expect("embedded runtime starts");

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
    }
}

#[cfg(feature = "embedded-code-interpreter")]
pub(crate) use embedded::EryxCodeInterpreterExecutor;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_to_a_closed_function_contract() {
        let function = CodeInterpreterHandler::function_tool();
        assert_eq!(function.name, CODE_INTERPRETER_FUNCTION_NAME);
        assert_eq!(function.strict, Some(true));
        assert_eq!(function.parameters.expect("schema")["additionalProperties"], false);
    }
}
