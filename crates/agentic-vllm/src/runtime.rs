//! Model preparation and lifetime, separate from per-request tool preparation.

use std::fmt;
use std::sync::Arc;

use agentic_core::executor::error::{ExecutorError, ExecutorResult};
use agentic_core::executor::inference::direct::{DirectInferenceBackend, EventStream, GenerationControl};
use agentic_core::types::request_response::UpstreamRequest;
use futures::future::BoxFuture;
use tokio::sync::watch;
use vllm_chat::{ChatLlm, LoadModelBackendsOptions, load_model_backends};
use vllm_engine_core_client::{EngineCoreClient, EngineCoreClientConfig};
use vllm_llm::EngineCoreLlm;
use vllm_text::TextLlm;

use crate::model_config::ModelConfig;
use crate::{Result, events, prepare};

/// Shared tokenizer, renderer, output parsers, and engine client for one model.
pub struct VllmBackend {
    chat: Arc<ChatLlm>,
}

impl fmt::Debug for VllmBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VllmBackend")
            .field("model", &self.chat.served_model_name())
            .finish_non_exhaustive()
    }
}

impl VllmBackend {
    /// Load the model frontend once and connect directly to headless `EngineCore`.
    ///
    /// # Errors
    /// Returns the underlying frontend-loading or engine-handshake failure.
    pub async fn connect(engine: EngineCoreClientConfig, model: LoadModelBackendsOptions) -> Result<Self> {
        let model_id = engine.model_name.clone();
        Ok(Self::from_chat(connect_model(&model_id, engine, model).await?))
    }

    /// Prepare one model recipe and connect its frontend to the owned engine.
    ///
    /// # Errors
    /// Returns tokenizer, renderer, or engine-handshake failures.
    pub async fn connect_recipe(engine: EngineCoreClientConfig, recipe: &ModelConfig) -> Result<Self> {
        recipe.validate().map_err(backend_error)?;
        let chat = connect_model(&recipe.model, engine, recipe.frontend_options())
            .await?
            .with_tool_call_parser(recipe.tool_call_parser.clone())
            .with_reasoning_parser(recipe.reasoning_parser.clone());
        Ok(Self::from_chat(chat))
    }

    #[must_use]
    pub fn model_name(&self) -> &str {
        self.chat.served_model_name()
    }

    #[must_use]
    pub fn max_model_len(&self) -> u32 {
        self.chat.text().llm().info().max_model_len
    }

    /// Observe permanent frontend/engine failure through the shared vLLM lifecycle signal.
    #[must_use]
    pub fn subscribe_health(&self) -> watch::Receiver<bool> {
        self.chat.text().llm().subscribe_health()
    }

    /// Observe whether the engine currently accepts work, including pause/resume transitions.
    #[must_use]
    pub fn subscribe_readiness(&self) -> watch::Receiver<bool> {
        self.chat.text().llm().subscribe_readiness()
    }

    /// Use an application-configured frontend, including its parser selections.
    #[must_use]
    pub fn from_chat(chat: ChatLlm) -> Self {
        Self { chat: Arc::new(chat) }
    }

    /// Shut down the runtime after its executor contexts and generations are released.
    ///
    /// # Errors
    /// Returns an error while a generation holds the runtime, or if shutdown fails.
    pub async fn shutdown(self) -> Result<()> {
        let chat = Arc::try_unwrap(self.chat).map_err(|_| {
            ExecutorError::InvalidRequest("direct generations must finish before runtime shutdown".to_owned())
        })?;
        Ok(chat.shutdown().await.map_err(backend_error)?)
    }
}

async fn connect_model(
    model_id: &str,
    engine: EngineCoreClientConfig,
    model: LoadModelBackendsOptions,
) -> Result<ChatLlm> {
    let loaded = load_model_backends(model_id, model).await.map_err(backend_error)?;
    let served_model_name = engine.model_name.clone();
    let client = EngineCoreClient::connect(engine).await.map_err(backend_error)?;
    let chat = ChatLlm::new(
        TextLlm::new(EngineCoreLlm::new(client), loaded.text_backend).with_served_model_name(served_model_name),
        loaded.chat_backend,
    );
    Ok(chat)
}

impl DirectInferenceBackend for VllmBackend {
    fn generate<'a>(
        &'a self,
        request: UpstreamRequest<'a>,
        control: GenerationControl,
    ) -> BoxFuture<'a, ExecutorResult<EventStream>> {
        Box::pin(async move {
            let request = prepare::chat_request(request, &control.request_id, self.chat.served_model_name())
                .map_err(|error| *error)?;
            let stream = tokio::select! {
                biased;
                () = control.cancellation.cancelled() => {
                    return Err(ExecutorError::StreamError("direct inference cancelled".to_owned()));
                }
                result = self.chat.chat(request) => result.map_err(backend_error)?,
            };
            Ok(events::convert(stream, control, Arc::clone(&self.chat)))
        })
    }
}

pub(crate) fn backend_error(error: impl std::error::Error + Send + Sync + 'static) -> ExecutorError {
    ExecutorError::DirectInference(Box::new(error))
}
