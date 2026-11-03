//! Storage CRUD latency for the rehydrate and persist paths.
//!
//! Each benchmark times one store call and nothing else: histories are seeded before timing starts,
//! and each persist sample gets a freshly prepared parent or conversation. Benchmarks are
//! parameterized by history length (items) to show how each read scales.
//!
//! Set `BENCH_DATABASE_URL` to run against another database, for example PostgreSQL; the default is a
//! fresh `SQLite` file in the system temporary directory.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agentic_core::storage::{ConversationStore, DbPool, ResponseMetadata, ResponseStore, create_pool_with_schema};
use agentic_core::types::conversations::ConversationMetadata;
use agentic_core::types::event::MessageStatus;
use agentic_core::types::io::{InOutItem, InputItem, InputMessage, InputMessageContent, OutputItem, OutputMessage};
use criterion::{BatchSize, BenchmarkId, Criterion, black_box, criterion_group};
use tokio::runtime::Runtime;

/// History lengths in items; each turn adds three items.
const HISTORY_ITEMS: [usize; 3] = [3, 30, 300];
const TENANT: &str = "default_tenant";

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_id(prefix: &str) -> String {
    format!("{prefix}{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// One turn: a user message, an assistant message, and a follow-up user message, each with a unique ID.
fn turn() -> Vec<InOutItem> {
    let user = || {
        InOutItem::Input(InputItem::Message(InputMessage {
            id: Some(next_id("msg_in_")),
            role: "user".to_owned(),
            content: InputMessageContent::Text("Test message".to_owned()),
            ..Default::default()
        }))
    };
    let answer = InOutItem::Output(OutputItem::Message(OutputMessage::new(
        next_id("msg_out_"),
        MessageStatus::Completed,
    )));
    vec![user(), answer, user()]
}

fn history(items: usize) -> Vec<InOutItem> {
    (0..items.div_ceil(3)).flat_map(|_| turn()).take(items).collect()
}

async fn seed_response(store: &ResponseStore, items: usize) -> String {
    let id = next_id("resp_");
    store
        .persist(&id, None, history(items), None, &ResponseMetadata::default())
        .await
        .expect("seed response");
    id
}

async fn seed_conversation(store: &ConversationStore, items: usize) -> String {
    let conversation = store
        .create(TENANT, &ConversationMetadata::new(), &[])
        .await
        .expect("create conversation");
    store
        .persist(
            &conversation.conversation_id,
            &next_id("resp_"),
            history(items),
            None,
            &ResponseMetadata::default(),
        )
        .await
        .expect("seed conversation");
    conversation.conversation_id
}

fn bench_response_rehydrate(c: &mut Criterion, rt: &Runtime, store: &ResponseStore) {
    let mut group = c.benchmark_group("response_rehydrate");
    for items in HISTORY_ITEMS {
        let id = rt.block_on(seed_response(store, items));
        group.bench_with_input(BenchmarkId::from_parameter(items), &id, |b, id| {
            b.to_async(rt)
                .iter(|| async { store.rehydrate(black_box(id)).await.expect("rehydrate") });
        });
    }
    group.finish();
}

fn bench_conversation_rehydrate(c: &mut Criterion, rt: &Runtime, store: &ConversationStore) {
    let mut group = c.benchmark_group("conversation_rehydrate");
    for items in HISTORY_ITEMS {
        let id = rt.block_on(seed_conversation(store, items));
        group.bench_with_input(BenchmarkId::from_parameter(items), &id, |b, id| {
            b.to_async(rt)
                .iter(|| async { store.rehydrate(black_box(id)).await.expect("rehydrate") });
        });
    }
    group.finish();
}

/// Continue a fixed parent with one new turn; each sample writes a new response ID.
fn bench_response_persist(c: &mut Criterion, rt: &Runtime, store: &ResponseStore) {
    let mut group = c.benchmark_group("response_persist");
    for items in HISTORY_ITEMS {
        let parent = rt.block_on(seed_response(store, items));
        group.bench_with_input(BenchmarkId::from_parameter(items), &parent, |b, parent| {
            b.to_async(rt).iter_batched(
                || (next_id("resp_"), turn()),
                |(id, new_items)| async move {
                    store
                        .persist(
                            &id,
                            Some(parent),
                            black_box(new_items),
                            None,
                            &ResponseMetadata::default(),
                        )
                        .await
                        .expect("persist");
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

/// Append one turn to a fresh conversation with `items` of history. Each sample prepares its conversation
/// outside the timed call.
fn bench_conversation_persist(c: &mut Criterion, rt: &Runtime, store: &ConversationStore) {
    let mut group = c.benchmark_group("conversation_persist");
    for items in HISTORY_ITEMS {
        group.bench_function(BenchmarkId::from_parameter(items), |b| {
            b.to_async(rt).iter_custom(|iters| async move {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iters {
                    let id = seed_conversation(store, items).await;
                    let (response_id, new_items) = (next_id("resp_"), turn());
                    let start = Instant::now();
                    store
                        .persist(
                            &id,
                            &response_id,
                            black_box(new_items),
                            None,
                            &ResponseMetadata::default(),
                        )
                        .await
                        .expect("persist");
                    elapsed += start.elapsed();
                }
                elapsed
            });
        });
    }
    group.finish();
}

fn pool(rt: &Runtime) -> Arc<DbPool> {
    let url = std::env::var("BENCH_DATABASE_URL").unwrap_or_else(|_| {
        let path = std::env::temp_dir().join(format!("agentic_storage_crud_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        format!("sqlite://{}?mode=rwc", path.display())
    });
    rt.block_on(create_pool_with_schema(Some(&url)))
        .expect("create pool with schema")
}

fn init_benches(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let pool = pool(&rt);
    let conversations = ConversationStore::new(Arc::clone(&pool));
    let responses = ResponseStore::new(pool);

    bench_response_rehydrate(c, &rt, &responses);
    bench_conversation_rehydrate(c, &rt, &conversations);
    bench_response_persist(c, &rt, &responses);
    bench_conversation_persist(c, &rt, &conversations);
}

criterion_group!(storage_benches, init_benches);
