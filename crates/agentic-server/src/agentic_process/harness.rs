//! Launch a coding harness: its binary, arguments, environment, and child process.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;

use agentic_core::error::Error;
use tokio::process::{Child, Command};

use crate::agentic_cli::{Harness, HarnessOptions};
use crate::agentic_harness::{CLAUDE_CANONICAL_MODEL, HarnessEnv, prepare_claude_home_with_state, prepare_codex_home};

use super::{HarnessModel, claude_effort};

const CLAUDE_TOOLS: &str = "Bash,Edit,Read,WebSearch";

const fn harness_binary_names(harness: Harness) -> (&'static str, &'static str) {
    match harness {
        Harness::Codex => ("codex", "AGENTIC_CODEX_BIN"),
        Harness::Claude => ("claude", "AGENTIC_CLAUDE_BIN"),
    }
}

pub(super) fn harness_binary(harness: Harness) -> OsString {
    let (binary_name, override_name) = harness_binary_names(harness);
    env::var_os(override_name).unwrap_or_else(|| binary_name.into())
}

pub(super) fn harness_launch_args(
    harness: Harness,
    yolo: bool,
    claude_effort: &str,
    managed: &[String],
    passthrough: &[String],
) -> Vec<String> {
    let mut args = Vec::with_capacity(managed.len() + passthrough.len() + 3);
    match harness {
        Harness::Codex => {
            if yolo {
                args.push("--dangerously-bypass-approvals-and-sandbox".to_owned());
            }
        }
        Harness::Claude => {
            if yolo {
                args.push("--dangerously-skip-permissions".to_owned());
            }
            args.extend([
                "--model".to_owned(),
                CLAUDE_CANONICAL_MODEL.to_owned(),
                "--tools".to_owned(),
                CLAUDE_TOOLS.to_owned(),
                "--setting-sources".to_owned(),
                "user".to_owned(),
            ]);
            args.extend(["--effort".to_owned(), claude_effort.to_owned()]);
        }
    }
    args.extend_from_slice(managed);
    args.extend_from_slice(passthrough);
    args
}

pub(super) fn validate_claude_passthrough(passthrough: &[String]) -> Result<(), Error> {
    const MANAGED_FLAGS: [&str; 4] = ["--model", "--settings", "--setting-sources", "--bare"];
    if let Some(argument) = passthrough.iter().find(|argument| {
        MANAGED_FLAGS
            .iter()
            .any(|flag| argument.as_str() == *flag || argument.starts_with(&format!("{flag}=")))
    }) {
        return Err(Error::Config(format!(
            "Claude Code argument {argument} cannot be forwarded because Agentic API manages model and setting isolation"
        )));
    }
    Ok(())
}

pub(super) fn harness_environment(
    harness_model: &HarnessModel,
    gateway_url: &str,
    options: &HarnessOptions,
    session_root: &Path,
    claude_state_root: &Path,
) -> Result<HarnessEnv, Error> {
    let inherited_auth_token = env::var("ANTHROPIC_AUTH_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let mut environment = match harness_model {
        HarnessModel::Codex(selection) => prepare_codex_home(
            session_root,
            gateway_url,
            &selection.model,
            &selection.capabilities,
            options.common.api_key.as_deref(),
        )
        .map_err(Error::from),
        HarnessModel::Claude(model) => prepare_claude_home_with_state(
            session_root,
            claude_state_root,
            gateway_url,
            model,
            inherited_auth_token.as_deref(),
            options.common.api_key.as_deref(),
        )
        .map_err(Error::from),
    }?;
    if matches!(harness_model, HarnessModel::Claude(_)) {
        // Claude Code gives CLAUDE_CODE_EFFORT_LEVEL precedence over --effort, so set both
        // to keep an inherited `high` from reaching the Qwen chat template.
        environment
            .environment
            .insert("CLAUDE_CODE_EFFORT_LEVEL".to_owned(), claude_effort());
    }
    Ok(environment)
}

pub(super) fn spawn_harness(
    harness: Harness,
    yolo: bool,
    passthrough: &[String],
    harness_env: &HarnessEnv,
) -> Result<Child, Error> {
    let (binary_name, override_name) = harness_binary_names(harness);
    let binary = harness_binary(harness);
    let mut harness_command = build_harness_command(&binary, harness, yolo, passthrough, harness_env);
    harness_command
        .spawn()
        .map_err(|error| Error::Config(format!("failed to launch {binary_name} ({override_name}): {error}")))
}

pub(super) fn build_harness_command(
    binary: &OsStr,
    harness: Harness,
    yolo: bool,
    passthrough: &[String],
    harness_env: &HarnessEnv,
) -> Command {
    let mut harness_command = Command::new(binary);
    harness_command.kill_on_drop(true);
    harness_command.args(harness_launch_args(
        harness,
        yolo,
        &claude_effort(),
        &harness_env.arguments,
        passthrough,
    ));
    for name in &harness_env.environment_remove {
        harness_command.env_remove(name);
    }
    harness_command.envs(&harness_env.environment);
    harness_command.stdin(Stdio::inherit());
    harness_command.stdout(Stdio::inherit());
    harness_command.stderr(Stdio::inherit());
    harness_command
}

pub(super) fn prepare_attached_harness_environment(
    harness_model: &HarnessModel,
    session_root: &Path,
    claude_state_root: &Path,
    gateway_url: &str,
    api_key: Option<&str>,
) -> Result<HarnessEnv, Error> {
    match harness_model {
        HarnessModel::Codex(selection) => prepare_codex_home(
            session_root,
            gateway_url,
            &selection.model,
            &selection.capabilities,
            api_key,
        )
        .map_err(Error::from),
        HarnessModel::Claude(model) => {
            prepare_claude_home_with_state(session_root, claude_state_root, gateway_url, model, None, api_key)
                .map_err(Error::from)
        }
    }
}
