//! Agentic executor.

pub mod accumulator;
pub mod engine;
pub mod error;
pub mod inference;
pub mod modes;
pub mod persist;
pub mod rehydrate;
pub mod request;

mod gateway;
mod upstream;

pub use engine::{BoxStream, ExecuteRequest, create_conversation, execute};
pub use error::{ExecutorError, ExecutorResult};
pub use inference::call_inference;
pub use modes::{ConversationHandler, ResponseHandler};
pub use persist::persist_response;
pub use rehydrate::rehydrate_conversation;
pub use request::ExecutionContext;
pub use request::RequestContext;
