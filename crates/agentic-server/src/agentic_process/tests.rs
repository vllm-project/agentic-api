use std::ffi::OsString;

use super::{CodexModelSelection, DEFAULT_CLAUDE_EFFORT, HarnessModel, harness_launch_args, server_args};
use crate::agentic_cli::{CommonOptions, Harness, SourceOptions};
use agentic_core::types::messages::ReasoningEffortLevel;

use crate::model_capabilities::{CodexModelCapabilities, InputModalities};

/// A gateway that answers catalog requests from a scripted queue and records what it was asked.
struct MockGateway {
    url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl MockGateway {
    fn request_count(&self) -> usize {
        self.requests.lock().expect("request log").len()
    }

    fn first_request(&self) -> String {
        self.requests
            .lock()
            .expect("request log")
            .first()
            .cloned()
            .unwrap_or_default()
    }
}

/// Serve `responses` in order, repeating the last one once the queue is exhausted.
async fn spawn_mock_gateway(responses: Vec<(&'static str, String)>) -> MockGateway {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("listener address");
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = std::sync::Arc::clone(&requests);
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut served = 0;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0_u8; 4096];
            let read = socket.read(&mut buffer).await.unwrap_or_default();
            recorded
                .lock()
                .expect("request log")
                .push(String::from_utf8_lossy(&buffer[..read]).into_owned());
            let (status, body) = responses
                .get(served)
                .or_else(|| responses.last())
                .cloned()
                .unwrap_or(("200 OK", String::new()));
            served += 1;
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    MockGateway {
        url: format!("http://{address}"),
        requests,
    }
}

fn catalog_body() -> String {
    r#"{"models":[
        {"slug":"first-model","input_modalities":["text"]},
        {"slug":"vision-model","input_modalities":["text","image"]}
    ]}"#
    .to_owned()
}

async fn select(
    gateway: &MockGateway,
    requested_model: Option<&str>,
    api_key: Option<&str>,
) -> Result<super::CodexModelSelection, agentic_core::error::Error> {
    super::catalog_selection(
        &reqwest::Client::new(),
        &gateway.url,
        "9.9.9",
        requested_model,
        api_key,
        super::CatalogBudget {
            timeout: std::time::Duration::from_millis(400),
            interval: std::time::Duration::from_millis(10),
            missing_grace: super::CATALOG_MODEL_GRACE,
        },
    )
    .await
}

#[tokio::test]
async fn catalog_selection_reads_the_resolved_modalities() {
    let gateway = spawn_mock_gateway(vec![("200 OK", catalog_body())]).await;

    let selection = select(&gateway, Some("vision-model"), None)
        .await
        .expect("the catalog lists the requested model");

    assert_eq!(selection.model, "vision-model");
    assert_eq!(selection.capabilities.input_modalities, InputModalities::TextAndImage);
    let request = gateway.first_request();
    assert!(
        request.starts_with("GET /v1/models?client_version=9.9.9 "),
        "the gateway only transforms its catalog for a client version: {request}"
    );
    assert!(
        !request.to_ascii_lowercase().contains("authorization:"),
        "no credential must be sent when none is configured"
    );
}

fn catalog_body_with_reasoning_levels() -> String {
    r#"{"models":[
        {"slug":"glm-model","input_modalities":["text"],
         "supported_reasoning_levels":[{"effort":"low","description":"Fast"},{"effort":"max","description":"Deepest"}]}
    ]}"#
    .to_owned()
}

#[tokio::test]
async fn catalog_selection_reads_the_advertised_reasoning_levels() {
    let gateway = spawn_mock_gateway(vec![("200 OK", catalog_body_with_reasoning_levels())]).await;

    let selection = select(&gateway, Some("glm-model"), None)
        .await
        .expect("the catalog lists the requested model");

    let levels = selection.capabilities.reasoning_levels();
    assert_eq!(
        levels.iter().map(|level| level.effort).collect::<Vec<_>>(),
        [ReasoningEffortLevel::Low, ReasoningEffortLevel::Max],
        "the launcher keeps the order the gateway advertised"
    );
    assert_eq!(levels[1].description, "Deepest");
    assert_eq!(
        selection.capabilities.default_reasoning_level(),
        ReasoningEffortLevel::Low,
        "a catalog without medium starts on its first level"
    );
}

#[tokio::test]
async fn catalog_selection_falls_back_to_the_default_reasoning_levels() {
    let gateway = spawn_mock_gateway(vec![("200 OK", catalog_body())]).await;

    let selection = select(&gateway, Some("first-model"), None)
        .await
        .expect("the catalog lists the requested model");

    assert_eq!(
        selection.capabilities.reasoning_levels(),
        crate::model_capabilities::default_codex_reasoning_levels(),
        "a gateway without advertised levels keeps the default catalog"
    );
    assert_eq!(
        selection.capabilities.default_reasoning_level(),
        ReasoningEffortLevel::Medium
    );
}

#[tokio::test]
async fn catalog_selection_defaults_to_the_first_advertised_model() {
    let gateway = spawn_mock_gateway(vec![("200 OK", catalog_body())]).await;

    let selection = select(&gateway, None, None).await.expect("a catalog entry is selected");

    assert_eq!(selection.model, "first-model");
    assert_eq!(selection.capabilities.input_modalities, InputModalities::Text);
}

#[tokio::test]
async fn catalog_selection_sends_the_configured_credential() {
    let gateway = spawn_mock_gateway(vec![("200 OK", catalog_body())]).await;

    select(&gateway, Some("first-model"), Some("gateway-key"))
        .await
        .expect("the catalog lists the requested model");

    assert!(
        gateway
            .first_request()
            .to_ascii_lowercase()
            .contains("authorization: bearer gateway-key"),
        "the configured API key must reach a protected gateway"
    );
}

#[tokio::test]
async fn catalog_selection_reports_a_model_the_gateway_does_not_serve() {
    let gateway = spawn_mock_gateway(vec![("200 OK", catalog_body())]).await;

    let error = select(&gateway, Some("absent-model"), None)
        .await
        .expect_err("a model the gateway does not serve must fail");
    let message = error.to_string();

    assert!(message.contains("absent-model"), "{message}");
    assert!(message.contains("first-model, vision-model"), "{message}");
}

#[tokio::test]
async fn catalog_selection_does_not_retry_rejected_credentials() {
    let gateway = spawn_mock_gateway(vec![("401 Unauthorized", "{}".to_owned())]).await;

    let error = select(&gateway, Some("first-model"), None)
        .await
        .expect_err("a rejected credential must fail");
    let message = error.to_string();

    assert!(message.contains("401"), "{message}");
    assert!(message.contains("--api-key"), "{message}");
    assert_eq!(
        gateway.request_count(),
        1,
        "authentication failures must not be retried"
    );
}

#[tokio::test]
async fn catalog_selection_retries_a_warming_gateway() {
    let gateway = spawn_mock_gateway(vec![
        ("503 Service Unavailable", "{}".to_owned()),
        ("200 OK", catalog_body()),
    ])
    .await;

    let selection = select(&gateway, Some("vision-model"), None)
        .await
        .expect("a warming gateway must be retried");

    assert_eq!(selection.capabilities.input_modalities, InputModalities::TextAndImage);
    assert_eq!(gateway.request_count(), 2);
}

#[tokio::test]
async fn catalog_selection_retries_an_empty_catalog() {
    let gateway = spawn_mock_gateway(vec![
        ("200 OK", r#"{"models":[]}"#.to_owned()),
        ("200 OK", catalog_body()),
    ])
    .await;

    let selection = select(&gateway, None, None)
        .await
        .expect("an upstream that is still loading must be retried");

    assert_eq!(selection.model, "first-model");
    assert_eq!(gateway.request_count(), 2);
}

#[tokio::test]
async fn catalog_selection_starts_the_missing_model_grace_at_the_first_miss() {
    let warming = ("503 Service Unavailable", "{}".to_owned());
    let without_the_model = (
        "200 OK",
        r#"{"models":[{"slug":"other-model","input_modalities":["text"]}]}"#.to_owned(),
    );
    let gateway = spawn_mock_gateway(vec![
        warming.clone(),
        warming.clone(),
        warming.clone(),
        warming.clone(),
        warming.clone(),
        warming,
        without_the_model,
        ("200 OK", catalog_body()),
    ])
    .await;

    // Warm-up alone outlasts the grace: six retries at 30ms exceed the 150ms window, so a
    // grace anchored at the first attempt would already have expired by the first miss.
    let selection = super::catalog_selection(
        &reqwest::Client::new(),
        &gateway.url,
        "9.9.9",
        Some("vision-model"),
        None,
        super::CatalogBudget {
            timeout: std::time::Duration::from_secs(3),
            interval: std::time::Duration::from_millis(30),
            missing_grace: std::time::Duration::from_millis(150),
        },
    )
    .await
    .expect("a slow warm-up must not consume the model-missing grace");

    assert_eq!(selection.model, "vision-model");
    assert_eq!(selection.capabilities.input_modalities, InputModalities::TextAndImage);
    assert_eq!(
        gateway.request_count(),
        8,
        "every warm-up response, the miss, and the successful catalog must each be requested"
    );
}

#[tokio::test]
async fn catalog_selection_rejects_an_undecodable_catalog() {
    let gateway = spawn_mock_gateway(vec![("200 OK", "not a catalog".to_owned())]).await;

    let error = select(&gateway, Some("first-model"), None)
        .await
        .expect_err("an undecodable catalog must fail");

    assert!(error.to_string().contains("not a Codex model catalog"), "{error}");
    assert_eq!(gateway.request_count(), 1, "an undecodable catalog must not be retried");
}

#[tokio::test]
async fn catalog_selection_rejects_an_oversized_catalog() {
    let oversized = format!(
        r#"{{"models":[{{"slug":"{}","input_modalities":["text"]}}]}}"#,
        "x".repeat(super::MAX_CATALOG_BYTES + 1)
    );
    let gateway = spawn_mock_gateway(vec![("200 OK", oversized)]).await;

    let error = select(&gateway, Some("first-model"), None)
        .await
        .expect_err("an oversized catalog must fail");

    assert!(error.to_string().contains("larger than"), "{error}");
    assert_eq!(gateway.request_count(), 1, "an oversized catalog must not be retried");
}

#[tokio::test]
async fn catalog_selection_redacts_gateway_credentials_when_unreachable() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("listener address");
    drop(listener);

    let error = super::catalog_selection(
        &reqwest::Client::new(),
        &format!("http://agentic:gateway-secret@{address}"),
        "9.9.9",
        Some("first-model"),
        None,
        super::CatalogBudget {
            timeout: std::time::Duration::from_millis(50),
            interval: std::time::Duration::from_millis(10),
            missing_grace: super::CATALOG_MODEL_GRACE,
        },
    )
    .await
    .expect_err("an unreachable gateway must fail");
    let message = error.to_string();

    assert!(!message.contains("gateway-secret"), "{message}");
    assert!(message.contains("[REDACTED]"), "{message}");
}

#[test]
fn integrated_mode_builds_server_arguments() {
    let args = server_args(
        &SourceOptions {
            upstream: None,
            model: Some("Qwen/test".to_owned()),
            llm_port: 8000,
        },
        &CommonOptions::default(),
    );
    let args: Vec<_> = args.iter().map(OsString::as_os_str).collect();

    assert_eq!(args[0], "serve");
    assert_eq!(args[1], "Qwen/test");
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--db-url", "sqlite://./agentic_api.db"])
    );
}

#[test]
fn request_size_limit_is_forwarded_only_when_configured() {
    let source = SourceOptions {
        upstream: Some("http://127.0.0.1:8000".to_owned()),
        model: None,
        llm_port: 8000,
    };

    let defaults = server_args(&source, &CommonOptions::default());
    assert!(
        !defaults
            .iter()
            .any(|argument| argument == "--max-request-body-size-bytes")
    );

    let configured = server_args(
        &source,
        &CommonOptions {
            max_request_body_size_bytes: std::num::NonZeroUsize::new(4_096),
            ..CommonOptions::default()
        },
    );
    let configured: Vec<_> = configured.iter().map(OsString::as_os_str).collect();
    assert!(
        configured
            .windows(2)
            .any(|pair| pair == ["--max-request-body-size-bytes", "4096"])
    );
}

#[test]
fn standalone_mode_builds_upstream_arguments() {
    let args = server_args(
        &SourceOptions {
            upstream: Some("http://127.0.0.1:8000".to_owned()),
            model: None,
            llm_port: 8000,
        },
        &CommonOptions::default(),
    );
    let args: Vec<_> = args.iter().map(OsString::as_os_str).collect();

    assert_eq!(args[0], "--llm-api-base");
    assert_eq!(args[1], "http://127.0.0.1:8000");
}

#[test]
fn explicit_upstream_wins_when_model_names_the_harness_model() {
    let args = server_args(
        &SourceOptions {
            upstream: Some("http://127.0.0.1:8000".to_owned()),
            model: Some("Qwen/test".to_owned()),
            llm_port: 8000,
        },
        &CommonOptions::default(),
    );
    let args: Vec<_> = args.iter().map(OsString::as_os_str).collect();

    assert_eq!(args[0], "--llm-api-base");
    assert!(!args.iter().any(|arg| *arg == "serve"));
}

#[test]
fn yolo_mode_uses_native_codex_bypass_flag() {
    assert_eq!(
        harness_launch_args(Harness::Codex, true, DEFAULT_CLAUDE_EFFORT, &[], &["exec".to_owned()]),
        ["--dangerously-bypass-approvals-and-sandbox", "exec"]
    );
}

#[test]
fn yolo_mode_uses_native_claude_bypass_and_compatible_effort() {
    assert_eq!(
        harness_launch_args(Harness::Claude, true, DEFAULT_CLAUDE_EFFORT, &[], &[]),
        [
            "--dangerously-skip-permissions",
            "--model",
            "claude-sonnet-4-5-20250929",
            "--tools",
            "Bash,Edit,Read,WebSearch",
            "--setting-sources",
            "user",
            "--effort",
            "medium"
        ]
    );
}

#[test]
fn claude_always_receives_a_compatible_effort() {
    assert_eq!(
        harness_launch_args(Harness::Claude, false, "low", &[], &["-p".to_owned(), "hi".to_owned()]),
        [
            "--model",
            "claude-sonnet-4-5-20250929",
            "--tools",
            "Bash,Edit,Read,WebSearch",
            "--setting-sources",
            "user",
            "--effort",
            "low",
            "-p",
            "hi"
        ]
    );
    assert_eq!(
        harness_launch_args(Harness::Codex, false, DEFAULT_CLAUDE_EFFORT, &[], &[]),
        Vec::<String>::new()
    );
}

#[test]
fn claude_environment_pins_effort_without_yolo() {
    let options = crate::agentic_cli::HarnessOptions {
        source: SourceOptions {
            upstream: Some("http://127.0.0.1:8000".to_owned()),
            model: None,
            llm_port: 8000,
        },
        common: CommonOptions::default(),
        harness_args: Vec::new(),
    };
    let root = std::env::temp_dir().join(format!("agentic-api-effort-test-{}", std::process::id()));
    let state_root = std::env::temp_dir().join(format!("agentic-api-state-test-{}", std::process::id()));
    let environment = super::harness_environment(
        &HarnessModel::Claude("served-discovered".to_owned()),
        "http://127.0.0.1:3000",
        &options,
        &root,
        &state_root,
    )
    .expect("Claude environment");

    assert_eq!(
        environment.environment.get("CLAUDE_CODE_EFFORT_LEVEL"),
        Some(&DEFAULT_CLAUDE_EFFORT.to_owned())
    );
    assert_eq!(
        environment.environment.get("CLAUDE_CONFIG_DIR"),
        Some(&state_root.display().to_string())
    );
    assert!(root.join("settings.json").is_file());
    std::fs::remove_dir_all(root).expect("cleanup");
    std::fs::remove_dir_all(state_root).expect("state cleanup");
}

#[test]
fn integrated_claude_preserves_inherited_auth_token() {
    const CHILD_MARKER: &str = "AGENTIC_TEST_INTEGRATED_CLAUDE_AUTH";
    if std::env::var_os(CHILD_MARKER).is_some() {
        let settings_root = tempfile::tempdir().expect("settings root");
        let state_root = tempfile::tempdir().expect("state root");
        let options = crate::agentic_cli::HarnessOptions {
            source: SourceOptions {
                upstream: Some("http://127.0.0.1:8000".to_owned()),
                model: None,
                llm_port: 8000,
            },
            common: CommonOptions::default(),
            harness_args: Vec::new(),
        };
        let environment = super::harness_environment(
            &HarnessModel::Claude("served-discovered".to_owned()),
            "http://127.0.0.1:3000",
            &options,
            settings_root.path(),
            state_root.path(),
        )
        .expect("Claude environment");

        assert_eq!(
            environment.environment.get("ANTHROPIC_AUTH_TOKEN"),
            Some(&"oidc-token".to_owned())
        );
        return;
    }

    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "agentic_process::tests::integrated_claude_preserves_inherited_auth_token",
        ])
        .env(CHILD_MARKER, "1")
        .env("ANTHROPIC_AUTH_TOKEN", "oidc-token")
        .status()
        .expect("run isolated integrated auth test");

    assert!(status.success(), "isolated integrated auth test failed with {status}");
}

#[test]
fn claude_rejects_passthrough_that_can_replace_isolated_configuration() {
    for argument in [
        "--model",
        "--model=opus",
        "--settings",
        "--settings={}",
        "--setting-sources",
        "--setting-sources=project",
        "--bare",
    ] {
        let error = super::validate_claude_passthrough(&[argument.to_owned()])
            .expect_err("configuration-owning argument should be rejected");
        assert!(error.to_string().contains(argument));
    }
    super::validate_claude_passthrough(&["-p".to_owned(), "hello".to_owned()]).expect("normal passthrough arguments");
}

#[test]
fn attached_codex_uses_an_isolated_responses_provider() {
    let root = std::env::temp_dir().join(format!("agentic-api-attached-codex-test-{}", std::process::id()));
    let environment = super::prepare_attached_harness_environment(
        &HarnessModel::Codex(CodexModelSelection {
            model: "Qwen/Qwen3-8B".to_owned(),
            capabilities: CodexModelCapabilities {
                slug: "vision-model".to_owned(),
                input_modalities: InputModalities::TextAndImage,
                supported_reasoning_levels: Vec::new(),
            },
        }),
        &root,
        &root,
        "http://127.0.0.1:9000",
        None,
    )
    .expect("Codex environment");
    let config = std::fs::read_to_string(root.join("config.toml")).expect("Codex config");

    assert_eq!(
        environment.environment.get("CODEX_HOME"),
        Some(&root.display().to_string())
    );
    assert!(!environment.environment.contains_key("CLAUDE_CONFIG_DIR"));
    assert!(config.contains("model = \"Qwen/Qwen3-8B\""));
    assert!(config.contains("base_url = \"http://127.0.0.1:9000/v1\""));
    assert!(config.contains("wire_api = \"responses\""));
    let catalog: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("model_catalog.json")).expect("Codex catalog"))
            .expect("valid catalog JSON");
    assert_eq!(
        catalog["models"][0]["input_modalities"],
        serde_json::json!(["text", "image"]),
        "the attached launcher must write the modalities the gateway resolved"
    );

    std::fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn attached_claude_uses_persistent_state_root() {
    let session_root = tempfile::tempdir().expect("session root");
    let state_root = tempfile::tempdir().expect("state root");
    let environment = super::prepare_attached_harness_environment(
        &HarnessModel::Claude("Qwen/Qwen3-8B".to_owned()),
        session_root.path(),
        state_root.path(),
        "http://127.0.0.1:9000",
        None,
    )
    .expect("Claude environment");

    assert_eq!(
        environment.environment.get("CLAUDE_CONFIG_DIR"),
        Some(&state_root.path().display().to_string())
    );
}

#[test]
fn attached_claude_ignores_inherited_anthropic_auth_token() {
    const CHILD_MARKER: &str = "AGENTIC_TEST_ATTACHED_CLAUDE_AUTH";
    if std::env::var_os(CHILD_MARKER).is_some() {
        let session_root = tempfile::tempdir().expect("session root");
        let state_root = tempfile::tempdir().expect("state root");
        let environment = super::prepare_attached_harness_environment(
            &HarnessModel::Claude("Qwen/Qwen3-8B".to_owned()),
            session_root.path(),
            state_root.path(),
            "http://127.0.0.1:9000",
            None,
        )
        .expect("Claude environment");

        assert_eq!(
            environment.environment.get("ANTHROPIC_AUTH_TOKEN"),
            Some(&"agentic-api-local".to_owned())
        );
        return;
    }

    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "agentic_process::tests::attached_claude_ignores_inherited_anthropic_auth_token",
        ])
        .env(CHILD_MARKER, "1")
        .env("ANTHROPIC_AUTH_TOKEN", "must-not-be-forwarded")
        .status()
        .expect("run isolated attached auth test");

    assert!(status.success(), "isolated attached auth test failed with {status}");
}

#[cfg(unix)]
#[test]
fn session_root_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let root = super::create_session_root("agentic-api-private-test").expect("private session root");
    let mode = std::fs::metadata(root.path())
        .expect("session root metadata")
        .permissions()
        .mode()
        & 0o777;

    assert_eq!(mode, 0o700);

    std::fs::remove_dir_all(root.path()).expect("cleanup");
}

#[test]
fn session_root_is_removed_when_its_guard_drops() {
    let path = {
        let root = super::create_session_root("agentic-api-drop-test").expect("session root");
        root.path().to_owned()
    };

    let removed = !path.exists();
    if !removed {
        std::fs::remove_dir_all(&path).expect("cleanup failed session root");
    }
    assert!(removed, "session root survived its guard");
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_harness_child_prevents_later_side_effects() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let marker = temporary.path().join("child-survived");
    let environment = crate::agentic_harness::HarnessEnv {
        environment: std::collections::BTreeMap::new(),
        environment_remove: Vec::new(),
        arguments: Vec::new(),
        files: Vec::new(),
        summary: String::new(),
    };
    let passthrough = vec![
        "-c".to_owned(),
        "sleep 0.15; printf survived > \"$1\"".to_owned(),
        "agentic-test".to_owned(),
        marker.display().to_string(),
    ];
    let mut command = super::build_harness_command(
        std::ffi::OsStr::new("/bin/sh"),
        Harness::Codex,
        false,
        &passthrough,
        &environment,
    );
    let child = command.spawn().expect("test child");

    drop(child);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    assert!(!marker.exists(), "dropped harness child continued running");
}

#[tokio::test]
async fn resolve_model_does_not_double_the_v1_suffix() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0_u8; 1024];
        let read = socket.read(&mut buffer).await.unwrap();
        let request_line = String::from_utf8_lossy(&buffer[..read])
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned();
        let body = r#"{"data":[{"id":"Qwen/served"}]}"#;
        let status = if request_line.starts_with("GET /v1/models ") {
            "200 OK"
        } else {
            "404 Not Found"
        };
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    let client = reqwest::Client::new();
    let source = SourceOptions {
        upstream: Some(format!("http://{address}/v1")),
        model: None,
        llm_port: 8000,
    };
    assert_eq!(
        super::resolve_model(&client, &source, None).await.unwrap(),
        "Qwen/served"
    );
}

#[tokio::test]
async fn resolve_model_prefers_explicit_model() {
    let client = reqwest::Client::new();
    let source = SourceOptions {
        upstream: Some("http://127.0.0.1:9".to_owned()),
        model: Some("Qwen/test".to_owned()),
        llm_port: 8000,
    };
    assert_eq!(super::resolve_model(&client, &source, None).await.unwrap(), "Qwen/test");
}

#[tokio::test]
async fn readiness_timeout_redacts_gateway_password() {
    let error = super::wait_for_gateway(
        &reqwest::Client::new(),
        "https://agentic:gateway-secret@example.com",
        std::time::Duration::ZERO,
        std::time::Duration::from_millis(1),
        false,
    )
    .await
    .expect_err("readiness should time out");
    let message = error.to_string();

    assert!(!message.contains("gateway-secret"));
    assert!(message.contains("https://[REDACTED]@example.com"));
}

#[tokio::test(start_paused = true)]
async fn readiness_timeout_bounds_retry_sleep() {
    let timeout = std::time::Duration::from_secs(1);
    let started = tokio::time::Instant::now();
    // Make probe failure immediate so this test measures only the retry timer.
    let result = super::wait_for_gateway(
        &super::gateway_client().expect("gateway client"),
        "http://[invalid",
        timeout,
        std::time::Duration::from_secs(60),
        false,
    )
    .await;

    assert!(result.is_err());
    assert_eq!(started.elapsed(), timeout, "retry sleep exceeded the readiness budget");
}

async fn check_readiness_timeout_interrupts_probe(stalled_path: &'static str, skip_llm_ready_check: bool) {
    use axum::{Router, http::StatusCode, routing::get};

    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut application = Router::new();
    for path in ["/health", "/ready"] {
        let started_tx = started_tx.clone();
        application = application.route(
            path,
            get(move || async move {
                if path == stalled_path {
                    started_tx.send(()).expect("probe observer");
                    std::future::pending::<()>().await;
                }
                StatusCode::OK
            }),
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let gateway_url = format!("http://{}", listener.local_addr().expect("listener address"));
    let server = tokio::spawn(async move { axum::serve(listener, application).await.expect("test server") });
    let timeout = std::time::Duration::from_secs(60);
    let client = reqwest::Client::builder()
        .timeout(timeout * 2)
        .build()
        .expect("gateway client");
    let readiness = super::wait_for_gateway(
        &client,
        &gateway_url,
        timeout,
        std::time::Duration::from_secs(1),
        skip_llm_ready_check,
    );
    tokio::pin!(readiness);
    tokio::select! {
        started = started_rx.recv() => started.expect("stalled probe must reach the HTTP server"),
        result = &mut readiness => panic!("readiness finished before the stalled probe: {result:?}"),
    }

    // Start virtual time only after real HTTP I/O reaches the blocked endpoint.
    tokio::time::pause();
    tokio::time::advance(timeout).await;
    // Allow the runtime to process the expired timer without reaching the request timeout.
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), readiness).await;
    server.abort();

    assert!(
        matches!(result, Ok(Err(agentic_core::error::Error::Config(_)))),
        "readiness remained blocked in {stalled_path} after its deadline: {result:?}"
    );
}

#[tokio::test]
async fn readiness_timeout_interrupts_health_probe() {
    check_readiness_timeout_interrupts_probe("/health", false).await;
}

#[tokio::test]
async fn readiness_timeout_interrupts_upstream_probe() {
    check_readiness_timeout_interrupts_probe("/ready", false).await;
}

#[tokio::test]
async fn readiness_timeout_still_bounds_health_when_upstream_probe_is_skipped() {
    check_readiness_timeout_interrupts_probe("/health", true).await;
}

#[tokio::test]
async fn readiness_rejects_success_observed_after_deadline() {
    use axum::{Router, routing::get};

    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
    let application = Router::new().route("/health", get(|| async {})).route(
        "/ready",
        get(move || async move { ready_tx.send(()).expect("probe observer") }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let gateway_url = format!("http://{}", listener.local_addr().expect("listener address"));
    let server = tokio::spawn(async move { axum::serve(listener, application).await.expect("test server") });
    let timeout = std::time::Duration::from_secs(60);
    let client = reqwest::Client::builder()
        .timeout(timeout * 2)
        .build()
        .expect("gateway client");
    let readiness = super::wait_for_gateway(&client, &gateway_url, timeout, timeout, false);
    tokio::pin!(readiness);
    tokio::select! {
        biased;
        ready = ready_rx.recv() => ready.expect("both probes must reach the HTTP server"),
        result = &mut readiness => panic!("readiness finished before the final response: {result:?}"),
    }

    tokio::time::pause();
    tokio::time::advance(timeout).await;
    tokio::time::resume();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), readiness)
        .await
        .expect("readiness must finish after its deadline");
    server.abort();

    assert!(result.is_err(), "successful probes bypassed the readiness deadline");
}

#[tokio::test]
async fn readiness_accepts_healthy_gateway_and_skips_only_upstream_probe() {
    use axum::{Router, routing::get};

    for skip_llm_ready_check in [false, true] {
        let (probe_tx, mut probe_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut application = Router::new();
        for path in ["/health", "/ready"] {
            let probe_tx = probe_tx.clone();
            application = application.route(
                path,
                get(move || async move { probe_tx.send(path).expect("probe observer") }),
            );
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let gateway_url = format!("http://{}", listener.local_addr().expect("listener address"));
        let server = tokio::spawn(async move { axum::serve(listener, application).await.expect("test server") });
        let result = super::wait_for_gateway(
            &super::gateway_client().expect("gateway client"),
            &gateway_url,
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(1),
            skip_llm_ready_check,
        )
        .await;
        server.abort();

        assert!(result.is_ok());
        assert_eq!(probe_rx.try_recv(), Ok("/health"));
        if !skip_llm_ready_check {
            assert_eq!(probe_rx.try_recv(), Ok("/ready"));
        }
        assert!(probe_rx.try_recv().is_err(), "unexpected readiness probe");
    }
}

#[tokio::test]
async fn empty_model_listing_error_redacts_upstream_userinfo() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("listener address");
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut socket, _) = listener.accept().await.expect("connection");
        let mut buffer = [0_u8; 1024];
        let _ = socket.read(&mut buffer).await;
        let body = r#"{"data":[]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.expect("response");
    });
    let source = SourceOptions {
        upstream: Some(format!("http://secret-token@{address}")),
        model: None,
        llm_port: 8000,
    };

    let error = super::resolve_model(&reqwest::Client::new(), &source, None)
        .await
        .expect_err("empty model listing should fail");
    let message = error.to_string();

    assert!(!message.contains("secret-token"));
    assert!(message.contains("http://[REDACTED]@"));
}

#[tokio::test]
async fn readiness_rejects_redirected_login_pages() {
    use axum::{Router, response::Redirect, routing::get};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("listener address");
    let login_visits = Arc::new(AtomicUsize::new(0));
    let login_visits_for_route = Arc::clone(&login_visits);
    let application = Router::new()
        .route("/health", get(|| async { Redirect::temporary("/login") }))
        .route("/ready", get(|| async { Redirect::temporary("/login") }))
        .route(
            "/login",
            get(move || {
                login_visits_for_route.fetch_add(1, Ordering::SeqCst);
                async { "sign in" }
            }),
        );
    let server = tokio::spawn(async move {
        axum::serve(listener, application).await.expect("test server");
    });

    let result = super::wait_for_gateway(
        &super::gateway_client().expect("gateway client"),
        &format!("http://{address}"),
        std::time::Duration::from_millis(100),
        std::time::Duration::from_millis(1),
        false,
    )
    .await;
    server.abort();

    assert!(result.is_err(), "redirected login page passed readiness");
    assert_eq!(login_visits.load(Ordering::SeqCst), 0, "readiness followed a redirect");
}

#[tokio::test]
async fn resolve_model_discovers_first_upstream_model() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0_u8; 1024];
        let _ = socket.read(&mut buffer).await;
        let body = r#"{"object":"list","data":[{"id":"Qwen/served"},{"id":"other"}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    let client = reqwest::Client::new();
    let source = SourceOptions {
        upstream: Some(format!("http://{address}")),
        model: None,
        llm_port: 8000,
    };
    assert_eq!(
        super::resolve_model(&client, &source, None).await.unwrap(),
        "Qwen/served"
    );
}

#[test]
fn yolo_claude_environment_overrides_inherited_effort() {
    let options = crate::agentic_cli::HarnessOptions {
        source: SourceOptions {
            upstream: Some("http://127.0.0.1:8000".to_owned()),
            model: Some("served-test".to_owned()),
            llm_port: 8000,
        },
        common: CommonOptions {
            yolo: true,
            ..CommonOptions::default()
        },
        harness_args: Vec::new(),
    };
    let root = std::env::temp_dir().join(format!("agentic-api-yolo-test-{}", std::process::id()));
    let environment = super::harness_environment(
        &HarnessModel::Claude("served-test".to_owned()),
        "http://127.0.0.1:3000",
        &options,
        &root,
        &root,
    )
    .expect("Claude environment");

    assert_eq!(
        environment.environment.get("CLAUDE_CODE_EFFORT_LEVEL"),
        Some(&"medium".to_owned())
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}
