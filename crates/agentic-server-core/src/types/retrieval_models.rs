//! Typed text-only model transport for in-tree file search.
use serde::{Deserialize, Serialize};
#[derive(Serialize)]
pub struct EmbeddingRequest<'a> {
    pub model: &'a str,
    pub input: &'a [String],
    pub encoding_format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<usize>,
}
#[derive(Deserialize)]
pub struct EmbeddingResponse {
    pub model: String,
    pub data: Vec<EmbeddingData>,
}
#[derive(Deserialize)]
pub struct EmbeddingData {
    pub index: usize,
    pub embedding: Vec<f64>,
}
#[derive(Serialize)]
pub struct RetrievalChatRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<RetrievalMessage<'a>>,
    pub stream: bool,
    pub temperature: f64,
    pub max_tokens: usize,
}
#[derive(Serialize)]
pub struct RetrievalMessage<'a> {
    pub role: RetrievalRole,
    pub content: &'a str,
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalRole {
    System,
    User,
}
#[derive(Deserialize)]
pub struct RetrievalChatResponse {
    pub choices: Vec<RetrievalChoice>,
}
#[derive(Deserialize)]
pub struct RetrievalChoice {
    pub message: RetrievalChatMessage,
}
#[derive(Deserialize)]
pub struct RetrievalChatMessage {
    pub content: Option<RetrievalText>,
}
#[derive(Deserialize)]
#[serde(untagged)]
pub enum RetrievalText {
    Text(String),
    Parts(Vec<RetrievalTextPart>),
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RetrievalTextPart {
    Text { text: String },
}
#[derive(Serialize)]
pub struct TextRerankRequest<'a> {
    pub model: &'a str,
    pub query: &'a str,
    pub documents: Vec<&'a str>,
    pub top_n: usize,
}
#[derive(Deserialize)]
pub struct TextRerankResponse {
    pub results: Vec<TextRerankResult>,
}
#[derive(Deserialize)]
pub struct TextRerankResult {
    pub index: usize,
    pub relevance_score: f64,
}
