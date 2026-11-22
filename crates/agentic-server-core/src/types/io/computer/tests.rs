use serde_json::json;

use super::*;

fn round_trip<T>(wire: &Value) -> T
where
    T: Serialize + for<'de> Deserialize<'de>,
{
    let typed: T = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(&serde_json::to_value(&typed).unwrap(), wire);
    typed
}

fn documented_actions() -> Vec<Value> {
    vec![
        json!({"type": "click", "button": "left", "x": 10, "y": 20}),
        json!({"type": "double_click", "x": 30, "y": 40, "keys": null}),
        json!({"type": "drag", "path": [{"x": 1, "y": 2}, {"x": 3, "y": 4}], "keys": ["shift"]}),
        json!({"type": "keypress", "keys": ["ctrl", "l"]}),
        json!({"type": "move", "x": 5, "y": 6}),
        json!({"type": "scroll", "x": 7, "y": 8, "scroll_x": 0, "scroll_y": -120}),
        json!({"type": "type", "text": "hello"}),
        json!({"type": "wait"}),
        json!({"type": "screenshot"}),
    ]
}

#[test]
fn each_documented_action_round_trips_as_a_known_action() {
    for wire in documented_actions() {
        let action: ComputerAction = round_trip(&wire);
        assert!(matches!(action, ComputerAction::Known(_)), "{wire}");
    }
}

#[test]
fn computer_call_round_trips_every_documented_action_in_order() {
    let wire = json!({
        "id": "cu_1",
        "call_id": "call_1",
        "actions": documented_actions(),
        "pending_safety_checks": [],
        "status": "completed"
    });
    let call: ComputerCall = round_trip(&wire);
    assert_eq!(call.ordered_actions().len(), 9);
    assert!(matches!(
        call.ordered_actions()[0],
        ComputerAction::Known(KnownComputerAction::Click {
            button: ComputerMouseButton::Left,
            x: 10,
            y: 20,
            keys: None,
            ..
        })
    ));
    assert!(matches!(
        call.ordered_actions()[8],
        ComputerAction::Known(KnownComputerAction::Screenshot { .. })
    ));
    assert_eq!(call.status, Some(ComputerCallStatus::Completed));
}

#[test]
fn legacy_single_action_and_extension_fields_round_trip() {
    let wire = json!({
        "id": "cu_2",
        "call_id": "call_2",
        "action": {"type": "click", "button": "right", "x": 1, "y": 2, "future_action_field": true},
        "pending_safety_checks": [{"id": "sc_1", "code": "malicious_instructions", "message": "check", "level": 1}],
        "status": "in_progress",
        "future_item_field": "kept"
    });
    let call: ComputerCall = round_trip(&wire);
    assert!(call.actions.is_none());
    assert_eq!(
        call.ordered_actions(),
        std::slice::from_ref(call.action.as_ref().unwrap())
    );
    assert_eq!(call.extra["future_item_field"], "kept");
}

#[test]
fn unknown_action_type_round_trips_verbatim() {
    let unknown = json!({"type": "zoom", "factor": 2, "anchor": {"x": 1, "y": 2}});
    let action: ComputerAction = round_trip(&unknown);
    let ComputerAction::Unknown(fields) = &action else {
        panic!("an unmodeled action type is kept as sent");
    };
    assert_eq!(fields["type"], "zoom");

    let wire = json!({"call_id": "call_3", "actions": [{"type": "wait"}, unknown]});
    let call: ComputerCall = round_trip(&wire);
    assert!(matches!(call.ordered_actions()[1], ComputerAction::Unknown(_)));
}

#[test]
fn malformed_known_action_is_rejected_not_kept_as_unknown() {
    for wire in [
        json!({"type": "click", "button": "left", "y": 2}),
        json!({"type": "click", "button": "middle", "x": 1, "y": 2}),
        json!({"type": "type"}),
        json!({"type": "keypress", "keys": null}),
        json!({"type": "drag", "path": [{"x": 1}]}),
        json!({"type": "scroll", "x": 1, "y": 2, "scroll_x": 0}),
        json!({"x": 1, "y": 2}),
        json!({"type": 7}),
        json!("click"),
    ] {
        assert!(
            serde_json::from_value::<ComputerAction>(wire.clone()).is_err(),
            "{wire}"
        );
    }
    let call = json!({"call_id": "call_4", "actions": [{"type": "click", "x": 1, "y": 2}]});
    assert!(serde_json::from_value::<ComputerCall>(call).is_err());
    let legacy_call = json!({"call_id": "call_5", "action": {"type": "click", "x": 1, "y": 2}});
    assert!(serde_json::from_value::<ComputerCall>(legacy_call).is_err());
    for keys in [json!(123), json!([1])] {
        let bad_keys = json!({"type": "move", "x": 1, "y": 2, "keys": keys});
        assert!(serde_json::from_value::<ComputerAction>(bad_keys).is_err());
    }
    let missing_call_id = json!({"actions": [{"type": "wait"}]});
    assert!(serde_json::from_value::<ComputerCall>(missing_call_id).is_err());
}

#[test]
fn safety_checks_preserve_absent_null_and_string_fields() {
    for check in [
        json!({"id": "sc_1"}),
        json!({"id": "sc_1", "code": null, "message": null}),
        json!({"id": "sc_1", "code": "malicious_instructions", "message": "confirm"}),
    ] {
        let wire = json!({
            "type": "computer_call",
            "call_id": "call_1",
            "actions": [{"type": "screenshot"}],
            "pending_safety_checks": [check],
        });
        let call: ComputerCall = round_trip(&wire);
        let parsed = &call.pending_safety_checks.as_ref().unwrap()[0];
        if wire["pending_safety_checks"][0].get("code").is_none() {
            assert_eq!(parsed.code, RequestField::Missing);
            assert_eq!(parsed.message, RequestField::Missing);
        }
    }
    let null_check: ComputerSafetyCheck = round_trip(&json!({"id": "sc_2", "code": null, "message": null}));
    assert_eq!(null_check.code, RequestField::Null);
    assert_eq!(null_check.message, RequestField::Null);
}

#[test]
fn action_keys_preserve_absent_null_and_list() {
    for (wire, keys) in [
        (json!({"type": "move", "x": 1, "y": 2}), None),
        (
            json!({"type": "move", "x": 1, "y": 2, "keys": null}),
            Some(ComputerKeys::Null),
        ),
        (
            json!({"type": "move", "x": 1, "y": 2, "keys": ["alt"]}),
            Some(ComputerKeys::Keys(vec!["alt".to_owned()])),
        ),
    ] {
        let action: ComputerAction = round_trip(&wire);
        let ComputerAction::Known(KnownComputerAction::Move { keys: parsed, .. }) = action else {
            panic!("move action");
        };
        assert_eq!(parsed, keys, "{wire}");
    }
}

#[test]
fn computer_call_output_round_trips_screenshot_image_url_file_id_and_detail() {
    for screenshot in [
        json!({"type": "computer_screenshot", "image_url": "data:image/png;base64,AAAA"}),
        json!({"type": "computer_screenshot", "file_id": "file_1"}),
        json!({"type": "computer_screenshot", "image_url": "https://example.com/s.png", "detail": "original"}),
    ] {
        let wire = json!({
            "id": "cuo_1",
            "call_id": "call_1",
            "output": screenshot,
            "acknowledged_safety_checks": [{"id": "sc_1"}],
            "status": "completed",
            "future_item_field": 1
        });
        let output: ComputerCallOutputMessage = round_trip(&wire);
        let ComputerCallOutputContent::ComputerScreenshot(image) = &output.output;
        assert_eq!(image.image_url.as_deref(), wire["output"]["image_url"].as_str());
        assert_eq!(image.file_id.as_deref(), wire["output"]["file_id"].as_str());
        assert_eq!(image.detail.as_deref(), wire["output"]["detail"].as_str());
    }
    let minimal = json!({"call_id": "call_1", "output": {"type": "computer_screenshot", "image_url": "x"}});
    let output: ComputerCallOutputMessage = round_trip(&minimal);
    assert!(output.status.is_none() && output.id.is_none());
}

#[test]
fn computer_call_output_rejects_non_screenshot_output() {
    for output in [
        json!({"type": "input_image", "image_url": "x"}),
        json!({"image_url": "x"}),
        json!("data:image/png;base64,AAAA"),
    ] {
        let wire = json!({"call_id": "call_1", "output": output});
        assert!(
            serde_json::from_value::<ComputerCallOutputMessage>(wire.clone()).is_err(),
            "{wire}"
        );
    }
    let empty_screenshot = json!({"call_id": "call_1", "output": {"type": "computer_screenshot"}});
    let output: ComputerCallOutputMessage = round_trip(&empty_screenshot);
    assert!(matches!(
        output.output,
        ComputerCallOutputContent::ComputerScreenshot(ref image)
            if image.image_url.is_none() && image.file_id.is_none()
    ));
    let missing_call_id = json!({"output": {"type": "computer_screenshot"}});
    assert!(serde_json::from_value::<ComputerCallOutputMessage>(missing_call_id).is_err());
}

#[test]
fn ordered_actions_prefers_nonempty_batch_and_falls_back_to_single_action() {
    let both: ComputerCall = serde_json::from_value(json!({
        "call_id": "call_1",
        "action": {"type": "wait"},
        "actions": [{"type": "screenshot"}]
    }))
    .unwrap();
    assert!(matches!(
        both.ordered_actions(),
        [ComputerAction::Known(KnownComputerAction::Screenshot { .. })]
    ));
    let empty_batch_with_legacy: ComputerCall = serde_json::from_value(json!({
        "call_id": "call_1",
        "actions": [],
        "action": {"type": "zoom", "factor": 2}
    }))
    .unwrap();
    assert!(matches!(
        empty_batch_with_legacy.ordered_actions(),
        [ComputerAction::Unknown(fields)] if fields["type"] == "zoom"
    ));
    let neither: ComputerCall = serde_json::from_value(json!({"call_id": "call_1"})).unwrap();
    assert!(neither.ordered_actions().is_empty());
}
