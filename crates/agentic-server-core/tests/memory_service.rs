//! Memory behavior over real SQL ingestion, keyword retrieval, and durable blobs.

use std::sync::Arc;

use agentic_core::storage::{DbPool, create_pool_with_schema};
use agentic_core::tool::file_search::FileSearchService;
use agentic_core::tool::memory::MemoryService;
use agentic_core::types::file_search::{
    AttachFileRequest, AttributeValue, ComparisonFilter, ComparisonOperator, CreateVectorStoreRequest, FileAttributes,
    FileSearchConfig, FileSearchError, FilterValue, ListParams, RankingOptions, SearchFilter,
};
use agentic_core::types::memory::{MemoryDocument, MemoryOptions, MemoryOwner};
use agentic_core::types::request_response::ResponsePayload;
use tempfile::TempDir;

struct Fixture {
    directory: TempDir,
    pool: Arc<DbPool>,
    search: FileSearchService,
    memory: MemoryService,
    store_id: String,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", directory.path().join("memory.db").display());
        let pool = create_pool_with_schema(Some(&url)).await.unwrap();
        let search =
            FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config(&directory)).unwrap();
        let store = search
            .create_vector_store(CreateVectorStoreRequest {
                name: Some("memory prototype".into()),
                ..CreateVectorStoreRequest::default()
            })
            .await
            .unwrap();
        let memory = MemoryService::open(search.clone(), store.id.clone()).await.unwrap();
        Self {
            directory,
            pool,
            search,
            memory,
            store_id: store.id,
        }
    }

    async fn write(&self, owner: &str, summary: &str, transcript: &str) -> String {
        self.memory
            .remember(
                &owner_id(owner),
                &MemoryOptions::default(),
                true,
                &completed(),
                &MemoryDocument {
                    conversation_id: "conv_launch",
                    response_id: "resp_launch",
                    summary,
                    transcript,
                },
            )
            .await
            .unwrap()
            .unwrap()
    }
}

fn config(directory: &TempDir) -> FileSearchConfig {
    FileSearchConfig {
        files_storage_dir: Some(directory.path().join("files")),
        ..FileSearchConfig::default()
    }
}

fn owner_id(id: &str) -> MemoryOwner {
    MemoryOwner::resolve(Some(id), None).unwrap().unwrap()
}

fn completed() -> ResponsePayload {
    serde_json::from_value(serde_json::json!({
        "id": "resp_launch", "object": "response", "created_at": 1,
        "model": "test", "status": "completed", "output": []
    }))
    .unwrap()
}

fn eq(key: &str, value: &str) -> SearchFilter {
    SearchFilter::Comparison(ComparisonFilter {
        operator: ComparisonOperator::Eq,
        key: key.into(),
        value: FilterValue::Scalar(AttributeValue::String(value.into())),
    })
}

#[tokio::test]
async fn recall_survives_a_real_database_and_service_restart_then_forget_removes_it() {
    let fixture = Fixture::new().await;
    let file_id = fixture
        .write(
            "alice",
            "The user's launch color is teal.",
            "User: Remember that my launch color is teal.",
        )
        .await;
    let remembered = fixture
        .memory
        .recall(&owner_id("alice"), &MemoryOptions::default(), "launch color")
        .await
        .unwrap()
        .unwrap();
    assert!(remembered.as_str().contains("teal"));
    assert!(remembered.as_str().contains("untrusted historical data"));
    assert!(
        serde_json::to_string(&remembered.as_message())
            .unwrap()
            .contains("system")
    );
    fixture.pool.close().await;

    let url = format!("sqlite://{}", fixture.directory.path().join("memory.db").display());
    let reopened = create_pool_with_schema(Some(&url)).await.unwrap();
    let search =
        FileSearchService::new(reopened, Arc::new(reqwest::Client::new()), config(&fixture.directory)).unwrap();
    let memory = MemoryService::open(search.clone(), fixture.store_id).await.unwrap();
    assert!(
        memory
            .recall(&owner_id("alice"), &MemoryOptions::default(), "launch color")
            .await
            .unwrap()
            .unwrap()
            .as_str()
            .contains("teal")
    );
    memory.forget(&owner_id("alice"), &file_id).await.unwrap();
    assert!(
        memory
            .recall(&owner_id("alice"), &MemoryOptions::default(), "launch color")
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        search.get_file(&file_id).await,
        Err(FileSearchError::NotFound(_))
    ));
}

#[tokio::test]
async fn owner_is_mandatory_and_authentication_cannot_be_overridden() {
    assert!(MemoryOwner::resolve(None, None).unwrap().is_none());
    assert_eq!(
        MemoryOwner::resolve(Some("alice"), Some("bob"))
            .unwrap()
            .unwrap()
            .as_str(),
        "alice"
    );
    assert!(MemoryOwner::resolve(Some(" "), Some("bob")).is_err());
    assert!(MemoryOwner::resolve(None, Some("\n")).is_err());
    assert_eq!(
        MemoryOwner::resolve(None, Some("local-user"))
            .unwrap()
            .unwrap()
            .as_str(),
        "local-user"
    );
    let fixture = Fixture::new().await;
    let file_id = fixture.write("alice", "Launch color teal.", "").await;
    fixture.write("bob", "Launch color magenta.", "").await;
    let alice = fixture
        .memory
        .recall(&owner_id("alice"), &MemoryOptions::default(), "launch color")
        .await
        .unwrap()
        .unwrap();
    assert!(alice.as_str().contains("teal"));
    assert!(!alice.as_str().contains("magenta"));
    assert!(matches!(
        fixture.memory.forget(&owner_id("bob"), &file_id).await,
        Err(FileSearchError::NotFound(_))
    ));
    let spoof = MemoryOptions {
        filters: Some(eq("owner_id", "bob")),
        ..MemoryOptions::default()
    };
    assert!(
        fixture
            .memory
            .recall(&owner_id("alice"), &spoof, "launch color")
            .await
            .unwrap()
            .is_none()
    );
    assert!(fixture.search.get_file(&file_id).await.is_ok());
}

#[tokio::test]
async fn ordinary_files_do_not_become_memories_or_deletable_memory_records() {
    let fixture = Fixture::new().await;
    let file = fixture
        .search
        .upload_file(
            "notes.txt",
            "text/plain",
            "assistants",
            b"Launch color orange.".to_vec(),
        )
        .await
        .unwrap();
    fixture
        .search
        .attach_file(
            &fixture.store_id,
            AttachFileRequest {
                file_id: file.id.clone(),
                attributes: FileAttributes::from([("owner_id".into(), AttributeValue::String("alice".into()))]),
                chunking_strategy: None,
            },
        )
        .await
        .unwrap();
    assert!(
        fixture
            .memory
            .recall(&owner_id("alice"), &MemoryOptions::default(), "launch color")
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        fixture.memory.forget(&owner_id("alice"), &file.id).await,
        Err(FileSearchError::NotFound(_))
    ));
    assert!(fixture.search.get_file(&file.id).await.is_ok());
}

#[tokio::test]
async fn opt_out_unstored_and_unsuccessful_responses_produce_no_memory_files() {
    let fixture = Fixture::new().await;
    let document = MemoryDocument {
        conversation_id: "conv_launch",
        response_id: "resp_launch",
        summary: "Launch color teal.",
        transcript: "",
    };
    for status in ["failed", "incomplete", "cancelled", "in_progress", "completed"] {
        for stored in [false, true] {
            for enabled in [false, true] {
                for write_enabled in [false, true] {
                    if stored && enabled && write_enabled && status == "completed" {
                        continue;
                    }
                    let mut response = completed();
                    response.status = status.into();
                    let options = MemoryOptions {
                        enabled,
                        write_enabled,
                        ..MemoryOptions::default()
                    };
                    assert!(
                        fixture
                            .memory
                            .remember(&owner_id("alice"), &options, stored, &response, &document)
                            .await
                            .unwrap()
                            .is_none()
                    );
                }
            }
        }
    }
    assert!(
        fixture
            .search
            .list_files(&ListParams::default())
            .await
            .unwrap()
            .data
            .is_empty()
    );
    fixture.write("alice", "Launch color teal.", "").await;
    let disabled = MemoryOptions {
        enabled: false,
        ..MemoryOptions::default()
    };
    assert!(
        fixture
            .memory
            .recall(&owner_id("alice"), &disabled, "launch color")
            .await
            .unwrap()
            .is_none()
    );
    let read_only = MemoryOptions {
        write_enabled: false,
        ..MemoryOptions::default()
    };
    assert!(
        fixture
            .memory
            .recall(&owner_id("alice"), &read_only, "launch color")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn searchable_transcript_recovers_a_detail_omitted_by_the_summary() {
    let fixture = Fixture::new().await;
    fixture
        .write(
            "alice",
            "The user is preparing a launch.",
            "The emergency rollback code is zephyr947.",
        )
        .await;
    let context = fixture
        .memory
        .recall(&owner_id("alice"), &MemoryOptions::default(), "zephyr947")
        .await
        .unwrap()
        .unwrap();
    assert!(context.as_str().contains("zephyr947"));
    let extra_filter = MemoryOptions {
        filters: Some(eq("conversation_id", "different-conversation")),
        ..MemoryOptions::default()
    };
    assert!(
        fixture
            .memory
            .recall(&owner_id("alice"), &extra_filter, "zephyr947")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn unicode_escaping_and_long_queries_respect_the_context_budget() {
    let fixture = Fixture::new().await;
    let transcript = format!("launch {} END_OF_OVERSIZED_TRANSCRIPT", "🌊\n\"\\".repeat(8000));
    let id = fixture.write("alice", "Launch color teal.", &transcript).await;
    let content = fixture.search.file_content(&id).await.unwrap();
    let text = String::from_utf8(content).unwrap();
    assert!(!text.contains("END_OF_OVERSIZED_TRANSCRIPT"));
    assert!(text.len() < 28_500);
    for max_context_tokens in [1, 64, 100, 200, 1200] {
        let options = MemoryOptions {
            max_context_tokens,
            ..MemoryOptions::default()
        };
        if let Some(context) = fixture
            .memory
            .recall(&owner_id("alice"), &options, &"launch ".repeat(1000))
            .await
            .unwrap()
        {
            assert!(context.as_str().len() <= max_context_tokens * 4);
            for line in context.as_str().lines().skip(1) {
                serde_json::from_str::<String>(line).unwrap();
            }
        }
    }
}

#[tokio::test]
async fn invalid_options_and_summary_fail_before_upload_and_missing_store_recalls_nothing() {
    let fixture = Fixture::new().await;
    let response = completed();
    for max_context_tokens in [0, 8193, usize::MAX] {
        let options = MemoryOptions {
            max_context_tokens,
            ..MemoryOptions::default()
        };
        assert!(
            fixture
                .memory
                .recall(&owner_id("alice"), &options, "launch")
                .await
                .is_err()
        );
    }
    for max_num_results in [0, 51] {
        let options = MemoryOptions {
            max_num_results,
            ..MemoryOptions::default()
        };
        assert!(
            fixture
                .memory
                .recall(&owner_id("alice"), &options, "launch")
                .await
                .is_err()
        );
    }
    for (response_id, summary) in [("different", "launch".into()), ("resp_launch", "x".repeat(8193))] {
        let document = MemoryDocument {
            conversation_id: "conv_launch",
            response_id,
            summary: &summary,
            transcript: "",
        };
        assert!(
            fixture
                .memory
                .remember(
                    &owner_id("alice"),
                    &MemoryOptions::default(),
                    true,
                    &response,
                    &document
                )
                .await
                .is_err()
        );
    }
    assert!(
        fixture
            .search
            .list_files(&ListParams::default())
            .await
            .unwrap()
            .data
            .is_empty()
    );
    let invalid_ranking = MemoryOptions {
        ranking_options: Some(RankingOptions {
            score_threshold: Some(2.0),
            ..RankingOptions::default()
        }),
        ..MemoryOptions::default()
    };
    assert!(matches!(
        fixture
            .memory
            .recall(&owner_id("alice"), &invalid_ranking, "launch")
            .await,
        Err(FileSearchError::InvalidRequest(_))
    ));
    fixture.search.delete_vector_store(&fixture.store_id).await.unwrap();
    assert!(
        fixture
            .memory
            .recall(&owner_id("alice"), &MemoryOptions::default(), "launch")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn an_attachment_failure_cleans_up_the_uploaded_memory_file() {
    let fixture = Fixture::new().await;
    fixture.search.delete_vector_store(&fixture.store_id).await.unwrap();
    let document = MemoryDocument {
        conversation_id: "conv_launch",
        response_id: "resp_launch",
        summary: "Launch color teal.",
        transcript: "",
    };
    assert!(
        fixture
            .memory
            .remember(
                &owner_id("alice"),
                &MemoryOptions::default(),
                true,
                &completed(),
                &document
            )
            .await
            .is_err()
    );
    assert!(
        fixture
            .search
            .list_files(&ListParams::default())
            .await
            .unwrap()
            .data
            .is_empty()
    );
}
