use std::time::Duration;

pub mod app;
pub mod handler;

/// Maximum time allowed for in-flight requests and `WebSockets` to drain during shutdown.
pub const GATEWAY_DRAIN_TIMEOUT: Duration = Duration::from_secs(8);
