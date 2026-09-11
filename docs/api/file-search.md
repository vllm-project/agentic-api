# Files, vector stores, and file search

Agentic API stores uploaded file bytes on the local filesystem. File metadata,
vector stores, chunks, and vectors use its SQLite or PostgreSQL database.
Its built-in `file_search` tool retrieves document passages and supplies them to
the model within the Responses tool loop. An OGX service is not required.

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

A vector store records its embedding endpoint, model, and vector dimensions.
Changing that configuration requires a new store and reingestion for semantic
search. Existing stores remain available for keyword search.

## Model-assisted retrieval configuration

Use `file_search.vector_stores` to group independently configured model providers.
Requests select allowlisted models; they cannot supply endpoints or credentials.
Qualified selectors split once into provider ID and provider-local model name, so
`local/org/rerank` sends model `org/rerank` to provider `local`. Unqualified selectors
use `default_provider_id`. Unknown providers and models fail before model calls.

```toml
[file_search.vector_stores]
default_provider_id = "local"

[file_search.vector_stores.providers.local]
base_url = "http://localhost:8001/v1"
models = ["embedding-model", "context-model", "org/rerank"]
api_key_env = "LOCAL_RETRIEVAL_KEY"
protocol = "vllm"
score_interpretation = "probability"

[file_search.vector_stores.default_embedding_model]
provider_id = "local"
model_id = "embedding-model"
embedding_dimensions = 768

[file_search.vector_stores.default_reranker_model]
provider_id = "local"
model_id = "org/rerank"

[file_search.vector_stores.contextual_retrieval_params]
model = { provider_id = "local", model_id = "context-model" }
default_timeout_seconds = 120
default_max_concurrency = 3
max_document_tokens = 100000

[file_search.vector_stores.rewrite_query_params]
model = { provider_id = "local", model_id = "context-model" }
max_tokens = 100
temperature = 0.0

[file_search.vector_stores.chunk_retrieval_params]
chunk_multiplier = 5
max_tokens_in_context = 4000
default_reranker_strategy = "rrf"
rrf_impact_factor = 60.0
weighted_search_alpha = 0.5
# Optional: default_search_mode = "vector"
```

Grouped embeddings and the legacy embedding connection are alternative forms;
configuring both is an error. Existing legacy environment overrides still apply.
Each provider's secret is resolved from its `api_key_env`, redacted in diagnostics,
and omitted from serialized configuration. Credentials are never inherited from
the Responses model. Different providers can serve each operation. The supported
`vllm` text protocol uses embeddings and Chat Completions below the base URL and
`/rerank` after removing a trailing `/v1`.

Absent explicit defaults preserve keyword-only local setup and hybrid search when
embeddings exist. `file_ingestion_params.default_chunk_size_tokens` and
`default_chunk_overlap_tokens` configure auto ingestion; defaults preserve native
800/400 behavior. Static overlap remains limited to half the chunk size.
`file_batch_params` validates `max_concurrent_files_per_batch` (default 3, range
1–32), `file_batch_chunk_size` (10, 1–1000), and `cleanup_interval_seconds` (86400,
1–604800); asynchronous workers are a later layer.

### Contextual ingestion

```json
{
  "file_id": "file-...",
  "chunking_strategy": {
    "type": "contextual",
    "contextual": {
      "model_id": "local/context-model",
      "max_chunk_size_tokens": 700,
      "chunk_overlap_tokens": 400
    }
  }
}
```

The nested `contextual` object is required; all its fields have defaults. Chunk
size is 100–4096; contextual overlap must be strictly less than chunk size.
`model_id` overrides the configured contextual model. `timeout_seconds` (1–600)
overrides the deployment timeout (10–600, default 120). `max_concurrency` (1–32)
can reduce per-ingestion concurrency, still capped by the deployment's shared
contextual semaphore (1–32, default 3). Document size uses a character-count/4 token
estimate, bounded by `max_document_tokens` (1000–1000000) and existing extraction
byte limits. Optional `context_prompt` must contain `{{WHOLE_DOCUMENT}}` before
`{{CHUNK_CONTENT}}` and fit 16 KiB.

The model receives the document as a shared system prefix and the chunk in a user
message, with temperature zero and at most 256 output tokens. Nonempty context is
prepended to the embedding input and stored separately as `embedding_text`.
Returned/cited source text stays original. Every context call must succeed before
embedding or publishing the attachment; partial failure, timeout, cancellation,
or malformed/empty output publishes nothing. Contextual ingestion requires
embeddings. Calls have no retries or silent fallback. Model request bodies are
limited to 32 MiB, chat/rerank responses to 1 MiB, contextual output to 8 KiB, and
rewritten queries to 4096 bytes. Rewrite and rerank calls time out after 45 seconds.

### Rewrite and ranking semantics

Direct search and the Responses tool declaration accept `rewrite_query` and
`search_mode`. Rewriting joins input queries with spaces and makes one model call
before retrieval. `search_query` (tool `queries`) contains the single rewritten
query; without rewriting, established multi-query retrieval and deduplication
apply. Rewrite `temperature` includes explicit zero (range 0–2), `max_tokens` is
1–4096, and optional `prompt` must include `{query}` and fit 16 KiB. Missing
configuration, failed calls, and empty output fail the search.

`ranking_options.ranker` accepts `auto`, `none`, `rrf`, `normalized`, `weighted`,
`neural`, and `classifier`. `auto` uses the configured strategy; `none` bypasses
model reranking while retaining the selected retrieval mode's base ranking.
`normalized` aliases normalized RRF. `weighted` min-max normalizes each present
score list and combines vector proportion `alpha` with keyword proportion
`1-alpha`. Equal nonempty scores normalize to one; absent scores normalize to zero.
RRF uses one-based ranks with configurable `impact_factor` (0–10000) and scales
scores to [0,1]. Explicit `weights` contains nonnegative `vector` and `keyword`
values summing to one. Existing `hybrid_search.embedding_weight`/`text_weight`
remain supported; do not combine these two weight forms. Explicit parameters
override deployment defaults. Fusion weights require hybrid mode. Neural score
blending is not supported.

OpenAI selectors `default-2024-11-15` (vector store search) and
`default_2024_08_21` (Responses file search) select the configured model ranker;
they do not reproduce OpenAI's hosted models or scores. `ranking_options.model`
overrides `default_reranker_model` for both neural and classifier ranking. Missing
model configuration is an error. Both use vLLM/Cohere-style text reranking
(`documents`, `top_n`, `results`) before final truncation. The deployment
`chunk_multiplier` (1–20) expands the initial result count within existing
pgvector aggregate candidate limits. No additional corpus data is loaded to
refill results. Filters and store isolation apply before provider calls.

Reranker indexes must form a complete unique permutation of bounded candidates.
Scores must be finite. `score_interpretation = "probability"` requires [0,1];
`"logit"` applies a numerically stable sigmoid. Arbitrary scores are never clipped.
Model scores are implementation-specific: select the interpretation for the model
being deployed. `score_threshold` (0–1) applies to final model scores for both
neural and classifier ranking, before the final result limit. Zero initial
similarity does not exclude an otherwise selected model-reranking candidate.
Without model reranking, the threshold applies to base retrieval scores.
Provider errors return no partial search response.

The Responses tool keeps whole source chunks within `max_tokens_in_context`
(1–32768, default 4000), using bounded `cl100k_base` token counting. Chunks that do
not fit are omitted. Direct search is independent of that context budget.
Citation item types and file IDs remain unchanged.

## Select PostgreSQL indexed retrieval

The default `exact` backend works with SQLite and plain PostgreSQL. To use
pgvector, configure PostgreSQL as the database and add this deployment setting
alongside the embedding configuration above:

```toml
[file_search.backend]
type = "pgvector"
dimensions = 768
candidate_limit = 100

[file_search.backend.index]
type = "hnsw"
m = 16
ef_construction = 64
ef_search = 100
```

The pgvector extension must be version 0.8.0 or later. Provision it with
`CREATE EXTENSION vector` as a database administrator. Startup verifies the
extension and initializes the optional projection and indexes; the runtime role
needs schema/table/index modification permissions. Selecting pgvector on SQLite
or without configured embeddings is a configuration error. `dimensions` must
match the provider and existing stores, between 1 and 2000. Embeddings must be
finite, nonzero float32 vectors. Deployment settings never accept client-supplied
endpoints or credentials.

For IVFFlat, replace the index table with:

```toml
[file_search.backend.index]
type = "ivfflat"
lists = 100
probes = 10
```

`lists` must be 2–32768 and `probes` 1–`lists - 1`. HNSW accepts `m` 2–100,
`ef_construction` 4–1000 and at least twice `m`, and `ef_search` 1–1000.
`candidate_limit` is 50–1000 per query and retrieval method. Queries use cosine
distance operators, SQL store/attribute filters, and transaction-local search
settings with iterative scans. Established-index retrieval does not acquire the
schema initialization lock; index maintenance commits before candidate queries.
ANN recall depends on index/search settings;
PostgreSQL can choose an exact scan for small or selective corpora. Keyword
candidates use a `simple` text-search GIN index; the shared ranker applies BM25
and hybrid fusion to the bounded candidate union before the final result limit.
This candidate selection can differ from portable full-corpus BM25. The aggregate
candidate transfer is capped at 64 MiB and 10000 rows across queries and methods;
exceeding either bound returns a resource-limit error.

The optional vector column is generated from the existing chunk row, so legacy
vectors are backfilled when it is installed. Metadata and vector publication,
rollback, updates, and cascading deletion remain one database transaction.
Extension setup does not modify the portable migrations. Back up first and plan
for a table lock during initial projection creation or index replacement.
Incompatible legacy vectors cause initialization to fail without partial schema
publication. Restart preserves indexes; changing construction settings atomically
replaces the selected index for that dimension. All replicas sharing a database
must use the same index construction settings. HNSW is suitable for empty stores. IVFFlat training waits for at least
`lists * 1000` rows of the configured dimension, matching OGX; until then queries
use bounded SQL cosine retrieval without an ANN index. Search creates the index
when sufficient rows exist. IVFFlat may need
`REINDEX INDEX file_search_vector_768` after substantial corpus growth.

To return to portable retrieval, configure `[file_search.backend]` with
`type = "exact"`. Existing projected columns and indexes remain consistent.

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
  -d '{"file_id":"file-REPLACE","attributes":{"department":"engineering"}}'
```

Use the returned IDs in later requests. A successful attachment is fully ingested;
failed or cancelled ingestion publishes no partial chunks. Creating a store with
`file_ids` ingests those files before publishing the store. Attachments preserve
the uploaded file ID, filename, attributes, and chunking strategy.

The Files API accepts binary uploads independently of search ingestion. Uploads
are limited to 20 MiB and must use purpose `assistants` or `user_data`. Attaching a
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
number, or boolean type. String ranges use UTF-8 byte ordering on both backends,
independently of the PostgreSQL database collation. Result limits range from 1 to 50 and apply globally
across queries and, for the built-in tool, selected stores.

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

Lists accept `limit`, `after`, `before`, and `order`. Detaching a file preserves the
original upload. Deleting an upload removes its metadata, attachments, and chunks
from all stores, then removes its local file bytes. Deleting a vector store
preserves uploaded files.

Uploads publish complete, synced files before committing metadata. Failures and
cancellation before commit clean up the upload. Filesystem and SQL commits are
separate: a process crash, failed unlink, or uncertain database commit can leave
unreferenced files. Automatic orphan-file cleanup is not included. A missing or
damaged file referenced by metadata returns a storage error instead of a partial
download. Uploads stored inline by an earlier draft remain readable.

The routes use the gateway's configured authentication policy. Retrieval uses bounded exact SQL or the configured pgvector backend and returns
explicit capacity errors when limits are exceeded.
Each store is limited to 10,000 chunks and 64 MiB of serialized chunk data,
including embeddings; ingestion enforces these limits before publication.
Searching multiple stores shares the same aggregate retrieval budget.
It does not expose OGX's provider catalog or asynchronous file batches.
