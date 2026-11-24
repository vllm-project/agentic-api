//! Model preparation and lifetime, separate from per-request tool preparation.

use std::fmt;
use std::sync::Arc;

use agentic_core::executor::error::{ExecutorError, ExecutorResult};
use agentic_core::executor::inference::direct::{DirectInferenceBackend, EventStream, GenerationControl};
use agentic_core::types::request_response::UpstreamRequest;
use futures::future::BoxFuture;
use vllm_chat::{ChatLlm, LoadModelBackendsOptions, load_model_backends};
use vllm_engine_core_client::{EngineCoreClient, EngineCoreClientConfig};
use vllm_llm::EngineCoreLlm;
use vllm_text::TextLlm;

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
        let loaded = load_model_backends(&engine.model_name, model)
            .await
            .map_err(backend_error)?;
        let client = EngineCoreClient::connect(engine).await.map_err(backend_error)?;
        let chat = ChatLlm::new(
            TextLlm::new(EngineCoreLlm::new(client), loaded.text_backend),
            loaded.chat_backend,
        );
        Ok(Self::from_chat(chat))
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
