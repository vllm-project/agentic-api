mod bounded_http;
pub mod client;
pub mod handler;
pub mod pool;
mod propagation;
pub mod registry;

pub use client::{McpClient, McpError, McpOperation};
pub use handler::{McpDiscoveredHandler, McpHandler};
pub use pool::{McpClientPool, McpServerEntry};

pub(crate) mod messages;
