use std::sync::Arc;

use kurama_core::testing::{AllowAllPolicy, EchoTool, MemoryStore, ScriptedBackend, SequenceIds};
use kurama_protocol::{
    KuramaError,
    model::{FinishReason, ModelEvent},
    policy::{PolicyContext, PolicyDecision},
    tool::Operation,
    traits::ApprovalPolicy,
};
use kurama_sdk::{Agent, Event, ModelProfile};

type ScriptEvent = Result<ModelEvent, KuramaError>;

#[test]
fn agent_supplies_runtime_defaults() {
    let agent = Agent::new()
        .backend(ScriptedBackend::new(Vec::new()))
        .tool(EchoTool)
        .build()
        .expect("runtime");

    assert_eq!(agent.active_profile(), "scripted");
    assert_eq!(agent.registered_tools(), vec!["echo"]);
}

#[test]
fn agent_rejects_duplicate_tools() {
    let error = Agent::new()
        .tool(EchoTool)
        .tool(EchoTool)
        .build()
        .expect_err("duplicate tool");
    assert!(error.to_string().contains("duplicate tool"));
}

#[tokio::test]
async fn prompt_returns_assistant_text() {
    let mut agent = Agent::new()
        .profile(
            ModelProfile::new("custom", "frontier", 32_000, 4_000),
            Arc::new(ScriptedBackend::new(vec![vec![
                text("hello from kurama"),
                completed(),
            ]])),
        )
        .policy(Arc::new(AllowAllPolicy))
        .ids(Arc::new(SequenceIds::new(1)))
        .build()
        .expect("runtime");

    let reply = agent.prompt("hi").await.expect("prompt");
    assert_eq!(reply.text, "hello from kurama");
    assert_eq!(reply.to_string(), "hello from kurama");
    assert_eq!(reply.session_id.as_ref(), "s_1");
}

#[tokio::test]
async fn prompt_errors_when_approval_is_required() {
    let mut agent = Agent::new()
        .backend(ScriptedBackend::new(vec![vec![
            Ok(ModelEvent::ToolCall {
                call_id: "call".into(),
                name: "echo".into(),
                arguments: Default::default(),
            }),
            completed(),
        ]]))
        .tool(EchoTool)
        .policy(Arc::new(AskPolicy))
        .build()
        .expect("runtime");

    let error = agent.prompt("mutate").await.expect_err("approval");
    assert!(error.to_string().contains("approval required"));
}

#[tokio::test]
async fn prompt_after_approval_error_still_runs() {
    let mut agent = Agent::new()
        .backend(ScriptedBackend::new(vec![
            vec![
                Ok(ModelEvent::ToolCall {
                    call_id: "call".into(),
                    name: "echo".into(),
                    arguments: Default::default(),
                }),
                completed(),
            ],
            vec![text("recovered"), completed()],
        ]))
        .tool(EchoTool)
        .policy(Arc::new(AskPolicy))
        .build()
        .expect("runtime");

    let _ = agent.prompt("mutate").await.expect_err("approval");
    let reply = agent.prompt("continue").await.expect("second prompt");
    assert_eq!(reply.text, "recovered");
}

#[tokio::test]
async fn turn_next_returns_none_after_done() {
    let mut agent = Agent::new()
        .backend(ScriptedBackend::new(vec![vec![text("done"), completed()]]))
        .policy(Arc::new(AllowAllPolicy))
        .build()
        .expect("runtime");

    let mut turn = agent.turn("hi").await.expect("turn");
    let mut saw_done = false;
    while let Some(event) = turn.next().await.expect("event") {
        if matches!(event, Event::Done(_)) {
            saw_done = true;
        }
    }
    assert!(saw_done);
    assert!(turn.next().await.expect("after done").is_none());
}

#[tokio::test]
async fn cancelling_and_draining_a_partial_turn_keeps_the_next_prompt_clean() {
    let mut agent = Agent::new()
        .backend(ScriptedBackend::new(vec![
            vec![
                text("abandoned text"),
                Ok(ModelEvent::ToolCall {
                    call_id: "pending".into(),
                    name: "echo".into(),
                    arguments: Default::default(),
                }),
                completed(),
            ],
            vec![text("second response"), completed()],
        ]))
        .tool(EchoTool)
        .policy(Arc::new(AskPolicy))
        .build()
        .expect("agent");
    let mut turn = agent.turn("first request").await.expect("turn");
    let mut first_text = String::new();
    loop {
        match turn.next().await.expect("event").expect("live turn") {
            Event::Text(text) => first_text.push_str(&text),
            Event::Approval(_) => break,
            Event::Done(_) | Event::Error(_) => panic!("turn ended before approval"),
            _ => {}
        }
    }
    assert_eq!(first_text, "abandoned text");
    turn.cancel().await.expect("cancel");
    tokio::time::timeout(std::time::Duration::from_secs(1), turn.drain())
        .await
        .expect("drain stops");
    assert!(turn.next().await.expect("after drain").is_none());
    drop(turn);
    assert_eq!(
        agent
            .prompt("second request")
            .await
            .expect("next prompt")
            .text,
        "second response"
    );
}

#[tokio::test]
async fn session_id_tracks_the_live_session() {
    let mut agent = Agent::new()
        .profile(
            ModelProfile::new("custom", "frontier", 32_000, 4_000),
            Arc::new(ScriptedBackend::new(vec![vec![text("hi"), completed()]])),
        )
        .policy(Arc::new(AllowAllPolicy))
        .ids(Arc::new(SequenceIds::new(1)))
        .build()
        .expect("runtime");

    assert!(agent.session_id().is_none());
    let reply = agent.prompt("hi").await.expect("prompt");
    assert_eq!(agent.session_id().map(AsRef::as_ref), Some("s_1"));
    assert_eq!(reply.session_id.as_ref(), "s_1");
}

#[tokio::test]
async fn resume_rejects_a_foreign_workspace() {
    let store = Arc::new(MemoryStore::default());
    let mut first = Agent::new()
        .backend(ScriptedBackend::new(vec![vec![text("hi"), completed()]]))
        .store(store.clone())
        .workspace("/tmp/kurama-e2e-a")
        .policy(Arc::new(AllowAllPolicy))
        .build()
        .expect("first");
    let reply = first.prompt("hi").await.expect("prompt");

    let mut second = Agent::new()
        .backend(ScriptedBackend::new(Vec::new()))
        .store(store)
        .workspace("/tmp/kurama-e2e-b")
        .build()
        .expect("second");
    let error = second
        .resume(reply.session_id.to_string())
        .await
        .expect_err("foreign workspace");
    assert!(error.to_string().contains("another project"));
}

#[tokio::test]
async fn delegate_requires_orchestration() {
    let mut agent = Agent::new()
        .backend(ScriptedBackend::new(Vec::new()))
        .build()
        .expect("runtime");
    let error = agent
        .delegate("split the work")
        .await
        .expect_err("delegate");
    assert!(error.to_string().contains("orchestrate"));
}

#[tokio::test]
async fn resume_without_a_session_is_not_found() {
    let mut agent = Agent::new()
        .backend(ScriptedBackend::new(Vec::new()))
        .build()
        .expect("runtime");

    let error = agent.resume("ses_missing").await.expect_err("missing");
    assert!(matches!(error, KuramaError::NotFound(_)));
}

fn text(value: &str) -> ScriptEvent {
    Ok(ModelEvent::TextDelta { text: value.into() })
}

fn completed() -> ScriptEvent {
    Ok(ModelEvent::ResponseCompleted {
        cursor: None,
        finish_reason: FinishReason::Stop,
    })
}

#[derive(Default)]
struct AskPolicy;

impl ApprovalPolicy for AskPolicy {
    fn decide(&self, _context: &PolicyContext, _operation: &Operation) -> PolicyDecision {
        PolicyDecision::Ask {
            reason: "test approval".into(),
        }
    }
}
