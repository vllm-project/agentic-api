//! Exercise native startup and process cleanup without a GPU or external model downloads.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command as StdCommand, Stdio};
use std::time::Duration;

use tokio::process::Command;

/// A local tokenizer is enough to reach the real engine handshake without a GPU or Hub downloads.
fn fixture(engine_script: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let model = home.path().join("model");
    fs::create_dir(&model).unwrap();
    fs::write(
        model.join("config.json"),
        r#"{"model_type":"llama","vocab_size":3,"bos_token_id":0,"eos_token_id":1,"max_position_embeddings":128}"#,
    )
    .unwrap();
    fs::write(model.join("tokenizer.json"), r#"{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],"normalizer":null,"pre_tokenizer":{"type":"Whitespace"},"post_processor":null,"decoder":null,"model":{"type":"WordLevel","vocab":{"<s>":0,"</s>":1,"hello":2},"unk_token":"</s>"}}"#).unwrap();
    fs::write(model.join("tokenizer_config.json"), r#"{"bos_token":"<s>","eos_token":"</s>","chat_template":"{% for message in messages %}{{ message.content }}{% endfor %}"}"#).unwrap();
    let python = home.path().join("python");
    fs::write(
        &python,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}/engine.pid'\n{engine_script}\n",
            home.path().display()
        ),
    )
    .unwrap();
    fs::set_permissions(&python, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        home.path().join("model.config"),
        format!(
            "model = {:?}\npython = {:?}\n",
            model.to_str().unwrap(),
            python.to_str().unwrap()
        ),
    )
    .unwrap();
    home
}

fn gateway(home: &tempfile::TempDir, timeout: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentic-server-native"));
    command
        .env("AGENTIC_API_HOME", home.path())
        .env_remove("LLM_API_BASE")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OIDC_ISSUER")
        .env_remove("OIDC_AUDIENCE")
        .args([
            "--db-url",
            "sqlite::memory:",
            "--gateway-port",
            "0",
            "--llm-ready-timeout-s",
            timeout,
        ])
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn assert_engine_stopped(home: &tempfile::TempDir) {
    let pid = fs::read_to_string(home.path().join("engine.pid")).expect("engine must have started");
    assert!(
        !StdCommand::new("kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success(),
        "engine {pid} survived startup cleanup"
    );
}

#[tokio::test]
async fn handshake_timeout_reaps_the_owned_engine() {
    let home = fixture("exec sleep 3");
    let output = tokio::time::timeout(Duration::from_secs(10), gateway(&home, "0.5").output())
        .await
        .unwrap()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(error.contains("timed out"), "{error}");
    assert_engine_stopped(&home);
}

#[tokio::test]
async fn early_engine_exit_interrupts_model_startup() {
    let home = fixture("exit 23");
    let output = tokio::time::timeout(Duration::from_secs(10), gateway(&home, "60").output())
        .await
        .unwrap()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(error.contains("LlmProcessExited") && error.contains("23"), "{error}");
    assert_engine_stopped(&home);
}

#[tokio::test]
async fn termination_during_startup_reaps_the_owned_engine() {
    let home = fixture("exec sleep 3");
    let child = gateway(&home, "60").spawn().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !home.path().join("engine.pid").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let status = Command::new("kill")
        .args(["-TERM", &child.id().unwrap().to_string()])
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_engine_stopped(&home);
}
