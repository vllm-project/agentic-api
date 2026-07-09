use criterion::{BatchSize, Criterion, black_box, criterion_group};

use agentic_core::storage::{ConversationStore, InOutItem, ResponseMetadata, ResponseStore, create_pool_with_schema};
use agentic_core::types::event::MessageStatus;
use agentic_core::types::io::{InputItem, InputMessage, InputMessageContent, OutputItem, OutputMessage};

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_id() -> String {
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("id_{count}")
}

fn create_test_items() -> Vec<InOutItem> {
    let input_item = InputItem::Message(InputMessage {
        role: "user".to_string(),
        content: InputMessageContent::Text("Test message".to_string()),
    });

    let output_msg = OutputMessage::new("msg_123", MessageStatus::Completed);

    vec![
        InOutItem::Input(input_item.clone()),
        InOutItem::Output(OutputItem::Message(output_msg)),
        InOutItem::Input(input_item),
    ]
}

fn create_test_metadata() -> ResponseMetadata {
    ResponseMetadata::default()
}

fn bench_conversation_persist(c: &mut Criterion, store: &ConversationStore) {
    use std::sync::{Arc, Mutex};
    let previous_response_id = Arc::new(Mutex::new(None::<String>));

    c.bench_function("conversation_persist", |b| {
        b.to_async(tokio::runtime::Runtime::new().unwrap()).iter_batched(
            || async {
                let conversation = store.create().await.expect("failed to create conversation");
                let new_items = create_test_items();
                let test_metadata = create_test_metadata();
                let response_id = next_id();
                let prev_id = previous_response_id.lock().unwrap().as_deref().map(ToString::to_string);
                (
                    conversation.conversation_id.clone(),
                    new_items,
                    test_metadata,
                    response_id,
                    prev_id,
                )
            },
            |setup| {
                let previous_response_id = previous_response_id.clone();
                async move {
                    let (conversation_id, new_items, test_metadata, response_id, prev_id) = setup.await;
                    store
                        .persist(
                            &conversation_id,
                            &response_id,
                            prev_id.as_deref(),
                            black_box(new_items),
                            &black_box(test_metadata),
                        )
                        .await
                        .expect("persist failed");

                    *previous_response_id.lock().unwrap() = Some(response_id);
                }
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_response_persist(c: &mut Criterion, store: &ResponseStore) {
    use std::sync::{Arc, Mutex};
    let previous_id = Arc::new(Mutex::new(None::<String>));

    c.bench_function("response_persist", |b| {
        b.to_async(tokio::runtime::Runtime::new().unwrap()).iter_batched(
            || {
                let new_items = create_test_items();
                let test_metadata = create_test_metadata();
                let current_id = next_id();
                let prev_id = previous_id.lock().unwrap().as_deref().map(ToString::to_string);
                (new_items, test_metadata, current_id, prev_id)
            },
            |(new_items, test_metadata, current_id, prev_id)| {
                let previous_id = previous_id.clone();
                async move {
                    store
                        .persist(
                            &current_id,
                            prev_id.as_deref(),
                            black_box(new_items),
                            &black_box(test_metadata),
                        )
                        .await
                        .expect("persist failed");

                    *previous_id.lock().unwrap() = Some(current_id);
                }
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_conversation_rehydrate(c: &mut Criterion, store: &ConversationStore) {
    use std::sync::Mutex;
    let previous_response_id = Mutex::new(None::<String>);

    c.bench_function("conversation_rehydrate", |b| {
        b.to_async(tokio::runtime::Runtime::new().unwrap()).iter_batched(
            || async {
                let conversation = store.create().await.expect("failed to create conversation");
                let new_items = create_test_items();
                let test_metadata = create_test_metadata();
                let response_id = next_id();
                let prev_id = previous_response_id.lock().unwrap().as_deref().map(ToString::to_string);

                store
                    .persist(
                        &conversation.conversation_id,
                        &response_id,
                        prev_id.as_deref(),
                        new_items,
                        &test_metadata,
                    )
                    .await
                    .expect("setup persist failed");

                *previous_response_id.lock().unwrap() = Some(response_id);
                conversation.conversation_id.clone()
            },
            |setup| async move {
                let conversation_id = setup.await;
                store
                    .rehydrate(&black_box(conversation_id))
                    .await
                    .expect("rehydrate failed")
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_response_rehydrate(c: &mut Criterion, store: &ResponseStore) {
    use std::sync::Mutex;
    let previous_response_id = Mutex::new(None::<String>);

    c.bench_function("response_rehydrate", |b| {
        b.to_async(tokio::runtime::Runtime::new().unwrap()).iter_batched(
            || async {
                let new_items = create_test_items();
                let test_metadata = create_test_metadata();
                let response_id = next_id();
                let prev_id = previous_response_id.lock().unwrap().as_deref().map(ToString::to_string);

                store
                    .persist(&response_id, prev_id.as_deref(), new_items, &test_metadata)
                    .await
                    .expect("setup persist failed");

                *previous_response_id.lock().unwrap() = Some(response_id.clone());
                response_id
            },
            |setup| async move {
                let response_id = setup.await;
                store
                    .rehydrate(&black_box(response_id))
                    .await
                    .expect("rehydrate failed")
            },
            BatchSize::SmallInput,
        );
    });
}

fn init_benches(c: &mut Criterion) {
    COUNTER.store(0, std::sync::atomic::Ordering::SeqCst);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let pool = rt.block_on(async {
        create_pool_with_schema(None)
            .await
            .expect("failed to create pool with schema")
    });

    let conversation_store = ConversationStore::new(pool.clone());
    let response_store = ResponseStore::new(pool.clone());

    bench_conversation_persist(c, &conversation_store);
    bench_response_persist(c, &response_store);
    bench_conversation_rehydrate(c, &conversation_store);
    bench_response_rehydrate(c, &response_store);

    rt.block_on(async {
        sqlx::query("DELETE FROM items").execute(pool.as_ref()).await.ok();
        sqlx::query("DELETE FROM responses").execute(pool.as_ref()).await.ok();
        sqlx::query("DELETE FROM conversations")
            .execute(pool.as_ref())
            .await
            .ok();
    });
}

criterion_group!(storage_benches, init_benches);
