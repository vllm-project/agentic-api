//! Concurrent storage benchmarks for [`ConversationStore`] and [`ResponseStore`].
//!
//! Each benchmark group spawns N tasks simultaneously to measure throughput and
//! contention under concurrent load.
//!
//! | Group                                        | What it measures                                     |
//! |----------------------------------------------|------------------------------------------------------|
//! | `concurrent_conversation_persist`            | N writers to the **same** conversation (seq contention) |
//! | `concurrent_conversation_persist_independent`| N writers each to their **own** conversation         |
//! | `concurrent_conversation_rehydrate`          | N readers of the **same** conversation               |
//! | `concurrent_conversation_rehydrate_independent` | N readers each on their **own** conversation      |
//! | `concurrent_conversation_mixed_read_write` | 1 writer and N readers across independent `SQLite` pools |
//! | `concurrent_response_persist`               | N independent response writes in parallel            |
//! | `concurrent_response_rehydrate`             | N readers of the **same** response                  |
//!
//! # Configuring concurrency levels
//!
//! Set `BENCH_CONCURRENCY` to a comma-separated list of integers before running.
//! Defaults to `2,4,8,16` when unset.
//!
//! ```bash
//! # Run with default concurrency levels (2, 4, 8, 16)
//! cargo bench --bench storage_concurrent
//!
//! # Run with custom concurrency levels
//! BENCH_CONCURRENCY=2,4,8,16,32 cargo bench --bench storage_concurrent
//!
//! # Run a single group
//! cargo bench --bench storage_concurrent -- concurrent_conversation_persist
//! ```

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group};
use tokio::runtime::Runtime;

use agentic_core::storage::{ConversationStore, InOutItem, ResponseMetadata, ResponseStore, create_pool_with_schema};
use agentic_core::types::event::MessageStatus;
use agentic_core::types::io::{InputItem, InputMessage, InputMessageContent, OutputItem, OutputMessage};

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Parse `BENCH_CONCURRENCY` env var as a comma-separated list of integers.
/// Falls back to `[2, 4, 8, 16]` when unset or unparseable.
///
/// Example: `BENCH_CONCURRENCY=1,2,4,8,16,32 cargo bench`
fn concurrency_levels() -> Vec<usize> {
    std::env::var("BENCH_CONCURRENCY")
        .ok()
        .and_then(|val| {
            let levels: Vec<usize> = val.split(',').filter_map(|s| s.trim().parse().ok()).collect();
            if levels.is_empty() { None } else { Some(levels) }
        })
        .unwrap_or_else(|| vec![2, 4, 8, 16])
}

fn next_id() -> String {
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!("bench_id_{count}")
}

fn make_items() -> Vec<InOutItem> {
    let input = InputItem::Message(InputMessage {
        role: "user".to_string(),
        content: InputMessageContent::Text("Test message".to_string()),
    });
    vec![
        InOutItem::Input(input.clone()),
        InOutItem::Output(OutputItem::Message(OutputMessage::new(
            "msg_123",
            MessageStatus::Completed,
        ))),
        InOutItem::Input(input),
    ]
}

fn make_metadata() -> ResponseMetadata {
    ResponseMetadata::default()
}

fn bench_concurrent_conversation_persist(c: &mut Criterion, store: &ConversationStore) {
    let store = Arc::new(store.clone());
    let mut group = c.benchmark_group("concurrent_conversation_persist");

    for concurrency in concurrency_levels() {
        group.throughput(Throughput::Elements(concurrency as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            &concurrency,
            |b, &concurrency| {
                b.to_async(Runtime::new().unwrap()).iter_batched(
                    || {
                        let store = store.clone();
                        async move { store.create().await.expect("create conversation").conversation_id }
                    },
                    |setup| {
                        let store = store.clone();
                        async move {
                            let conversation_id = setup.await;
                            let handles: Vec<_> = (0..concurrency)
                                .map(|_| {
                                    let store = store.clone();
                                    let cid = conversation_id.clone();
                                    tokio::spawn(async move {
                                        store
                                            .persist(
                                                &cid,
                                                &next_id(),
                                                None,
                                                black_box(make_items()),
                                                &black_box(make_metadata()),
                                            )
                                            .await
                                            .expect("concurrent conversation persist failed");
                                    })
                                })
                                .collect();
                            for h in handles {
                                h.await.expect("task panicked");
                            }
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_concurrent_conversation_rehydrate(c: &mut Criterion, store: &ConversationStore) {
    let store = Arc::new(store.clone());
    let rt = Runtime::new().unwrap();

    let conversation_id = rt.block_on(async {
        let conv = store.create().await.expect("create conversation");
        store
            .persist(&conv.conversation_id, &next_id(), None, make_items(), &make_metadata())
            .await
            .expect("setup persist");
        conv.conversation_id
    });

    let mut group = c.benchmark_group("concurrent_conversation_rehydrate");

    for concurrency in concurrency_levels() {
        group.throughput(Throughput::Elements(concurrency as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            &concurrency,
            |b, &concurrency| {
                b.to_async(Runtime::new().unwrap()).iter_batched(
                    || conversation_id.clone(),
                    |cid| {
                        let store = store.clone();
                        async move {
                            let handles: Vec<_> = (0..concurrency)
                                .map(|_| {
                                    let store = store.clone();
                                    let id = cid.clone();
                                    tokio::spawn(async move {
                                        store
                                            .rehydrate(&black_box(id))
                                            .await
                                            .expect("concurrent rehydrate failed")
                                    })
                                })
                                .collect();
                            for h in handles {
                                h.await.expect("task panicked");
                            }
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_concurrent_conversation_persist_independent(c: &mut Criterion, store: &ConversationStore) {
    let store = Arc::new(store.clone());
    let mut group = c.benchmark_group("concurrent_conversation_persist_independent");

    for concurrency in concurrency_levels() {
        group.throughput(Throughput::Elements(concurrency as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            &concurrency,
            |b, &concurrency| {
                b.to_async(Runtime::new().unwrap()).iter_batched(
                    || {
                        let store = store.clone();
                        async move {
                            let mut ids = Vec::with_capacity(concurrency);
                            for _ in 0..concurrency {
                                ids.push(store.create().await.expect("create conversation").conversation_id);
                            }
                            ids
                        }
                    },
                    |setup| {
                        let store = store.clone();
                        async move {
                            let conversation_ids = setup.await;
                            let handles: Vec<_> = conversation_ids
                                .into_iter()
                                .map(|cid| {
                                    let store = store.clone();
                                    tokio::spawn(async move {
                                        store
                                            .persist(
                                                &cid,
                                                &next_id(),
                                                None,
                                                black_box(make_items()),
                                                &black_box(make_metadata()),
                                            )
                                            .await
                                            .expect("independent conversation persist failed");
                                    })
                                })
                                .collect();
                            for h in handles {
                                h.await.expect("task panicked");
                            }
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_concurrent_conversation_rehydrate_independent(c: &mut Criterion, store: &ConversationStore) {
    let store = Arc::new(store.clone());
    let rt = Runtime::new().unwrap();

    let max_concurrency = concurrency_levels().into_iter().max().unwrap_or(16);
    let conversation_ids: Vec<String> = rt.block_on(async {
        let mut ids = Vec::with_capacity(max_concurrency);
        for _ in 0..max_concurrency {
            let conv = store.create().await.expect("create conversation");
            store
                .persist(&conv.conversation_id, &next_id(), None, make_items(), &make_metadata())
                .await
                .expect("setup persist");
            ids.push(conv.conversation_id);
        }
        ids
    });

    let mut group = c.benchmark_group("concurrent_conversation_rehydrate_independent");

    for concurrency in concurrency_levels() {
        group.throughput(Throughput::Elements(concurrency as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            &concurrency,
            |b, &concurrency| {
                b.to_async(Runtime::new().unwrap()).iter_batched(
                    || conversation_ids[..concurrency].to_vec(),
                    |ids| {
                        let store = store.clone();
                        async move {
                            let handles: Vec<_> = ids
                                .into_iter()
                                .map(|id| {
                                    let store = store.clone();
                                    tokio::spawn(async move {
                                        store
                                            .rehydrate(&black_box(id))
                                            .await
                                            .expect("independent rehydrate failed")
                                    })
                                })
                                .collect();
                            for h in handles {
                                h.await.expect("task panicked");
                            }
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_concurrent_conversation_mixed_read_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_conversation_mixed_read_write");

    for concurrency in concurrency_levels() {
        group.throughput(Throughput::Elements((concurrency + 1) as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            &concurrency,
            |b, &concurrency| {
                b.to_async(Runtime::new().unwrap()).iter_batched(
                    || async move {
                        let db_path =
                            std::env::temp_dir().join(format!("storage_mixed_rw_{}.db", uuid::Uuid::now_v7()));
                        let db_url = format!("sqlite://{}", db_path.display());
                        let writer_pool = create_pool_with_schema(Some(&db_url)).await.expect("writer pool");
                        let reader_pool = create_pool_with_schema(Some(&db_url)).await.expect("reader pool");
                        let writer_store = ConversationStore::new(writer_pool);
                        let reader_store = ConversationStore::new(reader_pool);
                        let conversation_id = writer_store
                            .create()
                            .await
                            .expect("create conversation")
                            .conversation_id;
                        writer_store
                            .persist(&conversation_id, &next_id(), None, make_items(), &make_metadata())
                            .await
                            .expect("seed conversation");
                        (Arc::new(writer_store), Arc::new(reader_store), conversation_id)
                    },
                    |setup| async move {
                        let (writer_store, reader_store, conversation_id) = setup.await;
                        let writer_id = conversation_id.clone();
                        let writer = {
                            let writer_store = writer_store.clone();
                            tokio::spawn(async move {
                                writer_store
                                    .persist(
                                        &writer_id,
                                        &next_id(),
                                        None,
                                        black_box(make_items()),
                                        &black_box(make_metadata()),
                                    )
                                    .await
                                    .expect("mixed writer failed");
                            })
                        };

                        let readers: Vec<_> = (0..concurrency)
                            .map(|_| {
                                let reader_store = reader_store.clone();
                                let reader_id = conversation_id.clone();
                                tokio::spawn(async move {
                                    reader_store
                                        .rehydrate(&black_box(reader_id))
                                        .await
                                        .expect("mixed reader failed")
                                })
                            })
                            .collect();

                        writer.await.expect("writer task panicked");
                        for reader in readers {
                            reader.await.expect("reader task panicked");
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_concurrent_response_persist(c: &mut Criterion, store: &ResponseStore) {
    let store = Arc::new(store.clone());
    let mut group = c.benchmark_group("concurrent_response_persist");

    for concurrency in concurrency_levels() {
        group.throughput(Throughput::Elements(concurrency as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            &concurrency,
            |b, &concurrency| {
                b.to_async(Runtime::new().unwrap()).iter_batched(
                    || (0..concurrency).map(|_| next_id()).collect::<Vec<_>>(),
                    |response_ids| {
                        let store = store.clone();
                        async move {
                            let handles: Vec<_> = response_ids
                                .into_iter()
                                .map(|response_id| {
                                    let store = store.clone();
                                    tokio::spawn(async move {
                                        store
                                            .persist(
                                                &response_id,
                                                None,
                                                black_box(make_items()),
                                                &black_box(make_metadata()),
                                            )
                                            .await
                                            .expect("concurrent response persist failed");
                                    })
                                })
                                .collect();
                            for h in handles {
                                h.await.expect("task panicked");
                            }
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_concurrent_response_rehydrate(c: &mut Criterion, store: &ResponseStore) {
    let store = Arc::new(store.clone());
    let rt = Runtime::new().unwrap();

    let response_id = rt.block_on(async {
        let id = next_id();
        store
            .persist(&id, None, make_items(), &make_metadata())
            .await
            .expect("setup persist");
        id
    });

    let mut group = c.benchmark_group("concurrent_response_rehydrate");

    for concurrency in concurrency_levels() {
        group.throughput(Throughput::Elements(concurrency as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            &concurrency,
            |b, &concurrency| {
                b.to_async(Runtime::new().unwrap()).iter_batched(
                    || response_id.clone(),
                    |rid| {
                        let store = store.clone();
                        async move {
                            let handles: Vec<_> = (0..concurrency)
                                .map(|_| {
                                    let store = store.clone();
                                    let id = rid.clone();
                                    tokio::spawn(async move {
                                        store
                                            .rehydrate(&black_box(id))
                                            .await
                                            .expect("concurrent rehydrate failed")
                                    })
                                })
                                .collect();
                            for h in handles {
                                h.await.expect("task panicked");
                            }
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn init_benches(c: &mut Criterion) {
    COUNTER.store(0, std::sync::atomic::Ordering::SeqCst);

    let rt = Runtime::new().unwrap();
    let pool = rt.block_on(async {
        create_pool_with_schema(None)
            .await
            .expect("failed to create pool with schema")
    });

    let conversation_store = ConversationStore::new(pool.clone());
    let response_store = ResponseStore::new(pool.clone());

    bench_concurrent_conversation_persist(c, &conversation_store);
    bench_concurrent_conversation_persist_independent(c, &conversation_store);
    bench_concurrent_conversation_rehydrate(c, &conversation_store);
    bench_concurrent_conversation_rehydrate_independent(c, &conversation_store);
    bench_concurrent_conversation_mixed_read_write(c);
    bench_concurrent_response_persist(c, &response_store);
    bench_concurrent_response_rehydrate(c, &response_store);

    rt.block_on(async {
        sqlx::query("DELETE FROM items").execute(pool.as_ref()).await.ok();
        sqlx::query("DELETE FROM responses").execute(pool.as_ref()).await.ok();
        sqlx::query("DELETE FROM conversations")
            .execute(pool.as_ref())
            .await
            .ok();
    });
}

criterion_group!(storage_concurrent_benches, init_benches);
