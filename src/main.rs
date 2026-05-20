use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use clap::Parser;
use tracing::info;

use agentic_api::config::RuntimeConfig;
use agentic_api::handler::{self, AppState};
use agentic_api::store::ogx::OgxStore;

#[derive(Parser)]
#[command(name = "agentic-api", about = "Stateful API gateway for vLLM Responses API")]
struct Cli {
    #[arg(long, default_value = "http://localhost:8000")]
    vllm_base_url: String,

    #[command(flatten)]
    runtime: RuntimeConfig,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();

    let client = reqwest::Client::new();
    let ogx_store = Arc::new(OgxStore::new(&cli.runtime.ogx_base_url, client.clone()));

    let state = Arc::new(AppState {
        vllm_base_url: cli.vllm_base_url,
        openai_api_key: cli.runtime.openai_api_key,
        max_iterations: cli.runtime.max_iterations,
        client,
        response_store: ogx_store.clone(),
        vector_search: ogx_store,
    });

    let app = Router::new()
        .route("/v1/responses", post(handler::handle_responses))
        .route("/health", get(handler::health))
        .with_state(state);

    let addr = format!("{}:{}", cli.runtime.gateway_host, cli.runtime.gateway_port);
    info!(%addr, "starting agentic-api gateway");

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
