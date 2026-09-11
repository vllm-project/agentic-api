//! Local HTTP contracts for model-assisted retrieval; never calls external models.
use agentic_core::{storage::create_pool_with_schema, tool::file_search::FileSearchService, types::file_search::*};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Fixture {
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    rerank_response: Arc<Mutex<Option<Value>>>,
    rerank_pause: Arc<Mutex<bool>>,
    rerank_started: Arc<tokio::sync::Notify>,
    rerank_resume: Arc<tokio::sync::Notify>,
    chat_fail: Arc<Mutex<bool>>,
    chat_fail_after: Arc<Mutex<Option<usize>>>,
    chat_response: Arc<Mutex<Option<Value>>>,
    chat_wait: Arc<Mutex<bool>>,
    chat_started: Arc<tokio::sync::Notify>,
    authorization: Arc<Mutex<Vec<String>>>,
}
async fn chat(
    State(state): State<Fixture>,
    headers: axum::http::HeaderMap,
    Json(input): Json<Value>,
) -> (StatusCode, Json<Value>) {
    state.authorization.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
    );
    state.requests.lock().unwrap().push(("chat".into(), input.clone()));
    state.chat_started.notify_one();
    let wait = *state.chat_wait.lock().unwrap();
    if wait {
        std::future::pending::<()>().await;
    }
    let count = state
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(kind, _)| kind == "chat")
        .count();
    if state.chat_fail_after.lock().unwrap().is_some_and(|limit| count > limit) {
        return (StatusCode::BAD_GATEWAY, Json(json!({"error":"private-secret"})));
    }
    if let Some(response) = state.chat_response.lock().unwrap().clone() {
        return (StatusCode::OK, Json(response));
    }
    if *state.chat_fail.lock().unwrap() {
        return (StatusCode::BAD_GATEWAY, Json(json!({"error":"private-secret"})));
    }
    let context = input["messages"].as_array().unwrap().len() == 2;
    (
        StatusCode::OK,
        Json(json!({"choices":[{"message":{"content": if context {"aquatic context"} else {"coral"}}}]})),
    )
}
async fn embed(State(state): State<Fixture>, headers: axum::http::HeaderMap, Json(input): Json<Value>) -> Json<Value> {
    state.authorization.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
    );
    state.requests.lock().unwrap().push(("embed".into(), input.clone()));
    let data: Vec<_> = input["input"].as_array().unwrap().iter().enumerate().map(|(index,text)| {
        let text = text.as_str().unwrap();
        json!({"index":index,"embedding":if text.contains("aquatic") || text=="ocean coral" {vec![1.0,0.0]} else {vec![0.0,1.0]}})
    }).collect();
    Json(json!({"model":input["model"],"data":data}))
}
async fn rerank(State(state): State<Fixture>, headers: axum::http::HeaderMap, Json(input): Json<Value>) -> Json<Value> {
    state.authorization.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
    );
    state.requests.lock().unwrap().push(("rerank".into(), input.clone()));
    let pause = *state.rerank_pause.lock().unwrap();
    if pause {
        state.rerank_started.notify_one();
        state.rerank_resume.notified().await;
    }
    if let Some(output) = state.rerank_response.lock().unwrap().clone() {
        return Json(output);
    }
    let data: Vec<_> = input["documents"].as_array().unwrap().iter().enumerate().map(|(index,text)| {
        json!({"index":index,"relevance_score": if text.as_str().unwrap().contains("preferred") {0.95} else {0.1}})
    }).collect();
    Json(json!({"results":data}))
}
struct Setup {
    service: FileSearchService,
    state: Fixture,
    config: FileSearchConfig,
    pool: Arc<agentic_core::storage::DbPool>,
    _files: tempfile::TempDir,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Setup {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn setup(embedding: bool) -> Setup {
    let state = Fixture::default();
    let app = axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(chat))
        .route("/v1/embeddings", axum::routing::post(embed))
        .route("/rerank", axum::routing::post(rerank))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let files = tempfile::tempdir().unwrap();
    let mut config: FileSearchConfig = serde_json::from_value(json!({"vector_stores":{
        "default_provider_id":"local",
        "providers":{"local":{"base_url":base,"models":["embed","chat","org/rerank"]}},
        "default_reranker_model":{"provider_id":"local","model_id":"org/rerank"},
        "contextual_retrieval_params":{"model":{"provider_id":"local","model_id":"chat"}},
        "rewrite_query_params":{"model":{"provider_id":"local","model_id":"chat"},"temperature":0.0}
    }}))
    .unwrap();
    config.files_storage_dir = Some(files.path().to_owned());
    if embedding {
        // Exercise grouped embedding routing, including provider-local model names.
        let mut wire = serde_json::to_value(&config).unwrap();
        wire["vector_stores"]["default_embedding_model"] =
            json!({"provider_id":"local","model_id":"embed","embedding_dimensions":2});
        config = serde_json::from_value(wire).unwrap();
    }
    let pool = create_pool_with_schema(Some("sqlite::memory:")).await.unwrap();
    let service = FileSearchService::new(
        pool.clone(),
        Arc::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        ),
        config.clone(),
    )
    .unwrap();
    Setup {
        service,
        state,
        config,
        pool,
        _files: files,
        task,
    }
}
async fn attach(service: &FileSearchService, store: &str, text: &str, strategy: Option<ChunkingStrategy>) -> String {
    let file = service
        .upload_file("source.txt", "text/plain", "assistants", text.as_bytes().to_vec())
        .await
        .unwrap();
    service
        .attach_file(
            store,
            AttachFileRequest {
                file_id: file.id.clone(),
                chunking_strategy: strategy,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    file.id
}
fn query(text: &str) -> SearchRequest {
    SearchRequest {
        query: SearchQuery::Text(text.into()),
        ..Default::default()
    }
}
fn contextual() -> ChunkingStrategy {
    serde_json::from_value(json!({"type":"contextual","contextual":{}})).unwrap()
}

#[tokio::test]
async fn contextual_embedding_changes_retrieval_but_preserves_original_source() {
    let setup = setup(true).await;
    let store = setup
        .service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let id = attach(&setup.service, &store.id, "unadorned source", Some(contextual())).await;
    let result = setup
        .service
        .search(
            &[store.id],
            &SearchRequest {
                search_mode: Some(SearchMode::Semantic),
                ..query("aquatic")
            },
        )
        .await
        .unwrap();
    assert_eq!(result.data[0].file_id, id);
    assert_eq!(result.data[0].content[0].text, "unadorned source");
    {
        let requests = setup.state.requests.lock().unwrap();
        let embedded = requests.iter().find(|(kind, _)| kind == "embed").unwrap();
        assert_eq!(embedded.1["input"][0], "aquatic context\n\nunadorned source");
    }
    let persisted: String = sqlx::query_scalar("SELECT data FROM file_search_chunks LIMIT 1")
        .fetch_one(setup.pool.as_ref())
        .await
        .unwrap();
    let persisted: Value = serde_json::from_str(&persisted).unwrap();
    assert_eq!(persisted["text"], "unadorned source");
    assert_eq!(persisted["embedding_text"], "aquatic context\n\nunadorned source");
}

#[tokio::test]
async fn rewrite_changes_retrieval_and_reported_query_preserving_zero_temperature() {
    let setup = setup(false).await;
    let store = setup
        .service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let id = attach(&setup.service, &store.id, "coral reefs", None).await;
    assert!(
        setup
            .service
            .search(std::slice::from_ref(&store.id), &query("ocean"))
            .await
            .unwrap()
            .data
            .is_empty()
    );
    let result = setup
        .service
        .search(
            &[store.id],
            &SearchRequest {
                rewrite_query: true,
                query: SearchQuery::Texts(vec!["ocean".into(), "habitat".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.search_query, vec!["coral"]);
    assert_eq!(result.data[0].file_id, id);
    let requests = setup.state.requests.lock().unwrap();
    assert_eq!(requests[0].1["temperature"], 0.0);
    assert!(
        requests[0].1["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("ocean habitat")
    );
}

#[tokio::test]
async fn reranker_sees_candidates_before_truncation_and_none_bypasses_model() {
    let setup = setup(false).await;
    let store = setup
        .service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    attach(&setup.service, &store.id, "coral coral coral", None).await;
    let preferred = attach(
        &setup.service,
        &store.id,
        "coral preferred with many other words to reduce lexical score",
        None,
    )
    .await;
    for ranker in [
        "neural",
        "classifier",
        "default-2024-11-15",
        "default-2024-08-21",
        "default_2024_08_21",
    ] {
        let request: SearchRequest =
            serde_json::from_value(json!({"query":"coral","max_num_results":1,"ranking_options":{"ranker":ranker}}))
                .unwrap();
        let result = setup
            .service
            .search(std::slice::from_ref(&store.id), &request)
            .await
            .unwrap();
        assert_eq!(result.data[0].file_id, preferred);
        assert!((result.data[0].score - 0.95).abs() < f64::EPSILON);
    }
    let request: SearchRequest =
        serde_json::from_value(json!({"query":"coral","max_num_results":1,"ranking_options":{"ranker":"none"}}))
            .unwrap();
    setup.service.search(&[store.id], &request).await.unwrap();
    let requests = setup.state.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[0].1["model"], "org/rerank");
    assert_eq!(requests[0].1["documents"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn failed_context_is_atomic_and_rerank_protocol_errors_are_not_partial_results() {
    let setup = setup(true).await;
    let store = setup
        .service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    *setup.state.chat_fail.lock().unwrap() = true;
    let file = setup
        .service
        .upload_file("failed.txt", "text/plain", "assistants", b"failed source".to_vec())
        .await
        .unwrap();
    let error = setup
        .service
        .attach_file(
            &store.id,
            AttachFileRequest {
                file_id: file.id,
                chunking_strategy: Some(contextual()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.status_code(), 502);
    assert!(!error.public_message().contains("private-secret"));
    assert_eq!(
        setup
            .service
            .get_vector_store(&store.id)
            .await
            .unwrap()
            .file_counts
            .total,
        0
    );
    assert!(
        !setup
            .state
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(kind, _)| kind == "embed")
    );
    *setup.state.chat_fail.lock().unwrap() = false;
    attach(&setup.service, &store.id, "coral one", None).await;
    attach(&setup.service, &store.id, "coral two", None).await;
    for results in [
        json!([]),
        json!([{"index":0,"relevance_score":0.9},{"index":0,"relevance_score":0.2}]),
        json!([{"index":0,"relevance_score":0.9},{"index":2,"relevance_score":0.2}]),
        json!([{"index":0,"relevance_score":2.0},{"index":1,"relevance_score":0.2}]),
        json!([{"index":0,"relevance_score":-0.1},{"index":1,"relevance_score":0.2}]),
    ] {
        *setup.state.rerank_response.lock().unwrap() = Some(json!({"results":results}));
        let request: SearchRequest = serde_json::from_value(
            json!({"query":"coral","search_mode":"keyword","ranking_options":{"ranker":"neural"}}),
        )
        .unwrap();
        assert_eq!(
            setup
                .service
                .search(std::slice::from_ref(&store.id), &request)
                .await
                .unwrap_err()
                .status_code(),
            502
        );
    }
}

#[tokio::test]
async fn configured_ranker_mode_logit_scores_and_request_overrides_apply_consistently() {
    let setup = setup(false).await;
    let store = setup
        .service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let id = attach(&setup.service, &store.id, "coral", None).await;
    let mut config = setup.config.clone();
    config.vector_stores.chunk_retrieval_params.default_search_mode = Some(SearchMode::Keyword);
    config.vector_stores.chunk_retrieval_params.default_reranker_strategy = Ranker::Neural;
    config
        .vector_stores
        .providers
        .get_mut("local")
        .unwrap()
        .score_interpretation = ScoreInterpretation::Logit;
    let service = FileSearchService::new(setup.pool.clone(), Arc::new(reqwest::Client::new()), config).unwrap();
    for (raw, expected) in [(0.0, 0.5), (1000.0, 1.0), (-1000.0, 0.0)] {
        *setup.state.rerank_response.lock().unwrap() = Some(json!({"results":[{"index":0,"relevance_score":raw}]}));
        let result = service
            .search(std::slice::from_ref(&store.id), &query("coral"))
            .await
            .unwrap();
        assert_eq!(result.data[0].file_id, id);
        assert!((result.data[0].score - expected).abs() < f64::EPSILON);
    }
    let none: SearchRequest =
        serde_json::from_value(json!({"query":"coral","ranking_options":{"ranker":"none"}})).unwrap();
    service.search(std::slice::from_ref(&store.id), &none).await.unwrap();
    assert_eq!(setup.state.requests.lock().unwrap().len(), 3);
    let filtered: SearchRequest = serde_json::from_value(
        json!({"query":"coral","ranking_options":{"ranker":"classifier","score_threshold":0.1}}),
    )
    .unwrap();
    assert!(service.search(&[store.id], &filtered).await.unwrap().data.is_empty());
}

#[tokio::test]
async fn unknown_model_selection_fails_before_provider_calls() {
    let setup = setup(false).await;
    let store = setup
        .service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    attach(&setup.service, &store.id, "coral", None).await;
    for model in ["foreign/org/rerank", "local/missing", "https://127.0.0.1/evil"] {
        let request: SearchRequest =
            serde_json::from_value(json!({"query":"coral","ranking_options":{"ranker":"neural","model":model}}))
                .unwrap();
        assert_eq!(
            setup
                .service
                .search(std::slice::from_ref(&store.id), &request)
                .await
                .unwrap_err()
                .status_code(),
            400
        );
    }
    assert!(setup.state.requests.lock().unwrap().is_empty());
}

#[test]
fn grouped_configuration_and_contextual_bounds_fail_closed() {
    for params in [
        json!({"chunk_retrieval_params":{"chunk_multiplier":0}}),
        json!({"chunk_retrieval_params":{"weighted_search_alpha":2.0}}),
        json!({"file_batch_params":{"max_concurrent_files_per_batch":0}}),
        json!({"file_batch_params":{"file_batch_chunk_size":0}}),
        json!({"contextual_retrieval_params":{"default_max_concurrency":0}}),
        json!({"rewrite_query_params":{"temperature":-1.0}}),
        json!({"rewrite_query_params":{"prompt":"missing placeholder"}}),
        json!({"default_provider_id":"missing"}),
    ] {
        let config: VectorStoresConfig = serde_json::from_value(params).unwrap();
        assert!(config.validate().is_err());
    }
    for params in [
        json!({"chunk_overlap_tokens":700}),
        json!({"max_concurrency":0}),
        json!({"timeout_seconds":0}),
        json!({"context_prompt":"{{CHUNK_CONTENT}} {{WHOLE_DOCUMENT}}"}),
    ] {
        let config: ContextualChunking = serde_json::from_value(params).unwrap();
        assert!(config.validate().is_err());
    }
    ContextualChunking::default().validate().unwrap();
    assert!(
        serde_json::from_value::<VectorStoresConfig>(
            json!({"providers":{"local":{"base_url":"http://localhost/v1","models":["model"],"api_key":"secret"}}})
        )
        .is_err()
    );
}

#[tokio::test]
async fn deployment_and_request_fusion_weights_choose_different_evidence() {
    let setup = setup(true).await;
    let store = setup
        .service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let semantic = attach(&setup.service, &store.id, "aquatic habitat", None).await;
    let lexical = attach(&setup.service, &store.id, "coral reefs", None).await;
    let mut config = setup.config.clone();
    config.vector_stores.chunk_retrieval_params.default_reranker_strategy = Ranker::Weighted;
    config.vector_stores.chunk_retrieval_params.weighted_search_alpha = 1.0;
    let service = FileSearchService::new(setup.pool.clone(), Arc::new(reqwest::Client::new()), config).unwrap();
    let result = service
        .search(std::slice::from_ref(&store.id), &query("ocean coral"))
        .await
        .unwrap();
    assert_eq!(result.data[0].file_id, semantic);
    let request: SearchRequest = serde_json::from_value(
        json!({"query":"ocean coral","ranking_options":{"ranker":"weighted","weights":{"vector":0.0,"keyword":1.0}}}),
    )
    .unwrap();
    let result = service.search(&[store.id], &request).await.unwrap();
    assert_eq!(result.data[0].file_id, lexical);
}

#[tokio::test]
async fn responses_tool_honors_rewrite_ranking_defaults_and_context_token_budget() {
    use agentic_core::tool::{
        GatewayExecutor,
        file_search::{FileSearchExecutionParams, FileSearchHandler},
    };
    let setup = setup(false).await;
    let store = setup
        .service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    attach(
        &setup.service,
        &store.id,
        "coral preferred source with enough words to exceed the configured context budget",
        None,
    )
    .await;
    let mut config = setup.config.clone();
    config.vector_stores.chunk_retrieval_params.default_reranker_strategy = Ranker::Neural;
    config.vector_stores.chunk_retrieval_params.max_tokens_in_context = 2;
    let service = FileSearchService::new(setup.pool.clone(), Arc::new(reqwest::Client::new()), config).unwrap();
    let handler = FileSearchHandler::new(service);
    let params = FileSearchExecutionParams {
        declaration: serde_json::from_value(
            json!({"vector_store_ids":[store.id],"rewrite_query":true,"search_mode":"keyword"}),
        )
        .unwrap(),
        include_results: true,
    };
    let output = handler
        .execute("call_1", "file_search", r#"{"queries":["ocean"]}"#, &params)
        .await
        .unwrap();
    let output: Value = serde_json::from_str(&output.output).unwrap();
    assert_eq!(output["queries"], json!(["coral"]));
    assert!(output["retrieved_passages"].as_array().unwrap().is_empty());
    assert_eq!(setup.state.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn partial_context_failure_timeout_and_cancellation_publish_nothing() {
    let setup = setup(true).await;
    let store = setup
        .service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let file = setup
        .service
        .upload_file(
            "multi.txt",
            "text/plain",
            "assistants",
            "word ".repeat(250).into_bytes(),
        )
        .await
        .unwrap();
    let strategy:ChunkingStrategy=serde_json::from_value(json!({"type":"contextual","contextual":{"max_chunk_size_tokens":100,"chunk_overlap_tokens":0,"max_concurrency":1,"timeout_seconds":1}})).unwrap();
    let request = AttachFileRequest {
        file_id: file.id,
        chunking_strategy: Some(strategy),
        ..Default::default()
    };
    *setup.state.chat_fail_after.lock().unwrap() = Some(1);
    assert_eq!(
        setup
            .service
            .attach_file(&store.id, request.clone())
            .await
            .unwrap_err()
            .status_code(),
        502
    );
    assert_eq!(
        setup
            .service
            .get_vector_store(&store.id)
            .await
            .unwrap()
            .file_counts
            .total,
        0
    );
    assert!(
        !setup
            .state
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(kind, _)| kind == "embed")
    );
    *setup.state.chat_fail_after.lock().unwrap() = None;
    *setup.state.chat_wait.lock().unwrap() = true;
    assert_eq!(
        setup
            .service
            .attach_file(&store.id, request.clone())
            .await
            .unwrap_err()
            .status_code(),
        502
    );
    setup.state.chat_started.notified().await;
    // Cancel while a context request is pending; the service must release permits and publish no attachment.
    let service = setup.service.clone();
    let store_id = store.id.clone();
    let work_request = request.clone();
    let work = tokio::spawn(async move { service.attach_file(&store_id, work_request).await });
    tokio::time::timeout(std::time::Duration::from_secs(3), setup.state.chat_started.notified())
        .await
        .unwrap();
    work.abort();
    assert!(work.await.unwrap_err().is_cancelled());
    assert_eq!(
        setup
            .service
            .get_vector_store(&store.id)
            .await
            .unwrap()
            .file_counts
            .total,
        0
    );
    *setup.state.chat_wait.lock().unwrap() = false;
    setup.service.attach_file(&store.id, request).await.unwrap();
    assert_eq!(
        setup
            .service
            .get_vector_store(&store.id)
            .await
            .unwrap()
            .file_counts
            .completed,
        1
    );
}

#[tokio::test]
async fn malformed_or_oversized_rewrite_outputs_fail_without_fallback() {
    let setup = setup(false).await;
    let store = setup
        .service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    attach(&setup.service, &store.id, "coral", None).await;
    for response in [
        json!({"choices":[]}),
        json!({"choices":[{"message":{"content":null}}]}),
        json!({"choices":[{"message":{"content":" "}}]}),
        json!({"choices":[{"message":{"content":"a".repeat(4097)}}]}),
        json!({"choices":[{"message":{"content":"a".repeat(1024*1024)}}]}),
    ] {
        *setup.state.chat_response.lock().unwrap() = Some(response);
        assert_eq!(
            setup
                .service
                .search(
                    std::slice::from_ref(&store.id),
                    &SearchRequest {
                        rewrite_query: true,
                        ..query("coral")
                    }
                )
                .await
                .unwrap_err()
                .status_code(),
            502
        );
    }
    *setup.state.chat_response.lock().unwrap() = None;
    *setup.state.chat_fail.lock().unwrap() = true;
    assert_eq!(
        setup
            .service
            .search(
                &[store.id],
                &SearchRequest {
                    rewrite_query: true,
                    ..query("coral")
                }
            )
            .await
            .unwrap_err()
            .status_code(),
        502
    );
}

#[tokio::test]
async fn neural_ranker_promotes_zero_similarity_before_applying_final_threshold() {
    let setup = setup(true).await;
    let store = setup
        .service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let preferred = attach(&setup.service, &store.id, "coral preferred", None).await;
    attach(&setup.service, &store.id, "aquatic", None).await;
    let request:SearchRequest=serde_json::from_value(json!({"query":"aquatic","search_mode":"semantic","max_num_results":1,"ranking_options":{"ranker":"neural","score_threshold":0.9}})).unwrap();
    let result = setup.service.search(&[store.id], &request).await.unwrap();
    assert_eq!(result.data.len(), 1);
    assert_eq!(result.data[0].file_id, preferred);
    assert!((result.data[0].score - 0.95).abs() < f64::EPSILON);
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL with pgvector"]
async fn postgres_pgvector_contextual_and_neural_ranking_preserve_source_and_candidate_bounds() {
    let setup = setup(true).await;
    let url = std::env::var("TEST_POSTGRES_URL").unwrap();
    let pool = create_pool_with_schema(Some(&url)).await.unwrap();
    let mut config = setup.config.clone();
    config.backend = FileSearchBackend::Pgvector {
        dimensions: 2,
        index: PgvectorIndex::Hnsw {
            m: 16,
            ef_construction: 64,
            ef_search: 100,
        },
        candidate_limit: 50,
    };
    let service = FileSearchService::new(pool, Arc::new(reqwest::Client::new()), config).unwrap();
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let contextual_id = attach(&service, &store.id, "original source", Some(contextual())).await;
    let preferred = attach(&service, &store.id, "coral preferred", None).await;
    let result = service
        .search(
            std::slice::from_ref(&store.id),
            &SearchRequest {
                search_mode: Some(SearchMode::Semantic),
                ..query("aquatic")
            },
        )
        .await
        .unwrap();
    assert_eq!(result.data[0].file_id, contextual_id);
    assert_eq!(result.data[0].content[0].text, "original source");
    let request:SearchRequest=serde_json::from_value(json!({"query":"aquatic","search_mode":"semantic","max_num_results":1,"ranking_options":{"ranker":"neural","score_threshold":0.9}})).unwrap();
    let result = service.search(std::slice::from_ref(&store.id), &request).await.unwrap();
    assert_eq!(result.data[0].file_id, preferred);
    let mut extra_files = Vec::new();
    for index in 0..60 {
        extra_files.push(attach(&service, &store.id, &format!("aquatic evidence {index}"), None).await);
    }
    let mut bounded_request = request;
    bounded_request.max_num_results = Some(50);
    service
        .search(std::slice::from_ref(&store.id), &bounded_request)
        .await
        .unwrap();
    {
        let requests = setup.state.requests.lock().unwrap();
        let last = requests.iter().rev().find(|(kind, _)| kind == "rerank").unwrap();
        assert_eq!(last.1["documents"].as_array().unwrap().len(), 50);
    }
    for file in extra_files {
        service.delete_file(&file).await.unwrap();
    }
    service.delete_vector_store(&store.id).await.unwrap();
    service.delete_file(&contextual_id).await.unwrap();
    service.delete_file(&preferred).await.unwrap();
}

#[tokio::test]
async fn provider_routing_keeps_embedding_and_generation_credentials_independent() {
    let embedding = setup(true).await;
    let generation = setup(false).await;
    let mut config = embedding.config.clone();
    config.vector_stores.providers.get_mut("local").unwrap().api_key = Some("embedding-secret".into());
    let mut other = generation.config.vector_stores.providers["local"].clone();
    other.api_key = Some("generation-secret".into());
    config.vector_stores.providers.insert("other".into(), other);
    config
        .vector_stores
        .default_reranker_model
        .as_mut()
        .unwrap()
        .provider_id = "other".into();
    config
        .vector_stores
        .rewrite_query_params
        .as_mut()
        .unwrap()
        .model
        .as_mut()
        .unwrap()
        .provider_id = "other".into();
    let service = FileSearchService::new(embedding.pool.clone(), Arc::new(reqwest::Client::new()), config).unwrap();
    let store = service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    attach(&service, &store.id, "coral preferred", None).await;
    let request: SearchRequest =
        serde_json::from_value(json!({"query":"ocean","rewrite_query":true,"ranking_options":{"ranker":"neural"}}))
            .unwrap();
    assert_eq!(service.search(&[store.id], &request).await.unwrap().data.len(), 1);
    assert_eq!(
        *embedding.state.authorization.lock().unwrap(),
        vec!["Bearer embedding-secret", "Bearer embedding-secret"]
    );
    assert_eq!(
        *generation.state.authorization.lock().unwrap(),
        vec!["Bearer generation-secret", "Bearer generation-secret"]
    );
}

#[tokio::test]
async fn repeated_context_placeholders_are_rejected_before_expansion_or_model_calls() {
    let setup = setup(true).await;
    let contextual = ContextualChunking {
        context_prompt: format!("{}{{{{CHUNK_CONTENT}}}}", "{{WHOLE_DOCUMENT}}".repeat(900)),
        ..ContextualChunking::default()
    };
    // Validate before allocating the large document: the old implementation would amplify it 900-fold.
    assert!(contextual.validate().is_err());
    let file = setup
        .service
        .upload_file(
            "amplification.txt",
            "text/plain",
            "assistants",
            "word ".repeat(79_000).into_bytes(),
        )
        .await
        .unwrap();
    let store = setup
        .service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let error = setup
        .service
        .attach_file(
            &store.id,
            AttachFileRequest {
                file_id: file.id,
                chunking_strategy: Some(ChunkingStrategy::Contextual { contextual }),
                ..AttachFileRequest::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.status_code(), 400);
    assert!(setup.state.requests.lock().unwrap().is_empty());
    assert_eq!(
        setup
            .service
            .get_vector_store(&store.id)
            .await
            .unwrap()
            .file_counts
            .total,
        0
    );
}

#[test]
fn hyphenated_openai_ranker_is_accepted_in_deployment_configuration() {
    let ranker: Ranker = serde_json::from_str("\"default-2024-08-21\"").unwrap();
    assert_eq!(ranker, Ranker::Default20240821);
    assert_eq!(serde_json::to_string(&ranker).unwrap(), "\"default-2024-08-21\"");
}

#[tokio::test]
async fn expiration_during_reranking_discards_cached_candidates() {
    let setup = setup(false).await;
    let store = setup
        .service
        .create_vector_store(CreateVectorStoreRequest::default())
        .await
        .unwrap();
    let file_id = attach(&setup.service, &store.id, "coral preferred", None).await;
    *setup.state.rerank_pause.lock().unwrap() = true;
    let service = setup.service.clone();
    let request: SearchRequest =
        serde_json::from_value(json!({"query":"coral","ranking_options":{"ranker":"neural"}})).unwrap();
    let search = tokio::spawn(async move { service.search(&[store.id], &request).await });
    setup.state.rerank_started.notified().await;
    sqlx::query("UPDATE file_search_files SET expires_at = 1 WHERE id = $1")
        .bind(file_id)
        .execute(setup.pool.as_ref())
        .await
        .unwrap();
    setup.state.rerank_resume.notify_one();
    assert!(search.await.unwrap().unwrap().data.is_empty());
}
