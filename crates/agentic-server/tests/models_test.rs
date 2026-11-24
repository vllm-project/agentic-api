// The shared helpers serve every integration test; this one needs a models-specific upstream.
#[allow(dead_code)]
mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agentic_core::executor::inference::direct::{DirectInferenceBackend, EventStream, GenerationControl};
use agentic_core::executor::{ExecutorError, ExecutorResult};
use agentic_core::types::messages::ReasoningEffortLevel;
use agentic_core::types::request_response::UpstreamRequest;
use agentic_server::app::AppState;
use agentic_server::model_capabilities::{
    CodexCatalogCapabilities, InputModalities, ModelCapabilities, ModelOverride, ServedModel, SupportedReasoningLevels,
};
use agentic_server::server::DirectInferenceStatus;
use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;
use common::{spawn_gateway, test_config, test_state};
use futures::future::BoxFuture;
use http::StatusCode;
use serde_json::Value;
use tokio::net::TcpListener;

#[derive(Debug, Default)]
struct NativeAdmission(AtomicBool);

impl DirectInferenceBackend for NativeAdmission {
    fn generate<'a>(
        &'a self,
        _request: UpstreamRequest<'a>,
        _control: GenerationControl,
    ) -> BoxFuture<'a, ExecutorResult<EventStream>> {
        self.0.store(true, Ordering::SeqCst);
        Box::pin(async { Err(ExecutorError::InvalidRequest("native admission reached".to_owned())) })
    }
}

#[tokio::test]
async fn direct_gateway_uses_native_catalog_readiness_authentication_and_admission() {
    // An invalid URL makes any accidental HTTP upstream access fail.
    let mut state = test_state(&test_config("invalid-http-url"));
    let native = Arc::new(NativeAdmission::default());
    let executor = Arc::try_unwrap(state.exec_ctx).expect("unique fixture context");
    state.exec_ctx = Arc::new(executor.with_direct_inference(native.clone()));
    let (readiness, receiver) = tokio::sync::watch::channel(true);
    state.direct_status = Some(Arc::new(DirectInferenceStatus::new(
        ServedModel::new("org/assistant".to_owned(), 8192),
        receiver,
    )));
    let shutdown = state.shutdown_token.clone();
    let (url, gateway) = spawn_gateway(state).await;
    let client = reqwest::Client::new();
    assert_eq!(
        client.get(format!("{url}/ready")).send().await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        client.get(format!("{url}/v1/models")).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    for path in ["/v1/models", "/v1/models?client_version=1", "/v1/models/org/assistant"] {
        let response = client
            .get(format!("{url}{path}"))
            .bearer_auth("test-key")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        if path.contains('?') {
            assert_eq!(body["models"][0]["slug"], "org/assistant");
            assert_eq!(body["models"][0]["context_window"], 8192);
            let catalog: CodexCatalogCapabilities = serde_json::from_value(body).unwrap();
            assert!(!catalog.models[0].supports_reasoning_summaries);
        } else if path.ends_with("assistant") {
            assert_eq!(body["id"], "org/assistant");
        } else {
            assert_eq!(body["data"][0]["id"], "org/assistant");
        }
    }
    assert_eq!(
        client
            .get(format!("{url}/v1/models/missing"))
            .bearer_auth("test-key")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let response = client
        .post(format!("{url}/v1/responses"))
        .bearer_auth("test-key")
        .json(&serde_json::json!({"model":"org/assistant","input":"hello","store":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response.text().await.unwrap().contains("native admission reached"));
    assert!(native.0.load(Ordering::SeqCst));
    for path in ["/v1/chat/completions", "/v1/messages", "/v1/embeddings"] {
        let response = client
            .post(format!("{url}{path}"))
            .bearer_auth("test-key")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }
    for (ready, expected) in [(false, StatusCode::SERVICE_UNAVAILABLE), (true, StatusCode::OK)] {
        readiness.send(ready).unwrap();
        assert_eq!(
            client.get(format!("{url}/ready")).send().await.unwrap().status(),
            expected
        );
    }
    drop(readiness);
    assert_eq!(
        client.get(format!("{url}/ready")).send().await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    shutdown.cancel();
    assert_eq!(
        client.get(format!("{url}/ready")).send().await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    gateway.abort();
    let _ = gateway.await;
}

/// Deliberately irregular whitespace and an unknown field, so a pass-through response can be
/// compared byte for byte against what the upstream actually sent.
const UPSTREAM_MODELS: &str = r#"{"object":"list",  "data":[
  {"id":"vision-model","max_model_len":32768,"owned_by":"mock-vllm"},
  {"id":"upstream-image-model","capabilities":["image"]},
  {"id":"pinned-text-model","capabilities":["image"]},
  {"id":"plain-model"}
]}"#;

/// A vLLM-style list whose second entry has an ID containing `/` and fields the gateway does
/// not model, so a retrieved entry can be compared byte for byte.
const UPSTREAM_MODELS_WITH_PATH_ID: &str = r#"{"object":"list","data":[
  {"id":"plain-model","object":"model","owned_by":"vllm"},
  {"id":"meta-llama/Llama-3.1-8B-Instruct",  "object":"model","max_model_len":131072,"root":"meta-llama/Llama-3.1-8B-Instruct"}]}"#;

const PATH_ID_ENTRY: &str = r#"{"id":"meta-llama/Llama-3.1-8B-Instruct",  "object":"model","max_model_len":131072,"root":"meta-llama/Llama-3.1-8B-Instruct"}"#;

/// Upstream `/v1/models` that answers with `body` and `status`, tagged with a request ID.
async fn spawn_upstream_models(body: &'static str, status: StatusCode) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route(
        "/v1/models",
        get(move || async move {
            let headers = [
                (http::header::CONTENT_TYPE, "application/json"),
                (http::HeaderName::from_static("x-request-id"), "req-upstream"),
            ];
            (status, headers, body).into_response()
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), upstream)
}

/// An override carrying only the listed input modalities.
fn with_modalities(modalities: InputModalities) -> ModelOverride {
    ModelOverride {
        input_modalities: Some(modalities),
        ..ModelOverride::default()
    }
}

/// An override carrying only the listed reasoning levels.
fn with_levels(levels: &[ReasoningEffortLevel]) -> ModelOverride {
    ModelOverride {
        supported_reasoning_levels: Some(SupportedReasoningLevels::try_new(levels).expect("valid reasoning levels")),
        ..ModelOverride::default()
    }
}

fn state_with_overrides(llm_url: &str, overrides: &[(&str, ModelOverride)]) -> AppState {
    let config = test_config(llm_url);
    let overrides = overrides
        .iter()
        .map(|(model_id, overrides)| ((*model_id).to_owned(), overrides.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut state = test_state(&config);
    Arc::get_mut(&mut state.exec_ctx).unwrap().model_capabilities = ModelCapabilities::new(overrides);
    state
}

async fn spawn_configured_gateway(
    body: &'static str,
    status: StatusCode,
    overrides: &[(&str, ModelOverride)],
) -> (String, tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    let (upstream_url, upstream) = spawn_upstream_models(body, status).await;
    let (gateway_url, gateway) = spawn_gateway(state_with_overrides(&upstream_url, overrides)).await;
    (gateway_url, upstream, gateway)
}

fn field(catalog: &Value, slug: &str, name: &str) -> Value {
    catalog["models"]
        .as_array()
        .expect("models array")
        .iter()
        .find(|model| model["slug"] == slug)
        .unwrap_or_else(|| panic!("catalog must contain {slug}"))[name]
        .clone()
}

fn modalities(catalog: &Value, slug: &str) -> Value {
    field(catalog, slug, "input_modalities")
}

fn reasoning_levels(catalog: &Value, slug: &str) -> Value {
    field(catalog, slug, "supported_reasoning_levels")
}

/// Fetch the Codex catalog from `gateway_url`.
async fn fetch_catalog(gateway_url: &str) -> Value {
    reqwest::get(format!("{gateway_url}/v1/models?client_version=1.2.3"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn ordinary_model_listing_is_passed_through_unchanged() {
    let (gateway_url, _upstream, _gateway) = spawn_configured_gateway(
        UPSTREAM_MODELS,
        StatusCode::OK,
        &[("vision-model", with_modalities(InputModalities::TextAndImage))],
    )
    .await;

    let response = reqwest::get(format!("{gateway_url}/v1/models")).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[http::header::CONTENT_TYPE], "application/json");
    assert_eq!(
        response.text().await.unwrap(),
        UPSTREAM_MODELS,
        "a request without client_version must not be transformed"
    );
}

#[tokio::test]
async fn codex_catalog_resolves_capabilities_by_precedence() {
    let (gateway_url, _upstream, _gateway) = spawn_configured_gateway(
        UPSTREAM_MODELS,
        StatusCode::OK,
        &[
            ("vision-model", with_modalities(InputModalities::TextAndImage)),
            ("pinned-text-model", with_modalities(InputModalities::Text)),
        ],
    )
    .await;

    let catalog: Value = reqwest::get(format!("{gateway_url}/v1/models?client_version=1.2.3"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(
        modalities(&catalog, "vision-model"),
        serde_json::json!(["text", "image"]),
        "a configured vision model advertises images without upstream metadata"
    );
    assert_eq!(
        modalities(&catalog, "upstream-image-model"),
        serde_json::json!(["text", "image"])
    );
    assert_eq!(
        modalities(&catalog, "pinned-text-model"),
        serde_json::json!(["text"]),
        "an explicit text-only override wins over upstream image metadata"
    );
    assert_eq!(modalities(&catalog, "plain-model"), serde_json::json!(["text"]));
    assert_eq!(
        catalog["models"][0]["supports_image_detail_original"],
        serde_json::json!(false)
    );
}

#[tokio::test]
async fn codex_catalog_lists_configured_reasoning_levels_in_order() {
    let (gateway_url, _upstream, _gateway) = spawn_configured_gateway(
        UPSTREAM_MODELS,
        StatusCode::OK,
        &[(
            "plain-model",
            with_levels(&[
                ReasoningEffortLevel::Low,
                ReasoningEffortLevel::High,
                ReasoningEffortLevel::Max,
            ]),
        )],
    )
    .await;

    let catalog = fetch_catalog(&gateway_url).await;

    assert_eq!(
        reasoning_levels(&catalog, "plain-model"),
        serde_json::json!([
            {"effort": "low", "description": "Fast responses with lighter reasoning"},
            {"effort": "high", "description": "Greater reasoning depth for complex problems"},
            {"effort": "max", "description": "Maximum reasoning depth for the hardest problems"}
        ]),
        "a configured model must advertise its levels with their descriptions, in the configured order"
    );
}

#[tokio::test]
async fn codex_catalog_keeps_the_default_reasoning_levels_without_configuration() {
    let (gateway_url, _upstream, _gateway) = spawn_configured_gateway(UPSTREAM_MODELS, StatusCode::OK, &[]).await;

    let catalog = fetch_catalog(&gateway_url).await;

    assert_eq!(
        reasoning_levels(&catalog, "plain-model"),
        serde_json::json!([
            {"effort": "low", "description": "Fast responses with lighter reasoning"},
            {"effort": "medium", "description": "Balances speed and reasoning depth"},
            {"effort": "high", "description": "Greater reasoning depth for complex problems"}
        ]),
        "an unconfigured model must keep the default levels and descriptions"
    );
}

#[tokio::test]
async fn codex_catalog_keeps_the_default_reasoning_levels_for_a_modalities_only_override() {
    let (gateway_url, _upstream, _gateway) = spawn_configured_gateway(
        UPSTREAM_MODELS,
        StatusCode::OK,
        &[("vision-model", with_modalities(InputModalities::TextAndImage))],
    )
    .await;

    let catalog = fetch_catalog(&gateway_url).await;

    assert_eq!(
        reasoning_levels(&catalog, "vision-model"),
        serde_json::json!([
            {"effort": "low", "description": "Fast responses with lighter reasoning"},
            {"effort": "medium", "description": "Balances speed and reasoning depth"},
            {"effort": "high", "description": "Greater reasoning depth for complex problems"}
        ]),
        "an override that sets only input modalities must not change the advertised levels"
    );
}

#[tokio::test]
async fn codex_catalog_is_text_only_without_configuration() {
    let (gateway_url, _upstream, _gateway) = spawn_configured_gateway(UPSTREAM_MODELS, StatusCode::OK, &[]).await;

    let catalog: Value = reqwest::get(format!("{gateway_url}/v1/models?client_version=1.2.3"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(modalities(&catalog, "vision-model"), serde_json::json!(["text"]));
    assert_eq!(modalities(&catalog, "plain-model"), serde_json::json!(["text"]));
}

#[tokio::test]
async fn upstream_failures_are_forwarded_without_transformation() {
    let (gateway_url, _upstream, _gateway) =
        spawn_configured_gateway(r#"{"error":"upstream exploded"}"#, StatusCode::SERVICE_UNAVAILABLE, &[]).await;

    let response = reqwest::get(format!("{gateway_url}/v1/models?client_version=1.2.3"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.text().await.unwrap(), r#"{"error":"upstream exploded"}"#);
}

#[tokio::test]
async fn undecodable_upstream_payload_is_reported_as_a_bad_gateway() {
    let (gateway_url, _upstream, _gateway) =
        spawn_configured_gateway(r#"{"data":"not-a-list"}"#, StatusCode::OK, &[]).await;

    let response = reqwest::get(format!("{gateway_url}/v1/models?client_version=1.2.3"))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::BAD_GATEWAY,
        "an undecodable payload must not be served as an empty catalog"
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "upstream_unavailable");
    assert_eq!(body["error"]["message"], "invalid model list from /v1/models");
}

#[tokio::test]
async fn upstream_without_models_yields_an_empty_catalog() {
    let (gateway_url, _upstream, _gateway) = spawn_configured_gateway("{}", StatusCode::OK, &[]).await;

    let catalog: Value = reqwest::get(format!("{gateway_url}/v1/models?client_version=1.2.3"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(catalog["models"], serde_json::json!([]));
}

/// The launcher reads the served catalog through [`CodexCatalogCapabilities`], so the catalog
/// written into an isolated Codex home can only match the HTTP catalog while that view keeps
/// parsing what this handler serves.
#[tokio::test]
async fn launcher_capability_view_parses_the_served_catalog() {
    let (gateway_url, _upstream, _gateway) = spawn_configured_gateway(
        UPSTREAM_MODELS,
        StatusCode::OK,
        &[
            ("vision-model", with_modalities(InputModalities::TextAndImage)),
            ("pinned-text-model", with_modalities(InputModalities::Text)),
        ],
    )
    .await;

    let catalog: CodexCatalogCapabilities = reqwest::get(format!("{gateway_url}/v1/models?client_version=1.2.3"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(
        catalog.select(None).expect("first entry").slug,
        "vision-model",
        "the launcher selects the first advertised model when none is requested"
    );
    assert_eq!(
        catalog
            .select(Some("vision-model"))
            .expect("configured vision model")
            .input_modalities,
        InputModalities::TextAndImage
    );
    assert_eq!(
        catalog
            .select(Some("upstream-image-model"))
            .expect("upstream image model")
            .input_modalities,
        InputModalities::TextAndImage
    );
    assert_eq!(
        catalog
            .select(Some("pinned-text-model"))
            .expect("pinned text model")
            .input_modalities,
        InputModalities::Text
    );
    assert!(catalog.select(Some("absent-model")).is_none());
}

#[tokio::test]
async fn listed_model_is_retrieved_as_the_upstream_wrote_it() {
    let (gateway_url, _upstream, _gateway) =
        spawn_configured_gateway(UPSTREAM_MODELS_WITH_PATH_ID, StatusCode::OK, &[]).await;

    let response = reqwest::get(format!("{gateway_url}/v1/models/meta-llama/Llama-3.1-8B-Instruct"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[http::header::CONTENT_TYPE], "application/json");
    assert_eq!(
        response.headers()["x-request-id"],
        "req-upstream",
        "upstream metadata headers are kept"
    );
    assert_eq!(
        response.headers()[http::header::CONTENT_LENGTH],
        PATH_ID_ENTRY.len().to_string(),
        "the length describes the entry, not the upstream list"
    );
    assert_eq!(response.text().await.unwrap(), PATH_ID_ENTRY);
}

#[tokio::test]
async fn encoded_slash_retrieves_the_same_model() {
    let (gateway_url, _upstream, _gateway) =
        spawn_configured_gateway(UPSTREAM_MODELS_WITH_PATH_ID, StatusCode::OK, &[]).await;

    let response = reqwest::get(format!("{gateway_url}/v1/models/meta-llama%2FLlama-3.1-8B-Instruct"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), PATH_ID_ENTRY);
}

#[tokio::test]
async fn unlisted_model_retrieval_is_a_not_found_error() {
    let (gateway_url, _upstream, _gateway) =
        spawn_configured_gateway(UPSTREAM_MODELS_WITH_PATH_ID, StatusCode::OK, &[]).await;

    for path in ["/v1/models/missing-model", "/v1/models/meta-llama"] {
        let response = reqwest::get(format!("{gateway_url}{path}")).await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "model_not_found", "{path}");
    }
}

#[tokio::test]
async fn model_retrieval_forwards_upstream_failures_unchanged() {
    let (gateway_url, _upstream, _gateway) =
        spawn_configured_gateway(r#"{"error":"upstream exploded"}"#, StatusCode::SERVICE_UNAVAILABLE, &[]).await;

    let response = reqwest::get(format!("{gateway_url}/v1/models/plain-model"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.text().await.unwrap(), r#"{"error":"upstream exploded"}"#);
}

#[tokio::test]
async fn model_retrieval_reports_an_undecodable_list_as_a_bad_gateway() {
    let (gateway_url, _upstream, _gateway) =
        spawn_configured_gateway(r#"{"data":"not-a-list"}"#, StatusCode::OK, &[]).await;

    let response = reqwest::get(format!("{gateway_url}/v1/models/plain-model"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "upstream_unavailable");
}
