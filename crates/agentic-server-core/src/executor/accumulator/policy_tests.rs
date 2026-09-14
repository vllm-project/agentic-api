use super::*;
use crate::executor::pipeline::RoundIngestion;
use crate::executor::translate::TranslationContext;
use crate::executor::translate::TranslationDispatcher;
use crate::tool::ToolType;
use serde_json::json;

#[test]
fn non_data_lines_do_not_change_lifecycle_or_satisfy_strict_finalization() {
    for validation in [Validation::Strict, Validation::Lenient] {
        let mut acc = ResponseAccumulator::with_validation("resp_1".to_owned(), None, validation);
        let mut translator = TranslationDispatcher::new(TranslationContext::default());
        for line in ["", ": heartbeat", "event: response.created", "data:   ", "data:[DONE]"] {
            assert!(
                RoundIngestion::translate_line(&mut acc, SseLine::parse(line), &mut translator)
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(acc.stream_lifecycle, StreamLifecycle::AwaitingCreated);
        assert!(acc.finish_strict_stream().is_err());

        for event in [
            json!({"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}),
            json!({"type":"response.in_progress","response":{"id":"resp_1","status":"in_progress"}}),
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[]}}),
        ] {
            RoundIngestion::translate_line(&mut acc, SseLine::parse(&format!("data:{event}")), &mut translator)
                .unwrap();
        }
        for line in [": heartbeat", "data: [DONE]"] {
            assert!(
                RoundIngestion::translate_line(&mut acc, SseLine::parse(line), &mut translator)
                    .unwrap()
                    .is_none()
            );
        }
        assert_eq!(acc.stream_lifecycle, StreamLifecycle::Terminal);
        acc.finish_strict_stream().unwrap();
        translator.finish().unwrap();
    }
}

#[test]
fn malformed_data_is_rejected_or_skipped_without_affecting_translation() {
    for validation in [Validation::Strict, Validation::Lenient] {
        let mut acc = ResponseAccumulator::with_validation("resp_1".to_owned(), None, validation);
        let mut translator = TranslationDispatcher::new(TranslationContext::default());
        for line in ["data:{", "data: null", "data: []", "data: {\"type\":3}"] {
            let result = RoundIngestion::translate_line(&mut acc, SseLine::parse(line), &mut translator);
            match validation {
                Validation::Strict => assert!(result.unwrap_err().to_string().contains("malformed data frame")),
                Validation::Lenient => assert!(result.unwrap().is_none()),
            }
            assert_eq!(acc.stream_lifecycle, StreamLifecycle::AwaitingCreated);
            assert_eq!(acc.slots.len(), 0);
        }
        assert!(translator.finish().unwrap().unfinished_tool_search_item_ids.is_empty());
    }
}

#[test]
fn both_policies_translate_the_same_accepted_custom_call() {
    let mut public_streams = Vec::new();
    for validation in [Validation::Strict, Validation::Lenient] {
        let mut acc = ResponseAccumulator::with_validation("resp_1".to_owned(), None, validation);
        let context = TranslationContext::new(
            HashMap::from([("raw_echo".to_owned(), ToolType::Custom)]),
            HashSet::new(),
            false,
        );
        let mut translator = TranslationDispatcher::new(context);
        let mut frames = Vec::new();
        let item = json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"raw_echo",
            "arguments":"{\"input\":\"hi\"}","status":"completed"});
        for event in [
            json!({"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}),
            json!({"type":"response.in_progress","response":{"id":"resp_1","status":"in_progress"}}),
            json!({"type":"response.output_item.added","output_index":0,
                "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"raw_echo","arguments":"","status":"in_progress"}}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":"{\"input\":\"hi\"}"}),
            json!({"type":"response.function_call_arguments.done","output_index":0,"item_id":"fc_1","name":"raw_echo","arguments":"{\"input\":\"hi\"}"}),
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[item]}}),
        ] {
            let translated =
                RoundIngestion::translate_line(&mut acc, SseLine::parse(&format!("data: {event}")), &mut translator)
                    .unwrap()
                    .unwrap();
            frames.extend(
                translated
                    .frames
                    .into_iter()
                    .map(|frame| serde_json::to_value(frame.wire).unwrap()),
            );
        }
        acc.finish_strict_stream().unwrap();
        translator.finish().unwrap();
        assert_eq!(frames[2]["item"]["type"], "custom_tool_call");
        assert_eq!(frames[3]["delta"], "hi");
        assert_eq!(frames[4]["type"], "response.custom_tool_call_input.done");
        assert_eq!(frames[5]["item"]["input"], "hi");
        public_streams.push(frames);
    }
    assert_eq!(public_streams[0], public_streams[1]);
}
