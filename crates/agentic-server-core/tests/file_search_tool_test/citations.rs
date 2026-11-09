//! Synthetic OGX-guided cases exercised through real retrieval and execution.

use super::*;
use agentic_core::types::io::OutputAnnotation;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Fixtures {
    files: Vec<Source>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Source {
    file_id: String,
    filename: String,
    text: String,
}

#[derive(Deserialize, Serialize)]
struct Case {
    name: String,
    query: String,
    deltas: Vec<String>,
    annotations: Vec<OutputAnnotation>,
    expected_annotations: Vec<OutputAnnotation>,
}

fn replace_ids(text: &str, ids: &[(String, String)]) -> String {
    ids.iter()
        .fold(text.to_owned(), |text, (from, to)| text.replace(from, to))
}

impl Case {
    fn render(&self, ids: &[(String, String)]) -> Self {
        let text = self.deltas.concat();
        let mut fixture = serde_json::to_value(self).unwrap();
        fixture["deltas"] = json!([text]);
        let mut rendered: Self = serde_json::from_str(&replace_ids(&fixture.to_string(), ids)).unwrap();
        // Stream one Unicode scalar at a time, including inside runtime-generated IDs.
        rendered.deltas = rendered
            .deltas
            .concat()
            .chars()
            .map(|character| character.to_string())
            .collect();
        // Expand fixture coordinates when generated file IDs have a different length.
        // The fixture supplies exact offsets; this does not search for citation markers.
        for annotation in rendered
            .annotations
            .iter_mut()
            .chain(&mut rendered.expected_annotations)
        {
            if let OutputAnnotation::FileCitation(citation) = annotation
                && citation.index <= text.chars().count()
            {
                let prefix: String = text.chars().take(citation.index).collect();
                citation.index = replace_ids(&prefix, ids).chars().count();
            }
        }
        rendered
    }

    fn message(&self) -> Value {
        json!({"type":"message","id":"msg_answer","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":self.deltas.concat(),"annotations":self.annotations}]})
    }

    fn expected_part(&self) -> Value {
        json!({"type":"output_text","text":self.deltas.concat(),
            "annotations":self.expected_annotations,"logprobs":[]})
    }
}

#[tokio::test]
async fn citation_fixtures_survive_json_sse_storage_and_continuation() {
    let fixtures: Fixtures = serde_json::from_str(include_str!("../fixtures/file_search/citations.json")).unwrap();
    for streaming in [false, true] {
        for include_results in [false, true] {
            let (service, store_id, _, _files) = fixture().await;
            let mut ids = Vec::new();
            for source in &fixtures.files {
                let uploaded = service
                    .upload_file(
                        &source.filename,
                        "text/plain",
                        "assistants",
                        source.text.as_bytes().to_vec(),
                    )
                    .await
                    .unwrap();
                service
                    .attach_file(
                        &store_id,
                        AttachFileRequest {
                            file_id: uploaded.id.clone(),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                ids.push((source.file_id.clone(), uploaded.id));
            }
            for case in &fixtures.cases {
                let case = case.render(&ids);
                let mut call = search_call();
                call["arguments"] = json!({"queries":[case.query]}).to_string().into();
                let llm = support::MockServer::start_deque(vec![
                    upstream(vec![call], streaming),
                    upstream_with_deltas(vec![case.message()], streaming, &case.deltas),
                    upstream_with_deltas(vec![case.message()], streaming, &case.deltas),
                ])
                .await;
                let ctx = context(service.clone(), llm.url()).await;
                let mut request = support::make_request("Explain the climate evidence", true, streaming, None, None);
                request.tools = Some(
                    serde_json::from_value(json!([
                        {"type":"file_search","vector_store_ids":[store_id]}
                    ]))
                    .unwrap(),
                );
                request.include = include_results.then(|| vec!["file_search_call.results".to_owned()]);
                let (response, events) =
                    collect(ExecuteRequest::new(request, Arc::clone(&ctx)).run().await.unwrap()).await;
                let output = serde_json::to_value(&response.output).unwrap();
                assert_eq!(output[0]["status"], "completed", "{} search", case.name);
                let expected = case.expected_part();
                assert_eq!(
                    output[1]["content"][0], expected,
                    "{} stream={streaming} include={include_results}",
                    case.name
                );
                assert_eq!(output[0].get("results").is_some(), include_results);
                if streaming {
                    assert_file_search_stream(&events, &output);
                    let deltas: String = events
                        .iter()
                        .filter(|event| event["type"] == "response.output_text.delta")
                        .map(|event| event["delta"].as_str().unwrap())
                        .collect();
                    assert_eq!(deltas, case.deltas.concat(), "{} delta reconstruction", case.name);
                }
                let stored = ctx.resp_handler.get_snapshot(&response.id).await.unwrap();
                assert_eq!(
                    serde_json::to_value(stored.output).unwrap(),
                    output,
                    "{} storage",
                    case.name
                );
                let continuation =
                    support::make_request("Explain those sources again", true, streaming, Some(response.id), None);
                let (continued, events) = collect(ExecuteRequest::new(continuation, ctx).run().await.unwrap()).await;
                let continued = serde_json::to_value(continued.output).unwrap();
                assert_eq!(continued[0]["content"][0], expected, "{} continuation", case.name);
                if streaming {
                    let done = events
                        .iter()
                        .find(|event| event["type"] == "response.content_part.done")
                        .unwrap();
                    assert_eq!(done["part"], expected, "{} streamed continuation", case.name);
                }
                assert_eq!(llm.request_bodies().await.len(), 3);
            }
        }
    }
}

#[tokio::test]
async fn native_upstream_citations_survive_without_gateway_file_search() {
    let native: Value = serde_json::from_str(include_str!("../fixtures/file_search/native_annotations.json")).unwrap();
    for streaming in [false, true] {
        for client_function in [false, true] {
            let (service, _, _, _files) = fixture().await;
            let upstream = if streaming {
                let mut stream = String::new();
                for event in native["events"].as_array().unwrap() {
                    write!(stream, "data: {event}\n\n").unwrap();
                }
                support::MockResponse::Sse(stream)
            } else {
                support::MockResponse::Json(native["response"].to_string())
            };
            let llm = support::MockServer::start_deque(vec![upstream]).await;
            let ctx = context(service, llm.url()).await;
            let mut request = support::make_request("Answer with sources", true, streaming, None, None);
            if client_function {
                request.tools = Some(
                    serde_json::from_value(json!([
                        {"type":"function","name":"file_search","parameters":{"type":"object","properties":{}}}
                    ]))
                    .unwrap(),
                );
            }
            let (response, events) = collect(ExecuteRequest::new(request, Arc::clone(&ctx)).run().await.unwrap()).await;
            let output = serde_json::to_value(&response.output).unwrap();
            assert_eq!(output, native["response"]["output"]);
            let stored = ctx.resp_handler.get_snapshot(&response.id).await.unwrap();
            assert_eq!(serde_json::to_value(stored.output).unwrap(), output);
            if streaming {
                let parts: Vec<_> = events
                    .iter()
                    .filter(|event| event["type"] == "response.content_part.done")
                    .map(|event| event["part"].clone())
                    .collect();
                assert_eq!(json!(parts), output[0]["content"]);
                let added: Vec<_> = events
                    .iter()
                    .filter(|event| event["type"] == "response.output_text.annotation.added")
                    .map(|event| event["annotation"].clone())
                    .collect();
                let expected: Vec<_> = native["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|event| event["type"] == "response.output_text.annotation.added")
                    .map(|event| event["annotation"].clone())
                    .collect();
                assert_eq!(added, expected);
            }
        }
    }
}
