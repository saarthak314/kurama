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
    todo::TodoTool,
};
use kurama_protocol::{
    KuramaError,
    agent::WriteScope,
    id::{AgentId, SessionId},
    model::{BackendCapabilities, FinishReason, ModelEvent, ModelItem, ModelProfile, ModelRequest},
    policy::{AutoBoundaries, ExecutionMode},
    runtime::RuntimeEvent,
    session::{EventEnvelope, SessionEvent, SessionMetadata, TodoItem, TodoStatus},
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

fn item(id: &str, status: TodoStatus) -> TodoItem {
    TodoItem {
        id: id.into(),
        content: format!("task {id}"),
        status,
    }
}

fn event(sequence: u64, agent_id: Option<AgentId>, event: SessionEvent) -> EventEnvelope {
    EventEnvelope::new(
        sequence,
        sequence,
        SessionId::from("todo-session"),
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
            id: "todo-session".into(),
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

#[test]
fn todo_list_validation_rejects_invalid_lists() {
    let too_many = (0..21)
        .map(|index| item(&index.to_string(), TodoStatus::Pending))
        .collect::<Vec<_>>();
    assert!(TodoItem::validate_list(&too_many).is_err());

    let two_active = vec![
        item("first", TodoStatus::InProgress),
        item("second", TodoStatus::InProgress),
    ];
    assert!(TodoItem::validate_list(&two_active).is_err());

    let duplicates = vec![
        item("same", TodoStatus::Pending),
        item("same", TodoStatus::Completed),
    ];
    assert!(TodoItem::validate_list(&duplicates).is_err());
}

#[tokio::test]
async fn parent_todo_call_persists_and_projects_current_list() {
    let expected = vec![
        item("inspect", TodoStatus::Completed),
        item("fix", TodoStatus::InProgress),
    ];
    let backend: Arc<dyn ModelBackend> = Arc::new(ScriptedBackend::new(vec![
        vec![
            Ok(ModelEvent::ToolCall {
                call_id: "todo-call".into(),
                name: "todo".into(),
                arguments: serde_json::json!({"items": expected}),
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

    handle
        .submit("track the work", false)
        .await
        .expect("submit");
    loop {
        match events.recv().await.expect("runtime event") {
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("engine error: {message}"),
            _ => {}
        }
    }

    let replay = store
        .replay(&SessionId::from("todo-session"))
        .expect("replay session");
    assert!(replay.iter().any(|event| matches!(
        &event.event,
        SessionEvent::TodoUpdated { items } if items == &expected
    )));
    let mut context = ContextManager::new(ContextPolicy::default());
    context.replay(replay);
    let request = context
        .assemble(
            &ModelProfile::new("test", "frontier", 4_000, 500),
            Vec::new(),
            false,
            ".",
        )
        .expect("assemble context")
        .request;
    assert!(request.items.iter().any(
        |model_item| matches!(model_item, ModelItem::TodoList { items } if items == &expected)
    ));
}

#[tokio::test]
async fn child_engine_does_not_receive_todo_descriptor() {
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
    let mut child_config = config(backend, store, Some(child_id));
    child_config.tools.push(Arc::new(TodoTool));
    let (handle, mut events) = Engine::spawn(child_config, replay).expect("spawn child engine");

    handle.submit("continue", false).await.expect("submit");
    loop {
        match events.recv().await.expect("runtime event") {
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("engine error: {message}"),
            _ => {}
        }
    }
    assert!(
        requests
            .lock()
            .expect("recorded requests lock")
            .iter()
            .all(|request| request.tools.iter().all(|tool| tool.name != "todo"))
    );
}

#[test]
fn resumed_context_projects_latest_todo_list() {
    let older = vec![item("old", TodoStatus::Completed)];
    let expected = vec![item("next", TodoStatus::Pending)];
    let replay = vec![
        event(0, None, SessionEvent::TodoUpdated { items: older }),
        event(
            1,
            None,
            SessionEvent::TodoUpdated {
                items: expected.clone(),
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
        .expect("assemble resumed context")
        .request;
    let todo_lists = request
        .items
        .iter()
        .filter_map(|model_item| match model_item {
            ModelItem::TodoList { items } => Some(items),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(todo_lists, vec![&expected]);
}
