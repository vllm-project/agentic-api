use clap::{Args, Parser, Subcommand};

use agentic_core::config::{Config, normalize_base_url};
use agentic_core::error::Error;

mod server;

#[derive(Args, Clone)]
struct CommonArgs {
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

#[derive(Parser)]
#[command(name = "agentic-server", about = "Stateful API gateway for vLLM Responses API")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(long)]
    llm_api_base: Option<String>,

    #[command(flatten)]
    common: CommonArgs,
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

        #[command(flatten)]
        common: CommonArgs,

        /// Additional arguments passed through to vLLM
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        llm_args: Vec<String>,
    },
}

fn build_config(llm_api_base: String, common: &CommonArgs) -> Config {
    Config {
        llm_api_base,
        openai_api_key: common.openai_api_key.clone(),
        llm_ready_timeout_s: common.llm_ready_timeout_s,
        llm_ready_interval_s: common.llm_ready_interval_s,
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
                    "standalone mode requires --llm-api-base; use `agentic-server serve <model>` for integrated mode"
                        .to_owned(),
                )
            })?;
            let config = build_config(normalize_base_url(&base), &cli.common);
            server::run(config, &cli.common.gateway_host, cli.common.gateway_port).await
        }
        Some(Commands::Serve {
            model,
            port,
            common,
            llm_args,
        }) => {
            let config = build_config(normalize_base_url(&format!("http://127.0.0.1:{port}")), &common);

            let mut args = vec!["--model".to_owned(), model];
            args.push("--port".to_owned());
            args.push(port.to_string());
            args.extend(llm_args);

            server::run_with_llm(config, &common.gateway_host, common.gateway_port, args).await
        }
    }
}
