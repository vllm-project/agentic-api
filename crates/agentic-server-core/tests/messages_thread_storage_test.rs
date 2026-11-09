//! Run against SQLite by default, or real Postgres with `TEST_POSTGRES_URL`.

use std::collections::BTreeSet;
use std::sync::Arc;

use agentic_core::storage::messages::{MessageThreadStore, MessageThreadStoreError};
use agentic_core::storage::{DbPool, create_pool_with_schema};
use agentic_core::types::messages::thread::{MessageThreadCheckpoint, MessagesThreadOwner, ThreadFingerprint};
use agentic_core::types::messages::{MessageContent, MessageParam, MessagesRequest};
use tokio::sync::Barrier;

struct Database {
    url: String,
    _directory: Option<tempfile::TempDir>,
}

impl Database {
    async fn new() -> Self {
        if let Ok(base_url) = std::env::var("TEST_POSTGRES_URL") {
            let admin = agentic_core::storage::create_pool(Some(&base_url)).await.unwrap();
            let name = format!("threads_{}", uuid::Uuid::now_v7().simple());
            sqlx::query(&format!("CREATE DATABASE {name}"))
                .execute(admin.as_ref())
                .await
                .unwrap();
            admin.close().await;
            let mut url = reqwest::Url::parse(&base_url).unwrap();
            assert!(matches!(url.scheme(), "postgres" | "postgresql"));
            url.set_path(&name);
            Self {
                url: url.to_string(),
                _directory: None,
            }
        } else {
            let directory = tempfile::tempdir().unwrap();
            Self {
                url: format!("sqlite://{}", directory.path().join("messages.db").display()),
                _directory: Some(directory),
            }
        }
    }

    async fn open(&self) -> Arc<DbPool> {
        create_pool_with_schema(Some(&self.url)).await.unwrap()
    }
}

fn owner() -> MessagesThreadOwner {
    MessagesThreadOwner::from_components(&[b"issuer", b"alice", b"workspace", b"key"])
}
fn checkpoint() -> MessageThreadCheckpoint {
    MessageThreadCheckpoint {
        messages: vec![MessageParam {
            role: "user".to_owned(),
            content: MessageContent::Text("durable history 雪".to_owned()),
        }],
        system: None,
        tools: None,
        fingerprint: ThreadFingerprint::from(
            &serde_json::from_str::<MessagesRequest>(r#"{"model":"m","max_tokens":1,"messages":[]}"#).unwrap(),
        ),
        betas: BTreeSet::new(),
        paused: false,
    }
}

#[tokio::test]
async fn checkpoint_survives_pool_and_store_recreation_with_an_independent_object_model() {
    let database = Database::new().await;
    let pool = database.open().await;
    let store = MessageThreadStore::new(Arc::clone(&pool));
    store
        .publish(&owner(), "msg_first", Some(&checkpoint()), None)
        .await
        .unwrap();
    store.shutdown().await;
    drop(store);
    pool.close().await;
    drop(pool);

    let pool = database.open().await;
    let store = MessageThreadStore::new(Arc::clone(&pool));
    let restored = store.load(&owner(), "msg_first").await.unwrap();
    assert!(matches!(&restored.messages[0].content, MessageContent::Text(text) if text == "durable history 雪"));
    let wrong = MessagesThreadOwner::from_components(&[b"issuer", b"bob", b"workspace", b"key"]);
    assert!(matches!(
        store.load(&wrong, "msg_first").await,
        Err(MessageThreadStoreError::NotFound)
    ));
    let lease = store.reserve(owner(), "msg_first".to_owned()).await.unwrap();
    store
        .publish(&owner(), "msg_second", Some(&restored), Some(lease))
        .await
        .unwrap();
    assert!(matches!(
        store.load(&owner(), "msg_first").await,
        Err(MessageThreadStoreError::AlreadyContinued)
    ));
    for table in ["responses", "items", "conversations"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
        assert_eq!(count, 0, "Messages must not use {table}");
    }
    store.shutdown().await;
    pool.close().await;
}

#[tokio::test]
async fn concurrent_instances_admit_only_one_continuation_and_fence_stale_writers() {
    let database = Database::new().await;
    let first_pool = database.open().await;
    let second_pool = database.open().await;
    let first = MessageThreadStore::new(Arc::clone(&first_pool));
    let second = MessageThreadStore::new(Arc::clone(&second_pool));
    first
        .publish(&owner(), "msg_parent", Some(&checkpoint()), None)
        .await
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [first.clone(), second.clone()]
        .into_iter()
        .map(|store| {
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                store.reserve(owner(), "msg_parent".to_owned()).await
            })
        })
        .collect();
    let mut lease = None;
    let mut rejected = 0;
    for handle in handles {
        match handle.await.unwrap() {
            Ok(winner) => {
                assert!(lease.is_none());
                lease = Some(winner);
            }
            Err(MessageThreadStoreError::InFlight) => rejected += 1,
            Err(error) => panic!("unexpected admission failure: {error}"),
        }
    }
    assert_eq!(rejected, 1);
    // Simulate a crashed instance's stale lease and let another instance reclaim it.
    sqlx::query("UPDATE message_threads SET lease_until = 0 WHERE message_id = 'msg_parent'")
        .execute(second_pool.as_ref())
        .await
        .unwrap();
    let replacement = second.reserve(owner(), "msg_parent".to_owned()).await.unwrap();
    let result = first.publish(&owner(), "msg_stale", Some(&checkpoint()), lease).await;
    assert!(matches!(result, Err(MessageThreadStoreError::LeaseLost)));
    first.shutdown().await;
    assert!(matches!(
        second.load(&owner(), "msg_parent").await,
        Err(MessageThreadStoreError::InFlight)
    ));
    second
        .publish(&owner(), "msg_winner", Some(&checkpoint()), Some(replacement))
        .await
        .unwrap();
    assert!(second.load(&owner(), "msg_winner").await.is_ok());
    assert!(matches!(
        first.load(&owner(), "msg_stale").await,
        Err(MessageThreadStoreError::NotFound)
    ));
    second.shutdown().await;
    first_pool.close().await;
    second_pool.close().await;
}

#[tokio::test]
async fn failed_child_insert_rolls_back_parent_consumption_and_cancellation_releases_lease() {
    let database = Database::new().await;
    let pool = database.open().await;
    let store = MessageThreadStore::new(Arc::clone(&pool));
    store
        .publish(&owner(), "msg_parent", Some(&checkpoint()), None)
        .await
        .unwrap();
    store
        .publish(&owner(), "msg_existing", Some(&checkpoint()), None)
        .await
        .unwrap();
    let lease = store.reserve(owner(), "msg_parent".to_owned()).await.unwrap();
    assert!(
        store
            .publish(&owner(), "msg_existing", Some(&checkpoint()), Some(lease))
            .await
            .is_err()
    );
    store.shutdown().await;
    assert!(store.load(&owner(), "msg_parent").await.is_ok());
    let lease = store.reserve(owner(), "msg_parent".to_owned()).await.unwrap();
    drop(lease);
    store.shutdown().await;
    let lease = store.reserve(owner(), "msg_parent".to_owned()).await.unwrap();
    store
        .publish(&owner(), "msg_child", Some(&checkpoint()), Some(lease))
        .await
        .unwrap();
    assert!(store.load(&owner(), "msg_child").await.is_ok());
    store.shutdown().await;
    pool.close().await;
}

#[tokio::test]
async fn maintenance_renews_active_leases_and_expires_only_inactive_checkpoints() {
    let database = Database::new().await;
    let pool = database.open().await;
    let store = MessageThreadStore::new(Arc::clone(&pool));
    store
        .publish(&owner(), "msg_active", Some(&checkpoint()), None)
        .await
        .unwrap();
    let lease = store.reserve(owner(), "msg_active".to_owned()).await.unwrap();
    let before = chrono::Utc::now().timestamp() + 2;
    sqlx::query("UPDATE message_threads SET lease_until = $1, expires_at = 0 WHERE message_id = 'msg_active'")
        .bind(before)
        .execute(pool.as_ref())
        .await
        .unwrap();
    store.maintain().await.unwrap();
    let renewed: i64 = sqlx::query_scalar("SELECT lease_until FROM message_threads WHERE message_id = 'msg_active'")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert!(renewed > before);
    drop(lease);
    store.shutdown().await;
    store.maintain().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_threads")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(count, 0);
    pool.close().await;
}

#[tokio::test]
async fn a_lease_cannot_transfer_a_conversation_to_another_owner() {
    let database = Database::new().await;
    let pool = database.open().await;
    let store = MessageThreadStore::new(Arc::clone(&pool));
    store
        .publish(&owner(), "msg_parent", Some(&checkpoint()), None)
        .await
        .unwrap();
    let lease = store.reserve(owner(), "msg_parent".to_owned()).await.unwrap();
    let wrong = MessagesThreadOwner::from_components(&[b"bob"]);
    assert!(matches!(
        store
            .publish(&wrong, "msg_other_owner", Some(&checkpoint()), Some(lease))
            .await,
        Err(MessageThreadStoreError::LeaseLost)
    ));
    store.shutdown().await;
    assert!(store.load(&owner(), "msg_parent").await.is_ok());
    assert!(matches!(
        store.load(&wrong, "msg_other_owner").await,
        Err(MessageThreadStoreError::NotFound)
    ));
    pool.close().await;
}
