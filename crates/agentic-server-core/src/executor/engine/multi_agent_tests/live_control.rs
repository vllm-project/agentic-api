//! Deterministic core execution tests, not recordings of WebSocket behavior.
#![cfg(test)]

use super::*;
use crate::executor::BoxStream;
use crate::executor::engine::streaming::AbortOnDrop;
use crate::executor::multi_agent::{
    AgentPhase, RunControl,
    control::{ControlAdmissionError, OutputDecision},
};
use crate::types::client_calls::{ClientToolOutput, ClientToolOutputBatch};

fn request() -> RequestPayload {
    serde_json::from_value(json!({
        "model":"test", "store":true, "stream":true, "input":"Compare proposals",
        "multi_agent":{"enabled":true,"max_concurrent_subagents":2},
        "tools":[{"type":"function","name":"get_proposal","parameters":{"type":"object"}}]
    }))
    .unwrap()
}

fn batch(response: &str, ids: &[&str]) -> ClientToolOutputBatch {
    ClientToolOutputBatch {
        response_id: response.into(),
        outputs: ids
            .iter()
            .map(|id| {
                ClientToolOutput::Function(FunctionToolResultMessage {
                    call_id: (*id).into(),
                    output: ToolCallOutput::Text(format!("output for {id}")),
                })
            })
            .collect(),
    }
}

async fn submit(control: &RunControl, response: &str, ids: &[&str]) -> OutputDecision {
    control
        .try_submit_outputs(batch(response, ids))
        .unwrap()
        .decision()
        .await
        .unwrap()
}

async fn next_event(events: &mut mpsc::Receiver<Value>) -> Value {
    let event = events.recv().await.expect("expected another response event");
    assert_ne!(event["type"], "error", "{event}");
    event
}

fn drain_stream(mut stream: BoxStream) -> (mpsc::Receiver<Value>, AbortOnDrop<()>) {
    let (sender, receiver) = mpsc::channel(64);
    let task = tokio::spawn(async move {
        let mut sequence = 0;
        while let Some(chunk) = stream.next().await {
            for data in chunk.lines().filter_map(|line| line.strip_prefix("data: ")) {
                if data == "[DONE]" {
                    continue;
                }
                let event: Value = serde_json::from_str(data).unwrap();
                assert_eq!(event["sequence_number"], sequence, "{event}");
                sequence += 1;
                if sender.send(event).await.is_err() {
                    return;
                }
            }
        }
    });
    (receiver, AbortOnDrop::new(task))
}

#[tokio::test]
async fn live_outputs_resume_one_child_while_sibling_waits_and_commit_once() {
    // Keep root inference active while testing child output admission; a tree
    // with only pending client calls now completes for explicit continuation.
    let gate = Arc::new(Semaphore::new(0));
    let (exec, server) = setup_with_options(Some(gate.clone()), true).await;
    let work = async {
        let (control, receiver) = RunControl::channel(NonZeroUsize::new(4096).unwrap());
        let Either::Right(stream) = ExecuteRequest::new(request(), exec.clone())
            .with_run_control(receiver)
            .run()
            .await
            .unwrap()
        else {
            panic!("expected stream")
        };
        let (mut stream, mut drain) = drain_stream(stream);
        let created = next_event(&mut stream).await;
        assert_eq!(created["type"], "response.created");
        let response_id = created["response"]["id"].as_str().unwrap().to_owned();
        let mut calls = HashSet::new();
        while calls.len() != 2 {
            let event = next_event(&mut stream).await;
            assert_ne!(event["type"], "response.completed");
            if event["type"] == "response.output_item.done" && event["item"]["type"] == "function_call" {
                calls.insert(event["item"]["call_id"].as_str().unwrap().to_owned());
            }
        }
        // Validation of a later element cannot apply the valid prefix.
        assert!(matches!(
            submit(&control, &response_id, &["proposal_alpha", "missing"]).await,
            OutputDecision::Rejected(_)
        ));
        assert!(matches!(
            submit(&control, "wrong_response", &["proposal_alpha"]).await,
            OutputDecision::Rejected(_)
        ));
        assert!(matches!(
            submit(&control, &response_id, &["proposal_alpha"]).await,
            OutputDecision::Accepted
        ));
        assert!(matches!(
            submit(&control, &response_id, &["proposal_alpha"]).await,
            OutputDecision::Rejected(_)
        ));
        loop {
            let event = next_event(&mut stream).await;
            assert_ne!(event["type"], "response.completed", "sibling output has not arrived");
            if event["type"] == "response.output_item.done"
                && event["item"]["type"] == "message"
                && event["item"]["agent"]["agent_name"] == "/root/alpha"
            {
                break;
            }
        }
        assert!(matches!(
            submit(&control, &response_id, &["proposal_beta"]).await,
            OutputDecision::Accepted
        ));
        gate.add_permits(16);
        let terminal = loop {
            let event = next_event(&mut stream).await;
            if event["type"] == "response.completed" {
                break event;
            }
        };
        assert_eq!(terminal["response"]["id"], response_id);
        assert!(
            terminal["response"]["output"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["content"][0]["text"] == "combined answer")
        );
        assert_eq!(
            control
                .try_submit_outputs(batch(&response_id, &["proposal_beta"]))
                .err()
                .unwrap()
                .reason,
            ControlAdmissionError::Closed
        );
        (&mut *drain).await.unwrap();
        // Restoring the actual committed tree proves accepted inputs weren't just queued.
        let continued: RequestPayload =
            serde_json::from_value(json!({"model":"test","store":true,"previous_response_id":response_id,"input":[]}))
                .unwrap();
        let restored = crate::executor::rehydrate::rehydrate_conversation(continued, &exec)
            .await
            .unwrap();
        let tree = restored.multi_agent_tree.unwrap().into_snapshot();
        for child in ["alpha", "beta"] {
            let agent = tree
                .agents
                .iter()
                .find(|agent| agent.identity.as_str() == format!("/root/{child}"))
                .unwrap();
            assert_eq!(agent.history.iter().filter(|item| matches!(item, InputItem::FunctionCallOutput(output) if output.call_id == format!("proposal_{child}"))).count(), 1);
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(15), work).await;
    server.abort();
    result.unwrap();
}

#[tokio::test]
async fn run_control_rejects_nonstreaming_and_single_agent_execution() {
    let (exec, server) = setup().await;
    for (stream, multi_agent) in [(false, true), (true, false)] {
        let mut request = request();
        request.stream = stream;
        request.multi_agent.as_mut().unwrap().enabled = multi_agent;
        let (_control, receiver) = RunControl::channel(NonZeroUsize::new(4096).unwrap());
        let result = ExecuteRequest::new(request, exec.clone())
            .with_run_control(receiver)
            .run()
            .await;
        assert!(
            matches!(result, Err(ExecutorError::InvalidRequest(message)) if message == "run control requires streaming multi-agent inference")
        );
    }
    server.abort();
}

#[tokio::test]
async fn partial_input_waits_for_all_owner_calls_and_does_not_revive_interrupted_agents() {
    use crate::types::{
        agent::AgentCompletion,
        client_calls::{ClientCallId, ClientCallKind, ClientCallOwner, ClientCallRegistration},
    };
    let (exec, server) = setup().await;
    let ctx = crate::executor::rehydrate::rehydrate_conversation(request(), &exec)
        .await
        .unwrap();
    let mut pipeline = AgentPipeline::new(ctx, None, None);
    let mut run = MultiAgentRun::new(&mut pipeline, &exec).await.unwrap();
    let root = AgentIdentity::root();
    let turn = AgentTurnKey {
        agent: root.clone(),
        turn: run.registry.get(&root).unwrap().turn,
    };
    let registrations = ["first", "second", "interrupted"].map(|id| ClientCallRegistration {
        call_id: ClientCallId::try_from(id.to_owned()).unwrap(),
        owner: ClientCallOwner {
            agent_turn: turn.clone(),
            kind: ClientCallKind::Function,
        },
    });
    run.pending.register_calls(&run.registry, &registrations[..2]).unwrap();
    run.registry
        .set_phase(&turn, AgentPhase::WaitingForClientOutputs)
        .unwrap();
    let generation = run.contexts[&root].generation;
    assert!(matches!(
        run.accept_live_outputs(batch(&run.payload.id, &["first"])),
        OutputDecision::Accepted
    ));
    assert_eq!(
        run.registry.get(&root).unwrap().state,
        AgentState::Active(AgentPhase::WaitingForClientOutputs)
    );
    assert_eq!(run.contexts[&root].generation, generation + 1);
    assert!(matches!(
        run.accept_live_outputs(batch(&run.payload.id, &["second"])),
        OutputDecision::Accepted
    ));
    assert_eq!(
        run.registry.get(&root).unwrap().state,
        AgentState::Active(AgentPhase::Runnable)
    );
    run.pending.register_calls(&run.registry, &registrations[2..]).unwrap();
    run.registry.settle_turn(&turn, &AgentCompletion::Interrupted).unwrap();
    assert!(matches!(
        run.accept_live_outputs(batch(&run.payload.id, &["interrupted"])),
        OutputDecision::Accepted
    ));
    assert_eq!(run.registry.get(&root).unwrap().state, AgentState::Interrupted);
    assert_eq!(run.contexts[&root].generation, generation + 3);
    assert_eq!(run.pending.pending().count(), 0);
    server.abort();
}

#[tokio::test]
async fn disconnect_while_waiting_fails_run_without_publication() {
    let (exec, server) = setup_with_options(Some(Arc::new(Semaphore::new(0))), true).await;
    let work = async {
        let (control, receiver) = RunControl::channel(NonZeroUsize::new(4096).unwrap());
        let Either::Right(stream) = ExecuteRequest::new(request(), exec.clone())
            .with_run_control(receiver)
            .run()
            .await
            .unwrap()
        else {
            panic!("expected stream")
        };
        let (mut events, mut drain) = drain_stream(stream);
        let response_id = next_event(&mut events).await["response"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        loop {
            let event = next_event(&mut events).await;
            if event["type"] == "response.output_item.done" && event["item"]["type"] == "function_call" {
                break;
            }
        }
        drop(control);
        let mut failed = false;
        while let Some(event) = events.recv().await {
            assert_ne!(event["type"], "response.completed");
            failed |= event["type"] == "error";
        }
        (&mut *drain).await.unwrap();
        assert!(failed);
        assert!(exec.resp_handler.retrieve(&response_id).await.is_err());
    };
    let result = tokio::time::timeout(Duration::from_secs(15), work).await;
    server.abort();
    result.unwrap();
}

#[tokio::test]
async fn accepted_output_invalidates_inflight_compaction_without_losing_history() {
    use crate::executor::multi_agent::{CompactionCommit, CompactionPlan};
    use crate::types::client_calls::{ClientCallId, ClientCallKind, ClientCallOwner, ClientCallRegistration};
    let (exec, server) = setup().await;
    let ctx = crate::executor::rehydrate::rehydrate_conversation(request(), &exec)
        .await
        .unwrap();
    let mut pipeline = AgentPipeline::new(ctx, None, None);
    let mut run = MultiAgentRun::new(&mut pipeline, &exec).await.unwrap();
    let root = AgentIdentity::root();
    let turn = AgentTurnKey {
        agent: root.clone(),
        turn: run.registry.get(&root).unwrap().turn,
    };
    run.pending
        .register_calls(
            &run.registry,
            &[ClientCallRegistration {
                call_id: ClientCallId::try_from("proposal".to_owned()).unwrap(),
                owner: ClientCallOwner {
                    agent_turn: turn,
                    kind: ClientCallKind::Function,
                },
            }],
        )
        .unwrap();
    let context = &run.contexts[&root];
    let plan = CompactionPlan::prepare_explicit(&root, context.generation, &context.stored.history, &context.request)
        .unwrap()
        .unwrap();
    assert!(matches!(
        run.accept_live_outputs(batch(&run.payload.id, &["proposal"])),
        OutputDecision::Accepted
    ));
    let before = serde_json::to_value(&run.contexts[&root].stored.history).unwrap();
    let result = plan.execute(&exec, None).await.unwrap();
    assert_eq!(
        run.try_commit_compaction(result, &mut pipeline).await.unwrap(),
        CompactionCommit::Stale
    );
    assert_eq!(
        serde_json::to_value(&run.contexts[&root].stored.history).unwrap(),
        before
    );
    assert!(
        run.payload.usage.unwrap().total_tokens > 0,
        "discarded compaction still counts usage"
    );
    server.abort();
}

#[tokio::test]
async fn dropping_event_stream_cancels_waiting_core_and_closes_control() {
    let (exec, server) = setup_with_options(Some(Arc::new(Semaphore::new(0))), true).await;
    let work = async {
        let (control, receiver) = RunControl::channel(NonZeroUsize::new(4096).unwrap());
        let Either::Right(stream) = ExecuteRequest::new(request(), exec.clone())
            .with_run_control(receiver)
            .run()
            .await
            .unwrap()
        else {
            panic!("expected stream")
        };
        let (mut events, drain) = drain_stream(stream);
        let response_id = next_event(&mut events).await["response"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        loop {
            let event = next_event(&mut events).await;
            if event["type"] == "response.output_item.done" && event["item"]["type"] == "function_call" {
                break;
            }
        }
        drop(drain);
        drop(events);
        loop {
            match control.try_submit_outputs(batch(&response_id, &["unknown"])) {
                Ok(receipt) => {
                    let _ = receipt.decision().await;
                }
                Err(rejected) => {
                    assert_eq!(rejected.reason, ControlAdmissionError::Closed);
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
        assert!(exec.resp_handler.retrieve(&response_id).await.is_err());
    };
    let result = tokio::time::timeout(Duration::from_secs(15), work).await;
    server.abort();
    result.unwrap();
}

#[derive(Clone, Copy, Debug)]
enum InjectionOrder {
    BeforeSnapshot,
    DuringSummary,
    AfterCommit,
}

async fn apply_through_control(
    run: &mut MultiAgentRun,
    control: &RunControl,
    receiver: &mut RunControlReceiver,
) -> OutputDecision {
    let submission = control
        .try_submit_outputs(batch(&run.payload.id, &["race_call"]))
        .unwrap();
    receiver
        .recv()
        .await
        .unwrap()
        .apply(|input| run.accept_live_outputs(input));
    submission.decision().await.unwrap()
}

fn register_race_call(run: &mut MultiAgentRun, agent: &AgentIdentity) -> AgentTurnKey {
    use crate::types::client_calls::{ClientCallId, ClientCallKind, ClientCallOwner, ClientCallRegistration};
    let turn = AgentTurnKey {
        agent: agent.clone(),
        turn: run.registry.get(agent).unwrap().turn,
    };
    run.pending
        .register_calls(
            &run.registry,
            &[ClientCallRegistration {
                call_id: ClientCallId::try_from("race_call".to_owned()).unwrap(),
                owner: ClientCallOwner {
                    agent_turn: turn.clone(),
                    kind: ClientCallKind::Function,
                },
            }],
        )
        .unwrap();
    run.contexts.get_mut(agent).unwrap().stored.history.push(
        serde_json::from_value(json!({
            "type":"function_call","id":"fc_race","call_id":"race_call","name":"get_proposal","arguments":"{}"
        }))
        .unwrap(),
    );
    run.registry
        .set_phase(&turn, AgentPhase::WaitingForClientOutputs)
        .unwrap();
    turn
}

#[tokio::test]
async fn control_injection_before_during_and_after_compaction_keeps_call_linkage() {
    use crate::executor::multi_agent::{CompactionCommit, CompactionPlan};
    for order in [
        InjectionOrder::BeforeSnapshot,
        InjectionOrder::DuringSummary,
        InjectionOrder::AfterCommit,
    ] {
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let (exec, server) = setup_with_compaction_gate(None, false, Some((started.clone(), release.clone()))).await;
        let work = async {
            let ctx = crate::executor::rehydrate::rehydrate_conversation(request(), &exec)
                .await
                .unwrap();
            let mut pipeline = AgentPipeline::new(ctx, None, None);
            let mut run = MultiAgentRun::new(&mut pipeline, &exec).await.unwrap();
            let root = AgentIdentity::root();
            register_race_call(&mut run, &root);
            let (control, mut receiver) = RunControl::channel(NonZeroUsize::new(4096).unwrap());
            if matches!(order, InjectionOrder::BeforeSnapshot) {
                assert!(matches!(
                    apply_through_control(&mut run, &control, &mut receiver).await,
                    OutputDecision::Accepted
                ));
            }
            let context = &run.contexts[&root];
            let plan =
                CompactionPlan::prepare_explicit(&root, context.generation, &context.stored.history, &context.request)
                    .unwrap()
                    .unwrap();
            let summary = plan.execute(&exec, None);
            tokio::pin!(summary);
            tokio::select! {
                () = started.notified() => {},
                result = &mut summary => panic!("summary escaped the barrier: {}", result.is_ok()),
            }
            if matches!(order, InjectionOrder::DuringSummary) {
                assert!(matches!(
                    apply_through_control(&mut run, &control, &mut receiver).await,
                    OutputDecision::Accepted
                ));
            }
            release.add_permits(1);
            let result = summary.await.unwrap();
            let expected = if matches!(order, InjectionOrder::DuringSummary) {
                CompactionCommit::Stale
            } else {
                CompactionCommit::Applied
            };
            assert_eq!(
                run.try_commit_compaction(result, &mut pipeline).await.unwrap(),
                expected,
                "{order:?}"
            );
            if matches!(order, InjectionOrder::AfterCommit) {
                assert!(matches!(
                    apply_through_control(&mut run, &control, &mut receiver).await,
                    OutputDecision::Accepted
                ));
            }
            let history = &run.contexts[&root].stored.history;
            // BeforeSnapshot may compact the resolved pair into the summary.
            if !matches!(order, InjectionOrder::BeforeSnapshot) {
                assert_eq!(
                    history
                        .iter()
                        .filter(|item| matches!(item, InputItem::FunctionCall(call) if call.call_id == "race_call"))
                        .count(),
                    1
                );
                assert_eq!(history.iter().filter(|item| matches!(item, InputItem::FunctionCallOutput(output) if output.call_id == "race_call")).count(), 1);
            }
            assert!(matches!(
                apply_through_control(&mut run, &control, &mut receiver).await,
                OutputDecision::Rejected(_)
            ));
            assert_eq!(run.pending.pending().count(), 0);
            assert!(run.payload.usage.unwrap().total_tokens > 0);
        };
        let result = tokio::time::timeout(Duration::from_secs(10), Box::pin(work)).await;
        server.abort();
        result.unwrap();
    }
}

#[tokio::test]
async fn sibling_output_and_mail_are_preserved_during_root_compaction() {
    use crate::executor::multi_agent::{CompactionCommit, CompactionPlan};
    use crate::types::agent::AgentMailContent;
    use crate::types::agent_commands::AgentCommand;
    for send_mail in [false, true] {
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let (exec, server) = setup_with_compaction_gate(None, false, Some((started.clone(), release.clone()))).await;
        let work =
            async {
                let ctx = crate::executor::rehydrate::rehydrate_conversation(request(), &exec)
                    .await
                    .unwrap();
                let mut pipeline = AgentPipeline::new(ctx, None, None);
                let mut run = MultiAgentRun::new(&mut pipeline, &exec).await.unwrap();
                let root = AgentIdentity::root();
                let root_turn = AgentTurnKey {
                    agent: root.clone(),
                    turn: run.registry.get(&root).unwrap().turn,
                };
                run.spawn_agent(
                    &root_turn,
                    serde_json::from_value(json!({
                        "task_name":"worker","message":"bounded work","fork_turns":"all"
                    }))
                    .unwrap(),
                    &mut pipeline,
                )
                .await
                .unwrap();
                let child = AgentIdentity::try_from("/root/worker".to_owned()).unwrap();
                let child_turn = register_race_call(&mut run, &child);
                let context = &run.contexts[&root];
                let plan = CompactionPlan::prepare_explicit(
                    &root,
                    context.generation,
                    &context.stored.history,
                    &context.request,
                )
                .unwrap()
                .unwrap();
                let summary = plan.execute(&exec, None);
                tokio::pin!(summary);
                tokio::select! {
                    () = started.notified() => {},
                    result = &mut summary => panic!("summary escaped barrier: {}", result.is_ok()),
                }
                let (control, mut receiver) = RunControl::channel(NonZeroUsize::new(4096).unwrap());
                assert!(matches!(
                    apply_through_control(&mut run, &control, &mut receiver).await,
                    OutputDecision::Accepted
                ));
                assert_eq!(
                    run.registry.get(&child).unwrap().state,
                    AgentState::Active(AgentPhase::Runnable)
                );
                if send_mail {
                    run.dispatch(
                        &child_turn,
                        AgentCommand::Send(
                            serde_json::from_value(json!({
                                "target":"/root","message":"arrived during summary"
                            }))
                            .unwrap(),
                        ),
                        "mail_race",
                        &mut pipeline,
                    )
                    .await
                    .unwrap();
                }
                release.add_permits(1);
                let expected = if send_mail {
                    CompactionCommit::Stale
                } else {
                    CompactionCommit::Applied
                };
                assert_eq!(
                    run.try_commit_compaction(summary.await.unwrap(), &mut pipeline)
                        .await
                        .unwrap(),
                    expected
                );
                assert_eq!(run.contexts[&child].stored.history.iter().filter(|item|
            matches!(item, InputItem::FunctionCallOutput(output) if output.call_id == "race_call")).count(), 1);
                if send_mail {
                    let mail = run.registry.take_mailbox(&root_turn).unwrap();
                    assert!(
                        mail.iter()
                            .any(|mail| mail.content == AgentMailContent::Message("arrived during summary".into()))
                    );
                }
            };
        let result = tokio::time::timeout(Duration::from_secs(10), Box::pin(work)).await;
        server.abort();
        result.unwrap();
    }
}

#[tokio::test]
async fn disconnect_during_compaction_cancels_control_without_publishing() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let (exec, server) = setup_with_compaction_gate(None, false, Some((started.clone(), release.clone()))).await;
    let work = async {
        let mut payload = request();
        payload.input = ResponsesInput::Text("simple compaction test".into());
        payload.context_management = Some(vec![ContextManagement {
            type_: "compaction".into(),
            compact_threshold: Some(1),
        }]);
        let (control, receiver) = RunControl::channel(NonZeroUsize::new(4096).unwrap());
        let Either::Right(stream) = ExecuteRequest::new(payload, exec.clone())
            .with_run_control(receiver)
            .run()
            .await
            .unwrap()
        else {
            panic!("expected stream");
        };
        let (mut events, drain) = drain_stream(stream);
        let response_id = next_event(&mut events).await["response"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        started.notified().await;
        drop(drain);
        drop(events);
        loop {
            match control.try_submit_outputs(batch(&response_id, &["unknown"])) {
                Ok(submission) => {
                    let _ = submission.decision().await;
                }
                Err(rejected) => {
                    assert_eq!(rejected.reason, ControlAdmissionError::Closed);
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
        // Release the fake upstream only after coordinator teardown: no late commit.
        release.add_permits(1);
        assert!(exec.resp_handler.retrieve(&response_id).await.is_err());
    };
    let result = tokio::time::timeout(Duration::from_secs(10), Box::pin(work)).await;
    server.abort();
    result.unwrap();
}
