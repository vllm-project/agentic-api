//! Direct vLLM frontend integration for the shared Agentic API executor.

mod events;
pub mod model_config;
mod prepare;
mod runtime;

pub use runtime::VllmBackend;
pub use vllm_chat::LoadModelBackendsOptions;
pub use vllm_engine_core_client::{EngineCoreClientConfig, TransportMode};

/// Native integration failures reuse the executor error, boxed at this boundary.
pub type Result<T> = std::result::Result<T, Box<agentic_core::executor::ExecutorError>>;
