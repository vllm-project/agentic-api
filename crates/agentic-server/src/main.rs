use clap::{Parser, Subcommand};

use agentic_core::config::{Config, normalize_base_url};
use agentic_core::error::Error;

mod server;

#[derive(Parser)]
#[command(name = "agentic-api", about = "Stateful API gateway for vLLM Responses API")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(long)]
    llm_api_base: Option<String>,

    #[arg(long, env = "OPENAI_API_KEY", hide_env_values = true)]
    openai_api_key: Option<String>,

    #[arg(long, default_value = "0.0.0.0")]
    gateway_host: String,

    #[arg(long, default_value_t = 9000)]
    gateway_port: u16,

    #[arg(long, default_value_t = 600.0)]
    llm_ready_timeout_s: f64,

    #[arg(long, default_value_t = 2.0)]
    llm_ready_interval_s: f64,
}

#[derive(Subcommand)]
enum Commands {
    /// Spawn vLLM and run the gateway in the foreground
    Serve {
        /// Model name or path
        model: String,

        /// vLLM server port
        #[arg(long, default_value_t = 8000)]
        port: u16,

        #[arg(long, env = "OPENAI_API_KEY", hide_env_values = true)]
        openai_api_key: Option<String>,

        #[arg(long, default_value = "0.0.0.0")]
        gateway_host: String,

        #[arg(long, default_value_t = 9000)]
        gateway_port: u16,

        #[arg(long, default_value_t = 600.0)]
        llm_ready_timeout_s: f64,

        #[arg(long, default_value_t = 2.0)]
        llm_ready_interval_s: f64,

        /// Additional arguments passed through to vLLM
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        llm_args: Vec<String>,
    },
}

fn build_config(
    llm_api_base: String,
    openai_api_key: Option<String>,
    llm_ready_timeout_s: f64,
    llm_ready_interval_s: f64,
) -> Config {
    Config {
        llm_api_base,
        openai_api_key,
        llm_ready_timeout_s,
        llm_ready_interval_s,
    }
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agentic_server=info,agentic_core=info".parse().expect("valid filter")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        None => {
            let base = cli.llm_api_base.ok_or_else(|| {
                Error::Config(
                    "standalone mode requires --llm-api-base; use `agentic-api serve <model>` for integrated mode"
                        .to_owned(),
                )
            })?;
            let config = build_config(
                normalize_base_url(&base),
                cli.openai_api_key,
                cli.llm_ready_timeout_s,
                cli.llm_ready_interval_s,
            );
            server::run(config, &cli.gateway_host, cli.gateway_port).await
        }
        Some(Commands::Serve {
            model,
            port,
            openai_api_key,
            gateway_host,
            gateway_port,
            llm_ready_timeout_s,
            llm_ready_interval_s,
            llm_args,
        }) => {
            let config = build_config(
                normalize_base_url(&format!("http://127.0.0.1:{port}")),
                openai_api_key,
                llm_ready_timeout_s,
                llm_ready_interval_s,
            );

            let mut args = vec!["--model".to_owned(), model];
            args.push("--port".to_owned());
            args.push(port.to_string());
            args.extend(llm_args);

            server::run_with_llm(config, &gateway_host, gateway_port, args).await
        }
    }
}
