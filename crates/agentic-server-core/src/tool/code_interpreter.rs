//! Gateway-executed Python code interpreter and backend-independent public projection.

use crate::types::io::FunctionTool;
use crate::types::tools::{CodeInterpreterToolParam, ResponsesTool};

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
        if declarations == 0 {
            return Ok(());
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

    fn validate(&self, _params: &Self::ToolParams) -> Result<(), ToolError> {
        Ok(())
    }

    fn normalize(&self, _params: &Self::ToolParams) -> Vec<FunctionTool> {
        vec![Self::function_tool()]
    }
}

#[cfg(feature = "embedded-code-interpreter")]
mod eryx;
#[cfg(feature = "embedded-code-interpreter")]
mod isolation;

/// Internal entry point for the resource-limited Eryx worker process.
///
/// The server executable dispatches here before loading telemetry, secrets,
/// configuration, or request handlers.
///
/// # Errors
///
/// Returns an I/O error if the worker control socket, cgroup verification,
/// runtime initialization, or result transfer fails.
#[cfg(feature = "embedded-code-interpreter")]
pub fn run_embedded_worker(socket_path: &std::path::Path) -> std::io::Result<()> {
    isolation::worker_main(socket_path)
}
/// Prepare Linux worker cgroup delegation before creating runtime threads.
///
/// May re-execute the current binary in a delegated systemd user scope. Call
/// only during single-threaded server startup, never from the worker entry point.
///
/// # Errors
///
/// Returns an I/O error when delegation or the required controllers are unavailable.
#[cfg(all(feature = "embedded-code-interpreter", target_os = "linux"))]
pub fn prepare_embedded_runtime() -> std::io::Result<()> {
    isolation::prepare()
}

mod provider;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{Semaphore, oneshot};

use crate::config::CodeInterpreterRuntimeConfig;
use crate::tool::{GatewayExecutor, GatewayToolEventPlan, ToolOutput};
use crate::types::io::{
    CodeInterpreterCall, CodeInterpreterCallOutput, CodeInterpreterCallStatus, FunctionToolCall, GatewayCallStatus,
    OutputItem,
};
use crate::types::tools::CodeInterpreterCallArguments;
use provider::{CodeInterpreterProvider, ExecutionCancellation, ExecutionOutput, ExecutionStatus};

pub(crate) struct CodeInterpreterExecutor {
    config: CodeInterpreterRuntimeConfig,
    provider: Arc<dyn CodeInterpreterProvider>,
    guest_permits: Arc<Semaphore>,
}

impl std::fmt::Debug for CodeInterpreterExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeInterpreterExecutor")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl CodeInterpreterExecutor {
    #[cfg(feature = "embedded-code-interpreter")]
    pub(crate) fn from_config(config: CodeInterpreterRuntimeConfig) -> Result<Self, ToolError> {
        config
            .validate()
            .map_err(|error| ToolError::Config(error.to_string()))?;
        let provider: Arc<dyn CodeInterpreterProvider> = Arc::new(eryx::EryxProvider::new(config)?);
        Self::with_provider(config, provider)
    }

    #[cfg_attr(not(any(feature = "embedded-code-interpreter", test)), allow(dead_code))]
    fn with_provider(
        config: CodeInterpreterRuntimeConfig,
        provider: Arc<dyn CodeInterpreterProvider>,
    ) -> Result<Self, ToolError> {
        config
            .validate()
            .map_err(|error| ToolError::Config(error.to_string()))?;
        provider.check_ready()?;
        let aggregate_slots = config.max_aggregate_guest_memory_bytes.get() / config.max_guest_memory_bytes.get();
        let worker_slots = config.max_aggregate_worker_memory_bytes.get() / config.max_worker_memory_bytes.get();
        let guest_slots = config
            .max_concurrent_guests
            .get()
            .min(provider.max_concurrency().get())
            .min(aggregate_slots)
            .min(worker_slots);
        let guest_permits = Arc::new(Semaphore::new(guest_slots));
        Ok(Self {
            config,
            provider,
            guest_permits,
        })
    }

    async fn execute_call(&self, arguments: &str) -> Result<ExecutionOutput, ToolError> {
        let arguments = CodeInterpreterCallArguments::from_json(arguments, self.config.max_source_bytes)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let permit = Arc::clone(&self.guest_permits)
            .try_acquire_owned()
            .map_err(|_| ToolError::Execution("code interpreter capacity is temporarily unavailable".to_owned()))?;
        let provider = Arc::clone(&self.provider);
        let code = arguments.into_code();
        let (sender, receiver) = oneshot::channel();
        let cancellation = Arc::new(ExecutionCancellation::default());
        let supervisor_cancellation = Arc::clone(&cancellation);

        // Hold the permit until the provider has terminated, including after
        // request cancellation or disconnect.
        tokio::spawn(async move {
            let _permit = permit;
            let output = provider.execute(code, supervisor_cancellation).await;
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
        output.map(|output| bound_output(output, self.config))
    }
}

fn bound_output(mut output: ExecutionOutput, config: CodeInterpreterRuntimeConfig) -> ExecutionOutput {
    let stdout_exceeded = truncate_output(&mut output.stdout, config.max_stdout_bytes.get());
    let stderr_exceeded = truncate_output(&mut output.stderr, config.max_stderr_bytes.get());
    if stdout_exceeded || stderr_exceeded {
        output.status = ExecutionStatus::Incomplete;
    }
    output
}

fn truncate_output(output: &mut String, limit: usize) -> bool {
    const MARKER: &str = "\n[output truncated]";

    if output.len() <= limit {
        return false;
    }
    let marker = &MARKER[..MARKER.len().min(limit)];
    let mut retained = limit - marker.len();
    while !output.is_char_boundary(retained) {
        retained -= 1;
    }
    output.truncate(retained);
    output.push_str(marker);
    true
}

struct CancelOnDrop {
    cancellation: Arc<ExecutionCancellation>,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
    }
}

impl GatewayExecutor for CodeInterpreterExecutor {
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

    fn plan_gateway_events(&self, call: &FunctionToolCall, _params: &Self::ExecutionParams) -> GatewayToolEventPlan {
        GatewayToolEventPlan::new(Some(output_item(
            call,
            self.config.max_source_bytes,
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
        Some(output_item(call, self.config.max_source_bytes, status, Some(outputs)))
    }
}

impl ToolHandler for CodeInterpreterExecutor {
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

fn output_item(
    call: &FunctionToolCall,
    max_source_bytes: std::num::NonZeroUsize,
    status: CodeInterpreterCallStatus,
    outputs: Option<Vec<CodeInterpreterCallOutput>>,
) -> OutputItem {
    let code = CodeInterpreterCallArguments::from_json(&call.arguments, max_source_bytes)
        .map(CodeInterpreterCallArguments::into_code)
        .unwrap_or_default();
    let suffix = call_output_id_suffix(call);
    OutputItem::CodeInterpreterCall(CodeInterpreterCall {
        agent: call.agent.clone(),
        id: format!("ci_{suffix}"),
        container_id: format!("cntr_{suffix}"),
        code,
        status,
        outputs,
        origin: crate::types::io::code_interpreter::CodeInterpreterCallOrigin::Gateway,
    })
}

fn call_output_id_suffix(call: &FunctionToolCall) -> String {
    if let Some(suffix) = call
        .id
        .strip_prefix("fc_")
        .filter(|suffix| !suffix.is_empty() && !suffix.starts_with("fc_"))
    {
        return suffix.to_owned();
    }
    // Fallbacks have a separate suffix namespace so an unrelated call_123
    // cannot collide with the public ID derived from fc_123.
    let hash = call
        .id
        .bytes()
        .chain(std::iter::once(0))
        .chain(call.call_id.bytes())
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    format!("h{hash:016x}")
}

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

    struct FakeProvider;

    impl CodeInterpreterProvider for FakeProvider {
        fn check_ready(&self) -> Result<(), ToolError> {
            Ok(())
        }

        fn max_concurrency(&self) -> std::num::NonZeroUsize {
            std::num::NonZeroUsize::new(1).expect("nonzero")
        }

        fn execute(
            &self,
            _code: String,
            _cancellation: Arc<ExecutionCancellation>,
        ) -> Pin<Box<dyn Future<Output = Result<ExecutionOutput, ToolError>> + Send + '_>> {
            Box::pin(async {
                Ok(ExecutionOutput {
                    status: ExecutionStatus::Completed,
                    stdout: "x".repeat(128),
                    stderr: String::new(),
                })
            })
        }
    }

    #[tokio::test]
    async fn fake_provider_executes_through_handler_with_bounded_public_output() {
        let config = CodeInterpreterRuntimeConfig {
            max_stdout_bytes: std::num::NonZeroUsize::new(32).expect("nonzero"),
            ..CodeInterpreterRuntimeConfig::default()
        };
        let handler = CodeInterpreterExecutor::with_provider(config, Arc::new(FakeProvider)).expect("provider ready");
        let params: CodeInterpreterToolParam =
            serde_json::from_value(serde_json::json!({"container": {"type": "auto"}})).expect("valid declaration");
        let call = FunctionToolCall {
            agent: Some(crate::types::io::AgentAttribution {
                agent_name: "/root/worker".to_owned(),
            }),
            id: "fc_123".to_owned(),
            call_id: "call_123".to_owned(),
            name: CODE_INTERPRETER_FUNCTION_NAME.to_owned(),
            namespace: None,
            arguments: r#"{"code":"print(42)"}"#.to_owned(),
            status: crate::types::event::MessageStatus::Completed,
        };
        let output = handler
            .execute(&call.call_id, &call.name, &call.arguments, &params)
            .await
            .expect("fake provider execution");
        let parsed: ExecutionOutput = serde_json::from_str(&output.output).expect("typed output");
        assert!(matches!(parsed.status, ExecutionStatus::Incomplete));
        assert!(parsed.stdout.len() <= config.max_stdout_bytes.get());
        assert!(parsed.stdout.contains("[output truncated]"));
        let OutputItem::CodeInterpreterCall(item) = handler
            .public_output(&call, &output, GatewayCallStatus::Completed, &params)
            .expect("public call item")
        else {
            panic!("expected code interpreter call");
        };
        assert_eq!(item.id, "ci_123");
        assert_eq!(
            item.agent.as_ref().map(|agent| agent.agent_name.as_str()),
            Some("/root/worker")
        );
        assert!(matches!(item.status, CodeInterpreterCallStatus::Incomplete));
        assert_eq!(item.outputs.expect("output").len(), 1);
    }

    #[test]
    fn gateway_code_interpreter_ids_match_across_started_and_completed_items() {
        let config = CodeInterpreterRuntimeConfig::default();
        let mut public_ids = Vec::new();
        for (id, call_id, expected_suffix) in [
            ("fc_123", "call_456", Some("123")),
            ("fc_fc_123", "call_456", None),
            ("provider-item", "call_123", None),
            ("provider-item", "provider-call", None),
        ] {
            let call = FunctionToolCall {
                agent: None,
                id: id.to_owned(),
                call_id: call_id.to_owned(),
                name: CODE_INTERPRETER_FUNCTION_NAME.to_owned(),
                namespace: None,
                arguments: r#"{"code":"print(42)"}"#.to_owned(),
                status: crate::types::event::MessageStatus::Completed,
            };
            let started_output = output_item(
                &call,
                config.max_source_bytes,
                CodeInterpreterCallStatus::InProgress,
                None,
            );
            assert!(
                started_output.to_input_item().is_none(),
                "gateway projection must not replay"
            );
            let stored = String::try_from(&crate::storage::InOutItem::Output(started_output.clone()))
                .expect("serialize gateway projection for history");
            let stored: serde_json::Value = serde_json::from_str(&stored).expect("stored gateway projection");
            assert_eq!(stored["_agentic_code_interpreter_origin"], "gateway");
            assert!(
                serde_json::to_value(&started_output)
                    .expect("public projection serializes")
                    .get("_agentic_code_interpreter_origin")
                    .is_none()
            );
            let OutputItem::CodeInterpreterCall(started) = started_output else {
                panic!("expected code interpreter item");
            };
            let OutputItem::CodeInterpreterCall(completed) = output_item(
                &call,
                config.max_source_bytes,
                CodeInterpreterCallStatus::Completed,
                Some(vec![CodeInterpreterCallOutput::logs("42\n".to_owned())]),
            ) else {
                panic!("expected code interpreter item");
            };

            assert_eq!(started.id, completed.id);
            assert_eq!(started.container_id, completed.container_id);
            assert_eq!(started.code, completed.code);
            assert!(started.id.starts_with("ci_"));
            assert!(started.container_id.starts_with("cntr_"));
            assert!(!started.container_id.starts_with("cntr_fc_"));
            if let Some(suffix) = expected_suffix {
                assert_eq!(started.id, format!("ci_{suffix}"));
                assert_eq!(started.container_id, format!("cntr_{suffix}"));
            } else {
                assert!(started.id.starts_with("ci_h"));
                assert_eq!(
                    started.id.strip_prefix("ci_"),
                    completed.container_id.strip_prefix("cntr_")
                );
            }
            public_ids.push(started.id);
        }
        assert_ne!(public_ids[0], public_ids[1]);
        assert_ne!(public_ids[0], public_ids[2]);
    }
}
