use std::sync::OnceLock;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde_json::{Value, json};
use tracing::warn;

use agentic_core::proxy::{ProxyBody, ProxyResponse, error_response, proxy_get};

use super::super::common::convert_response;
use crate::app::AppState;

/// Static fields shared by every Codex `ModelInfo` entry.
///
/// Built once on first use; cloned per model and patched with the per-model
/// values (`slug`, `display_name`, `auto_review_model_override`,
/// `supports_reasoning_summaries`, `input_modalities`, and optionally
/// `context_window` / `max_context_window`).
fn codex_model_template() -> &'static Value {
    static TEMPLATE: OnceLock<Value> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        json!({
            "supported_in_api": true,
            "priority": 1,
            "shell_type": "shell_command",
            "visibility": "list",
            "base_instructions": "",
            "supported_reasoning_levels": [
                {"effort": "low",    "description": "Fast responses with lighter reasoning"},
                {"effort": "medium", "description": "Balances speed and reasoning depth"},
                {"effort": "high",   "description": "Greater reasoning depth for complex problems"}
            ],
            "default_reasoning_summary": "auto",
            "support_verbosity": false,
            "default_verbosity": null,
            "apply_patch_tool_type": "freeform",
            "web_search_tool_type": "text",
            "truncation_policy": {"mode": "bytes", "limit": 100_000},
            "supports_parallel_tool_calls": true,
            "supports_image_detail_original": false,
            "effective_context_window_percent": 95,
            "experimental_supported_tools": [],
            "supports_search_tool": false,
            "use_responses_lite": false,
            "tool_mode": null,
            "multi_agent_version": null,
        })
    })
}

/// Transform a single upstream model entry into a Codex `ModelInfo` object.
///
/// Returns `None` when the entry has no `id` field (malformed upstream data).
fn upstream_model_to_codex(m: &Value) -> Option<Value> {
    let id = m["id"].as_str()?.to_owned();
    let display_name = m.get("name").and_then(Value::as_str).unwrap_or(&id).to_owned();
    // vLLM uses max_model_len; other providers may use context_length
    let context_length = m["max_model_len"].as_i64().or_else(|| m["context_length"].as_i64());
    // Single pass over capabilities for both flags
    let (supports_reasoning, supports_image) = m["capabilities"].as_array().map_or((false, false), |c| {
        c.iter().fold((false, false), |(r, i), v| {
            let s = v.as_str();
            (r || s == Some("reasoning"), i || s == Some("image"))
        })
    });
    let input_modalities = if supports_image {
        json!(["text", "image"])
    } else {
        json!(["text"])
    };

    let mut model = codex_model_template().clone();
    let obj = model.as_object_mut().expect("template is object");
    obj.insert("slug".into(), json!(id));
    obj.insert("display_name".into(), json!(display_name));
    obj.insert("auto_review_model_override".into(), json!(id));
    obj.insert("supports_reasoning_summaries".into(), json!(supports_reasoning));
    obj.insert("input_modalities".into(), input_modalities);
    if let Some(ctx) = context_length {
        obj.insert("context_window".into(), json!(ctx));
        obj.insert("max_context_window".into(), json!(ctx));
    }

    Some(model)
}

/// Build the Codex `ModelsResponse` from a raw upstream vLLM models payload.
fn build_codex_models_response(upstream_bytes: &[u8]) -> Value {
    let models: Vec<Value> = serde_json::from_slice::<Value>(upstream_bytes)
        .ok()
        .and_then(|mut v| match v["data"].take() {
            Value::Array(arr) => Some(arr),
            _ => None,
        })
        .into_iter()
        .flatten()
        .filter_map(|m| upstream_model_to_codex(&m))
        .collect();
    json!({ "models": models })
}

pub async fn health() -> impl IntoResponse {
    StatusCode::OK
}

pub async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let base = state.llm_api_base.trim_end_matches('/');
    let url = format!("{base}/health");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build();

    let Ok(client) = client else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => StatusCode::OK,
        Ok(resp) => {
            warn!("LLM backend not ready: {}", resp.status());
            StatusCode::SERVICE_UNAVAILABLE
        }
        Err(e) => {
            warn!("LLM backend unreachable: {e}");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

/// Query parameters for GET /v1/models.
///
/// Codex CLI appends `?client_version=<ver>` to identify itself; its presence
/// triggers transformation to the Codex `ModelsResponse` shape.
#[derive(serde::Deserialize)]
pub struct ModelsParams {
    client_version: Option<String>,
}

/// GET /v1/models — Codex-aware model list.
///
/// When `?client_version` is present (Codex CLI), fetches vLLM's model list via
/// [`proxy_get`] and transforms it into the Codex `ModelsResponse` shape
/// (`{ "models": [...] }` with rich metadata). Without `client_version`, the
/// upstream response is returned unchanged via [`proxy_get`].
pub async fn models(State(state): State<AppState>, headers: HeaderMap, Query(params): Query<ModelsParams>) -> Response {
    let upstream = proxy_get("/v1/models", &headers, &state.proxy_state).await;

    if params.client_version.is_none() {
        return convert_response(upstream);
    }

    let ProxyBody::Full(upstream_bytes) = upstream.body else {
        return convert_response(error_response(
            http::StatusCode::BAD_GATEWAY,
            "upstream_unavailable",
            "unexpected streaming response from /v1/models",
        ));
    };

    if !upstream.status.is_success() {
        return convert_response(ProxyResponse {
            body: ProxyBody::Full(upstream_bytes),
            ..upstream
        });
    }

    axum::Json(build_codex_models_response(&upstream_bytes)).into_response()
}
