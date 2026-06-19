use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseRequest {
    pub model: String,
    #[serde(default)]
    pub input: ResponseInput,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub tools: Vec<ToolConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseInput {
    Text(String),
    Items(Vec<serde_json::Value>),
}

impl Default for ResponseInput {
    fn default() -> Self {
        Self::Items(Vec::new())
    }
}

impl ResponseInput {
    #[must_use]
    pub fn to_values(&self) -> Vec<serde_json::Value> {
        match self {
            Self::Text(text) => vec![serde_json::json!({
                "type": "message",
                "role": "user",
                "content": text
            })],
            Self::Items(items) => items.clone(),
        }
    }

    pub fn prepend(&mut self, mut history: Vec<serde_json::Value>) {
        history.extend(self.to_values());
        *self = Self::Items(history);
    }

    pub fn push(&mut self, item: serde_json::Value) {
        match self {
            Self::Text(_) => {
                let mut items = self.to_values();
                items.push(item);
                *self = Self::Items(items);
            }
            Self::Items(items) => items.push(item),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolConfig {
    pub r#type: String,
    #[serde(default)]
    pub vector_store_ids: Option<Vec<String>>,
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseBody {
    pub id: String,
    #[serde(default)]
    pub output: Vec<VllmOutputItem>,
    #[serde(default)]
    pub status: String,
    #[serde(flatten)]
    pub rest: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum VllmOutputItem {
    #[serde(rename = "message")]
    Message {
        #[serde(flatten)]
        fields: serde_json::Map<String, serde_json::Value>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        arguments: String,
        #[serde(flatten)]
        rest: serde_json::Map<String, serde_json::Value>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub data: Vec<SearchResult>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_num_results: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ranking_options: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub file_id: String,
    pub filename: String,
    pub score: f64,
    #[serde(default)]
    pub attributes: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    pub content: Vec<ContentChunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentChunk {
    pub r#type: String,
    pub text: String,
}
