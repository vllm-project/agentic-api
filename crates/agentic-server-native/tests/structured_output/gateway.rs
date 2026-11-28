//! Replay model samples through the native frontend, Responses routes and SQLite with a local test tokenizer.
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};

use agentic_core::config::{Config, PostgresConfig, ResponsesConfig, SqliteConfig, ToolRuntimeConfig};
use agentic_core::executor::ExecutionContext;
use agentic_core::proxy::ProxyState;
use agentic_core::readiness::llm_readiness_client;
use agentic_core::types::{file_search::FileSearchConfig, model_capabilities::ModelCapabilities};
use agentic_server::app::{
    AppState, DEFAULT_MAX_REQUEST_BODY_SIZE, ReadinessTracker, ServerConfig, WebSocketTracker, build_router,
};
use agentic_server::model_capabilities::ServedModel;
use agentic_server::server::DirectInferenceStatus;
use agentic_vllm::VllmBackend;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use vllm_chat::{ChatLlm, LoadModelBackendsOptions, load_model_backends};
use vllm_llm::{BoxGenerateOutputStream, FinishReason, GenerateOutput, GenerateRequest, Llm, LlmInfo};
use vllm_text::TextLlm;

use super::{array, text, value_str};

pub struct Generation {
    pub format: Value,
    pub previous_message: Option<String>,
    pub tool_output: Option<Value>,
    pub tool: Option<Value>,
    pub item: Value,
}

impl Generation {
    // Auto tool choice allows either the tool grammar or an answer in the active format.
    fn tool_sample(&self, tool: &Value, structural_tag: &str) -> String {
        let tag: Value = serde_json::from_str(structural_tag).unwrap();
        let format = &tag["format"];
        let tool_format = if self.format["type"] == "json_schema" {
            assert_eq!(
                format["type"], "or",
                "the answer schema and the tool must both reach the model"
            );
            let alternatives = array(&format["elements"]);
            let answer = alternatives.iter().find(|f| f["type"] == "json_schema").unwrap();
            assert_eq!(answer["json_schema"].to_string(), self.format["schema"].to_string());
            alternatives.iter().find(|f| f["type"] == "triggered_tags").unwrap()
        } else {
            // The tool remains available on the default continuation, but the answer is unconstrained.
            assert_eq!(format["at_least_one"], false);
            format
        };
        assert_eq!(tool_format["type"], "triggered_tags");
        let [call] = array(&tool_format["tags"]) else {
            panic!("expected the recorded status tool")
        };
        let begin = value_str(&call["begin"]);
        assert!(begin.contains(&tool["name"].to_string()));
        assert_eq!(call["content"]["json_schema"], tool["parameters"]);
        if self.item["type"] == "function_call" {
            assert_eq!(self.item["name"], tool["name"]);
            format!(
                "{begin}{}{}",
                value_str(&self.item["arguments"]),
                value_str(&call["end"])
            )
        } else {
            text(&self.item).to_owned()
        }
    }
}

type Queue = Arc<Mutex<VecDeque<Generation>>>;

struct RecordedModel {
    info: LlmInfo,
    ready: watch::Sender<bool>,
    queue: Queue,
}

#[async_trait::async_trait]
impl Llm for RecordedModel {
    fn info(&self) -> &LlmInfo {
        &self.info
    }

    fn subscribe_health(&self) -> watch::Receiver<bool> {
        self.ready.subscribe()
    }

    fn subscribe_readiness(&self) -> watch::Receiver<bool> {
        self.subscribe_health()
    }

    async fn generate_n(
        &self,
        req: GenerateRequest,
        count: NonZeroU32,
    ) -> vllm_llm::Result<Vec<BoxGenerateOutputStream>> {
        assert_eq!(count.get(), 1);
        let step = self
            .queue
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected native generation");
        let constraint = req.sampling_params.structured_outputs.map(|p| p.params.constraint);
        let output = if let Some(tool) = &step.tool {
            let constraint = constraint.expect("auto tool choice must reach the model");
            step.tool_sample(tool, constraint.as_structural_tag().unwrap())
        } else {
            if step.format["type"] == "json_schema" {
                let constraint = constraint.expect("the answer schema must reach the model");
                assert_eq!(
                    constraint.as_json().unwrap().to_string(),
                    step.format["schema"].to_string()
                );
            } else {
                assert_eq!(constraint, None, "plain text must clear the previous grammar");
            }
            text(&step.item).to_owned()
        };
        let prompt: Vec<u8> = req
            .prompt_token_ids
            .iter()
            .map(|&id| u8::try_from(id).unwrap())
            .collect();
        let messages: Value = serde_json::from_slice(&prompt).unwrap();
        if let Some(previous) = step.previous_message {
            assert!(
                array(&messages)
                    .iter()
                    .any(|m| m["role"] == "assistant" && m["content"] == previous),
                "stored assistant answer missing: {messages}"
            );
        }
        if let Some(tool) = step.tool_output {
            let messages = array(&messages);
            assert!(messages.iter().any(|m| m["role"] == "assistant" && m["tool_calls"][0]["function"]["name"] == "get_project_status"));
            assert!(
                messages
                    .iter()
                    .any(|m| m["role"] == "tool" && m["content"] == tool["output"])
            );
        }
        // Split the sample before the terminal EOS token, as the real engine does.
        assert!(output.is_ascii(), "the fixture tokenizer encodes ASCII");
        let tokens: Vec<u32> = output.bytes().map(u32::from).chain([0]).collect();
        let (first, last) = tokens.split_at(tokens.len() / 2);
        let chunks = [
            GenerateOutput::for_test(Some(req.prompt_token_ids.into()), first.to_vec(), None),
            GenerateOutput::for_test(None, last.to_vec(), Some(FinishReason::Stop(None))),
        ];
        Ok(vec![futures::stream::iter(chunks.map(Ok)).boxed()])
    }

    async fn abort(&self, _: &[String]) -> vllm_llm::Result<()> {
        Ok(())
    }
    async fn shutdown(self: Box<Self>) -> vllm_llm::Result<()> {
        Ok(())
    }
}

pub async fn start(steps: VecDeque<Generation>, model: &str) -> (String, Queue, tokio::task::JoinHandle<()>) {
    let home = tempfile::tempdir().unwrap();
    // One token per ASCII byte makes the real renderer's complete prompt inspectable without model downloads.
    let vocab: serde_json::Map<_, _> = (0..=127u8).map(|b| (char::from(b).to_string(), json!(b))).collect();
    for (file, value) in [
        (
            "config.json",
            json!({"model_type":"qwen2","vocab_size":128,"eos_token_id":0,"max_position_embeddings":8192}),
        ),
        (
            "tokenizer_config.json",
            json!({"chat_template":"{{ messages | tojson }}"}),
        ),
        (
            "tokenizer.json",
            json!({"version":"1.0","added_tokens":[],"decoder":{"type":"Fuse"},
                                  "model":{"type":"BPE","vocab":vocab,"merges":[]}}),
        ),
    ] {
        std::fs::write(home.path().join(file), value.to_string()).unwrap();
    }
    let loaded = load_model_backends(home.path().to_str().unwrap(), LoadModelBackendsOptions::default())
        .await
        .unwrap();
    let queue = Arc::new(Mutex::new(steps));
    let llm = RecordedModel {
        queue: Arc::clone(&queue),
        ready: watch::Sender::new(true),
        info: LlmInfo {
            max_model_len: 8192,
            model_dtype: None,
            supports_lora: false,
            allow_missing_mm_embeddings: false,
            vllm_version: String::new(),
        },
    };
    let chat = ChatLlm::new(
        TextLlm::new(llm, loaded.text_backend).with_served_model_name(model),
        loaded.chat_backend,
    )
    .with_tool_call_parser("qwen3_json".parse().unwrap())
    .with_reasoning_parser("none".parse().unwrap());
    let backend = Arc::new(VllmBackend::from_chat(chat));
    let config = Config {
        llm_api_base: String::new(),
        openai_api_key: None,
        llm_ready_timeout_s: 1.0,
        llm_ready_interval_s: 0.1,
        skip_llm_ready_check: false,
        db_url: Some("sqlite::memory:".into()),
        postgres: PostgresConfig::default(),
        sqlite: SqliteConfig::default(),
        tools: ToolRuntimeConfig {
            file_search: FileSearchConfig {
                files_storage_dir: Some(home.path().join("files")),
                ..FileSearchConfig::default()
            },
            ..ToolRuntimeConfig::default()
        },
        responses: ResponsesConfig::default(),
        model_capabilities: ModelCapabilities::default(),
        max_request_body_size: DEFAULT_MAX_REQUEST_BODY_SIZE.get(),
    };
    let status = DirectInferenceStatus::new(
        ServedModel::new(model.into(), backend.max_model_len()),
        backend.subscribe_readiness(),
    );
    let state = AppState {
        proxy_state: ProxyState::new(config.clone()).unwrap(),
        exec_ctx: Arc::new(
            ExecutionContext::from_config(&config)
                .await
                .unwrap()
                .with_direct_inference(backend),
        ),
        direct_status: Some(Arc::new(status)),
        llm_readiness_client: llm_readiness_client().unwrap(),
        readiness_tracker: ReadinessTracker::default(),
        shutdown_token: CancellationToken::new(),
        websocket_tracker: WebSocketTracker::default(),
        llm_api_base: config.llm_api_base,
        skip_llm_ready_check: false,
        openai_api_key: None,
        max_request_body_size: DEFAULT_MAX_REQUEST_BODY_SIZE,
    };
    let router = build_router(state, &ServerConfig::from_env());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _home = home;
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{address}"), queue, handle)
}
