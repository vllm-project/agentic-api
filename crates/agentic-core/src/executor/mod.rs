//! Agentic loop executor.

pub mod accumulator;
pub mod dispatch;
pub mod engine;
pub mod error;
pub mod execute_loop;
pub mod modes;
pub mod request;
pub mod tool_context;

pub use dispatch::{LoopDecision, dispatch_tools};
pub use engine::{BoxStream, call_inference, create_conversation, execute, persist_response, rehydrate_conversation};
pub use error::{ExecutorError, ExecutorResult};
pub use execute_loop::execute_loop;
pub use modes::{ConversationHandler, ResponseHandler};
pub use request::ExecutionContext;
pub use request::RequestContext;
pub use tool_context::ToolContext;
