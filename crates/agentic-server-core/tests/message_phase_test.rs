//! Assistant phase is typed, optional, and durable; no phase is invented for legacy history.

mod support;

use agentic_core::executor::{ExecuteRequest, ExecutorError, rehydrate_conversation, upstream_request};
use agentic_core::storage::{ConversationStore, InOutItem, ResponseMetadata};
use agentic_core::types::io::{InputItem, MessagePhase, OutputItem};
use serde_json::json;

#[test]
fn phase_is_a_closed_optional_contract_preserved_by_output_conversion() {
    for (wire, expected) in [
        ("commentary", MessagePhase::Commentary),
        ("final_answer", MessagePhase::FinalAnswer),
    ] {
        let output: OutputItem = serde_json::from_value(json!({
            "type":"message", "id":"msg_phase", "role":"assistant", "status":"completed",
            "phase":wire, "content":[{"type":"output_text", "text":"answer", "annotations":[]}]
        }))
        .unwrap();
        let Some(InputItem::Message(input)) = output.to_input_item() else {
            panic!("message input")
        };
        assert_eq!(input.phase, Some(expected));
        assert_eq!(serde_json::to_value(input).unwrap()["phase"], wire);
    }
    for value in [json!("unknown"), json!(false), json!({})] {
        assert!(
            serde_json::from_value::<InputItem>(json!({
                "type":"message", "role":"assistant", "content":"answer", "phase":value
            }))
            .is_err()
        );
    }
    for phase in [None, Some(serde_json::Value::Null)] {
        let mut wire = json!({"type":"message", "role":"user", "content":"question"});
        if let Some(phase) = phase {
            wire["phase"] = phase;
        }
        let item: InputItem = serde_json::from_value(wire).unwrap();
        assert!(serde_json::to_value(item).unwrap().get("phase").is_none());
    }
}

#[tokio::test]
async fn assistant_phase_survives_storage_rehydration_and_wire_projection() {
    let pool = support::setup_pool().await;
    let store = ConversationStore::new(pool);
    let conversation = store.create().await.unwrap();
    let item: OutputItem = serde_json::from_value(json!({
        "type":"message", "id":"msg_phase", "role":"assistant", "status":"completed",
        "phase":"commentary", "content":[{"type":"output_text", "text":"working", "annotations":[]}]
    }))
    .unwrap();
    store
        .persist(
            &conversation.conversation_id,
            "resp_phase",
            None,
            vec![InOutItem::Output(item)],
            &ResponseMetadata::default(),
        )
        .await
        .unwrap();
    let input = InOutItem::into_input_items(store.rehydrate(&conversation.conversation_id).await.unwrap());
    assert_eq!(serde_json::to_value(&input).unwrap()[0]["phase"], "commentary");
    let fixture = support::TestFixture::new(&[]).await;
    let mut request = support::make_request("continue", false, false, None, None);
    request.input = agentic_core::types::ResponsesInput::Items(input);
    let context = rehydrate_conversation(request, &fixture.exec_ctx).await.unwrap();
    let wire: serde_json::Value = serde_json::from_str(&upstream_request(&context, false).unwrap()).unwrap();
    assert_eq!(wire["input"][0]["phase"], "commentary");
}

#[tokio::test]
async fn non_assistant_phase_is_rejected_before_inference() {
    let fixture = support::TestFixture::new(&[]).await;
    for role in ["user", "system", "developer"] {
        let mut request = support::make_request("hello", false, false, None, None);
        request.input = serde_json::from_value(json!([
            {"type":"message", "role":role, "content":"hello", "phase":"final_answer"}
        ]))
        .unwrap();
        let error = ExecuteRequest::new(request, fixture.exec_ctx.clone())
            .run()
            .await
            .err()
            .unwrap();
        assert!(matches!(error, ExecutorError::InvalidRequest(_)));
        assert!(error.to_string().contains("only on assistant"));
    }
    assert!(fixture.request_bodies().await.is_empty());
}
