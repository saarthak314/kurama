use std::{
    io::Write,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use kurama_adapters::FsSessionStore;
use kurama_core::testing::ScriptedBackend;
use kurama_protocol::{
    KuramaError,
    agent::{AgentBudget, AgentSpec, AgentState, DelegationRequest, WriteScope},
    model::{FinishReason, ModelEvent, ModelProfile},
    runtime::RuntimeEvent,
    session::SessionEvent,
    tool::{Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolResult},
    traits::{BoxFuture, CancelSignal, EventSink, SessionStore, Tool},
};
use kurama_sdk::Agent;
use tokio::sync::Notify;

struct ProgressGate(Arc<Notify>);

impl EventSink for ProgressGate {
    fn emit(&self, event: RuntimeEvent) -> Result<(), KuramaError> {
        if let RuntimeEvent::AgentUpdated { snapshot } = event
            && snapshot.state == AgentState::Running
            && snapshot.phase.as_deref() == Some("working")
        {
            self.0.notify_one();
        }
        Ok(())
    }
}

struct GatedRead {
    progress: Arc<Notify>,
    executions: Arc<AtomicUsize>,
}

impl Tool for GatedRead {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "gated_read".into(),
            description: "Read the fixture after child progress is observed".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn classify(
        &self,
        context: &ToolContext,
        _invocation: &ToolInvocation,
    ) -> Result<Operation, KuramaError> {
        Ok(Operation::Read {
            paths: vec![context.cwd.join("input.txt")],
            external: false,
        })
    }

    fn execute<'a>(
        &'a self,
        context: ToolContext,
        invocation: ToolInvocation,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, KuramaError>> {
        Box::pin(async move {
            self.progress.notified().await;
            self.executions.fetch_add(1, Ordering::Relaxed);
            let text = tokio::fs::read_to_string(context.cwd.join("input.txt")).await?;
            Ok(ToolResult::success(invocation.call_id, text))
        })
    }
}

fn completed(reason: FinishReason) -> Result<ModelEvent, KuramaError> {
    Ok(ModelEvent::ResponseCompleted {
        cursor: None,
        finish_reason: reason,
    })
}

#[tokio::test]
async fn durable_child_progress_interleaves_with_tool_completion_without_losing_events() {
    let directory = tempfile::tempdir().expect("temporary workspace");
    let workspace = directory
        .path()
        .canonicalize()
        .expect("canonical workspace");
    std::fs::write(workspace.join("input.txt"), "durable evidence\n").expect("fixture");
    let store = Arc::new(FsSessionStore::open(workspace.join("state")).expect("store"));
    let progress = Arc::new(Notify::new());
    let executions = Arc::new(AtomicUsize::new(0));
    let backend = ScriptedBackend::new(vec![
        vec![
            Ok(ModelEvent::Delegation {
                request: DelegationRequest {
                    agents: vec![AgentSpec {
                        role: "researcher".into(),
                        objective: "inspect the input fixture".into(),
                        profile: None,
                        context_refs: Vec::new(),
                        write_scope: WriteScope::default(),
                        budget: AgentBudget {
                            max_input_tokens: 16_000,
                            max_output_tokens: 2_000,
                            ..AgentBudget::default()
                        },
                        depends_on: Vec::new(),
                    }],
                },
            }),
            completed(FinishReason::ToolCalls),
        ],
        vec![
            Ok(ModelEvent::ToolCall {
                call_id: "read_fixture".into(),
                name: "gated_read".into(),
                arguments: serde_json::json!({}),
            }),
            completed(FinishReason::ToolCalls),
        ],
        vec![
            Ok(ModelEvent::TextDelta {
                text: "fixture checked".into(),
            }),
            completed(FinishReason::Stop),
        ],
        vec![
            Ok(ModelEvent::TextDelta {
                text: "inspection verified".into(),
            }),
            completed(FinishReason::Stop),
        ],
    ]);
    let mut agent = Agent::new()
        .profile(
            ModelProfile::new("fixture", "fixture", 32_000, 4_000),
            Arc::new(backend),
        )
        .store(store.clone())
        .workspace(PathBuf::from(&workspace))
        .sink(Arc::new(ProgressGate(progress.clone())))
        .tool(GatedRead {
            progress,
            executions: executions.clone(),
        })
        .orchestrate()
        .build()
        .expect("agent");
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        agent.delegate("inspect with a sub-agent"),
    )
    .await
    .expect("control plane must finish")
    .expect("delegation succeeds");
    let parent = store.replay(&outcome.session_id).expect("parent replay");
    for event in &parent {
        if let SessionEvent::AgentFailed { error, .. } = &event.event {
            panic!("child failed: {error}");
        }
    }
    assert_eq!(outcome.text, "inspection verified");
    assert_eq!(executions.load(Ordering::Relaxed), 1);
    let child_id = parent
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::AgentCompleted { snapshot, summary } => {
                assert_eq!(summary, "fixture checked");
                Some(snapshot.id.clone())
            }
            _ => None,
        })
        .expect("completed child in parent history");
    let child = store
        .replay_agent(&outcome.session_id, &child_id)
        .expect("strict child replay");
    for (sequence, event) in child.iter().enumerate() {
        assert_eq!(event.sequence, sequence as u64);
    }
    let progress_position = child.iter().position(|event| matches!(
        &event.event,
        SessionEvent::AgentProgress { snapshot } if snapshot.phase.as_deref() == Some("working")
    )).expect("durable progress");
    let completion_position = child
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                SessionEvent::ToolCompleted { result, .. } if result.output == "durable evidence\n"
            )
        })
        .expect("durable tool result");
    assert!(progress_position < completion_position);
    assert!(matches!(
        child.last().map(|event| &event.event),
        Some(SessionEvent::AgentCompleted { .. })
    ));
}

#[tokio::test]
async fn sdk_resume_after_a_torn_tail_completes_the_next_turn() {
    let directory = tempfile::tempdir().expect("temporary workspace");
    let workspace = directory
        .path()
        .canonicalize()
        .expect("canonical workspace");
    let store = Arc::new(FsSessionStore::open(workspace.join("state")).expect("store"));
    let mut first = Agent::new()
        .backend(ScriptedBackend::new(vec![vec![
            Ok(ModelEvent::TextDelta {
                text: "before interruption".into(),
            }),
            completed(FinishReason::Stop),
        ]]))
        .store(store.clone())
        .workspace(workspace.clone())
        .build()
        .expect("first agent");
    let session_id = first
        .prompt("first turn")
        .await
        .expect("first turn")
        .session_id;
    drop(first);
    std::fs::OpenOptions::new()
        .append(true)
        .open(
            store
                .root()
                .join("sessions")
                .join(session_id.as_ref())
                .join("events.jsonl"),
        )
        .expect("session log")
        .write_all(br#"{"schema_version":1"#)
        .expect("torn tail");

    let mut resumed = Agent::new()
        .backend(ScriptedBackend::new(vec![vec![
            Ok(ModelEvent::TextDelta {
                text: "after recovery".into(),
            }),
            completed(FinishReason::Stop),
        ]]))
        .store(store.clone())
        .workspace(workspace)
        .build()
        .expect("resumed agent");
    resumed
        .resume(session_id.to_string())
        .await
        .expect("resume and repair");
    let outcome = tokio::time::timeout(Duration::from_secs(5), resumed.prompt("next turn"))
        .await
        .expect("resumed turn must finish")
        .expect("resumed turn succeeds");
    assert_eq!(outcome.session_id, session_id);
    assert_eq!(outcome.text, "after recovery");

    let events = store
        .replay(&session_id)
        .expect("strict replay after resumed turn");
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(event.sequence, sequence as u64);
    }
    let repairs: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(event.event, SessionEvent::RecoveryRepair { .. }).then_some(index)
        })
        .collect();
    assert_eq!(repairs.len(), 1);
    assert!(events[repairs[0] + 1..].iter().any(|event| matches!(
        &event.event, SessionEvent::AssistantMessage { text } if text == "after recovery"
    )));
}
