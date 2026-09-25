use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The requested format of a custom tool's raw input.
///
/// The function-tool adapter presents grammar formats as model instructions;
/// it does not configure grammar-constrained decoding in the upstream service.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CustomToolInputFormat {
    Text {
        #[serde(default, flatten)]
        extra: HashMap<String, Value>,
    },
    Grammar {
        syntax: CustomToolGrammarSyntax,
        definition: String,
        #[serde(default, flatten)]
        extra: HashMap<String, Value>,
    },
}

/// Grammar syntaxes accepted by the Responses custom-tool declaration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum CustomToolGrammarSyntax {
    Lark,
    Regex,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_and_unknown_formats_are_rejected() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"type": "unknown"}),
            serde_json::json!({"type": "grammar", "syntax": "lark"}),
            serde_json::json!({"type": "grammar", "syntax": "unknown", "definition": "x"}),
            serde_json::json!({"type": "grammar", "syntax": "lark", "definition": 1}),
        ] {
            assert!(serde_json::from_value::<CustomToolInputFormat>(value).is_err());
        }
    }
}
