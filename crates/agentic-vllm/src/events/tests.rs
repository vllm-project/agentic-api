use agentic_core::executor::error::ExecutorResult;
use agentic_core::executor::inference::direct::DirectInferenceBackend;
use agentic_core::executor::{ConversationHandler, ExecuteRequest, ExecutionContext, ResponseHandler};
use agentic_core::storage::{ConversationStore, ResponseStore};
use agentic_core::types::request_response::{RequestPayload, UpstreamRequest};
use futures::future::BoxFuture;
use vllm_chat::{AssistantMessage, AssistantToolCall};
use vllm_llm::{PromptCacheUsage, RequestTimestamps, TokenUsage};

use super::*;

fn usage() -> ChatTokenUsage {
    ChatTokenUsage {
        engine: TokenUsage {
            prompt_token_count: 10,
            output_token_count: 4,
            prompt_cache_usage: PromptCacheUsage {
                cached_token_count: 7,
                cache_creation_token_count: 3,
            },
        },
        reasoning_tokens: 2,
    }
}

fn done(finish_reason: FinishReason) -> ChatEvent {
    ChatEvent::Done {
        message: AssistantMessage::default(),
        usage: usage(),
        finish_reason,
        timestamps: RequestTimestamps::default(),
        spec_decode_metrics: None,
        kv_transfer_params: None,
        ec_transfer_params: None,
        weight_versions: None,
        routed_experts: None,
    }
}

fn sample_events(finish_reason: FinishReason) -> Vec<ChatEvent> {
    vec![
        ChatEvent::Start {
            prompt_token_ids: Arc::from([1, 2]),
            prompt_logprobs: None,
        },
        ChatEvent::BlockStart {
            index: 0,
            kind: AssistantBlockKind::Reasoning,
        },
        ChatEvent::BlockDelta {
            index: 0,
            kind: AssistantBlockKind::Reasoning,
            delta: "think".to_owned(),
            token_count: Some(2),
        },
        ChatEvent::BlockEnd {
            index: 0,
            block: AssistantContentBlock::Reasoning {
                text: "think".to_owned(),
            },
        },
        ChatEvent::BlockStart {
            index: 1,
            kind: AssistantBlockKind::Text,
        },
        ChatEvent::BlockDelta {
            index: 1,
            kind: AssistantBlockKind::Text,
            delta: "answer".to_owned(),
            token_count: Some(1),
        },
        ChatEvent::BlockEnd {
            index: 1,
            block: AssistantContentBlock::Text {
                text: "answer".to_owned(),
            },
        },
        ChatEvent::ToolCallStart {
            index: 0,
            id: "call_1".to_owned(),
            name: "echo".to_owned(),
        },
        ChatEvent::ToolCallArgumentsDelta {
            index: 0,
            delta: "{}".to_owned(),
        },
        ChatEvent::ToolCallEnd {
            index: 0,
            call: AssistantToolCall {
                id: "call_1".to_owned(),
                name: "echo".to_owned(),
                arguments: "{}".to_owned(),
            },
        },
        done(finish_reason),
    ]
}

#[derive(Debug)]
struct ConvertedBackend(Vec<ChatEvent>);

impl DirectInferenceBackend for ConvertedBackend {
    fn generate<'a>(
        &'a self,
        _: UpstreamRequest<'a>,
        control: GenerationControl,
    ) -> BoxFuture<'a, ExecutorResult<EventStream>> {
        Box::pin(async move {
            let mut adapter = EventAdapter::new(&control.response_id, &control.request_id, "test");
            let frames = self
                .0
                .iter()
                .cloned()
                .map(|event| adapter.convert(event))
                .collect::<Result<Vec<_>>>()
                .map_err(|error| *error)?
                .into_iter()
                .flatten()
                .map(Ok)
                .collect::<Vec<_>>();
            Ok(Box::pin(futures::stream::iter(frames)) as EventStream)
        })
    }
}

#[tokio::test]
async fn native_blocks_and_calls_share_core_assembly_without_index_collisions_or_double_counting() {
    for (finish, status) in [
        (FinishReason::Stop(None), "completed"),
        (FinishReason::Length, "incomplete"),
    ] {
        let context = ExecutionContext::new(
            ConversationHandler::new(ConversationStore::disabled()),
            ResponseHandler::new(ResponseStore::disabled()),
            Arc::new(reqwest::Client::new()),
            "invalid-http-url".to_owned(),
        )
        .with_direct_inference(Arc::new(ConvertedBackend(sample_events(finish))));
        let request: RequestPayload = serde_json::from_value(json!({
            "model":"test","input":"hello","store":false,
            "tools":[{"type":"function","name":"echo","parameters":{"type":"object"}}],
        }))
        .unwrap();
        let response = ExecuteRequest::new(request, Arc::new(context))
            .run()
            .await
            .unwrap()
            .left()
            .unwrap();
        assert_eq!(response.status, status);
        let output = serde_json::to_value(&response.output).unwrap();
        assert_eq!(output.as_array().unwrap().len(), 3);
        assert_eq!(output[0]["content"][0]["text"], "think");
        assert_eq!(output[1]["content"][0]["text"], "answer");
        assert_eq!(output[2]["call_id"], "call_1");
        assert_eq!(output[2]["arguments"], "{}");
        let usage = response.usage.unwrap();
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (10, 4, 14)
        );
        assert_eq!(usage.input_tokens_details.cached_tokens, 7);
        assert_eq!(usage.input_tokens_details.cache_write_tokens, Some(3));
        assert_eq!(usage.output_tokens_details.reasoning_tokens, 2);
        if status == "incomplete" {
            assert_eq!(
                response.incomplete_details.unwrap().reason.as_deref(),
                Some("max_output_tokens")
            );
        }
    }
}

#[test]
fn invalid_native_usage_and_engine_failures_cannot_become_successful_responses() {
    let mut invalid = usage();
    invalid.reasoning_tokens = 5;
    assert!(response_usage(invalid).is_err());
    invalid = usage();
    invalid.engine.prompt_cache_usage.cached_token_count = 11;
    assert!(response_usage(invalid).is_err());
    invalid = usage();
    invalid.engine.prompt_token_count = usize::MAX;
    assert!(response_usage(invalid).is_err());
    for finish in [FinishReason::Abort, FinishReason::Error(None)] {
        assert!(
            EventAdapter::new("resp", "attempt", "test")
                .convert(done(finish))
                .is_err()
        );
    }
}
