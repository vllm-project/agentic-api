//! Opt-in memory storage and recall over the existing file-search service.
//!
//! This library prototype deliberately has no automatic request-path hooks.
//! The application owns authentication, summarization, and when to call it.

mod context;
pub use context::MemoryContext;

use crate::tool::file_search::FileSearchService;
use crate::types::file_search::{
    AttachFileRequest, AttributeValue, ComparisonFilter, ComparisonOperator, CompoundFilter, CompoundOperator,
    FileAttributes, FileSearchError, FilterValue, SearchFilter, SearchQuery, SearchRequest,
};
use crate::types::memory::{MemoryDocument, MemoryOptions, MemoryOwner};
use crate::types::request_response::ResponsePayload;

const MEMORY_KEY: &str = "agentic_memory";
const OWNER_KEY: &str = "owner_id";
const MAX_SUMMARY_BYTES: usize = 8192;
const MAX_TRANSCRIPT_BYTES: usize = 20_000;

/// Memory is enabled only by explicitly constructing this service with a
/// dedicated, operator-selected vector store. It reuses file-search ingestion,
/// retrieval, provider selection, and resource limits without another index.
#[derive(Clone, Debug)]
pub struct MemoryService {
    search: FileSearchService,
    store_id: String,
}

impl MemoryService {
    /// Open an existing dedicated memory store, including after process restart.
    ///
    /// # Errors
    /// Returns not-found or a storage error if the configured store is unavailable.
    pub async fn open(search: FileSearchService, store_id: String) -> Result<Self, FileSearchError> {
        search.get_vector_store(&store_id).await?;
        Ok(Self { search, store_id })
    }

    /// Store a summary and searchable transcript only after a stored response
    /// completed. Empty summaries mean there was nothing useful to remember.
    /// Await this call; it creates no detached summarization or memory-write task.
    ///
    /// Each successful call publishes an immutable artifact. Replacement,
    /// debouncing, and idempotency require the proposed durable memory-record owner.
    ///
    /// # Errors
    /// Returns invalid-request for invalid metadata or oversized summaries, or
    /// ingestion/storage errors. A failed attachment removes the uploaded file.
    pub async fn remember(
        &self,
        owner: &MemoryOwner,
        options: &MemoryOptions,
        stored: bool,
        response: &ResponsePayload,
        document: &MemoryDocument<'_>,
    ) -> Result<Option<String>, FileSearchError> {
        options.validate()?;
        if !options.enabled || !options.write_enabled || !stored || response.status != "completed" {
            return Ok(None);
        }
        if document.summary.trim().is_empty() {
            return Ok(None);
        }
        if document.response_id != response.id || document.summary.len() > MAX_SUMMARY_BYTES {
            return Err(FileSearchError::InvalidRequest(
                "memory summary exceeds 8192 bytes or does not identify the completed response".into(),
            ));
        }
        for id in [document.conversation_id, document.response_id] {
            if id.trim().is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
                return Err(FileSearchError::InvalidRequest(
                    "memory source identifiers must contain 1 to 512 bytes without control characters".into(),
                ));
            }
        }
        let text = format!(
            "# Conversation memory\n\n## Summary\n{}\n\n## Searchable transcript\n{}\n",
            document.summary,
            context::prefix(document.transcript, MAX_TRANSCRIPT_BYTES),
        );
        let file = self
            .search
            .upload_file(
                "conversation-memory.md",
                "text/markdown",
                "assistants",
                text.into_bytes(),
            )
            .await?;
        let attributes = FileAttributes::from([
            (MEMORY_KEY.into(), AttributeValue::Boolean(true)),
            (OWNER_KEY.into(), AttributeValue::String(owner.as_str().into())),
            (
                "conversation_id".into(),
                AttributeValue::String(document.conversation_id.into()),
            ),
            (
                "response_id".into(),
                AttributeValue::String(document.response_id.into()),
            ),
        ]);
        let attached = self
            .search
            .attach_file(
                &self.store_id,
                AttachFileRequest {
                    file_id: file.id.clone(),
                    attributes,
                    chunking_strategy: None,
                },
            )
            .await;
        if let Err(error) = attached {
            if let Err(cleanup_error) = self.search.delete_file(&file.id).await {
                tracing::warn!(error = ?cleanup_error, file_id = %file.id, "failed to clean up unpublished memory file");
            }
            return Err(error);
        }
        Ok(Some(file.id))
    }

    /// Retrieve owner-scoped memory without public tool-call items or citations.
    /// Request filters are always conjoined with mandatory memory and owner filters.
    /// The default search mode reuses the configured hybrid/keyword behavior.
    ///
    /// # Errors
    /// Returns validation, provider, or storage errors. Missing stores return no
    /// context; other failures remain visible to the caller rather than silently
    /// pretending recall succeeded.
    pub async fn recall(
        &self,
        owner: &MemoryOwner,
        options: &MemoryOptions,
        query: &str,
    ) -> Result<Option<MemoryContext>, FileSearchError> {
        options.validate()?;
        if !options.enabled || query.trim().is_empty() {
            return Ok(None);
        }
        let mut filters = vec![
            equality(MEMORY_KEY, AttributeValue::Boolean(true)),
            equality(OWNER_KEY, AttributeValue::String(owner.as_str().into())),
        ];
        filters.extend(options.filters.iter().cloned());
        let request = SearchRequest {
            query: SearchQuery::Text(context::prefix(query, 4096).into()),
            max_num_results: Some(options.max_num_results),
            filters: Some(SearchFilter::Compound(CompoundFilter {
                operator: CompoundOperator::And,
                filters,
            })),
            ranking_options: options.ranking_options.clone(),
            ..SearchRequest::default()
        };
        match self.search.search(std::slice::from_ref(&self.store_id), &request).await {
            Ok(results) => MemoryContext::from_results(&results, options.max_context_tokens * 4),
            Err(FileSearchError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Remove a memory from recall and delete its underlying file. A foreign or
    /// non-memory file is indistinguishable from a missing memory.
    /// Deletion is independent of the read/write opt-out controls.
    ///
    /// # Errors
    /// Returns not-found for an inaccessible memory, or storage errors.
    pub async fn forget(&self, owner: &MemoryOwner, file_id: &str) -> Result<(), FileSearchError> {
        let attachment = self.search.get_vector_store_file(&self.store_id, file_id).await?;
        if attachment.attributes.get(MEMORY_KEY) != Some(&AttributeValue::Boolean(true))
            || attachment.attributes.get(OWNER_KEY) != Some(&AttributeValue::String(owner.as_str().into()))
        {
            return Err(FileSearchError::NotFound("Memory not found".into()));
        }
        // Detach first, so the memory cannot be recalled even if blob cleanup fails.
        self.search.detach_file(&self.store_id, file_id).await?;
        self.search.delete_file(file_id).await?;
        Ok(())
    }
}

fn equality(key: &str, value: AttributeValue) -> SearchFilter {
    SearchFilter::Comparison(ComparisonFilter {
        operator: ComparisonOperator::Eq,
        key: key.into(),
        value: FilterValue::Scalar(value),
    })
}
