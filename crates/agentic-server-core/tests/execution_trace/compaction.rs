use super::*;

#[tokio::test]
async fn compaction_traces_distinguish_triggers_and_state_sources_without_content() {
    for trigger in ["context_management", "input_item", "explicit"] {
        let traces = Traces::new();
        let responses = match trigger {
            "context_management" => vec![text_response("private summary"), text_response("answer")],
            "input_item" => vec![text_response("prior answer"), text_response("private summary")],
            _ => vec![text_response("private summary")],
        };
        let fixture = TestFixture::new_with_responses(responses).await;
        let (source, destination) = if trigger == "explicit" {
            let payload = serde_json::from_value(json!({"model":"test", "input":"private compaction input"})).unwrap();
            traces
                .run(Box::pin(agentic_core::executor::compact_response(
                    payload,
                    &fixture.exec_ctx,
                    None,
                )))
                .await
                .unwrap();
            ("none", "response")
        } else {
            let mut payload = request(false);
            payload.input = ResponsesInput::Text("private compaction input".into());
            let labels = if trigger == "context_management" {
                let conversation = fixture.exec_ctx.conv_handler.create().await.unwrap();
                payload.conversation_id = Some(conversation.conversation_id);
                payload.context_management =
                    Some(serde_json::from_value(json!([{"type":"compaction", "compact_threshold":1}])).unwrap());
                ("conversation", "conversation")
            } else {
                let Either::Left(prior) = ExecuteRequest::new(request(false), Arc::clone(&fixture.exec_ctx))
                    .run()
                    .await
                    .unwrap()
                else {
                    panic!("blocking seed")
                };
                payload.previous_response_id = Some(prior.id);
                payload.input = serde_json::from_value(json!([
                    {"role":"user", "content":"private compaction input"}, {"type":"compaction_trigger"}
                ]))
                .unwrap();
                ("previous_response", "response")
            };
            traces
                .run(ExecuteRequest::new(payload, Arc::clone(&fixture.exec_ctx)).run())
                .await
                .unwrap();
            labels
        };
        let names = if trigger == "explicit" {
            vec!["agentic.compaction", "agentic.persist"]
        } else {
            vec!["agentic.compaction", "agentic.persist", "agentic.execute"]
        };
        let spans = traces.finished_by_name(&names).await;
        let compaction = spans.iter().find(|span| span.name == "agentic.compaction").unwrap();
        assert_eq!(
            attribute(compaction, "agentic.compaction.trigger"),
            Some(&Value::from(trigger))
        );
        assert_eq!(
            attribute(compaction, "agentic.compaction.operation"),
            Some(&Value::from("summarize"))
        );
        let parent = spans
            .iter()
            .find(|span| span.name == "agentic.execute")
            .map_or_else(|| traces.root_span_id(), |span| span.span_context.span_id());
        assert_eq!(compaction.parent_span_id, parent);
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.name == "http.client.request"
                    && span.parent_span_id == compaction.span_context.span_id())
                .count(),
            1
        );
        let rehydrate = spans.iter().find(|span| span.name == "agentic.rehydrate").unwrap();
        let persist = spans.iter().find(|span| span.name == "agentic.persist").unwrap();
        assert_eq!(
            attribute(rehydrate, "agentic.rehydrate.source"),
            Some(&Value::from(source))
        );
        assert_eq!(
            attribute(persist, "agentic.persist.destination"),
            Some(&Value::from(destination))
        );
    }
}
