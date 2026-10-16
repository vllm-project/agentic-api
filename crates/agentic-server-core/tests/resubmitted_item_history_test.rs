//! A response may be resubmitted as input after it has been stored (issue #385).

mod support;

use std::sync::Arc;

use agentic_core::executor::{ExecuteRequest, rehydrate_conversation};
use agentic_core::types::ResponsesInput;
use serde_json::{Value, json};

fn recording(name: &str) -> support::Cassette {
    support::load_cassette(&format!(
        "{}/tests/cassettes/reasoning/responses/{name}-nonstreaming.yaml",
        env!("CARGO_MANIFEST_DIR")
    ))
}

fn ids(items: &Value) -> Vec<String> {
    items
        .as_array()
        .expect("item array")
        .iter()
        .filter_map(|item| item["id"].as_str().map(str::to_owned))
        .collect()
}

/// The client sends turn 1's output back as input. Turn 2's upstream output has new IDs,
/// so only the resubmitted input items repeat stored public IDs.
#[tokio::test]
async fn resubmitted_output_items_are_stored_again() {
    let first_recording = recording("reasoning-single-Qwen-Qwen3-30B-A3B-FP8");
    let second_recording = recording("reasoning-single-openai-gpt-oss-20b");
    let fixture = support::TestFixture::new(&[&first_recording.turns[0], &second_recording.turns[0]]).await;
    let prompt = first_recording.turns[0].request.body.input.clone();
    let run = |input: Value| {
        ExecuteRequest::new(
            support::make_request(input, true, false, None, None),
            Arc::clone(&fixture.exec_ctx),
        )
        .run()
    };

    let first = support::unwrap_blocking(run(prompt.clone()).await.expect("turn 1"));
    let first_output = serde_json::to_value(&first.output).expect("serialize turn 1 output");
    let mut input = vec![json!({"role": "user", "content": prompt})];
    input.extend(first_output.as_array().expect("output array").iter().cloned());
    input.push(json!({"role": "user", "content": "Reply with exactly one word: HELLO"}));

    let second = match run(Value::Array(input)).await {
        Ok(result) => support::unwrap_blocking(result),
        Err(error) => panic!("turn 2: {} {error}", error.http_status()),
    };
    assert_eq!(second.status, "completed");

    // Turn 2's stored history keeps the resubmitted items in order.
    let context = rehydrate_conversation(
        support::make_request("next", false, false, Some(second.id.clone()), None),
        &fixture.exec_ctx,
    )
    .await
    .expect("rehydrate turn 2");
    let ResponsesInput::Items(items) = &context.enriched_request.input else {
        panic!("expected item history");
    };
    let stored = ids(&serde_json::to_value(items).expect("serialize stored history"));
    let expected: Vec<_> = ids(&first_output)
        .into_iter()
        .chain(ids(
            &serde_json::to_value(&second.output).expect("serialize turn 2 output")
        ))
        .collect();
    assert_eq!(stored, expected);
}
