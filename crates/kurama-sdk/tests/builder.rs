use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use kurama_core::orchestrator::SmartOrchestrator;
use kurama_core::testing::{
    AllowAllPolicy, CollectingSink, EchoTool, MemoryStore, NoDelegation, ScriptedBackend,
    SequenceIds,
};
use kurama_sdk::{
    AgentBudget, AgentBuilder, AgentCommand, AgentRuntime, AgentSpec, AgentState, ApprovalPolicy,
    ApprovalResponse, BackendCapabilities, BoxFuture, CancelSignal, DelegationRequest,
    EventEnvelope, ExecutionMode, FinishReason, KuramaError, ModelBackend, ModelEvent, ModelItem,
    ModelProfile, ModelRequest, ModelStream, Operation, OrchestrationContext, PolicyContext,
    PolicyDecision, RuntimeEvent, SessionEvent, SessionMetadata, SessionStore, Tool, ToolContext,
    ToolDescriptor, ToolInvocation, ToolResult, Usage, WriteScope,
};
use tokio::sync::Notify;

type ScriptEvent = Result<ModelEvent, KuramaError>;

#[test]
fn sdk_accepts_custom_runtime_parts() {
    let runtime = AgentBuilder::new()
        .profile(
            ModelProfile::new("custom", "frontier", 32_000, 4_000),
            Arc::new(ScriptedBackend::new(Vec::new())),
        )
        .tool(Arc::new(EchoTool))
        .policy(Arc::new(AllowAllPolicy))
        .store(Arc::new(MemoryStore::default()))
        .sink(Arc::new(CollectingSink::default()))
        .orchestrator(Arc::new(NoDelegation))
        .ids(Arc::new(SequenceIds::default()))
        .build()
        .expect("runtime");

    assert_eq!(runtime.active_profile(), "custom");
    assert_eq!(runtime.registered_tools(), vec!["echo"]);
}

#[test]
fn duplicate_tools_are_rejected() {
    let error = AgentBuilder::new()
        .tool(Arc::new(EchoTool))
        .tool(Arc::new(EchoTool))
        .build()
        .expect_err("duplicate tool");
    assert!(error.to_string().contains("duplicate tool"));
}

#[tokio::test]
async fn runtime_launches_explicit_depth_one_children() {
    let budget = child_budget(16_000, 2_000);
    let backend = Arc::new(RecordingBackend::new(vec![
        vec![delegation(budget), completed(FinishReason::ToolCalls)],
        vec![text("child complete"), completed(FinishReason::Stop)],
        vec![text("parent complete"), completed(FinishReason::Stop)],
    ]));
    let store = Arc::new(MemoryStore::default());
    let runtime = orchestrated_runtime(
        backend.clone(),
        Arc::new(AllowAllPolicy),
        store.clone(),
        None,
    );
    let (handle, mut events) = runtime
        .start(session("session"), Vec::new())
        .expect("start");
    handle.submit("use sub-agents", true).await.expect("submit");
    wait_for_turn(&mut events).await;

    assert!(
        store
            .replay(&"session".into())
            .expect("replay")
            .iter()
            .any(|event| matches!(event.event, SessionEvent::AgentCompleted { .. }))
    );
    let requests = backend.requests();
    let parent_requests = requests
        .iter()
        .filter(|request| request.agent_id.is_none())
        .collect::<Vec<_>>();
    assert_eq!(parent_requests.len(), 2);
    assert!(parent_requests[0].delegation.is_some());
    assert!(parent_requests[1].delegation.is_none());
}

#[tokio::test]
async fn second_child_turn_routes_parent_approval() {
    let mut budget = child_budget(16_000, 2_000);
    budget.max_turns = 2;
    let backend = Arc::new(GatedChildBackend::new(
        vec![
            vec![delegation(budget), completed(FinishReason::ToolCalls)],
            vec![text("parent complete"), completed(FinishReason::Stop)],
        ],
        vec![
            vec![text("first turn"), completed(FinishReason::Stop)],
            vec![
                Ok(ModelEvent::ToolCall {
                    call_id: "child-call".into(),
                    name: "mutate".into(),
                    arguments: Default::default(),
                }),
                completed(FinishReason::ToolCalls),
            ],
            vec![text("child complete"), completed(FinishReason::Stop)],
        ],
    ));
    let tool = Arc::new(ApprovalTool::default());
    let runtime = orchestrated_runtime(
        backend.clone(),
        Arc::new(AskPolicy),
        Arc::new(MemoryStore::default()),
        Some(tool.clone()),
    );
    let (handle, mut events) = runtime
        .start(session("approval"), Vec::new())
        .expect("start");
    handle.submit("delegate", true).await.expect("submit");

    let child_id = loop {
        match events.recv().await.expect("event") {
            RuntimeEvent::AgentUpdated { snapshot } if snapshot.state == AgentState::Running => {
                break snapshot.id;
            }
            RuntimeEvent::Error { message } => panic!("runtime error: {message}"),
            _ => {}
        }
    };
    backend.wait_for_child_turn().await;
    handle
        .agent_command(AgentCommand::Message {
            agent_id: child_id,
            text: "perform the write".into(),
        })
        .await
        .expect("queue child message");
    backend.release_child_turn();

    let request = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match events.recv().await.expect("event") {
                RuntimeEvent::ApprovalRequired { request } => break request,
                RuntimeEvent::TurnCompleted => panic!("child approval never reached the parent"),
                RuntimeEvent::Error { message } => panic!("runtime error: {message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("approval timeout");
    assert!(matches!(request.operation, Operation::Write { .. }));

    handle
        .resolve_approval(request.operation_id, ApprovalResponse::ApproveOnce)
        .await
        .expect("approve");
    wait_for_turn(&mut events).await;
    assert_eq!(tool.executions.load(Ordering::Relaxed), 1);
    assert_eq!(backend.child_requests().len(), 3);
}

#[tokio::test]
async fn child_finishes_when_no_queued_work_remains() {
    let budget = child_budget(1_000, 7);
    let backend = Arc::new(RecordingBackend::new(vec![
        vec![delegation(budget), completed(FinishReason::ToolCalls)],
        vec![
            usage(1_000, 7),
            text("exact budget"),
            completed(FinishReason::Stop),
        ],
        vec![text("parent complete"), completed(FinishReason::Stop)],
    ]));
    let runtime = orchestrated_runtime(
        backend.clone(),
        Arc::new(AllowAllPolicy),
        Arc::new(MemoryStore::default()),
        None,
    );
    let (handle, mut events) = runtime.start(session("budget"), Vec::new()).expect("start");
    handle.submit("delegate", true).await.expect("submit");

    let mut child_snapshot = None;
    loop {
        match events.recv().await.expect("event") {
            RuntimeEvent::AgentUpdated { snapshot } => child_snapshot = Some(snapshot),
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("runtime error: {message}"),
            _ => {}
        }
    }

    let child_requests: Vec<_> = backend
        .requests()
        .into_iter()
        .filter(|request| request.agent_id.is_some())
        .collect();
    assert_eq!(child_requests.len(), 1);
    let child_request = &child_requests[0];
    assert_eq!(child_request.profile.max_input_tokens, 1_000);
    assert_eq!(child_request.profile.max_output_tokens, 7);
    let child_snapshot = child_snapshot.expect("child snapshot");
    assert_eq!(child_snapshot.state, AgentState::Completed);
    assert_eq!(child_snapshot.last_error, None);
}

#[tokio::test]
async fn child_final_turn_overage_preserves_the_completed_result() {
    let budget = child_budget(1_000, 7);
    let backend = Arc::new(RecordingBackend::new(vec![
        vec![delegation(budget), completed(FinishReason::ToolCalls)],
        vec![
            usage(1_001, 7),
            text("over budget"),
            completed(FinishReason::Stop),
        ],
        vec![text("parent complete"), completed(FinishReason::Stop)],
    ]));
    let runtime = orchestrated_runtime(
        backend.clone(),
        Arc::new(AllowAllPolicy),
        Arc::new(MemoryStore::default()),
        None,
    );
    let (handle, mut events) = runtime
        .start(session("final-overage"), Vec::new())
        .expect("start");
    handle.submit("delegate", true).await.expect("submit");

    let child_snapshot = wait_for_child_result(&mut events).await;

    assert_eq!(child_requests(&backend).len(), 1);
    assert_eq!(child_snapshot.state, AgentState::Completed);
    assert_eq!(child_snapshot.last_error, None);
    let parent_requests: Vec<_> = backend
        .requests()
        .into_iter()
        .filter(|request| request.agent_id.is_none())
        .collect();
    assert!(parent_requests.last().is_some_and(|request| {
        request.items.iter().any(|item| {
            matches!(item, ModelItem::AgentResult { summary, .. } if summary == "over budget")
        })
    }));
}

#[tokio::test]
async fn failed_child_outcome_reaches_the_parent_context() {
    let backend = Arc::new(RecordingBackend::new(vec![
        vec![
            delegation(child_budget(16_000, 2_000)),
            completed(FinishReason::ToolCalls),
        ],
        vec![Err(KuramaError::Model("child failed".into()))],
        vec![Err(KuramaError::Model("child failed again".into()))],
        vec![text("parent complete"), completed(FinishReason::Stop)],
    ]));
    let runtime = orchestrated_runtime(
        backend.clone(),
        Arc::new(AllowAllPolicy),
        Arc::new(MemoryStore::default()),
        None,
    );
    let (handle, mut events) = runtime
        .start(session("failed-child-context"), Vec::new())
        .expect("start");
    handle.submit("delegate", true).await.expect("submit");

    wait_for_turn(&mut events).await;

    let parent_requests: Vec<_> = backend
        .requests()
        .into_iter()
        .filter(|request| request.agent_id.is_none())
        .collect();
    assert!(parent_requests.last().is_some_and(|request| {
        request.items.iter().any(|item| {
            matches!(item, ModelItem::AgentResult { summary, .. } if summary.contains("failed"))
        })
    }));
}

#[tokio::test]
async fn child_replay_clamps_initial_request_to_remaining_budget() {
    let session_id = "replay-budget";
    let store = Arc::new(MemoryStore::default());
    seed_child_replay(
        store.as_ref(),
        session_id,
        [
            SessionEvent::ModelUsage {
                usage: Usage {
                    input_tokens: 1_600,
                    output_tokens: 200,
                    cached_input_tokens: 0,
                },
            },
            SessionEvent::TurnCompleted,
        ],
    );
    let mut budget = child_budget(4_000, 500);
    budget.max_turns = 2;
    let backend = Arc::new(RecordingBackend::new(vec![
        vec![delegation(budget), completed(FinishReason::ToolCalls)],
        vec![
            usage(2_400, 300),
            text("child complete"),
            completed(FinishReason::Stop),
        ],
        vec![text("parent complete"), completed(FinishReason::Stop)],
    ]));
    let runtime = orchestrated_runtime(backend.clone(), Arc::new(AllowAllPolicy), store, None);
    let (handle, mut events) = runtime
        .start(session(session_id), Vec::new())
        .expect("start");
    handle.submit("delegate", true).await.expect("submit");

    let child_snapshot = wait_for_child_result(&mut events).await;

    let requests = child_requests(&backend);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].profile.max_input_tokens, 2_400);
    assert_eq!(requests[0].profile.max_output_tokens, 300);
    assert_eq!(child_snapshot.state, AgentState::Completed);
}

#[tokio::test]
async fn exhausted_child_replay_skips_model_request() {
    for (session_id, budget, replay) in [
        (
            "replay-token-exhausted",
            child_budget(1_000, 100),
            vec![SessionEvent::ModelUsage {
                usage: Usage {
                    input_tokens: 1_000,
                    output_tokens: 40,
                    cached_input_tokens: 0,
                },
            }],
        ),
        (
            "replay-turn-exhausted",
            AgentBudget {
                max_turns: 1,
                ..child_budget(1_000, 100)
            },
            vec![SessionEvent::TurnCompleted],
        ),
    ] {
        let store = Arc::new(MemoryStore::default());
        seed_child_replay(store.as_ref(), session_id, replay);
        let backend = Arc::new(RecordingBackend::new(vec![
            vec![delegation(budget), completed(FinishReason::ToolCalls)],
            vec![text("parent complete"), completed(FinishReason::Stop)],
        ]));
        let runtime = orchestrated_runtime(backend.clone(), Arc::new(AllowAllPolicy), store, None);
        let (handle, mut events) = runtime
            .start(session(session_id), Vec::new())
            .expect("start");
        handle.submit("delegate", true).await.expect("submit");

        let child_snapshot = wait_for_child_result(&mut events).await;

        assert!(child_requests(&backend).is_empty());
        assert_budget_exhausted(child_snapshot);
    }
}

#[tokio::test]
async fn queued_child_message_uses_remaining_token_budget() {
    let mut budget = child_budget(4_000, 500);
    budget.max_turns = 2;
    let (backend, child_snapshot) = run_queued_child(
        budget,
        vec![
            usage(1_600, 200),
            text("first turn"),
            completed(FinishReason::Stop),
        ],
    )
    .await;

    let requests = backend.child_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].profile.max_input_tokens, 4_000);
    assert_eq!(requests[0].profile.max_output_tokens, 500);
    assert_eq!(requests[1].profile.max_input_tokens, 2_400);
    assert_eq!(requests[1].profile.max_output_tokens, 300);
    assert_eq!(child_snapshot.state, AgentState::Completed);
}

#[tokio::test]
async fn queued_child_message_surfaces_exhausted_token_budget() {
    for usage_event in [usage(1_000, 40), usage(400, 101)] {
        let mut budget = child_budget(1_000, 100);
        budget.max_turns = 2;
        let (backend, child_snapshot) = run_queued_child(
            budget,
            vec![
                usage_event,
                text("first turn"),
                completed(FinishReason::Stop),
            ],
        )
        .await;

        assert_eq!(backend.child_requests().len(), 1);
        assert_budget_exhausted(child_snapshot);
    }
}

#[tokio::test]
async fn queued_child_message_surfaces_exhausted_turn_budget() {
    let mut budget = child_budget(1_000, 100);
    budget.max_turns = 1;
    let (backend, child_snapshot) = run_queued_child(
        budget,
        vec![text("first turn"), completed(FinishReason::Stop)],
    )
    .await;

    assert_eq!(backend.child_requests().len(), 1);
    assert_budget_exhausted(child_snapshot);
}

async fn run_queued_child(
    budget: AgentBudget,
    first_child_turn: Vec<ScriptEvent>,
) -> (Arc<GatedChildBackend>, kurama_sdk::AgentSnapshot) {
    let backend = Arc::new(GatedChildBackend::new(
        vec![
            vec![delegation(budget), completed(FinishReason::ToolCalls)],
            vec![text("parent complete"), completed(FinishReason::Stop)],
        ],
        vec![
            first_child_turn,
            vec![text("queued work complete"), completed(FinishReason::Stop)],
        ],
    ));
    let runtime = orchestrated_runtime(
        backend.clone(),
        Arc::new(AllowAllPolicy),
        Arc::new(MemoryStore::default()),
        None,
    );
    let (handle, mut events) = runtime
        .start(session("turn-budget"), Vec::new())
        .expect("start");
    handle.submit("delegate", true).await.expect("submit");

    let child_id = loop {
        match events.recv().await.expect("event") {
            RuntimeEvent::AgentUpdated { snapshot } if snapshot.state == AgentState::Running => {
                break snapshot.id;
            }
            RuntimeEvent::Error { message } => panic!("runtime error: {message}"),
            _ => {}
        }
    };
    backend.wait_for_child_turn().await;
    handle
        .agent_command(AgentCommand::Message {
            agent_id: child_id,
            text: "do another turn".into(),
        })
        .await
        .expect("queue child message");
    backend.release_child_turn();

    let mut child_snapshot = None;
    loop {
        match events.recv().await.expect("event") {
            RuntimeEvent::AgentUpdated { snapshot } => child_snapshot = Some(snapshot),
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("runtime error: {message}"),
            _ => {}
        }
    }
    (backend, child_snapshot.expect("child snapshot"))
}

fn assert_budget_exhausted(snapshot: kurama_sdk::AgentSnapshot) {
    assert_eq!(snapshot.state, AgentState::Cancelled);
    assert_eq!(
        snapshot.last_error.as_deref(),
        Some("child budget exhausted")
    );
}

async fn wait_for_child_result(
    events: &mut tokio::sync::mpsc::Receiver<RuntimeEvent>,
) -> kurama_sdk::AgentSnapshot {
    let mut child_snapshot = None;
    loop {
        match events.recv().await.expect("event") {
            RuntimeEvent::AgentUpdated { snapshot } => child_snapshot = Some(snapshot),
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("runtime error: {message}"),
            _ => {}
        }
    }
    child_snapshot.expect("child snapshot")
}

fn child_requests(backend: &RecordingBackend) -> Vec<ModelRequest> {
    backend
        .requests()
        .into_iter()
        .filter(|request| request.agent_id.is_some())
        .collect()
}

fn seed_child_replay(
    store: &MemoryStore,
    session_id: &str,
    events: impl IntoIterator<Item = SessionEvent>,
) {
    for (sequence, event) in events.into_iter().enumerate() {
        store
            .append(&EventEnvelope::new(
                sequence as u64,
                sequence as u64,
                session_id.into(),
                Some("a_0".into()),
                event,
            ))
            .expect("seed child replay");
    }
}

fn orchestrated_runtime(
    backend: Arc<dyn ModelBackend>,
    policy: Arc<dyn ApprovalPolicy>,
    store: Arc<MemoryStore>,
    tool: Option<Arc<dyn Tool>>,
) -> AgentRuntime {
    let ids = Arc::new(SequenceIds::default());
    let profile = ModelProfile::new("custom", "frontier", 32_000, 4_000);
    let mut builder = AgentBuilder::new()
        .profile(profile.clone(), backend)
        .policy(policy)
        .store(store)
        .sink(Arc::new(CollectingSink::default()))
        .orchestrator(Arc::new(SmartOrchestrator::new(ids.clone())))
        .ids(ids)
        .orchestration_context(orchestration(&profile));
    if let Some(tool) = tool {
        builder = builder.tool(tool);
    }
    builder.build().expect("runtime")
}

fn orchestration(profile: &ModelProfile) -> OrchestrationContext {
    OrchestrationContext {
        parent_profile: profile.clone(),
        profiles: BTreeMap::from([(profile.name.clone(), profile.clone())]),
        role_routes: BTreeMap::new(),
        role_escalations: BTreeMap::new(),
        profile_escalations: BTreeMap::new(),
        parent_write_scope: WriteScope {
            roots: vec![PathBuf::from(".")],
            files: Vec::new(),
        },
        max_concurrency: 1,
        depth: 0,
        yolo: false,
    }
}

fn session(id: &str) -> SessionMetadata {
    SessionMetadata {
        id: id.into(),
        created_at_ms: 0,
        project_root: ".".into(),
        profile: "custom".into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    }
}

fn child_budget(max_input_tokens: u64, max_output_tokens: u64) -> AgentBudget {
    AgentBudget {
        max_input_tokens,
        max_output_tokens,
        ..AgentBudget::default()
    }
}

fn delegation(budget: AgentBudget) -> ScriptEvent {
    Ok(ModelEvent::Delegation {
        request: DelegationRequest {
            agents: vec![AgentSpec {
                role: "implementer".into(),
                objective: "perform the child task".into(),
                profile: None,
                context_refs: Vec::new(),
                write_scope: WriteScope::default(),
                budget,
                depends_on: Vec::new(),
            }],
        },
    })
}

fn text(value: &str) -> ScriptEvent {
    Ok(ModelEvent::TextDelta { text: value.into() })
}

fn completed(finish_reason: FinishReason) -> ScriptEvent {
    Ok(ModelEvent::ResponseCompleted {
        cursor: None,
        finish_reason,
    })
}

fn usage(input_tokens: u64, output_tokens: u64) -> ScriptEvent {
    Ok(ModelEvent::Usage {
        usage: Usage {
            input_tokens,
            output_tokens,
            cached_input_tokens: 0,
        },
    })
}

async fn wait_for_turn(events: &mut tokio::sync::mpsc::Receiver<RuntimeEvent>) {
    loop {
        match events.recv().await.expect("event") {
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("runtime error: {message}"),
            _ => {}
        }
    }
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

#[derive(Default)]
struct ApprovalTool {
    executions: AtomicUsize,
}

impl Tool for ApprovalTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "mutate".into(),
            description: "Mutate a test fixture".into(),
            parameters: Default::default(),
        }
    }

    fn classify(
        &self,
        _context: &ToolContext,
        _invocation: &ToolInvocation,
    ) -> Result<Operation, KuramaError> {
        Ok(Operation::Write {
            paths: vec![PathBuf::from("child.txt")],
            destructive: false,
            external: false,
        })
    }

    fn execute<'a>(
        &'a self,
        _context: ToolContext,
        invocation: ToolInvocation,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, KuramaError>> {
        Box::pin(async move {
            self.executions.fetch_add(1, Ordering::Relaxed);
            Ok(ToolResult::success(invocation.call_id, "mutated"))
        })
    }
}

struct RecordingBackend {
    inner: ScriptedBackend,
    requests: Mutex<Vec<ModelRequest>>,
}

struct GatedChildBackend {
    parent: ScriptedBackend,
    child: ScriptedBackend,
    child_requests: Mutex<Vec<ModelRequest>>,
    child_started: Notify,
    child_release: Notify,
}

impl GatedChildBackend {
    fn new(parent: Vec<Vec<ScriptEvent>>, child: Vec<Vec<ScriptEvent>>) -> Self {
        Self {
            parent: ScriptedBackend::new(parent),
            child: ScriptedBackend::new(child),
            child_requests: Mutex::new(Vec::new()),
            child_started: Notify::new(),
            child_release: Notify::new(),
        }
    }

    async fn wait_for_child_turn(&self) {
        self.child_started.notified().await;
    }

    fn release_child_turn(&self) {
        self.child_release.notify_one();
    }

    fn child_requests(&self) -> Vec<ModelRequest> {
        self.child_requests.lock().expect("requests").clone()
    }
}

impl ModelBackend for GatedChildBackend {
    fn backend_name(&self) -> &'static str {
        "gated-child"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::remote_default()
    }

    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ModelStream, KuramaError>> {
        Box::pin(async move {
            if request.agent_id.is_none() {
                return self.parent.stream(request, cancel).await;
            }
            let request_index = {
                let mut requests = self.child_requests.lock().expect("requests");
                let request_index = requests.len();
                requests.push(request.clone());
                request_index
            };
            if request_index == 0 {
                self.child_started.notify_one();
                self.child_release.notified().await;
            }
            self.child.stream(request, cancel).await
        })
    }
}

impl RecordingBackend {
    fn new(streams: Vec<Vec<ScriptEvent>>) -> Self {
        Self {
            inner: ScriptedBackend::new(streams),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().expect("requests").clone()
    }
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
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ModelStream, KuramaError>> {
        self.requests
            .lock()
            .expect("requests")
            .push(request.clone());
        self.inner.stream(request, cancel)
    }
}
