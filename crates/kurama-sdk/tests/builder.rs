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
    AgentBudget, AgentBuilder, AgentRuntime, AgentSpec, AgentState, ApprovalPolicy,
    ApprovalResponse, BackendCapabilities, BoxFuture, CancelSignal, DelegationRequest,
    ExecutionMode, FinishReason, KuramaError, ModelBackend, ModelEvent, ModelProfile, ModelRequest,
    ModelStream, Operation, OrchestrationContext, PolicyContext, PolicyDecision, RuntimeEvent,
    SessionEvent, SessionMetadata, SessionStore, Tool, ToolContext, ToolDescriptor, ToolInvocation,
    ToolResult, Usage, WriteScope,
};

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
    let backend = Arc::new(ScriptedBackend::new(vec![
        vec![delegation(budget), completed(FinishReason::ToolCalls)],
        vec![text("child complete"), completed(FinishReason::Stop)],
        vec![text("parent complete"), completed(FinishReason::Stop)],
    ]));
    let store = Arc::new(MemoryStore::default());
    let runtime = orchestrated_runtime(backend, Arc::new(AllowAllPolicy), store.clone(), None);
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
}

#[tokio::test]
async fn supervised_child_write_routes_parent_approval() {
    let backend = Arc::new(ScriptedBackend::new(vec![
        vec![
            delegation(child_budget(16_000, 2_000)),
            completed(FinishReason::ToolCalls),
        ],
        vec![
            Ok(ModelEvent::ToolCall {
                call_id: "child-call".into(),
                name: "mutate".into(),
                arguments: Default::default(),
            }),
            completed(FinishReason::ToolCalls),
        ],
        vec![text("child complete"), completed(FinishReason::Stop)],
        vec![text("parent complete"), completed(FinishReason::Stop)],
    ]));
    let tool = Arc::new(ApprovalTool::default());
    let runtime = orchestrated_runtime(
        backend,
        Arc::new(AskPolicy),
        Arc::new(MemoryStore::default()),
        Some(tool.clone()),
    );
    let (handle, mut events) = runtime
        .start(session("approval"), Vec::new())
        .expect("start");
    handle.submit("delegate", true).await.expect("submit");

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
        .resolve_approval(ApprovalResponse::ApproveOnce)
        .await
        .expect("approve");
    wait_for_turn(&mut events).await;
    assert_eq!(tool.executions.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn child_token_budget_is_enforced() {
    let budget = child_budget(1_000, 7);
    let backend = Arc::new(RecordingBackend::new(vec![
        vec![delegation(budget), completed(FinishReason::ToolCalls)],
        vec![
            Ok(ModelEvent::Usage {
                usage: Usage {
                    input_tokens: 900,
                    output_tokens: 8,
                    cached_input_tokens: 0,
                },
            }),
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

    let child_request = backend
        .requests()
        .into_iter()
        .find(|request| request.agent_id.is_some())
        .expect("child request");
    assert_eq!(child_request.profile.max_input_tokens, 1_000);
    assert_eq!(child_request.profile.max_output_tokens, 7);
    let child_snapshot = child_snapshot.expect("child snapshot");
    assert_eq!(child_snapshot.state, AgentState::Cancelled);
    assert_eq!(
        child_snapshot.last_error.as_deref(),
        Some("child budget exhausted")
    );
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
