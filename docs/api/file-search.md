# Files, vector stores, and file search

Agentic API stores uploaded file bytes on the local filesystem. File metadata,
vector stores, chunks, and vectors use its SQLite or PostgreSQL database. The
Files and vector-store HTTP APIs can ingest and search documents directly.
The Responses `file_search` built-in tool uses the same service to supply
retrieved passages to the model.

## Configure local file storage

The Files API saves uploaded bytes under `~/.agentic-api/files` by default, or
`$AGENTIC_API_HOME/files` when `AGENTIC_API_HOME` is set. Configure another directory
in `config.toml`:

```toml
[files]
storage_dir = "/var/lib/agentic-api/files"
```

`AGENTIC_FILES_STORAGE_DIR` overrides this setting. Use an absolute path. The
directory is created when needed for the first upload. Files are stored under
generated file IDs; uploaded filenames remain metadata and never select a filesystem path.

Use a persistent writable volume for container deployments. Restoring or moving
an installation requires both the database and the files directory. Multiple
replicas must share the same database and files directory; independent local
disks are suitable for a single replica.

## Configure embeddings

With no embedding configuration, ingestion and search use keyword retrieval.
For semantic and hybrid retrieval, configure an OpenAI-compatible embeddings
endpoint in `~/.agentic-api/config.toml`:

```toml
[file_search]
embedding_base_url = "http://localhost:8001/v1"
embedding_model = "your-embedding-model"
api_key_env = "EMBEDDING_API_KEY"
```

The endpoint receives `POST /v1/embeddings`; include `/v1` in the base URL if your
provider requires it. The key environment variable is optional. Environment
variables `AGENTIC_FILE_SEARCH_EMBEDDING_BASE_URL`,
`AGENTIC_FILE_SEARCH_EMBEDDING_MODEL`, and `AGENTIC_FILE_SEARCH_EMBEDDING_API_KEY`
override these settings. The generation model and embedding model can be served
by separate processes.

A vector store records its search provider identity, embedding endpoint, model,
and vector dimensions. Changing the provider or embedding configuration requires
a new store and reingestion. The default SQL repository preserves the identities
of stores created before provider injection was added.

### Use sqlite-vec for semantic search

For a file-backed SQLite deployment, install a trusted [sqlite-vec loadable
extension](https://github.com/asg017/sqlite-vec) and set its absolute path:

```toml
[file_search]
sqlite_vec_extension_path = "/opt/sqlite-vec/vec0.so"
```

`AGENTIC_FILE_SEARCH_SQLITE_VEC_EXTENSION_PATH` overrides the configuration
file. The gateway loads this binary at startup, so keep it under deployment
control. The SQLite vector index stores unit-normalized float32 embeddings and
uses cosine distance for semantic search. Keyword and hybrid modes use the
bounded exact SQL ranker. SQLite-vec is optional; without an extension path,
all search modes use the SQL repository unless pgvector is configured. Existing vector stores retain their
provider identity and must be reingested to change providers.

## Integrate storage providers

`FileSearchService::new` uses the existing SQLite or PostgreSQL pool for file
metadata, vector stores, attachments, and exact chunk retrieval, plus the local
filesystem for uploaded bytes. A persistent volume claim (PVC) mounted at the
configured files directory uses this same local-files implementation.

Rust integrations can instead supply implementations of `FileSearchRepository`
and `FileBlobStore` through `FileSearchService::with_providers`, then wrap the
service with `FileHandler::new` and assign it to `ExecutionContext.file_handler`.
The HTTP routes and Responses built-in tool use that handler. A repository adapter
can compose SQL metadata with Milvus or another vector index; a blob-store
adapter can use S3, GCS, or Azure Blob Storage. This layer provides SQL and
sqlite-vec and pgvector repositories and the local blob store. Other adapters and a
configuration-file provider catalog are follow-up work.
Repository adapters expose their shared `FileSearchStorage` through `metadata()`;
trait defaults forward metadata and blob-cleanup operations there. Adapters
implement index identity, publication, and search, and override deletion when
index cleanup needs extra coordination (as sqlite-vec does).

Repository implementations must publish an attachment only after its chunks
are searchable, keep store embedding identities and dimensions compatible, and
remove indexed chunks on deletion. When metadata and an external index cannot
share a transaction, the adapter needs a durable recovery or compensation path
for partial publication and deletion. Blob stores must create objects under
generated file IDs without replacing existing bytes, validate bounded reads,
and make deletion idempotent. The service retains a local metadata fallback for
uploads stored inline by an earlier draft.
Each external repository supplies a stable, nonempty `index_identity`; changing
it makes existing vector stores incompatible until they are reingested.
Switching blob-store providers also requires migrating the existing uploaded
objects or pointing the new provider at the same object namespace.

## Upload and ingest

```bash
curl http://localhost:8080/v1/files \
  -F purpose=assistants \
  -F file=@handbook.md

curl http://localhost:8080/v1/vector_stores \
  -H 'Content-Type: application/json' \
  -d '{"name":"handbooks"}'

curl http://localhost:8080/v1/vector_stores/vs_REPLACE/files \
  -H 'Content-Type: application/json' \
  -d '{"file_id":"file_REPLACE","attributes":{"department":"engineering"}}'
```

Use the returned IDs in later requests. A successful attachment is fully ingested;
failed or cancelled ingestion publishes no partial chunks. Creating a store with
`file_ids` ingests those files before publishing the store. Attachments preserve
the uploaded file ID, filename, attributes, and chunking strategy.

The Files API accepts binary uploads independently of search ingestion. Files
are limited to 20 MiB, and the configured gateway request-body limit also applies
to multipart uploads and vector-store JSON requests. The default request-body
limit is 10 MiB; raise `server.max_request_body_size_bytes` for larger uploads.
Uploads accept the OpenAI purposes `fine-tune`, `assistants`, `batch`, `user_data`, `vision`, and
`evals`. Use `vision` for images referenced from Responses input; a `vision` upload must use an
image media type (`image/gif`, `image/jpeg`, `image/jpg`, `image/png`, or `image/webp`). The
`GET /v1/files/{file_id}/content` route serves `vision` uploads only; another purpose returns 400
`Not allowed to download files of purpose: <purpose>`, matching the recorded OpenAI rows. Attaching a
file to a vector store validates its format: UTF-8 text, Markdown, CSV, JSON, source
files, and other supported text formats work in the default build. The default
chunk size is 800 tokens with a 400-token overlap. Override it with:

```json
{
  "file_id": "file-REPLACE",
  "chunking_strategy": {
    "type": "static",
    "static": {"max_chunk_size_tokens": 512, "chunk_overlap_tokens": 128}
  }
}
```

Chunk counts use `cl100k_base` tokenization in bounded UTF-8 blocks. Token boundaries
can differ slightly from tokenizing an entire document at once. Ingestion rejects
files exceeding 2,048 chunks or 16 MiB of extracted text.

PDF ingestion is an optional build feature:

```bash
cargo build -p agentic-server --features file-search-pdf
```

This feature requires Rust 1.88 or newer for the parser's decompression limits.
PDFs must contain extractable text; scanned PDFs require OCR before ingestion.
Encrypted PDFs and documents exceeding parsing, decompression, or extracted-text
limits are rejected. The default build can store and download PDFs, but returns
an actionable error when a PDF is attached for ingestion.

## Reference uploads from Responses input

A message `input_image` can reference an uploaded image with `file_id` instead of an
inline `image_url`, and a message `input_file` can reference an uploaded text file with
`file_id` instead of inline `file_data`. When the gateway builds the model-facing request,
it reads the file from the same store and substitutes the model-facing content: the text
for an `input_file`, a base64 data URL for an `input_image`. Stored response
and conversation history keep the client's reference, so each continuation resolves it
again; deleting the file makes a later continuation fail with 404. Compaction windows and
multi-agent tree snapshots keep the same client-facing item, including the reference.

An `input_image` reference must resolve to a stored file whose filename has an accepted
image extension: `.jpeg`, `.jpg`, `.png`, `.gif`, or `.webp`; upload those files with
purpose `vision`. An `input_file` reference must resolve to a stored file whose filename
passes the text filename rules (a stored file carries no client-visible media type), and
its content must decode as UTF-8; the stored filename supplies the `File: <name>` prefix.
A part must use `file_id` or its inline field, not both. A missing file returns 404
`invalid_request_error` with `Files [<id>] were not found`; a wrong file type returns 400
`invalid_request_error`. `file_url` is never fetched.

Resolved content counts toward the same request body limit
(`server.max_request_body_size_bytes`) as inline content. Exceeding the configured limit
returns HTTP 413 with error code `body_too_large`.

## Search directly

```bash
curl http://localhost:8080/v1/vector_stores/vs_REPLACE/search \
  -H 'Content-Type: application/json' \
  -d '{
    "query":"How do we request leave?",
    "search_mode":"hybrid",
    "max_num_results":5,
    "filters":{"type":"eq","key":"department","value":"engineering"},
    "ranking_options":{"score_threshold":0.2}
  }'
```

`query` accepts a string or a list of strings. `search_mode` accepts `semantic`
(`vector` is also accepted), `keyword`, or `hybrid`. The default is hybrid when
embeddings are configured and keyword otherwise. Semantic retrieval uses cosine
similarity; keyword retrieval uses BM25; hybrid retrieval combines ranked lists.
Results include file ID, filename, attributes, score, and text content.

Filters support `eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `in`, and `nin`, combined with
nested `and` and `or` filters. Comparisons use the attribute's actual string,
number, or boolean type. Result limits range from 1 to 50 and apply globally
across queries and selected stores.

## Use the Responses built-in tool

```json
{
  "model": "your-generation-model",
  "input": "What is our leave policy?",
  "tools": [{
    "type": "file_search",
    "vector_store_ids": ["vs_REPLACE"],
    "max_num_results": 5
  }],
  "tool_choice": {"type": "file_search"},
  "include": ["file_search_call.results"]
}
```

The model-facing declaration is normalized to a function and executed inside
Agentic API. Public output contains `file_search_call`; retrieved passages appear
there only when `file_search_call.results` is included. Private tool call outputs
remain available for stored continuation. The tool works alongside client-executed
functions and supports `previous_response_id` continuation.

Answers may include `【file-id】` citation markers. Valid markers produce typed
`file_citation` annotations containing the uploaded file ID and filename. A source
is annotated only when the model cites a retrieved file. Streaming keeps text
deltas and final text identical, and includes annotations on completed content
parts, output items, and the terminal response.
Citation validation applies to gateway-owned `file_search` execution. Native upstream
annotations are preserved when the gateway does not own that tool, including when a
client function happens to be named `file_search`.

## Manage stored data

| Operation | Route |
|---|---|
| Upload/list files | `POST` / `GET /v1/files` |
| Retrieve/delete file metadata | `GET` / `DELETE /v1/files/{file_id}` |
| Download original bytes | `GET /v1/files/{file_id}/content` |
| Create/list vector stores | `POST` / `GET /v1/vector_stores` |
| Retrieve/delete a vector store | `GET` / `DELETE /v1/vector_stores/{store_id}` |
| Attach/list files in a store | `POST` / `GET /v1/vector_stores/{store_id}/files` |
| Retrieve/detach a store file | `GET` / `DELETE /v1/vector_stores/{store_id}/files/{file_id}` |
| Search a store | `POST /v1/vector_stores/{store_id}/search` |

Lists accept `limit`, `after`, `before`, and `order`; `GET /v1/files` additionally accepts
`purpose` to return only files uploaded for that purpose. Detaching a file preserves the
original upload. Deleting an upload removes its metadata, attachments, and chunks
from all stores, then removes its local file bytes. Deleting a vector store
preserves uploaded files.

Uploads publish complete, synced files before committing metadata. Failures and
cancellation before commit clean up the upload. Filesystem and SQL commits are
separate: a process crash, failed unlink, or uncertain database commit can leave
unreferenced files. Automatic orphan-file cleanup is not included. A missing or
damaged file referenced by metadata returns a storage error instead of a partial
download. Uploads stored inline by an earlier draft remain readable.

The routes use the gateway's configured authentication policy. Retrieval is
bounded and intended for small corpora; it returns capacity errors when limits
are exceeded.
Each store is limited to 10,000 chunks and 64 MiB of serialized chunk data
with the default exact-search provider, or 256 MiB with sqlite-vec.
These limits include embeddings; ingestion enforces them before publication.
Searching multiple stores shares the same aggregate retrieval budget.
It does not implement OGX's provider catalog, contextual chunking, query rewriting,
neural reranking, or asynchronous file batches.

## PostgreSQL pgvector index

For PostgreSQL deployments, enable [pgvector 0.8.0 or later](https://github.com/pgvector/pgvector)
with a typed configuration in `config.toml`:

```toml
[file_search.pgvector]
dimensions = 768
candidate_limit = 100

[file_search.pgvector.index]
type = "hnsw"
m = 16
ef_construction = 64
ef_search = 100
```

Keep the embedding endpoint and model under `[file_search]` as above. Configure
only one of `pgvector` and `sqlite_vec_extension_path`. `PgvectorRepository`
implements `FileSearchRepository`, so Rust integrations can also inject it with
any `FileBlobStore`. The Files API, vector-store API, and Responses tool share
this adapter and their existing wire contracts.

Dimensions must be 1–2000. Embeddings must match that dimension and contain
finite float32-representable values with a nonzero, normal float32 component;
invalid embeddings fail before attachment publication. `candidate_limit` is
50–1000 per query and retrieval leg. HNSW accepts `m` 2–100,
`ef_construction` 4–1000 and at least twice `m`, and `ef_search` 1–1000.
For IVFFlat, replace the index section:

```toml
[file_search.pgvector.index]
type = "ivfflat"
lists = 100
probes = 10
```

`lists` must be 2–32768 and `probes` 1–`lists - 1`. The adapter waits for at
least `lists * 1000` vectors of the configured dimension before building
IVFFlat. Until then it uses bounded exact distance selection in PostgreSQL.
Newly sufficient data triggers index construction on the next semantic or hybrid
search. Both index methods use iterative scans for filtered retrieval.

Semantic candidates use cosine distance; keyword candidates use PostgreSQL's
`simple` full-text configuration. Hybrid search combines both candidate sets.
Typed attribute filters are applied in SQL before candidate limits, with binary
string ordering matching the shared Rust filter. The shared cosine/BM25/fusion
ranker then applies deduplication, score thresholds, and the global result limit.
ANN retrieval and candidate-bounded keyword scoring can differ from full-corpus
exact ranking; increasing recall settings and candidate limits can help. Selected
candidate reads are limited to 64 MiB and 10,000 rows across all query legs.
Per-store ingestion retains the SQL provider's 64 MiB and 10,000-chunk limits.

### Startup and existing data

The PostgreSQL backend schema step in `storage/schema/pgvector.rs` creates the
optional `vector` extension, a stored generated vector projection of chunk data
(unit-normalized after float32 conversion), a full-text GIN index, and one ANN
index per configured dimension. This is an optional backend schema step after
portable migrations; SQLite and unconfigured PostgreSQL databases do not require
the extension.
Projection updates and cascading deletion occur in the same transaction as
metadata, so failed publication cannot expose partial attachments.

The database role needs extension, table-alteration, and index-creation privileges;
managed services may require an administrator to install `vector` first.
With `AGENTIC_API_SCHEMA_READY`, startup only verifies the extension version,
generated projection, GIN index, and configured ANN index. It rejects missing or
incompatible objects without changing them. Operators must prepare these objects
with the same index construction settings before enabling supervisor mode.
An untrained IVFFlat configuration may use exact distance retrieval below its
training floor; once it reaches the floor, the operator must build its index.
Supervisor-managed searches never create or rebuild indexes.

Projection backfill and index construction acquire database locks and can be
expensive on populated databases. Plan an initialization window. Index changes
rebuild the selected dimension's index transactionally, so replicas sharing a
database must use identical index construction settings. Established searches do
not hold the setup advisory lock. Increasing only search-time settings does not
rebuild the index.

The generated column backfills existing chunk rows without changing their JSON.
Vector stores retain their provider identity: recreate and reingest stores when
switching from SQL exact search or sqlite-vec, or changing pgvector dimensions.
Changing HNSW/IVFFlat construction or recall settings preserves store identity.
