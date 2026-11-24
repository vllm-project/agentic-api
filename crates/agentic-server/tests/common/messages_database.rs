//! Isolated SQLite/Postgres database for Messages HTTP tests.

pub async fn database() -> (String, Option<tempfile::TempDir>) {
    if let Ok(base_url) = std::env::var("TEST_POSTGRES_URL") {
        let admin = agentic_core::storage::create_pool(Some(&base_url)).await.unwrap();
        let name = format!("messages_http_{}", uuid::Uuid::now_v7().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(admin.as_ref())
            .await
            .unwrap();
        admin.close().await;
        let mut url = reqwest::Url::parse(&base_url).unwrap();
        assert!(matches!(url.scheme(), "postgres" | "postgresql"));
        url.set_path(&name);
        (url.to_string(), None)
    } else {
        let directory = tempfile::tempdir().unwrap();
        (
            format!("sqlite://{}", directory.path().join("messages.db").display()),
            Some(directory),
        )
    }
}
