use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use crate::types::io::FunctionTool;
use crate::types::tools::FunctionToolParam;

use super::handler::{ToolError, ToolHandler, ToolOutput};
use super::registry::ToolType;

impl From<&FunctionToolParam> for FunctionTool {
    fn from(p: &FunctionToolParam) -> Self {
        Self {
            type_: "function".to_owned(),
            name: p.name.clone(),
            description: p.description.clone(),
            parameters: p.parameters.clone(),
            strict: p.strict,
        }
    }
}

/// Handler for `type: "function"` tools.
///
/// Function tools are client-owned: the gateway normalises them for vLLM but
/// never executes them. `execute()` is a no-op that should never be called.
#[derive(Debug)]
pub struct FunctionHandler;

impl ToolHandler for FunctionHandler {
    fn tool_type(&self) -> ToolType {
        ToolType::Function
    }

    fn validate(&self, param: &Value) -> Result<(), ToolError> {
        match param.get("name").and_then(Value::as_str) {
            Some(name) if !name.is_empty() => Ok(()),
            _ => Err(ToolError::Config("function tool must have a non-empty name".into())),
        }
    }

    fn normalize(&self, param: &Value) -> Vec<FunctionTool> {
        // Deserialize into the typed struct so From<&FunctionToolParam> is the single
        // conversion path — no risk of the manual-extraction path diverging from it.
        let p: FunctionToolParam = serde_json::from_value(param.clone())
            .expect("normalize() called with invalid param — validate() must be called first");
        vec![FunctionTool::from(&p)]
    }

    fn execute(
        &self,
        _tool_name: &str,
        _arguments: &str,
        _config: &Value,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, ToolError>> + Send + '_>> {
        Box::pin(async {
            Err(ToolError::Execution(
                "function tools are client-owned and are not executed by the gateway".into(),
            ))
        })
    }
}
