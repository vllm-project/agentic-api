# Files, vector stores, and file search

Agentic API stores files, chunks, and vectors in its SQLite or PostgreSQL database.
Its built-in `file_search` tool retrieves document passages and supplies them to
the model within the Responses tool loop. An OGX service is not required.

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

UTF-8 text, Markdown, CSV, JSON, source files, and other supported text formats
work in the default build. Uploads are limited to 20 MiB and must use purpose
`assistants` or `user_data`. The default chunk size is 800 tokens with a 400-token
overlap. Override it with:

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
PDFs must contain extractable text; scanned PDFs require OCR before upload.
Encrypted PDFs and documents exceeding parsing, decompression, or extracted-text
limits are rejected. The default build returns an actionable error for PDFs.

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
original upload. Deleting an upload removes its attachments and chunks from all
stores. Deleting a vector store preserves uploaded files.

The routes use the gateway's configured authentication policy. This first
implementation uses bounded exact retrieval over SQL storage. It is intended for
small corpora and returns explicit capacity errors when limits are exceeded.
Each store is limited to 10,000 chunks and 64 MiB of serialized chunk data,
including embeddings; ingestion enforces these limits before publication.
Searching multiple stores shares the same aggregate retrieval budget.
It does not implement OGX's provider catalog, contextual chunking, query rewriting,
neural reranking, or asynchronous file batches.
