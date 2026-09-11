# In-tree file search

The file search flow runs inside Agentic API. OGX is the behavioral reference, but
is not a runtime dependency. The service integrates with the current typed
executor and shared persistence layer.

## Data flow

1. Upload a file through `POST /v1/files` into local filesystem storage. Text
   ingestion is built in; optional `file-search-pdf` builds also extract PDF text
   using bounded parser APIs (Rust 1.88 or newer).
2. Create a vector store and attach the uploaded file through `/v1/vector_stores`.
3. Extract text, create overlapping token chunks, obtain embeddings from a
   configured OpenAI-compatible embeddings endpoint, and persist chunks.
4. Search through `/v1/vector_stores/{id}/search` or declare the Responses
   `file_search` built-in tool. Both routes use the same Rust search service.
5. The existing tool loop supplies retrieved content to inference and exposes
   `file_search_call` output items, results on request, and file citations.

## Boundaries

- `types/file_search.rs` owns typed file, vector store, search, filter, ranking,
  chunking, and configuration contracts.
- `storage/file_search.rs` owns SQL persistence. SQLite and PostgreSQL use the
  existing pool and a new additive migration; the database retains file metadata.
- `storage/local_files.rs` owns generated-ID file paths, atomic publication,
  bounded reads, and file removal. Restarts reuse the configured directory.
  Replicas require both a shared database and a shared file directory.
- `tool/file_search/service.rs` owns ingestion and retrieval orchestration;
  focused sibling modules own embeddings, parsing/chunking, and ranking.
- `tool/file_search/handler.rs` implements the existing typed tool interfaces.
- HTTP handlers call core service methods through `ExecutionContext`; they do
  not import the storage layer.

## Behavior

Provide file upload/list/retrieve/content/delete, vector store
create/list/retrieve/delete, file attach/list/retrieve/detach, and search.
Ingestion completes before a successful attachment is returned. Failed ingestion
must not publish partial chunks. Reattaching an existing file must be idempotent
or an explicit conflict. Deletion removes metadata and associated chunks
transactionally, then unlinks
the uploaded bytes. A crash after metadata deletion can leave an unreferenced file,
which is no longer accessible through the API.

Text and PDF files use token-based overlapping chunking. The tokenizer operates
on bounded UTF-8 blocks with cancellation checks to prevent quadratic processing
of long lexical sequences; windows preserve the resulting bounded-block tokens.
File IDs and filenames are preserved across ingestion, retrieval, and citations. File attributes support
strings, numbers, and booleans; filters support typed comparisons and nested
`and`/`or` expressions with validation limits. Search accepts one or more queries,
merges and deduplicates chunks across selected stores, and applies a global result
limit and score threshold. Semantic, keyword, and hybrid retrieval are supported;
hybrid fusion uses reciprocal rank fusion with configurable signal weights.

Embedding configuration is deployment-controlled. Validate embedding count,
indexes, dimensions, and finite values. Persist the embedding model identity and
dimension with each store and reject incompatible configurations. Bound file
size, extracted text, chunk counts, request fan-out, outbound response size, and
embedding batch sizes. CPU-heavy extraction and scoring run off the async runtime.
Untrusted document text is clearly delimited in tool outputs.

The first in-tree storage implementation is portable exact retrieval over the
persisted corpus; it does not claim approximate-nearest-neighbor scaling. OGX's
provider catalog, contextual chunking, query rewriting, neural/classifier reranking,
and asynchronous batch ingestion are separate extensions, and unsupported options
must be rejected explicitly rather than silently ignored.

## Shared implementation contract

Types live at `crate::types::file_search`:

- `FileSearchConfig`: `files_storage_dir: Option<PathBuf>`,
  `embedding_base_url: Option<String>`,
  `embedding_model: Option<String>`, `embedding_api_key: Option<String>`;
  implements `Default`, `Clone`, `Debug`, `Serialize`, and `Deserialize`.
- `SearchRequest`: `query: SearchQuery`, `max_num_results: Option<usize>`,
  `filters: Option<SearchFilter>`, `ranking_options: Option<RankingOptions>`,
  `search_mode: Option<SearchMode>`, `rewrite_query: bool`; implements `Default`.
- `SearchQuery`: untagged `Text(String)` or `Texts(Vec<String>)`.
- `SearchResponse`: `object`, `search_query: Vec<String>`, `data: Vec<SearchResult>`,
  `has_more: bool`, `next_page: Option<String>`.
- `SearchResult`: `file_id: String`, `filename: String`, `score: f64`,
  `attributes: FileAttributes`, `content: Vec<SearchContent>`.
- `SearchContent`: `type_: String` (wire name `type`) and `text: String`.
- `CreateVectorStoreRequest`, `AttachFileRequest`, `FileObject`,
  `VectorStoreObject`, `VectorStoreFileObject`, `DeleteObject`,
  `ListResponse<T>` provide the corresponding OpenAI-shaped payloads.
- `ListParams`: optional `limit`, `after`, `before`, `order`, validated by service.

`crate::tool::file_search::FileSearchService` is cloneable and exposes:

```rust,ignore
pub fn new(pool: Arc<DbPool>, client: Arc<reqwest::Client>, config: FileSearchConfig) -> Result<Self, FileSearchError>;
pub async fn upload_file(&self, filename: &str, content_type: &str, purpose: &str, bytes: Vec<u8>) -> Result<FileObject, FileSearchError>;
pub async fn list_files(&self, params: &ListParams) -> Result<ListResponse<FileObject>, FileSearchError>;
pub async fn get_file(&self, id: &str) -> Result<FileObject, FileSearchError>;
pub async fn file_content(&self, id: &str) -> Result<Vec<u8>, FileSearchError>;
pub async fn delete_file(&self, id: &str) -> Result<DeleteObject, FileSearchError>;
pub async fn create_vector_store(&self, request: CreateVectorStoreRequest) -> Result<VectorStoreObject, FileSearchError>;
pub async fn list_vector_stores(&self, params: &ListParams) -> Result<ListResponse<VectorStoreObject>, FileSearchError>;
pub async fn get_vector_store(&self, id: &str) -> Result<VectorStoreObject, FileSearchError>;
pub async fn delete_vector_store(&self, id: &str) -> Result<DeleteObject, FileSearchError>;
pub async fn attach_file(&self, store_id: &str, request: AttachFileRequest) -> Result<VectorStoreFileObject, FileSearchError>;
pub async fn list_vector_store_files(&self, store_id: &str, params: &ListParams) -> Result<ListResponse<VectorStoreFileObject>, FileSearchError>;
pub async fn get_vector_store_file(&self, store_id: &str, file_id: &str) -> Result<VectorStoreFileObject, FileSearchError>;
pub async fn detach_file(&self, store_id: &str, file_id: &str) -> Result<DeleteObject, FileSearchError>;
pub async fn search(&self, store_ids: &[String], request: &SearchRequest) -> Result<SearchResponse, FileSearchError>;
```

`FileSearchError` variants distinguish `InvalidRequest(String)`, `NotFound(String)`,
`Conflict(String)`, `Unavailable(String)`, storage errors, and provider errors with
sources. It exposes `status_code() -> u16` and `public_message() -> String` to keep
HTTP error handling out of the service and avoid exposing provider credentials.

## Verification

Use deterministic real SQL and fixture files, mocking only inference and embedding
HTTP boundaries. Cover persistence across service recreation, deletion, malformed
input, transactional ingestion failure, Unicode chunk boundaries, semantic search
independent of lexical overlap, filter isolation, global ordering/limits, empty
results, embedding failures, and streamed/blocking Responses tool loops. Existing
cassettes remain unchanged; new captured cassettes require the recorder workflow.
Run focused tests, the full workspace suite, Clippy, formatting, OpenAPI checks, and
all pre-commit hooks before publishing a PR.
