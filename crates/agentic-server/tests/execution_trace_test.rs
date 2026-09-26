//! Execution traces through the production HTTP and WebSocket router.

#[allow(dead_code)]
mod common;
#[path = "execution_traces/harness.rs"]
mod harness;
#[path = "execution_traces/proxy.rs"]
mod proxy;
#[path = "execution_traces/attributes.rs"]
mod trace_attributes;
#[path = "execution_traces/websocket.rs"]
mod websocket;
