//! Run with `cargo run -p agentic-server-core --example memory_demo`.
//! A storage/recall demonstration; no live model or API credentials are used.

use std::{error::Error, sync::Arc};

use agentic_core::storage::create_pool_with_schema;
use agentic_core::tool::{file_search::FileSearchService, memory::MemoryService};
use agentic_core::types::file_search::{CreateVectorStoreRequest, FileSearchConfig};
use agentic_core::types::memory::{MemoryDocument, MemoryOptions, MemoryOwner};
use agentic_core::types::request_response::ResponsePayload;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let url = format!("sqlite://{}?mode=rwc", directory.path().join("memory.db").display());
    let config = FileSearchConfig {
        files_storage_dir: Some(directory.path().join("files")),
        ..FileSearchConfig::default()
    };
    let pool = create_pool_with_schema(Some(&url)).await?;
    let search = FileSearchService::new(pool.clone(), Arc::new(reqwest::Client::new()), config.clone())?;
    let store = search
        .create_vector_store(CreateVectorStoreRequest {
            name: Some("memory-demo".into()),
            ..CreateVectorStoreRequest::default()
        })
        .await?;
    let memory = MemoryService::open(search, store.id.clone()).await?;
    let owner = MemoryOwner::resolve(Some("alice"), None)?.ok_or("missing demo owner")?;
    let other = MemoryOwner::resolve(Some("bob"), None)?.ok_or("missing demo owner")?;
    let options = MemoryOptions::default();
    // The host application will supply a real completed response and a generated
    // summary. This fixture exercises storage without claiming model behavior.
    let response: ResponsePayload = serde_json::from_value(serde_json::json!({
        "id": "resp_demo", "object": "response", "created_at": 1,
        "model": "storage-demo", "status": "completed", "output": []
    }))?;
    let id = memory
        .remember(
            &owner,
            &options,
            true,
            &response,
            &MemoryDocument {
                conversation_id: "conv_demo",
                response_id: "resp_demo",
                summary: "The user's launch color is teal.",
                transcript: "User: Remember that my launch color is teal.",
            },
        )
        .await?
        .ok_or("memory was not stored")?;
    println!("Stored a conversation memory for Alice.");
    pool.close().await;

    let pool = create_pool_with_schema(Some(&url)).await?;
    let search = FileSearchService::new(pool, Arc::new(reqwest::Client::new()), config)?;
    let memory = MemoryService::open(search, store.id).await?;
    let recalled = memory
        .recall(&owner, &options, "launch color")
        .await?
        .ok_or("recall was empty")?;
    assert!(recalled.as_str().contains("teal"));
    println!("Reopened database: Alice recalls teal.");
    assert!(memory.recall(&other, &options, "launch color").await?.is_none());
    println!("Bob cannot recall Alice's memory through the memory service.");
    memory.forget(&owner, &id).await?;
    assert!(memory.recall(&owner, &options, "launch color").await?.is_none());
    println!("Forgot the memory: recall is empty.");
    Ok(())
}
