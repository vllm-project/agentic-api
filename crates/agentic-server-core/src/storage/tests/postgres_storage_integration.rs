use super::test_support as support;

use agentic_core::types::conversations::ConversationMetadata;
use std::sync::Arc;
use std::time::Duration;

use agentic_core::config::{PostgresConfig, SqliteConfig};
use agentic_core::storage::{
    ConversationStore, ResponseCheckpoint, ResponseMetadata, ResponseStore, create_pool_with_configs,
    create_pool_with_schema_and_configs,
};
use agentic_core::types::io::InOutItem;
use agentic_core::types::io::{InputItem, InputMessage, InputMessageContent};
use support::persist_turn;
use tokio::sync::Barrier;
use tokio::time::timeout;

fn input_item(text: &str) -> InOutItem {
    InOutItem::Input(InputItem::Message(InputMessage {
        role: "user".to_owned(),
        content: InputMessageContent::Text(text.to_owned()),
        ..Default::default()
    }))
}

#[tokio::test]
#[ignore = "requires TEST_POSTGRES_URL pointing to an isolated PostgreSQL database"]
#[allow(
    clippy::too_many_lines,
    reason = "keeps the complete restart persistence lifecycle in one integration test"
)]
async fn postgres_migrations_and_state_survive_pool_restarts() {
    let database_url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL must be set");
    let postgres_config = PostgresConfig {
        max_connections: 2,
        acquire_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
        migration_timeout: Duration::from_secs(5),
        statement_timeout: Duration::from_secs(5),
        idle_timeout: Some(Duration::from_secs(30)),
        max_lifetime: Some(Duration::from_secs(60)),
    };
    let response_id = format!("resp_postgres_{}", uuid::Uuid::now_v7());
    let continuation_id = format!("resp_postgres_{}", uuid::Uuid::now_v7());
    let conversation_response_id = format!("resp_postgres_{}", uuid::Uuid::now_v7());
    let conversation_continuation_id = format!("resp_postgres_{}", uuid::Uuid::now_v7());
    let metadata = ResponseMetadata::default();

    let first_pool = create_pool_with_schema_and_configs(Some(&database_url), SqliteConfig::default(), postgres_config)
        .await
        .expect("initialize clean PostgreSQL database");
    assert_eq!(first_pool.options().get_max_connections(), 2);
    let statement_timeout: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(first_pool.as_ref())
        .await
        .expect("inspect PostgreSQL statement timeout");
    assert_eq!(statement_timeout, "5s");
    let created_at_type: String = sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'conversations' AND column_name = 'created_at'",
    )
    .fetch_one(first_pool.as_ref())
    .await
    .expect("inspect PostgreSQL timestamp column");
    assert_eq!(created_at_type, "bigint");
    let future_conversation_id = format!("conv_postgres_{}", uuid::Uuid::now_v7());
    let future_timestamp = i64::from(i32::MAX) + 1;
    sqlx::query("INSERT INTO conversations (id, created_at) VALUES ($1, $2)")
        .bind(&future_conversation_id)
        .bind(future_timestamp)
        .execute(first_pool.as_ref())
        .await
        .expect("persist timestamp beyond PostgreSQL INT4 range");
    let stored_future_timestamp: i64 = sqlx::query_scalar("SELECT created_at FROM conversations WHERE id = $1")
        .bind(&future_conversation_id)
        .fetch_one(first_pool.as_ref())
        .await
        .expect("load timestamp beyond PostgreSQL INT4 range");
    assert_eq!(stored_future_timestamp, future_timestamp);

    ResponseStore::new(first_pool.clone())
        .persist(&response_id, None, vec![input_item("first turn")], None, &metadata)
        .await
        .expect("persist first response");
    let first_conversation_store = ConversationStore::new(first_pool.clone());
    let conversation = first_conversation_store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .expect("create conversation");
    persist_turn(
        &first_conversation_store,
        &conversation.conversation_id,
        &conversation_response_id,
        vec![
            input_item("conversation turn one"),
            input_item("conversation turn one follow-up"),
        ],
        &metadata,
    )
    .await
    .expect("persist first conversation turn");
    first_pool.close().await;

    let second_pool =
        create_pool_with_schema_and_configs(Some(&database_url), SqliteConfig::default(), postgres_config)
            .await
            .expect("repeat migrations after first restart");
    let second_store = ResponseStore::new(second_pool.clone());
    assert_eq!(
        second_store
            .rehydrate(&response_id)
            .await
            .expect("rehydrate first response after restart")
            .1
            .len(),
        1
    );
    second_store
        .persist(
            &continuation_id,
            Some(&response_id),
            vec![input_item("second turn")],
            None,
            &metadata,
        )
        .await
        .expect("persist continuation");
    let second_conversation_store = ConversationStore::new(second_pool.clone());
    assert_eq!(
        second_conversation_store
            .rehydrate(&conversation.conversation_id)
            .await
            .expect("rehydrate conversation after restart")
            .items
            .len(),
        2
    );
    persist_turn(
        &second_conversation_store,
        &conversation.conversation_id,
        &conversation_continuation_id,
        vec![input_item("conversation turn two")],
        &metadata,
    )
    .await
    .expect("persist second conversation turn");
    second_pool.close().await;

    let third_pool = create_pool_with_schema_and_configs(Some(&database_url), SqliteConfig::default(), postgres_config)
        .await
        .expect("repeat migrations after second restart");
    assert_eq!(
        ResponseStore::new(third_pool.clone())
            .rehydrate(&continuation_id)
            .await
            .expect("rehydrate continuation after restart")
            .1
            .len(),
        2
    );
    assert_eq!(
        ConversationStore::new(third_pool)
            .rehydrate(&conversation.conversation_id)
            .await
            .expect("rehydrate full conversation after restart")
            .items
            .len(),
        3
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TEST_POSTGRES_URL pointing to an isolated PostgreSQL database"]
async fn postgres_concurrent_conversation_writes_have_contiguous_sequences() {
    const WRITE_COUNT: usize = 12;

    let database_url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL must be set");
    let postgres_config = PostgresConfig {
        max_connections: u32::try_from(WRITE_COUNT).expect("test write count must fit in u32"),
        acquire_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
        migration_timeout: Duration::from_secs(5),
        statement_timeout: Duration::from_secs(5),
        idle_timeout: Some(Duration::from_secs(30)),
        max_lifetime: Some(Duration::from_secs(60)),
    };
    let first_pool = create_pool_with_schema_and_configs(Some(&database_url), SqliteConfig::default(), postgres_config)
        .await
        .expect("initialize PostgreSQL database");
    let second_pool = create_pool_with_configs(Some(&database_url), SqliteConfig::default(), postgres_config)
        .await
        .expect("create independent PostgreSQL pool");
    let conversation_store = ConversationStore::new(first_pool.clone());
    let conversation = conversation_store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .expect("create conversation");
    let barrier = Arc::new(Barrier::new(WRITE_COUNT + 1));
    let mut tasks = Vec::with_capacity(WRITE_COUNT);

    for index in 0..WRITE_COUNT {
        let barrier = Arc::clone(&barrier);
        let pool = if index % 2 == 0 {
            first_pool.clone()
        } else {
            second_pool.clone()
        };
        let conversation_id = conversation.conversation_id.clone();
        // Versioned turns to one conversation conflict by design; concurrent appends exercise the
        // same locked sequence allocation.
        tasks.push(tokio::spawn(async move {
            let store = ConversationStore::new(pool);
            barrier.wait().await;
            store
                .create_items(
                    "default_tenant",
                    &conversation_id,
                    vec![input_item(&format!("concurrent turn {index}"))],
                )
                .await
        }));
    }

    barrier.wait().await;
    for task in tasks {
        task.await
            .expect("join concurrent write")
            .expect("persist concurrent turn");
    }

    let rows = crate::storage::models::conversation::get_with_items(&first_pool, &conversation.conversation_id)
        .await
        .expect("load concurrent conversation items")
        .expect("conversation exists")
        .items;
    let sequences = rows
        .iter()
        .map(|row| row.seq.expect("conversation item sequence"))
        .collect::<Vec<_>>();
    let write_count = i64::try_from(WRITE_COUNT).expect("test write count must fit in i64");
    assert_eq!(sequences, (0..write_count).collect::<Vec<_>>());

    first_pool.close().await;
    second_pool.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires TEST_POSTGRES_URL pointing to an isolated PostgreSQL database"]
#[allow(
    clippy::too_many_lines,
    reason = "keeps the complete two-pool race and persistence assertions in one integration test"
)]
async fn postgres_racing_conversation_turns_both_append() {
    let database_url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL must be set");
    let postgres_config = PostgresConfig {
        max_connections: 2,
        acquire_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
        migration_timeout: Duration::from_secs(5),
        statement_timeout: Duration::from_secs(5),
        idle_timeout: Some(Duration::from_secs(30)),
        max_lifetime: Some(Duration::from_secs(60)),
    };
    let first_pool = create_pool_with_schema_and_configs(Some(&database_url), SqliteConfig::default(), postgres_config)
        .await
        .expect("initialize PostgreSQL database");
    let second_pool = create_pool_with_configs(Some(&database_url), SqliteConfig::default(), postgres_config)
        .await
        .expect("create independent PostgreSQL pool");
    let store = ConversationStore::new(first_pool.clone());
    let conversation = store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .expect("create conversation");
    let barrier = Arc::new(Barrier::new(2));

    let writer = |pool: &Arc<_>, name: &'static str| {
        let store = ConversationStore::new(Arc::clone(pool));
        let conversation_id = conversation.conversation_id.clone();
        let barrier = Arc::clone(&barrier);
        tokio::spawn(async move {
            let response_id = format!("resp_postgres_{}", uuid::Uuid::now_v7());
            let items = vec![
                input_item(&format!("{name} input")),
                input_item(&format!("{name} follow-up")),
            ];
            barrier.wait().await;
            store
                .persist(
                    &conversation_id,
                    &ResponseCheckpoint::default(),
                    &response_id,
                    items.clone(),
                    None,
                    &ResponseMetadata::default(),
                )
                .await
                .map(|()| (response_id, items))
        })
    };
    let (writer_one, writer_two) = (writer(&first_pool, "writer one"), writer(&second_pool, "writer two"));
    let (one_response_id, one_items) = writer_one
        .await
        .expect("join first write")
        .expect("first turn persists");
    let (two_response_id, two_items) = writer_two
        .await
        .expect("join second write")
        .expect("second turn persists");

    // As on OpenAI, both turns commit; the row lock gives each a contiguous run of sequence numbers.
    let rows = crate::storage::models::conversation::get_with_items(&first_pool, &conversation.conversation_id)
        .await
        .expect("load conversation items")
        .expect("conversation exists")
        .items;
    let sequences = rows
        .iter()
        .map(|row| row.seq.expect("conversation item sequence"))
        .collect::<Vec<_>>();
    assert_eq!(sequences, vec![0, 1, 2, 3]);
    let items = store
        .rehydrate(&conversation.conversation_id)
        .await
        .expect("rehydrate conversation items")
        .items;
    assert!(items == [one_items.clone(), two_items.clone()].concat() || items == [two_items, one_items].concat());
    let responses = ResponseStore::new(first_pool.clone());
    for response_id in [one_response_id, two_response_id] {
        responses.get(&response_id).await.expect("both turns are stored");
    }

    first_pool.close().await;
    second_pool.close().await;
}

#[tokio::test]
#[ignore = "requires TEST_POSTGRES_URL pointing to an isolated PostgreSQL database"]
async fn postgres_lock_wait_is_bounded_without_blocking_other_conversations() {
    let database_url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL must be set");
    let postgres_config = PostgresConfig {
        max_connections: 3,
        acquire_timeout: Duration::from_secs(5),
        lock_timeout: Duration::from_secs(1),
        migration_timeout: Duration::from_secs(5),
        statement_timeout: Duration::from_secs(5),
        idle_timeout: Some(Duration::from_secs(30)),
        max_lifetime: Some(Duration::from_secs(60)),
    };
    let first_pool = create_pool_with_schema_and_configs(Some(&database_url), SqliteConfig::default(), postgres_config)
        .await
        .expect("initialize PostgreSQL database");
    let second_pool = create_pool_with_configs(Some(&database_url), SqliteConfig::default(), postgres_config)
        .await
        .expect("create independent PostgreSQL pool");
    let first_store = ConversationStore::new(first_pool.clone());
    let locked_conversation = first_store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .expect("create locked conversation");
    let unrelated_conversation = first_store
        .create("default_tenant", &ConversationMetadata::new(), &[])
        .await
        .expect("create unrelated conversation");
    let mut holding_transaction = first_pool.begin().await.expect("begin lock-holding transaction");
    crate::storage::models::conversation::lock_in_tx(&mut holding_transaction, &locked_conversation.conversation_id)
        .await
        .expect("hold conversation row lock");

    let blocked_store = ConversationStore::new(second_pool.clone());
    let blocked_conversation_id = locked_conversation.conversation_id.clone();
    let mut blocked_write = tokio::spawn(async move {
        persist_turn(
            &blocked_store,
            &blocked_conversation_id,
            &format!("resp_postgres_{}", uuid::Uuid::now_v7()),
            vec![input_item("blocked turn")],
            &ResponseMetadata::default(),
        )
        .await
    });
    let unrelated_store = ConversationStore::new(second_pool.clone());
    let unrelated_response_id = format!("resp_postgres_{}", uuid::Uuid::now_v7());
    let unrelated_metadata = ResponseMetadata::default();
    let unrelated_write = persist_turn(
        &unrelated_store,
        &unrelated_conversation.conversation_id,
        &unrelated_response_id,
        vec![input_item("unrelated turn")],
        &unrelated_metadata,
    );
    tokio::pin!(unrelated_write);
    tokio::select! {
        result = &mut unrelated_write => {
            result.expect("persist unrelated conversation turn");
        }
        result = &mut blocked_write => {
            let result = result.expect("join blocked write");
            panic!("locked write completed before unrelated conversation write: {result:?}");
        }
    }

    let blocked_result = timeout(Duration::from_secs(2), blocked_write)
        .await
        .expect("locked write should respect PostgreSQL lock_timeout")
        .expect("join blocked write");
    assert!(blocked_result.is_err(), "locked write unexpectedly succeeded");

    holding_transaction
        .rollback()
        .await
        .expect("release conversation row lock");
    first_pool.close().await;
    second_pool.close().await;
}

#[tokio::test]
#[ignore = "requires TEST_POSTGRES_URL pointing to an isolated PostgreSQL database"]
async fn postgres_get_or_create_creates_then_returns_the_existing_conversation() {
    let database_url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL must be set");
    let pool =
        create_pool_with_schema_and_configs(Some(&database_url), SqliteConfig::default(), PostgresConfig::default())
            .await
            .expect("initialize PostgreSQL database");
    let store = ConversationStore::new(pool.clone());
    let conversation_id = format!("conv_postgres_{}", uuid::Uuid::now_v7());

    // A stored Responses turn that names a conversation goes through this upsert first.
    let created = store
        .get_or_create(&conversation_id)
        .await
        .expect("create conversation on first use");
    let response_id = format!("resp_postgres_{}", uuid::Uuid::now_v7());
    persist_turn(
        &store,
        &conversation_id,
        &response_id,
        vec![input_item("first turn")],
        &ResponseMetadata::default(),
    )
    .await
    .expect("persist turn");
    let before = store
        .rehydrate(&conversation_id)
        .await
        .expect("rehydrate after first turn");

    let existing = store
        .get_or_create(&conversation_id)
        .await
        .expect("return the existing conversation");
    assert_eq!(existing.conversation_id, created.conversation_id);
    assert_eq!(existing.created_at, created.created_at);
    let after = store
        .rehydrate(&conversation_id)
        .await
        .expect("rehydrate after second use");
    assert_eq!(after.items, before.items);
    assert!(after.checkpoint.response_id.is_some());
    assert_eq!(after.items.len(), 1);

    pool.close().await;
}
