//! Provider conformance through the shared executor replay harness.
#[path = "support/provider.rs"]
mod provider;
mod support;

use provider::{Provider, run_function_tool_call, run_stateful_two_turn};
const PROVIDER: Provider = Provider {
    directory: "dynamo",
    prefix: "dynamo",
    model_slug: "openai-gpt-oss-20b",
    version: None,
};

#[tokio::test]
async fn stateful_blocking() {
    run_stateful_two_turn(&PROVIDER, false).await;
}
#[tokio::test]
async fn stateful_streaming() {
    run_stateful_two_turn(&PROVIDER, true).await;
}
#[tokio::test]
async fn function_blocking() {
    run_function_tool_call(&PROVIDER, false).await;
}
#[tokio::test]
async fn function_streaming() {
    run_function_tool_call(&PROVIDER, true).await;
}
