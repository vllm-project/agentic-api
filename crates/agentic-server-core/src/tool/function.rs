use serde_json::Value;

use crate::types::io::FunctionTool;
use crate::types::tools::FunctionToolParam;

use super::handler::{ToolError, ToolHandler};
use super::registry::ToolType;

impl From<&FunctionToolParam> for FunctionTool {
    fn from(p: &FunctionToolParam) -> Self {
        Self {
            type_: "function".to_owned(),
            name: p.name.as_str().to_owned(),
            description: p.description.clone(),
            parameters: p.parameters.clone(),
            strict: p.strict,
        }
    }
}

/// Handler for `type: "function"` tools.
///
/// Function tools are client-owned: the gateway normalises them for vLLM but
/// never executes them. `FunctionHandler` intentionally implements only
/// [`ToolHandler`], not [`super::handler::GatewayExecutor`] — the type system
/// makes it impossible to call `execute()` on a client-owned tool.
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
        // conversion path. name is NonEmptyToolName so serde rejects empty names;
        // any remaining deserialize error means validate() was not called first.
        match serde_json::from_value::<FunctionToolParam>(param.clone()) {
            Ok(p) => vec![FunctionTool::from(&p)],
            Err(e) => {
                tracing::warn!("normalize() called with invalid param: {e} — validate() must be called first");
                vec![]
            }
        }
    }
}
