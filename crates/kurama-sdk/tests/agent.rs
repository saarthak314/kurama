use std::sync::Arc;

use kurama_core::testing::{AllowAllPolicy, EchoTool, ScriptedBackend, SequenceIds};
use kurama_protocol::{
    KuramaError,
    model::{FinishReason, ModelEvent},
    policy::{PolicyContext, PolicyDecision},
    tool::Operation,
    traits::ApprovalPolicy,
};
use kurama_sdk::{Agent, ModelProfile};

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
