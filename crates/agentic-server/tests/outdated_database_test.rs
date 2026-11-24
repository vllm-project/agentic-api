//! Startup against a database holding data from an earlier release.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sqlx::sqlite::SqlitePool;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Writes a database an earlier release created and wrote to: migrated to before migration 9, the first this
/// release reads, holding one conversation.
async fn write_earlier_release_database(path: &Path) {
    let pool = SqlitePool::connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .expect("create database");
    let mut migrator = sqlx::migrate!("../agentic-server-core/migrations");
    migrator.migrations.to_mut().retain(|migration| migration.version < 9);
    migrator.run(&pool).await.expect("apply earlier migrations");
    sqlx::query("INSERT INTO conversations (id, created_at) VALUES ('conv_earlier', 0)")
        .execute(&pool)
        .await
        .expect("store a conversation");
    pool.close().await;
}

async fn conversation_count(path: &Path) -> i64 {
    let pool = SqlitePool::connect(&format!("sqlite://{}", path.display()))
        .await
        .expect("open database");
    let count = sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
        .fetch_one(&pool)
        .await
        .expect("count conversations");
    pool.close().await;
    count
}

#[tokio::test]
async fn explicit_database_url_naming_the_default_database_is_not_recreated() {
    let home = tempfile::tempdir().expect("temporary Agentic API home");
    let database = home.path().join("agentic_api.db");
    write_earlier_release_database(&database).await;

    // The URL the server would choose by default, supplied explicitly.
    let mut server = Command::new(env!("CARGO_BIN_EXE_agentic-server"))
        .env_clear()
        .env("AGENTIC_API_HOME", home.path())
        .env("DATABASE_URL", format!("sqlite://{}", database.display()))
        .args([
            "--llm-api-base",
            "http://127.0.0.1:9",
            "--skip-llm-ready-check",
            "--gateway-host",
            "127.0.0.1",
            "--gateway-port",
            "0",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("agentic-server must start");
    let started = Instant::now();
    while server.try_wait().expect("poll server").is_none() {
        if started.elapsed() > STARTUP_TIMEOUT {
            server.kill().expect("stop server");
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let output = server.wait_with_output().expect("server output");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "startup must be refused: {stderr}");
    assert!(stderr.contains("earlier agentic-api release"), "{stderr}");
    assert_eq!(conversation_count(&database).await, 1, "stored data must be kept");
}
