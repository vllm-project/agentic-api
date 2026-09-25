use super::*;

async fn search_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = axum::Router::new().route(
        "/v1/search",
        axum::routing::get(|| async {
            axum::Json(
                json!({"results":{"web":[{"url":"https://example.com", "title":"private search result"}],"news":[]}}),
            )
        }),
    );
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, task)
}

#[tokio::test]
async fn recorded_two_round_stream_has_exact_stage_tree_and_upstream_parent() {
    let traces = Traces::new();
    let cassette = support::load_cassette(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/cassettes/codex/codex-openai-web-search-then-namespace-gpt-4o-streaming.yaml"
    ));
    let mut fixture = TestFixture::new(&support::responses_turns(&cassette)).await;
    let (search_url, search_task) = search_server().await;
    let context = Arc::try_unwrap(fixture.exec_ctx).ok().unwrap();
    fixture.exec_ctx = Arc::new(context.with_gateway_executor(Arc::new(
        agentic_core::tool::WebSearchHandler::with_api_key(
            Arc::new(reqwest::Client::new()),
            "private-search-key".into(),
            &search_url,
        ),
    )));
    let mut payload = request(true);
    payload.tools = Some(serde_json::from_value(json!([
        {"type":"web_search_preview"},
        {"type":"namespace","name":"mcp__shell","tools":[{"type":"function","name":"run","parameters":{"type":"object"}}]}
    ])).unwrap());
    let Either::Right(stream) = Box::pin(
        traces.run(
            ExecuteRequest::new(payload, fixture.exec_ctx)
                .with_auth(Some("private-auth".into()))
                .run(),
        ),
    )
    .await
    .unwrap() else {
        panic!("expected stream");
    };
    // Poll outside the initiating future: the executor must carry its own context.
    let chunks: Vec<_> = stream.collect().await;
    search_task.abort();
    let _ = search_task.await;
    assert!(chunks.iter().any(|chunk| chunk.contains("response.completed")));
    assert!(chunks.iter().any(|chunk| chunk.contains("private search result")));
    let spans = traces.finished_by_name(&["agentic.execute", "agentic.persist"]).await;
    let requests = fixture.server.request_bodies().await;
    let arguments: Vec<_> = requests[1]["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["arguments"].as_str())
        .collect();
    assert!(!arguments.is_empty(), "scan the actual replayed tool arguments");
    trace_attributes::assert_allowed(&spans, &arguments);
    assert_stage_tree(&traces, &spans);
    let stages = &spans;
    let headers = fixture.server.request_headers().await;
    assert_eq!(headers.len(), 2);
    for (index, headers) in headers.iter().enumerate() {
        let round = stages
            .iter()
            .find(|span| {
                span.name == "agentic.inference_round"
                    && attribute(span, "agentic.inference.round") == Some(&Value::I64(i64::try_from(index).unwrap()))
            })
            .unwrap();
        let client = stages
            .iter()
            .find(|span| span.name == "http.client.request" && span.parent_span_id == round.span_context.span_id())
            .unwrap();
        assert_eq!(client.span_kind, opentelemetry::trace::SpanKind::Client);
        assert_eq!(
            headers["traceparent"],
            format!("00-{}-{}-01", traces.trace_id, client.span_context.span_id())
        );
        assert_eq!(attribute(client, "http.response.status_code"), Some(&Value::I64(200)));
    }
    assert_eq!(
        attribute(
            stages.iter().find(|span| span.name == "agentic.tool.execute").unwrap(),
            "agentic.tool.type"
        ),
        Some(&Value::from("web_search"))
    );
    assert_eq!(
        attribute(
            stages.iter().find(|span| span.name == "agentic.rehydrate").unwrap(),
            "agentic.rehydrate.source"
        ),
        Some(&Value::from("none"))
    );
}

fn assert_stage_tree(traces: &Traces, spans: &[SpanData]) {
    let execute = spans.iter().find(|span| span.name == "agentic.execute").unwrap();
    let stages: Vec<_> = spans
        .iter()
        .filter(|span| span.name.starts_with("agentic.") || span.name == "http.client.request")
        .collect();
    let mut names: Vec<_> = stages.iter().map(|span| span.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "agentic.execute",
            "agentic.inference_round",
            "agentic.inference_round",
            "agentic.persist",
            "agentic.rehydrate",
            "agentic.tool.execute",
            "http.client.request",
            "http.client.request"
        ]
    );
    for span in &stages {
        if span.name == "agentic.execute" {
            assert_eq!(span.parent_span_id, traces.root_span_id());
        } else if span.name != "http.client.request" {
            assert_eq!(
                span.parent_span_id,
                execute.span_context.span_id(),
                "{} parent",
                span.name
            );
        }
    }
}

#[tokio::test]
async fn http_failure_records_status_without_error_body() {
    let traces = Traces::new();
    let fixture =
        TestFixture::new_with_responses(vec![MockResponse::Status(503, "private upstream error".into())]).await;
    assert!(
        Box::pin(traces.run(ExecuteRequest::new(request(false), fixture.exec_ctx).run()))
            .await
            .is_err()
    );
    let spans = traces
        .finished_by_name(&["agentic.execute", "http.client.request"])
        .await;
    let client = spans.iter().find(|span| span.name == "http.client.request").unwrap();
    assert_eq!(attribute(client, "http.response.status_code"), Some(&Value::I64(503)));
    assert_eq!(attribute(client, "error.type"), Some(&Value::from("upstream_status")));
    assert!(matches!(client.status, Status::Error { .. }));
    assert!(!format!("{client:?}").contains("private upstream error"));
}
