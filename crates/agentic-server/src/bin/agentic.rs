use std::{ffi::OsStr, io::IsTerminal, path::Path, process::ExitCode};

use agentic_core::error::Error;
use agentic_server::{
    agentic_cli::{AttachedHarnessCommand, Cli, Command, HarnessCommand},
    agentic_output::{colorize_help, redact_url, redact_urls, render_banner, render_help},
    agentic_process::{run_attached_harness, run_server, run_session, server_binary_path},
};
use clap::{Parser, error::ErrorKind};

#[tokio::main]
async fn main() -> Result<ExitCode, Error> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
            let help = colorize_help(&error.render().to_string(), color);
            println!("{}", render_help(&help, color));
            return Ok(ExitCode::SUCCESS);
        }
        Err(error) if error.kind() == ErrorKind::DisplayVersion => {
            print!("{}", error.render());
            return Ok(ExitCode::SUCCESS);
        }
        Err(error) => {
            eprint!("{}", redact_urls(&error.render().to_string()));
            return Ok(ExitCode::from(2));
        }
    };
    match cli.command {
        Command::Run { harness } => run_harness(harness).await,
        Command::Harness { harness } => attach_harness(harness).await,
        Command::Serve(options) => serve(options.source, options.common).await,
        Command::Validate(options) => validate(options).await,
    }
}

async fn attach_harness(harness: AttachedHarnessCommand) -> Result<ExitCode, Error> {
    let (selected, options) = match harness {
        AttachedHarnessCommand::Codex(options) => (agentic_server::agentic_cli::Harness::Codex, options),
        AttachedHarnessCommand::Claude(options) => (agentic_server::agentic_cli::Harness::Claude, options),
    };
    if !options.quiet {
        println!("{}", render_banner(!options.no_color));
        if options.no_color {
            println!("Starting {selected:?} via {}", redact_url(&options.gateway_url));
        } else {
            println!("\u{1b}[35mStarting {selected:?}\u{1b}[0m");
        }
    }
    let status = run_attached_harness(selected, options).await?;
    Ok(ExitCode::from(status.code().unwrap_or(1).try_into().unwrap_or(1)))
}

async fn run_harness(harness: HarnessCommand) -> Result<ExitCode, Error> {
    let selected = harness.harness();
    let options = harness.options().clone();
    let current_exe = std::env::current_exe()?;
    if !options.common.quiet {
        println!("{}", render_banner(!options.common.no_color));
        let gateway = format!("http://{}:{}", options.common.gateway_host, options.common.gateway_port);
        if options.common.no_color {
            println!("Starting {selected:?} via {}", redact_url(&gateway));
        } else {
            println!("\u{1b}[35mStarting {selected:?}\u{1b}[0m");
        }
    }
    let status = run_session(&current_exe, selected, options).await?;
    Ok(ExitCode::from(status.code().unwrap_or(1).try_into().unwrap_or(1)))
}

async fn serve(
    source: agentic_server::agentic_cli::SourceOptions,
    common: agentic_server::agentic_cli::CommonOptions,
) -> Result<ExitCode, Error> {
    let current_exe = std::env::current_exe()?;
    let status = run_server(&current_exe, &source, &common).await?;
    Ok(ExitCode::from(status.code().unwrap_or(1).try_into().unwrap_or(1)))
}

async fn validate(options: agentic_server::agentic_cli::ValidateOptions) -> Result<ExitCode, Error> {
    let current_exe = std::env::current_exe()?;
    let server_path = server_binary_path(&current_exe, &options.source);
    if !server_path.is_file() {
        return Err(Error::Config(format!(
            "gateway binary not found at {}",
            server_path.display()
        )));
    }
    let address = format!("{}:{}", options.common.gateway_host, options.common.gateway_port);
    let listener = tokio::net::TcpListener::bind(&address).await?;
    drop(listener);
    if !(options.common.database_url.starts_with("sqlite:") || options.common.database_url.starts_with("postgres")) {
        return Err(Error::Config(format!(
            "unsupported database URL {}; use sqlite:// or postgresql://",
            redact_url(&options.common.database_url)
        )));
    }
    agentic_core::storage::create_pool_with_schema(Some(&options.common.database_url))
        .await
        .map_err(|error| Error::Config(format!("database validation failed: {error}")))?;
    if let Some(harness) = options.harness {
        let binary_name = match harness {
            agentic_server::agentic_cli::Harness::Codex => "codex",
            agentic_server::agentic_cli::Harness::Claude => "claude",
        };
        let override_name = match harness {
            agentic_server::agentic_cli::Harness::Codex => "AGENTIC_CODEX_BIN",
            agentic_server::agentic_cli::Harness::Claude => "AGENTIC_CLAUDE_BIN",
        };
        let binary = std::env::var_os(override_name).unwrap_or_else(|| binary_name.into());
        if !executable_available(&binary) {
            return Err(Error::Config(format!(
                "{} not found; install it or set {override_name}",
                binary.to_string_lossy()
            )));
        }
    }
    if !options.common.quiet {
        println!("Gateway and harness prerequisites look valid.");
        if options.source.upstream.is_none() {
            println!("The native model recipe and engine are checked at startup.");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn executable_available(binary: &OsStr) -> bool {
    let path = Path::new(binary);
    if path.components().count() > 1 {
        path.is_file()
    } else {
        which(binary).is_some()
    }
}

fn which(binary: &OsStr) -> Option<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|path| path.join(binary))
        .find(|path| path.is_file())
}
