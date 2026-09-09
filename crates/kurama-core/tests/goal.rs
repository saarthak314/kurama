use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use futures_util::stream;

use kurama_core::{
    context::{ContextManager, ContextPolicy},
    engine::{Engine, EngineConfig},
    testing::{
        AllowAllPolicy, CollectingSink, MemoryStore, NoDelegation, ScriptedBackend, SequenceIds,
    },
};
use kurama_protocol::{
    KuramaError,
    agent::WriteScope,
    id::{AgentId, SessionId},
    model::{BackendCapabilities, FinishReason, ModelEvent, ModelItem, ModelProfile, ModelRequest},
    policy::{AutoBoundaries, ExecutionMode},
    runtime::RuntimeEvent,
    session::{EventEnvelope, GoalStatus, SessionEvent, SessionGoal, SessionMetadata, latest_goal},
    traits::{BoxFuture, CancelSignal, ModelBackend, ModelStream, SessionStore},
};

struct RecordingBackend {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl ModelBackend for RecordingBackend {
    fn backend_name(&self) -> &'static str {
        "recording"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::remote_default()
    }

    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ModelStream, KuramaError>> {
        Box::pin(async move {
            self.requests
                .lock()
                .expect("recorded requests lock")
                .push(request);
            Ok(Box::pin(stream::iter([Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            })])) as ModelStream)
        })
    }
}

fn event(sequence: u64, agent_id: Option<AgentId>, event: SessionEvent) -> EventEnvelope {
    EventEnvelope::new(
        sequence,
        sequence,
        SessionId::from("goal-session"),
        agent_id,
        event,
    )
}

fn config(
    backend: Arc<dyn ModelBackend>,
    store: Arc<MemoryStore>,
    agent_id: Option<AgentId>,
) -> EngineConfig {
    EngineConfig {
        session: SessionMetadata {
            id: "goal-session".into(),
            created_at_ms: 0,
            project_root: ".".into(),
            profile: "test".into(),
            mode: ExecutionMode::Supervised,
            redaction_best_effort: false,
        },
        profile: ModelProfile::new("test", "frontier", 4_000, 500),
        backend,
        tools: Vec::new(),
        policy: Arc::new(AllowAllPolicy),
        store,
        sink: Arc::new(CollectingSink::default()),
        orchestrator: Arc::new(NoDelegation),
        ids: Arc::new(SequenceIds::new(1)),
        context_policy: ContextPolicy::default(),
        workspace_root: PathBuf::from("."),
        write_scope: WriteScope::default(),
        auto: AutoBoundaries::default(),
        agent_id,
        orchestration: None,
        provider_retry_delays_ms: Vec::new(),
        command_capacity: 32,
        event_capacity: 128,
    }
}

async fn wait_for_turn(events: &mut tokio::sync::mpsc::Receiver<RuntimeEvent>) {
    loop {
        match events.recv().await.expect("runtime event") {
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("engine error: {message}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn goal_continues_until_update_goal_complete() {
    let backend: Arc<dyn ModelBackend> = Arc::new(ScriptedBackend::new(vec![
        vec![Ok(ModelEvent::ResponseCompleted {
            cursor: None,
            finish_reason: FinishReason::Stop,
        })],
        vec![
            Ok(ModelEvent::ToolCall {
                call_id: "goal-call".into(),
                name: "update_goal".into(),
                arguments: serde_json::json!({
                    "status": "complete",
                    "reason": "tests pass"
                }),
            }),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::ToolCalls,
            }),
        ],
        vec![Ok(ModelEvent::ResponseCompleted {
            cursor: None,
            finish_reason: FinishReason::Stop,
        })],
    ]));
    let store = Arc::new(MemoryStore::default());
    let (handle, mut events) =
        Engine::spawn(config(backend, Arc::clone(&store), None), Vec::new()).expect("spawn engine");

    handle.set_goal("keep tests green").await.expect("set goal");
    wait_for_turn(&mut events).await;

    let replay = store
        .replay(&SessionId::from("goal-session"))
        .expect("replay session");
    let goal = latest_goal(&replay).expect("persisted goal");
    assert_eq!(goal.objective, "keep tests green");
    assert_eq!(goal.status, GoalStatus::Achieved);
    assert!(goal.turns >= 1);
}

#[tokio::test]
async fn child_engine_does_not_receive_update_goal_descriptor() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(RecordingBackend {
        requests: Arc::clone(&requests),
    });
    let child_id = AgentId::from("child");
    let replay = vec![
        event(
            0,
            Some(child_id.clone()),
            SessionEvent::UserMessage {
                text: "previous".into(),
            },
        ),
        event(1, Some(child_id.clone()), SessionEvent::TurnCompleted),
    ];
    let store = Arc::new(MemoryStore::default());
    for replay_event in &replay {
        store.append(replay_event).expect("seed child replay");
    }
    let (handle, mut events) =
        Engine::spawn(config(backend, store, Some(child_id)), replay).expect("spawn child");

    handle.submit("continue", false).await.expect("submit");
    wait_for_turn(&mut events).await;
    assert!(
        requests
            .lock()
            .expect("recorded requests lock")
            .iter()
            .all(|request| request.tools.iter().all(|tool| tool.name != "update_goal"))
    );
}

#[test]
fn resumed_context_projects_the_latest_goal() {
    let expected = SessionGoal {
        objective: "ship the migration".into(),
        status: GoalStatus::Pursuing,
        turns: 3,
        blocked_streak: 0,
    };
    let replay = vec![
        event(
            0,
            None,
            SessionEvent::GoalUpdated {
                goal: SessionGoal {
                    objective: "old".into(),
                    status: GoalStatus::Paused,
                    turns: 1,
                    blocked_streak: 0,
                },
            },
        ),
        event(
            1,
            None,
            SessionEvent::GoalUpdated {
                goal: expected.clone(),
            },
        ),
    ];
    let mut context = ContextManager::new(ContextPolicy::default());
    context.replay(replay);
    let request = context
        .assemble(
            &ModelProfile::new("test", "frontier", 4_000, 500),
            Vec::new(),
            false,
            ".",
        )
        .expect("assemble")
        .request;
    assert!(request.items.iter().any(|item| matches!(
        item,
        ModelItem::Goal { goal, continuation: true } if goal == &expected
    )));
}
