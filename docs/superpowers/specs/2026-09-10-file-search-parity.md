# File search: OGX capabilities and OpenAI compatibility

The existing draft PR #34 supplies local Files storage, portable SQL metadata,
exact retrieval, and the Responses file search tool. The user has approved adding
the missing OGX capabilities and OpenAI API operations as stacked PRs.

## Stack and boundaries

1. Keep #34 as the foundation, based on `main`.
2. `codex/file-search-pgvector`, based on #34's head
   `feat/axum-ogx-integration-v2`: real PostgreSQL vector storage and indexed
   retrieval, typed backend configuration, and pgvector CI.
3. `codex/file-search-retrieval`, based on the pgvector branch: OGX-style vector
   store configuration, contextual chunk embeddings, neural/classifier
   reranking, configurable fusion, and query rewriting.
4. `codex/files-api`, based on the retrieval branch: streaming Files API,
   documented upload purposes, expiration and list/delete contracts.
5. `codex/vector-store-lifecycle`, based on the Files API branch: store/file
   updates, expiration and parsed content.
6. `codex/vector-store-batches`, based on the lifecycle branch: asynchronous
   durable batches, restart recovery, cancellation and explicit worker shutdown.
7. `codex/file-search-openai-api`, based on the batches branch: Responses
   annotation events, SDK contract verification and compatibility documentation.

Each layer must build and pass its relevant tests independently. Open PRs only
after pre-commit verification, with Summary and Test Plan sections, explicit
stack dependencies, and signed-off conventional commits. The user's stack
request overrides the repository's usual instruction to target every PR at main.

## Storage and retrieval

Keep SQLite usable for local development and preserve local filesystem uploads.
Select exact SQL or pgvector explicitly through typed deployment configuration;
selecting pgvector on SQLite is a configuration error. PostgreSQL vector rows
must use the vector extension and database distance operators, with configurable
HNSW or IVFFlat indexes and validated index/search parameters. Do not call an
in-process full corpus scan pgvector support. Search must respect store isolation,
file attributes, deletion, configured dimensions, and embedding identity. Indexed
candidates must be bounded. Publication and deletion must preserve metadata/vector
consistency, including rollback and restart. Existing portable migrations must
continue working on plain PostgreSQL and SQLite. Provision the extension in the
pgvector CI service and test ingestion, indexed search, filters, restart, and
deletion against a real database.

Use OGX's implementations at `/Users/farceo/dev/ogx/src/ogx/providers/remote/vector_io/pgvector/`
and `providers/utils/memory/` as behavioral references; implement typed Rust
within this repository's existing architecture.

## Models and configuration

Provide a typed `VectorStoresConfig` with the useful OGX groups: default provider,
embedding model, reranker model, ingestion defaults, retrieval defaults,
contextual retrieval settings, query rewriting settings, and batch settings.
Model configuration must support endpoint/model/credentials and redact secrets.
Preserve existing embedding environment/config options. Default model selections
are deployment controlled; never accept arbitrary request-supplied endpoints or
credentials. Store embedding identity and dimensions; reject incompatible reuse.

Contextual chunking generates document-aware context for each chunk, prepends it
before embedding, and retains original chunk text for retrieval/citations. Bound
document size, output, concurrency, timeouts, and cancellation. A configured
contextual strategy must actually call the model; missing model configuration or
malformed responses are explicit errors and must not publish partial ingestion.

Neural and classifier rerankers call the configured provider using typed
requests/responses. Validate result indexes, uniqueness, finite scores, and output
size. Over-retrieve bounded candidates, rerank before final truncation, and apply
thresholds to the final scores. `none` skips model reranking; `auto` and documented
OpenAI dated ranker names select the configured default. OGX ranker extensions
include weighted, rrf, normalized, neural, and classifier. Preserve ordinary
keyword, semantic/vector, and hybrid search.

Query rewriting uses a configured model, respects request `rewrite_query`, and
returns the actual search queries. Explicitly requested rewriting without a model
must return an actionable configuration error. Do not silently claim rewriting
by returning the original query.

## OpenAI contracts

Use the current official Files, Vector Stores, vector store files/batches/search,
and Responses streaming contracts as the source of wire field names and values.
OGX configuration, contextual chunking, and provider choices are documented
extensions, not requirements of OpenAI's API.

Implement vector store and file-attribute updates, store expiration policy and
timestamps, parsed file content retrieval, file list purpose filtering, vector
store file status filtering, documented upload purposes and expiration, and file
batch create/retrieve/list-files/cancel. Preserve documented nullability and list
pagination. Durable batches must survive restart, publish visible per-file state,
honor cancellation, record failures, and have bounded workers with clear shutdown.
Expired files/stores must no longer be readable/searchable and their associated
search data must be cleaned up without deleting unrelated uploads.

Support the documented 512 MB file upload limit without unbounded multipart
buffering. Stream local file publication/download; ingestion parsing may retain
documented format-specific resource limits, which must be explicit operational
limits rather than a claim of identical OpenAI service behavior. Preserve safe
filenames, generated storage IDs, atomic publication, cancellation cleanup, and
legacy inline-file reads. Emit `response.output_text.annotation.added` for file
citations with correct item/content/index fields, consistent with final output.

Conformance is scoped to implemented Files/Vector Stores/file-search contracts,
not the entire OpenAI platform or equality with its proprietary retrieval models.
Add independently derived HTTP/SDK contract tests and an explicit compatibility
matrix. Do not infer conformance merely from self-generated OpenAPI schemas.

## Global constraints

- Rust edition: 2024; minimum supported Rust version (MSRV): 1.85. The existing
  optional PDF feature requires Rust 1.88.
- Public request/response/tool boundaries use typed Rust, not loose JSON.
- Core owns business logic and storage; handlers own HTTP transport.
- No unsafe code; no locks held across await; bound blocking work and resources.
- Preserve local filesystem file storage and existing SQL data.
- Tests must exercise real service behavior; mock only external model endpoints.
- Read cassette recorder instructions before any captured replay fixture changes.
