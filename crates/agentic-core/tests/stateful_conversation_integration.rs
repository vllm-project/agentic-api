//! Cassette-based integration tests for the Conversation API (cases 6–10).
//!
//! Mirrors `test_conversation_api.py`. Each conversation cassette includes a
//! `/v1/conversations` creation turn — mirrored here via `create_conversation()`.
//! `TestFixture` serves only `/v1/responses` turns on the mock HTTP server.

mod support;

use agentic_core::executor::{create_conversation, execute};
use std::sync::Arc;
use support::{
    TestFixture, collect_stream, expected_text, load_cassette, make_request, output_text, request_input_texts,
    responses_turns, text_response, unwrap_blocking,
};

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cassettes/text_only/conversation");

/// Case 6 — two turns, non-streaming, via `conversation_id`.
#[tokio::test]
async fn test_two_turn_nonstreaming_conversation() {
    // Arrange
    let cassette = load_cassette(&format!("{DIR}/conv-two-turn-gpt-4o-nonstreaming.yaml"));
    let all: Vec<_> = cassette.turns.iter().collect();
    let fixture = TestFixture::new(&all).await;
    let ctx = &fixture.exec_ctx;
    let resp = responses_turns(&cassette);
    let (t1, t2) = (resp[0], resp[1]);

    // Mirrors /v1/conversations creation turn
    let conv_id = create_conversation(ctx).await.expect("create conv").conversation_id;

    // Act
    let p1 = unwrap_blocking(
        execute(
            make_request(&t1.request.body.input, true, false, None, Some(conv_id.clone())),
            Arc::clone(ctx),
        )
        .await
        .expect("t1"),
    );
    let p2 = unwrap_blocking(
        execute(
            make_request(&t2.request.body.input, true, false, None, Some(conv_id)),
            Arc::clone(ctx),
        )
        .await
        .expect("t2"),
    );

    // Assert
    assert!(p1.id.starts_with("resp_"));
    assert_eq!(p1.status, "completed");
    assert_eq!(output_text(&p1), expected_text(t1));
    assert_ne!(p2.id, p1.id);
    assert_eq!(p2.status, "completed");
    assert_eq!(output_text(&p2), expected_text(t2));
}

/// Case 7 — two turns, streaming, via `conversation_id`.
#[tokio::test]
async fn test_two_turn_streaming_conversation() {
    // Arrange
    let cassette = load_cassette(&format!("{DIR}/conv-two-turn-gpt-4o-streaming.yaml"));
    let all: Vec<_> = cassette.turns.iter().collect();
    let fixture = TestFixture::new(&all).await;
    let ctx = &fixture.exec_ctx;
    let resp = responses_turns(&cassette);
    let (t1, t2) = (resp[0], resp[1]);

    let conv_id = create_conversation(ctx).await.expect("create conv").conversation_id;

    // Act
    let p1 = collect_stream(
        execute(
            make_request(&t1.request.body.input, true, true, None, Some(conv_id.clone())),
            Arc::clone(ctx),
        )
        .await
        .expect("t1"),
    )
    .await;
    let p2 = collect_stream(
        execute(
            make_request(&t2.request.body.input, true, true, None, Some(conv_id)),
            Arc::clone(ctx),
        )
        .await
        .expect("t2"),
    )
    .await;

    // Assert
    assert!(p1.id.starts_with("resp_"));
    assert_eq!(p1.status, "completed");
    assert_eq!(output_text(&p1), expected_text(t1));
    assert_ne!(p2.id, p1.id);
    assert_eq!(p2.status, "completed");
    assert_eq!(output_text(&p2), expected_text(t2));
}

/// Case 8 — two independent conversations must not share context.
#[tokio::test]
async fn test_conversation_isolation() {
    // Arrange
    let cassette = load_cassette(&format!("{DIR}/conv-isolation-gpt-4o-nonstreaming.yaml"));
    let all: Vec<_> = cassette.turns.iter().collect();
    let fixture = TestFixture::new(&all).await;
    let ctx = &fixture.exec_ctx;
    let resp = responses_turns(&cassette);
    let (ta1, ta2, ta3, tb1, tb2, tb3) = (resp[0], resp[1], resp[2], resp[3], resp[4], resp[5]);

    // Conv A
    let conv_a = create_conversation(ctx).await.expect("create conv A").conversation_id;
    let pa1 = unwrap_blocking(
        execute(
            make_request(&ta1.request.body.input, true, false, None, Some(conv_a.clone())),
            Arc::clone(ctx),
        )
        .await
        .expect("a1"),
    );
    assert_eq!(output_text(&pa1), expected_text(ta1));
    let pa2 = unwrap_blocking(
        execute(
            make_request(&ta2.request.body.input, true, false, None, Some(conv_a.clone())),
            Arc::clone(ctx),
        )
        .await
        .expect("a2"),
    );
    assert_eq!(output_text(&pa2), expected_text(ta2));
    let pa3 = unwrap_blocking(
        execute(
            make_request(&ta3.request.body.input, true, false, None, Some(conv_a.clone())),
            Arc::clone(ctx),
        )
        .await
        .expect("a3"),
    );
    assert_eq!(output_text(&pa3), expected_text(ta3));

    // Conv B
    let conv_b = create_conversation(ctx).await.expect("create conv B").conversation_id;
    let pb1 = unwrap_blocking(
        execute(
            make_request(&tb1.request.body.input, true, false, None, Some(conv_b.clone())),
            Arc::clone(ctx),
        )
        .await
        .expect("b1"),
    );
    assert_eq!(output_text(&pb1), expected_text(tb1));
    let pb2 = unwrap_blocking(
        execute(
            make_request(&tb2.request.body.input, true, false, None, Some(conv_b.clone())),
            Arc::clone(ctx),
        )
        .await
        .expect("b2"),
    );
    assert_eq!(output_text(&pb2), expected_text(tb2));
    let pb3 = unwrap_blocking(
        execute(
            make_request(&tb3.request.body.input, true, false, None, Some(conv_b.clone())),
            Arc::clone(ctx),
        )
        .await
        .expect("b3"),
    );
    assert_eq!(output_text(&pb3), expected_text(tb3));

    // Assert — conversations are isolated
    assert_ne!(conv_a, conv_b, "conversations must not share an id");
}

/// Case 9 — 3-turn chain then branch off turn 1 via `previous_response_id`.
#[tokio::test]
async fn test_branch_off_turn_1() {
    // Arrange
    let cassette = load_cassette(&format!("{DIR}/conv-multi-turn-single-branch-gpt-4o-nonstreaming.yaml"));
    let all: Vec<_> = cassette.turns.iter().collect();
    let fixture = TestFixture::new(&all).await;
    let ctx = &fixture.exec_ctx;
    let resp = responses_turns(&cassette);
    let (t1, t2, t3, t4) = (resp[0], resp[1], resp[2], resp[3]);

    let conv_id = create_conversation(ctx).await.expect("create conv").conversation_id;

    // Main chain
    let p1 = unwrap_blocking(
        execute(
            make_request(&t1.request.body.input, true, false, None, Some(conv_id.clone())),
            Arc::clone(ctx),
        )
        .await
        .expect("t1"),
    );
    assert_eq!(output_text(&p1), expected_text(t1));
    let r1_id = p1.id.clone();

    let p2 = unwrap_blocking(
        execute(
            make_request(&t2.request.body.input, true, false, None, Some(conv_id.clone())),
            Arc::clone(ctx),
        )
        .await
        .expect("t2"),
    );
    assert_eq!(output_text(&p2), expected_text(t2));

    let p3 = unwrap_blocking(
        execute(
            make_request(&t3.request.body.input, true, false, None, Some(conv_id)),
            Arc::clone(ctx),
        )
        .await
        .expect("t3"),
    );
    assert_eq!(output_text(&p3), expected_text(t3));

    // Branch off turn 1 — only turn 1 context visible
    let p4 = unwrap_blocking(
        execute(
            make_request(&t4.request.body.input, true, false, Some(r1_id), None),
            Arc::clone(ctx),
        )
        .await
        .expect("t4"),
    );
    assert_eq!(p4.status, "completed");
    assert_eq!(output_text(&p4), expected_text(t4));
}

/// Case 10 — 5-turn chain with 2 inline branches.
#[tokio::test]
async fn test_multi_branch() {
    // Arrange
    let cassette = load_cassette(&format!("{DIR}/conv-multi-branch-multi-turn-gpt-4o-nonstreaming.yaml"));
    let all: Vec<_> = cassette.turns.iter().collect();
    let fixture = TestFixture::new(&all).await;
    let ctx = &fixture.exec_ctx;
    let resp = responses_turns(&cassette);
    let (t1, t2, t3, t4, t5) = (resp[0], resp[1], resp[2], resp[3], resp[4]);

    let conv_id = create_conversation(ctx).await.expect("create conv").conversation_id;

    // Turn 1
    let p1 = unwrap_blocking(
        execute(
            make_request(&t1.request.body.input, true, false, None, Some(conv_id.clone())),
            Arc::clone(ctx),
        )
        .await
        .expect("t1"),
    );
    assert_eq!(output_text(&p1), expected_text(t1));
    let r1_id = p1.id.clone();

    // Turn 2 (main branch)
    let p2 = unwrap_blocking(
        execute(
            make_request(&t2.request.body.input, true, false, None, Some(conv_id)),
            Arc::clone(ctx),
        )
        .await
        .expect("t2"),
    );
    assert_eq!(output_text(&p2), expected_text(t2));
    let r2_id = p2.id.clone();

    // Branch 1 — off turn 1
    let p3 = unwrap_blocking(
        execute(
            make_request(&t3.request.body.input, true, false, Some(r1_id), None),
            Arc::clone(ctx),
        )
        .await
        .expect("t3"),
    );
    assert_eq!(p3.status, "completed");
    assert_eq!(output_text(&p3), expected_text(t3));

    let p4 = unwrap_blocking(
        execute(
            make_request(&t4.request.body.input, true, false, Some(p3.id.clone()), None),
            Arc::clone(ctx),
        )
        .await
        .expect("t4"),
    );
    assert_eq!(p4.status, "completed");
    assert_eq!(output_text(&p4), expected_text(t4));

    // Branch 2 — off turn 2
    let p5 = unwrap_blocking(
        execute(
            make_request(&t5.request.body.input, true, false, Some(r2_id), None),
            Arc::clone(ctx),
        )
        .await
        .expect("t5"),
    );
    assert_eq!(p5.status, "completed");
    assert_eq!(output_text(&p5), expected_text(t5));
}

#[tokio::test]
async fn test_store_false_with_conversation_id_hydrates_but_does_not_persist() {
    let fixture = TestFixture::new_with_responses(vec![
        text_response("stored answer"),
        text_response("stateless answer"),
        text_response("final answer"),
    ])
    .await;
    let ctx = &fixture.exec_ctx;
    let conv_id = create_conversation(ctx).await.expect("create conv").conversation_id;

    let p1 = unwrap_blocking(
        execute(
            make_request("seed", true, false, None, Some(conv_id.clone())),
            Arc::clone(ctx),
        )
        .await
        .expect("stored turn"),
    );
    assert_eq!(output_text(&p1), "stored answer");

    let p2 = unwrap_blocking(
        execute(
            make_request("follow up", false, false, None, Some(conv_id.clone())),
            Arc::clone(ctx),
        )
        .await
        .expect("store=false follow-up"),
    );
    assert_eq!(output_text(&p2), "stateless answer");

    let p3 = unwrap_blocking(
        execute(make_request("third", true, false, None, Some(conv_id)), Arc::clone(ctx))
            .await
            .expect("stored third turn"),
    );
    assert_eq!(output_text(&p3), "final answer");

    let requests = fixture.request_bodies().await;
    assert_eq!(requests.len(), 3);
    assert_eq!(
        request_input_texts(&requests[1]),
        vec!["seed", "stored answer", "follow up"]
    );
    assert_eq!(
        request_input_texts(&requests[2]),
        vec!["seed", "stored answer", "third"]
    );
}
