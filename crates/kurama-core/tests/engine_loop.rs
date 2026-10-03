use parking_lot::Mutex;
use std::{
    future::pending,
    path::PathBuf,
    sync::Arc,
    sync::atomic::{AtomicUsize, Ordering},
};

use futures_util::{StreamExt as _, stream};
use kurama_core::{
    context::{ContextManager, ContextPolicy},
    engine::{Engine, EngineConfig, EngineOrchestration},
    orchestrator::SmartOrchestrator,
    testing::{
        AllowAllPolicy, CollectingSink, EchoTool, ImmediateChildRunner, MemoryStore, NoDelegation,
        ScriptedBackend, SequenceIds, orchestration_context,
    },
};
use kurama_protocol::{
    agent::{AgentBudget, AgentSnapshot, AgentSpec, AgentState, DelegationRequest, WriteScope},
    id::{CallId, OperationId, SessionId},
    model::{BackendCapabilities, FinishReason, ModelEvent, ModelItem, ModelProfile, ModelRequest},
    policy::{ApprovalResponse, AutoBoundaries, ExecutionMode, PolicyContext, PolicyDecision},
    runtime::RuntimeEvent,
    session::{EventEnvelope, FileCheckpoint, SessionEvent, SessionMetadata},
    tool::{
        CommandClass, Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolLimits,
        ToolResult,
    },
    traits::{
        ApprovalPolicy, BoxFuture, CancelSignal, ModelBackend, ModelStream, SessionStore, Tool,
    },
};
use tokio::sync::Notify;

fn replay_event(sequence: u64, event: SessionEvent) -> EventEnvelope {
    EventEnvelope::new(sequence, sequence, SessionId::from("resume"), None, event)
}

#[derive(Default)]
struct LimitRecordingTool {
    observed: Arc<Mutex<Option<ToolLimits>>>,
}

impl Tool for LimitRecordingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "limits".into(),
            description: "record tool limits".into(),
            parameters: serde_json::json!({"type":"object"}),
        }
    }

    fn classify(
        &self,
        _context: &ToolContext,
        _invocation: &ToolInvocation,
    ) -> Result<Operation, kurama_protocol::KuramaError> {
        Ok(Operation::Read {
            paths: vec![PathBuf::from(".")],
            external: false,
        })
    }

    fn execute<'a>(
        &'a self,
        context: ToolContext,
        invocation: ToolInvocation,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, kurama_protocol::KuramaError>> {
        Box::pin(async move {
            *self.observed.lock() = Some(context.limits);
            Ok(ToolResult::success(invocation.call_id, "ok"))
        })
    }
}

fn resume_config(
    store: Arc<MemoryStore>,
    tools: Vec<Arc<dyn Tool>>,
    policy: Arc<dyn ApprovalPolicy>,
) -> EngineConfig {
    EngineConfig {
        session: SessionMetadata {
            id: "resume".into(),
            created_at_ms: 0,
            project_root: ".".into(),
            profile: "test".into(),
            mode: ExecutionMode::Supervised,
            redaction_best_effort: false,
        },
        profile: ModelProfile::new("test", "frontier", 4_000, 500),
        backend: Arc::new(ScriptedBackend::new(vec![vec![Ok(
            ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            },
        )]])),
        tools,
        policy,
        store,
        sink: Arc::new(CollectingSink::default()),
        orchestrator: Arc::new(NoDelegation),
        ids: Arc::new(SequenceIds::new(100)),
        context_policy: ContextPolicy::default(),
        workspace_root: PathBuf::from("."),
        write_scope: WriteScope::default(),
        auto: AutoBoundaries::default(),
        agent_id: None,
        orchestration: None,
        provider_retry_delays_ms: Vec::new(),
        command_capacity: 32,
        event_capacity: 128,
    }
}

fn seed_replay(store: &MemoryStore, replay: &[EventEnvelope]) {
    for event in replay {
        store.append(event).expect("seed replay event");
    }
}

struct RecordingBackend {
    inner: ScriptedBackend,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ModelBackend for RecordingBackend {
    fn backend_name(&self) -> &'static str {
        self.inner.backend_name()
    }

    fn capabilities(&self) -> BackendCapabilities {
        self.inner.capabilities()
    }

    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ModelStream, kurama_protocol::KuramaError>> {
        self.requests.lock().push(request.clone());
        self.inner.stream(request, cancel)
    }
}

fn completed_compaction_turns() -> Vec<EventEnvelope> {
    ["A", "B", "C", "D", "E"]
        .into_iter()
        .enumerate()
        .flat_map(|(turn, name)| {
            let sequence = turn as u64 * 3;
            [
                replay_event(
                    sequence,
                    SessionEvent::UserMessage {
                        text: name.into(),
                        explicit_delegation: false,
                    },
                ),
                replay_event(
                    sequence + 1,
                    SessionEvent::AssistantMessage {
                        text: format!("decision {name}"),
                    },
                ),
                replay_event(sequence + 2, SessionEvent::TurnCompleted),
            ]
        })
        .collect()
}

#[tokio::test]
async fn repeated_engine_compaction_preserves_the_first_decision_in_later_context() {
    let first_summary = serde_json::json!({
        "summary": "completed A", "decisions": ["decision A"],
        "open_tasks": [], "files": [], "operation_ids": []
    })
    .to_string();
    let second_summary = serde_json::json!({
        "summary": "completed A and B", "decisions": ["decision A", "decision B"],
        "open_tasks": [], "files": [], "operation_ids": []
    })
    .to_string();
    let backend = Arc::new(RecordingBackend {
        inner: ScriptedBackend::new(
            [
                first_summary,
                "F complete".into(),
                second_summary,
                "G complete".into(),
            ]
            .into_iter()
            .map(|text| {
                vec![
                    Ok(ModelEvent::TextDelta { text }),
                    Ok(ModelEvent::ResponseCompleted {
                        cursor: None,
                        finish_reason: FinishReason::Stop,
                    }),
                ]
            })
            .collect(),
        ),
        requests: Mutex::new(Vec::new()),
    });
    let replay = completed_compaction_turns();
    let store = Arc::new(MemoryStore::default());
    seed_replay(&store, &replay);
    let mut config = resume_config(store.clone(), Vec::new(), Arc::new(AllowAllPolicy));
    config.backend = backend.clone();
    let (handle, mut events) = Engine::spawn(config, replay).expect("spawn engine");
    for (compaction_index, next_turn) in ["F", "G"].into_iter().enumerate() {
        handle.compact().await.expect("request compaction");
        loop {
            match control_event(&mut events).await {
                RuntimeEvent::Status { .. } => {
                    let completed = store
                        .replay(&SessionId::from("resume"))
                        .expect("compacted replay")
                        .iter()
                        .filter(|event| {
                            matches!(event.event, SessionEvent::ContextCompacted { .. })
                        })
                        .count();
                    if completed == compaction_index + 1 {
                        break;
                    }
                }
                RuntimeEvent::Error { message } => panic!("compaction error: {message}"),
                _ => {}
            }
        }
        handle
            .submit(next_turn, false)
            .await
            .expect("submit next turn");
        loop {
            match control_event(&mut events).await {
                RuntimeEvent::TurnCompleted => break,
                RuntimeEvent::Error { message } => panic!("engine error: {message}"),
                _ => {}
            }
        }
    }
    handle.shutdown().await.expect("shutdown");

    let requests = backend.requests.lock();
    assert_eq!(
        requests.len(),
        4,
        "only explicit compactions and submitted turns call the backend"
    );
    let second = &requests[2];
    let second_input = serde_json::to_string(&second.items).expect("model input");
    assert_eq!(second_input.matches("decision A").count(), 1);
    assert!(second_input.contains("decision B"));
    assert!(!second.system.contains("decision A"));
    assert!(!second_input.contains("decision C"));
    assert!(requests[3].items.iter().any(|item| matches!(
        item,
        ModelItem::Summary { text, covered_through_sequence: 5, .. }
            if text.contains("decision A") && text.contains("decision B")
    )));
    let later_users: Vec<_> = requests[3]
        .items
        .iter()
        .filter_map(|item| match item {
            ModelItem::User { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(later_users, vec!["C", "D", "E", "F", "G"]);

    let durable = store.replay(&SessionId::from("resume")).expect("replay");
    let summaries: Vec<_> = durable
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::ContextCompacted {
                covered_through_sequence,
                summary,
                ..
            } => Some((event.sequence, *covered_through_sequence, summary)),
            _ => None,
        })
        .collect();
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].1, 2);
    assert_eq!(summaries[1].1, 5);
    assert!(
        summaries[0].0 > summaries[1].1,
        "prior summary is outside the new prefix"
    );
    let mut context = ContextManager::new(ContextPolicy::default());
    context.replay(durable);
    let assembled = context
        .assemble(
            &ModelProfile::new("test", "frontier", 4_000, 500),
            Vec::new(),
            false,
            ".",
        )
        .expect("assemble replayed summaries");
    assert!(assembled.request.items.iter().any(|item| matches!(
        item,
        ModelItem::Summary { text, covered_through_sequence: 5, .. } if text.contains("decision A")
    )));
}

#[tokio::test]
async fn compaction_overflow_preserves_the_previous_summary_and_coverage() {
    let summary = format!("decision A: {}", "retained fact ".repeat(2_000));
    let mut replay = completed_compaction_turns();
    replay.extend([
        replay_event(
            15,
            SessionEvent::ContextCompacted {
                covered_through_sequence: 2,
                summary: summary.clone(),
                tokens: 1,
            },
        ),
        replay_event(
            16,
            SessionEvent::UserMessage {
                text: "F".into(),
                explicit_delegation: false,
            },
        ),
        replay_event(17, SessionEvent::TurnCompleted),
    ]);
    let store = Arc::new(MemoryStore::default());
    seed_replay(&store, &replay);
    let backend = Arc::new(RecordingBackend {
        inner: ScriptedBackend::new(Vec::new()),
        requests: Mutex::new(Vec::new()),
    });
    let mut config = resume_config(store.clone(), Vec::new(), Arc::new(AllowAllPolicy));
    config.backend = backend.clone();
    let (handle, mut events) = Engine::spawn(config, replay).expect("spawn engine");
    handle
        .compact()
        .await
        .expect("request oversized compaction");
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::Error { .. } => break,
            RuntimeEvent::Shutdown => panic!("engine stopped instead of rejecting compaction"),
            _ => {}
        }
    }
    handle.shutdown().await.expect("shutdown");
    assert!(backend.requests.lock().is_empty());
    let durable = store.replay(&SessionId::from("resume")).expect("replay");
    let summaries: Vec<_> = durable
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::ContextCompacted {
                covered_through_sequence,
                summary,
                ..
            } => Some((*covered_through_sequence, summary.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(summaries, vec![(2, summary.as_str())]);
    let mut context = ContextManager::new(ContextPolicy::default());
    context.replay(durable);
    let assembled = context
        .assemble(
            &ModelProfile::new("test", "larger", 128_000, 8_000),
            Vec::new(),
            false,
            ".",
        )
        .expect("assemble preserved summary with a larger model");
    assert!(assembled.request.items.iter().any(|item| matches!(
        item,
        ModelItem::Summary { text, covered_through_sequence: 2, .. } if text == &summary
    )));
}

struct StagedOutputTool {
    output_path: PathBuf,
}

impl Tool for StagedOutputTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "staged".into(),
            description: "return staged display output".into(),
            parameters: serde_json::json!({"type":"object"}),
        }
    }

    fn classify(
        &self,
        _context: &ToolContext,
        _invocation: &ToolInvocation,
    ) -> Result<Operation, kurama_protocol::KuramaError> {
        Ok(Operation::Read {
            paths: vec![PathBuf::from(".")],
            external: false,
        })
    }

    fn execute<'a>(
        &'a self,
        _context: ToolContext,
        invocation: ToolInvocation,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, kurama_protocol::KuramaError>> {
        Box::pin(async move {
            Ok(ToolResult {
                call_id: invocation.call_id,
                output: "head\n[omitted]\ntail\n\n[stderr]\nwarn\n".into(),
                is_error: false,
                metadata: serde_json::json!({
                    "_display_staging": {
                        "output": self.output_path
                    }
                }),
                truncated: true,
                blob_refs: Vec::new(),
            })
        })
    }
}

fn unique_staging_path(stream: &str) -> PathBuf {
    static NEXT_PATH: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "kurama-engine-test-{}-{}-{stream}",
        std::process::id(),
        NEXT_PATH.fetch_add(1, Ordering::Relaxed)
    ))
}

#[tokio::test]
async fn tool_completion_keeps_durable_output_bounded_and_hydrates_live_display() {
    let full_output = "head\nfull middle output\ntail\n";
    let output_path = unique_staging_path("output");
    std::fs::write(&output_path, full_output).expect("stage output");
    let backend = ScriptedBackend::new(vec![
        vec![
            Ok(ModelEvent::ToolCall {
                call_id: CallId::from("call_1"),
                name: "staged".into(),
                arguments: serde_json::json!({}),
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
    ]);
    let store = Arc::new(MemoryStore::default());
    let config = EngineConfig {
        session: SessionMetadata {
            id: "session".into(),
            created_at_ms: 0,
            project_root: ".".into(),
            profile: "test".into(),
            mode: ExecutionMode::Supervised,
            redaction_best_effort: false,
        },
        profile: ModelProfile::new("test", "frontier", 4_000, 500),
        backend: Arc::new(backend),
        tools: vec![Arc::new(StagedOutputTool {
            output_path: output_path.clone(),
        })],
        policy: Arc::new(AllowAllPolicy),
        store: store.clone(),
        sink: Arc::new(CollectingSink::default()),
        orchestrator: Arc::new(NoDelegation),
        ids: Arc::new(SequenceIds::new(1)),
        context_policy: ContextPolicy::default(),
        workspace_root: PathBuf::from("."),
        write_scope: WriteScope::default(),
        auto: AutoBoundaries::default(),
        agent_id: None,
        orchestration: None,
        provider_retry_delays_ms: Vec::new(),
        command_capacity: 32,
        event_capacity: 128,
    };
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn engine");
    handle.submit("inspect", false).await.expect("submit");
    let mut live_result = None;
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::ToolCompleted { result, .. } => live_result = Some(result),
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("engine error: {message}"),
            _ => {}
        }
    }

    let live_result = live_result.expect("live tool result");
    assert_eq!(
        live_result.metadata["display_output"].as_str(),
        Some(full_output)
    );
    assert_eq!(
        live_result.output,
        "head\n[omitted]\ntail\n\n[stderr]\nwarn\n"
    );
    assert_eq!(store.blob_reads(), 0);
    assert_eq!(store.blob_read_bytes(), 0);

    let completion = store
        .events("session")
        .into_iter()
        .find(|event| matches!(event.event, SessionEvent::ToolCompleted { .. }))
        .expect("tool completion");
    let SessionEvent::ToolCompleted { result, .. } = &completion.event else {
        unreachable!();
    };
    assert!(result.metadata.get("_display_staging").is_none());
    assert!(result.metadata.get("display_output").is_none());
    assert!(result.blob_refs.is_empty());
    let display_blobs = result.metadata["display_blobs"]
        .as_object()
        .expect("display blob references");
    let output: kurama_protocol::session::BlobRef =
        serde_json::from_value(display_blobs["output"].clone()).expect("output blob reference");
    assert_eq!(
        store.get_blob(&output).expect("output blob"),
        full_output.as_bytes()
    );
    assert!(
        !serde_json::to_string(&completion)
            .expect("serialize completion")
            .contains("full middle output")
    );
    let mut context = ContextManager::new(ContextPolicy::default());
    context.replay(store.replay(&SessionId::from("session")).expect("replay"));
    let model_request = context
        .assemble(
            &ModelProfile::new("test", "frontier", 4_000, 500),
            Vec::new(),
            false,
            ".",
        )
        .expect("assemble model context")
        .request;
    let model_tool_result = model_request
        .items
        .iter()
        .find_map(|item| match item {
            kurama_protocol::model::ModelItem::ToolResult {
                content, blob_refs, ..
            } => Some((content, blob_refs)),
            _ => None,
        })
        .expect("model tool result");
    assert_eq!(
        model_tool_result.0,
        "head\n[omitted]\ntail\n\n[stderr]\nwarn\n"
    );
    assert!(model_tool_result.1.is_empty());
    assert!(
        !serde_json::to_string(&model_request)
            .expect("serialize model request")
            .contains("full middle output")
    );
    assert!(!output_path.exists());
}

#[tokio::test]
async fn engine_executes_tool_and_finishes_turn() {
    let backend = ScriptedBackend::new(vec![
        vec![
            Ok(ModelEvent::ToolCall {
                call_id: "call_1".into(),
                name: "echo".into(),
                arguments: serde_json::json!({}),
            }),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::ToolCalls,
            }),
        ],
        vec![
            Ok(ModelEvent::TextDelta {
                text: "Done.".into(),
            }),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            }),
        ],
    ]);
    let store = Arc::new(MemoryStore::default());
    let config = EngineConfig {
        session: SessionMetadata {
            id: "session".into(),
            created_at_ms: 0,
            project_root: ".".into(),
            profile: "test".into(),
            mode: ExecutionMode::Supervised,
            redaction_best_effort: false,
        },
        profile: ModelProfile::new("test", "frontier", 4_000, 500),
        backend: Arc::new(backend),
        tools: vec![Arc::new(EchoTool)],
        policy: Arc::new(AllowAllPolicy),
        store: store.clone(),
        sink: Arc::new(CollectingSink::default()),
        orchestrator: Arc::new(NoDelegation),
        ids: Arc::new(SequenceIds::new(1)),
        context_policy: ContextPolicy::default(),
        workspace_root: PathBuf::from("."),
        write_scope: WriteScope::default(),
        auto: AutoBoundaries::default(),
        agent_id: None,
        orchestration: None,
        provider_retry_delays_ms: Vec::new(),
        command_capacity: 32,
        event_capacity: 128,
    };
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn engine");
    handle.submit("inspect", false).await.expect("submit");

    let mut saw_tool = false;
    let mut saw_tool_context = false;
    let mut text = String::new();
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::ToolStarted { name, context, .. } => {
                saw_tool_context = name == "echo" && context == ".";
            }
            RuntimeEvent::ToolCompleted { .. } => saw_tool = true,
            RuntimeEvent::AssistantDelta { text: delta } => text.push_str(&delta),
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("engine error: {message}"),
            _ => {}
        }
    }
    assert!(saw_tool);
    assert!(saw_tool_context);
    assert_eq!(text, "Done.");
    assert_eq!(
        store
            .events("session")
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::AssistantMessage { .. }))
            .count(),
        1
    );
    assert_eq!(store.operation_completion_count("session", "o_1"), 1);
}

#[tokio::test]
async fn engine_derives_tool_limits_from_the_effective_model_input_budget() {
    let backend = ScriptedBackend::new(vec![
        vec![
            Ok(ModelEvent::ToolCall {
                call_id: "call_1".into(),
                name: "limits".into(),
                arguments: serde_json::json!({}),
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
    ]);
    let tool = LimitRecordingTool::default();
    let observed = tool.observed.clone();
    let config = EngineConfig {
        session: SessionMetadata {
            id: "limits-session".into(),
            created_at_ms: 0,
            project_root: ".".into(),
            profile: "test".into(),
            mode: ExecutionMode::Supervised,
            redaction_best_effort: false,
        },
        profile: ModelProfile::new("test", "frontier", 10_000, 1_000),
        backend: Arc::new(backend),
        tools: vec![Arc::new(tool)],
        policy: Arc::new(AllowAllPolicy),
        store: Arc::new(MemoryStore::default()),
        sink: Arc::new(CollectingSink::default()),
        orchestrator: Arc::new(NoDelegation),
        ids: Arc::new(SequenceIds::new(1)),
        context_policy: ContextPolicy {
            max_input_tokens: 12_000,
            reserve_output_tokens: 2_000,
            ..ContextPolicy::default()
        },
        workspace_root: PathBuf::from("."),
        write_scope: WriteScope::default(),
        auto: AutoBoundaries::default(),
        agent_id: None,
        orchestration: None,
        provider_retry_delays_ms: Vec::new(),
        command_capacity: 32,
        event_capacity: 128,
    };
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn engine");
    handle.submit("inspect", false).await.expect("submit");

    while !matches!(
        events.recv().await.expect("runtime event"),
        RuntimeEvent::TurnCompleted
    ) {}

    assert_eq!(
        *observed.lock(),
        Some(ToolLimits {
            max_bytes: 27_000,
            max_lines: usize::MAX,
        })
    );
}

#[tokio::test]
async fn cancellation_is_observed_while_streaming() {
    let token = kurama_core::cancel::CancelToken::new();
    let waiter = token.clone();
    let task = tokio::spawn(async move {
        waiter.cancelled().await;
        waiter.is_cancelled()
    });
    assert!(token.cancel());
    assert!(task.await.expect("join"));
}

struct BlockingPartialBackend {
    blocked: Arc<Notify>,
}

struct CompletionThenPendingBackend {
    trailing_polls: Arc<AtomicUsize>,
}

impl ModelBackend for CompletionThenPendingBackend {
    fn backend_name(&self) -> &'static str {
        "completion-then-pending"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::remote_default()
    }

    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ModelStream, kurama_protocol::KuramaError>> {
        Box::pin(async move {
            let trailing_polls = Arc::clone(&self.trailing_polls);
            let events = stream::iter([Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            })])
            .chain(stream::once(async move {
                trailing_polls.fetch_add(1, Ordering::SeqCst);
                pending::<Result<ModelEvent, kurama_protocol::KuramaError>>().await
            }));
            Ok(Box::pin(events) as ModelStream)
        })
    }
}

#[derive(Default)]
struct ToolCallsWithoutWorkBackend {
    calls: AtomicUsize,
}

impl ModelBackend for ToolCallsWithoutWorkBackend {
    fn backend_name(&self) -> &'static str {
        "tool-calls-without-work"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::remote_default()
    }

    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ModelStream, kurama_protocol::KuramaError>> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
                return Err(kurama_protocol::KuramaError::Model(
                    "backend was called again after a no-progress round".into(),
                ));
            }
            Ok(Box::pin(stream::iter([Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::ToolCalls,
            })])) as ModelStream)
        })
    }
}

impl ModelBackend for BlockingPartialBackend {
    fn backend_name(&self) -> &'static str {
        "blocking-partial"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::remote_default()
    }

    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ModelStream, kurama_protocol::KuramaError>> {
        Box::pin(async move {
            let blocked = Arc::clone(&self.blocked);
            let events = stream::iter([Ok(ModelEvent::TextDelta {
                text: "partial cancellation".into(),
            })])
            .chain(stream::once(async move {
                blocked.notify_one();
                pending::<Result<ModelEvent, kurama_protocol::KuramaError>>().await
            }));
            Ok(Box::pin(events) as ModelStream)
        })
    }
}

struct GatedTextBackend {
    text: String,
    blocked: Arc<Notify>,
    complete: Arc<Notify>,
}

impl ModelBackend for GatedTextBackend {
    fn backend_name(&self) -> &'static str {
        "gated-text"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::remote_default()
    }

    fn stream<'a>(
        &'a self,
        _request: ModelRequest,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ModelStream, kurama_protocol::KuramaError>> {
        Box::pin(async move {
            let blocked = Arc::clone(&self.blocked);
            let complete = Arc::clone(&self.complete);
            let events = stream::iter([Ok(ModelEvent::TextDelta {
                text: self.text.clone(),
            })])
            .chain(stream::once(async move {
                blocked.notify_one();
                complete.notified().await;
                Ok(ModelEvent::ResponseCompleted {
                    cursor: None,
                    finish_reason: FinishReason::Stop,
                })
            }));
            Ok(Box::pin(events) as ModelStream)
        })
    }
}

fn partial_stream_config(
    session_id: &str,
    backend: Arc<dyn ModelBackend>,
    store: Arc<MemoryStore>,
) -> EngineConfig {
    EngineConfig {
        session: SessionMetadata {
            id: session_id.into(),
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
        agent_id: None,
        orchestration: None,
        provider_retry_delays_ms: Vec::new(),
        command_capacity: 32,
        event_capacity: 128,
    }
}

struct FirstRoundGateBackend {
    inner: RecordingBackend,
    blocked: Arc<Notify>,
    release: Arc<Notify>,
}

impl ModelBackend for FirstRoundGateBackend {
    fn backend_name(&self) -> &'static str {
        self.inner.backend_name()
    }

    fn capabilities(&self) -> BackendCapabilities {
        self.inner.capabilities()
    }

    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ModelStream, kurama_protocol::KuramaError>> {
        Box::pin(async move {
            let first = self.inner.requests.lock().is_empty();
            let stream = self.inner.stream(request, cancel).await?;
            let blocked = self.blocked.clone();
            let release = self.release.clone();
            Ok(Box::pin(stream.then(move |event| {
                let blocked = blocked.clone();
                let release = release.clone();
                async move {
                    if first && matches!(event, Ok(ModelEvent::ResponseCompleted { .. })) {
                        blocked.notify_one();
                        release.notified().await;
                    }
                    event
                }
            })) as ModelStream)
        })
    }
}

async fn control_event(events: &mut kurama_core::engine::RuntimeEvents) -> RuntimeEvent {
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("control event timed out")
            .expect("engine event stream closed");
        if !matches!(event, RuntimeEvent::Ready) {
            return event;
        }
    }
}

#[tokio::test]
async fn steering_at_text_completion_continues_before_turn_completed() {
    let backend = Arc::new(FirstRoundGateBackend {
        inner: RecordingBackend {
            inner: ScriptedBackend::new(vec![
                vec![
                    Ok(ModelEvent::TextDelta {
                        text: "first answer".into(),
                    }),
                    Ok(ModelEvent::ResponseCompleted {
                        cursor: None,
                        finish_reason: FinishReason::Stop,
                    }),
                ],
                vec![Ok(ModelEvent::ResponseCompleted {
                    cursor: None,
                    finish_reason: FinishReason::Stop,
                })],
            ]),
            requests: Mutex::new(Vec::new()),
        },
        blocked: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    });
    let store = Arc::new(MemoryStore::default());
    let (handle, mut events) = Engine::spawn(
        partial_stream_config("steer-stop", backend.clone(), store.clone()),
        Vec::new(),
    )
    .expect("spawn");
    handle.inspect_context().await.expect("idle inspection");
    assert!(matches!(
        control_event(&mut events).await,
        RuntimeEvent::ContextInspected { .. }
    ));
    assert!(backend.inner.requests.lock().is_empty());
    handle
        .submit("original request", false)
        .await
        .expect("submit");
    backend.blocked.notified().await;
    handle.steer("new direction", true).await.expect("steer");
    handle.inspect_context().await.expect("active inspection");
    let mut queued = false;
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::SteeringQueued { text } => {
                assert_eq!(text, "new direction");
                queued = true;
            }
            RuntimeEvent::ContextInspected { inspection } => {
                assert!(queued);
                assert!(inspection.assembly_error.is_none());
                assert_eq!(backend.inner.requests.lock().len(), 1);
                break;
            }
            RuntimeEvent::SteeringApplied { .. } | RuntimeEvent::TurnCompleted => {
                panic!("steering crossed an incomplete model batch");
            }
            RuntimeEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    backend.release.notify_one();
    let mut applied = false;
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::SteeringApplied { text } => {
                assert_eq!(text, "new direction");
                applied = true;
            }
            RuntimeEvent::TurnCompleted => {
                assert!(applied);
                break;
            }
            RuntimeEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    {
        let requests = backend.inner.requests.lock();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].delegation.is_none());
        assert!(requests[1].delegation.is_some());
        let users: Vec<_> = requests[1]
            .items
            .iter()
            .filter_map(|item| match item {
                ModelItem::User { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(users, ["original request", "new direction"]);
        assert!(requests[1].items.iter().any(|item| matches!(item,
            ModelItem::Assistant { text } if text == "first answer")));
    }
    let durable = store.events("steer-stop");
    assert_eq!(
        durable
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::TurnCompleted))
            .count(),
        1
    );
    assert_eq!(
        durable
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::UserMessage { .. }))
            .count(),
        1
    );
    assert_eq!(
        durable
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::UserSteered { .. }))
            .count(),
        1
    );
    handle.shutdown().await.expect("shutdown");
    assert!(matches!(
        control_event(&mut events).await,
        RuntimeEvent::Shutdown
    ));

    // Restart from the applied steering boundary. The text itself does not
    // request delegation; authorization must survive through the durable flag.
    let replay = durable[..durable.len() - 1].to_vec();
    let replay_store = Arc::new(MemoryStore::default());
    seed_replay(&replay_store, &replay);
    let replay_backend = Arc::new(RecordingBackend {
        inner: ScriptedBackend::new(vec![vec![Ok(ModelEvent::ResponseCompleted {
            cursor: None,
            finish_reason: FinishReason::Stop,
        })]]),
        requests: Mutex::new(Vec::new()),
    });
    let (handle, mut events) = Engine::spawn(
        partial_stream_config("steer-stop", replay_backend.clone(), replay_store),
        replay,
    )
    .expect("resume explicitly authorized steering");
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    assert!(replay_backend.requests.lock()[0].delegation.is_some());
    handle.shutdown().await.expect("shutdown resumed engine");
    assert!(matches!(
        control_event(&mut events).await,
        RuntimeEvent::Shutdown
    ));
}

#[tokio::test]
async fn rejected_and_cancelled_steering_returns_input_without_applying_it() {
    let blocked = Arc::new(Notify::new());
    let store = Arc::new(MemoryStore::default());
    let (handle, mut events) = Engine::spawn(
        partial_stream_config(
            "steer-cancel",
            Arc::new(BlockingPartialBackend {
                blocked: blocked.clone(),
            }),
            store.clone(),
        ),
        Vec::new(),
    )
    .expect("spawn");
    handle.submit("original", false).await.expect("submit");
    blocked.notified().await;
    handle.steer("keep this draft", false).await.expect("steer");
    let oversized = "x".repeat(262_145);
    handle
        .steer(oversized.clone(), false)
        .await
        .expect("oversized steer");
    let mut accepted = false;
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::SteeringQueued { text } => {
                assert_eq!(text, "keep this draft");
                accepted = true;
            }
            RuntimeEvent::SteeringRejected { text, .. } => {
                assert_eq!(text, oversized);
                assert!(accepted);
                break;
            }
            RuntimeEvent::SteeringApplied { .. }
            | RuntimeEvent::Error { .. }
            | RuntimeEvent::TurnCompleted => panic!("rejection terminated the active turn"),
            _ => {}
        }
    }
    handle
        .inspect_context()
        .await
        .expect("inspect after rejection");
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::ContextInspected { .. } => break,
            RuntimeEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    handle.cancel_turn().await.expect("cancel");
    let mut returned = Vec::new();
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::SteeringRejected { text, .. } => returned.push(text),
            RuntimeEvent::Error { .. } => break,
            RuntimeEvent::SteeringApplied { .. } | RuntimeEvent::TurnCompleted => {
                panic!("cancelled steering was applied")
            }
            _ => {}
        }
    }
    assert_eq!(returned, ["keep this draft"]);
    assert!(
        !store
            .events("steer-cancel")
            .iter()
            .any(|e| matches!(e.event, SessionEvent::UserSteered { .. }))
    );
    handle.shutdown().await.expect("shutdown");
    assert!(matches!(
        control_event(&mut events).await,
        RuntimeEvent::Shutdown
    ));
}

#[tokio::test]
async fn eof_without_response_completed_fails_and_preserves_partial_text() {
    let store = Arc::new(MemoryStore::default());
    let backend = Arc::new(ScriptedBackend::new(vec![vec![Ok(
        ModelEvent::TextDelta {
            text: "unterminated response".into(),
        },
    )]]));
    let (handle, mut events) = Engine::spawn(
        partial_stream_config("missing-completion", backend, Arc::clone(&store)),
        Vec::new(),
    )
    .expect("spawn engine");

    handle.submit("inspect", false).await.expect("submit");
    let message = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match control_event(&mut events).await {
                RuntimeEvent::Error { message } => break message,
                RuntimeEvent::TurnCompleted => {
                    panic!("unterminated model stream completed the turn")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("missing-completion failure timed out");

    assert!(message.contains("ended before response completion"));
    let assistant_messages = store
        .events("missing-completion")
        .into_iter()
        .filter_map(|event| match event.event {
            SessionEvent::AssistantMessage { text } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(assistant_messages, ["unterminated response"]);
}

#[tokio::test]
async fn response_completed_stops_stream_consumption() {
    let trailing_polls = Arc::new(AtomicUsize::new(0));
    let backend = Arc::new(CompletionThenPendingBackend {
        trailing_polls: Arc::clone(&trailing_polls),
    });
    let (handle, mut events) = Engine::spawn(
        partial_stream_config(
            "completion-terminal",
            backend,
            Arc::new(MemoryStore::default()),
        ),
        Vec::new(),
    )
    .expect("spawn engine");

    handle.submit("inspect", false).await.expect("submit");
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match control_event(&mut events).await {
                RuntimeEvent::TurnCompleted => break,
                RuntimeEvent::Error { message } => panic!("engine error: {message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("turn remained blocked after response completion");

    assert_eq!(trailing_polls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn tool_calls_finish_without_work_fails_before_another_round() {
    let backend = Arc::new(ToolCallsWithoutWorkBackend::default());
    let (handle, mut events) = Engine::spawn(
        partial_stream_config(
            "tool-calls-without-work",
            backend.clone(),
            Arc::new(MemoryStore::default()),
        ),
        Vec::new(),
    )
    .expect("spawn engine");

    handle.submit("inspect", false).await.expect("submit");
    let message = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match control_event(&mut events).await {
                RuntimeEvent::Error { message } => break message,
                RuntimeEvent::TurnCompleted => {
                    panic!("no-progress tool-call round completed the turn")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("no-progress tool-call failure timed out");

    assert!(message.contains("tool calls without producing work"));
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn length_finish_rejects_tool_calls_before_execution() {
    let backend: Arc<dyn ModelBackend> = Arc::new(ScriptedBackend::new(vec![
        vec![
            Ok(ModelEvent::ToolCall {
                call_id: "terminal_call".into(),
                name: "count".into(),
                arguments: serde_json::json!({}),
            }),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Length,
            }),
        ],
        vec![Ok(ModelEvent::ResponseCompleted {
            cursor: None,
            finish_reason: FinishReason::Stop,
        })],
    ]));
    let tool = Arc::new(CountingTool::default());
    let executions = tool.executions.clone();
    let (handle, mut events) = Engine::spawn(
        counting_config(
            "length-with-tool-call",
            backend,
            tool,
            Arc::new(MemoryStore::default()),
        ),
        Vec::new(),
    )
    .expect("spawn engine");

    handle.submit("inspect", false).await.expect("submit");
    let message = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match control_event(&mut events).await {
                RuntimeEvent::Error { message } => break message,
                RuntimeEvent::TurnCompleted => panic!("length finish completed the turn"),
                _ => {}
            }
        }
    })
    .await
    .expect("length finish failure timed out");

    assert!(message.contains("output limit"));
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelled_finish_rejects_delegation_before_execution() {
    let backend: Arc<dyn ModelBackend> = Arc::new(ScriptedBackend::new(vec![
        vec![
            Ok(ModelEvent::Delegation {
                request: DelegationRequest { agents: Vec::new() },
            }),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Cancelled,
            }),
        ],
        vec![Ok(ModelEvent::ResponseCompleted {
            cursor: None,
            finish_reason: FinishReason::Stop,
        })],
    ]));
    let (handle, mut events) = Engine::spawn(
        partial_stream_config(
            "cancelled-with-delegation",
            backend,
            Arc::new(MemoryStore::default()),
        ),
        Vec::new(),
    )
    .expect("spawn engine");

    handle.submit("delegate", true).await.expect("submit");
    let message = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match control_event(&mut events).await {
                RuntimeEvent::Error { message } => break message,
                RuntimeEvent::AgentUpdated { .. } => {
                    panic!("cancelled finish started delegation")
                }
                RuntimeEvent::TurnCompleted => panic!("cancelled finish completed the turn"),
                _ => {}
            }
        }
    })
    .await
    .expect("cancelled finish failure timed out");

    assert_eq!(message, "cancelled");
}

fn delegation_request(objective: &str) -> DelegationRequest {
    DelegationRequest {
        agents: vec![AgentSpec {
            role: "researcher".into(),
            objective: objective.into(),
            profile: None,
            context_refs: Vec::new(),
            write_scope: WriteScope::default(),
            budget: AgentBudget::default(),
            depends_on: Vec::new(),
        }],
    }
}

fn delegation_config(
    session_id: &str,
    backend: Arc<dyn ModelBackend>,
    store: Arc<MemoryStore>,
) -> EngineConfig {
    let mut config = partial_stream_config(session_id, backend, store);
    config.orchestrator = Arc::new(SmartOrchestrator::new(Arc::new(SequenceIds::new(100))));
    config.orchestration = Some(EngineOrchestration {
        context: orchestration_context(),
        runner: Arc::new(ImmediateChildRunner {
            summary: "complete".into(),
        }),
    });
    config
}

struct ScopeReportingRunner;

impl kurama_core::agent_manager::ChildRunner for ScopeReportingRunner {
    fn run(
        &self,
        context: kurama_core::agent_manager::ChildRunContext,
    ) -> BoxFuture<'static, Result<kurama_protocol::agent::AgentResult, kurama_protocol::KuramaError>>
    {
        Box::pin(async move {
            Ok(kurama_protocol::agent::AgentResult {
                agent_id: context.agent_id,
                summary: "scope accepted".into(),
                changed_files: context
                    .launch
                    .write_scope
                    .roots
                    .iter()
                    .chain(&context.launch.write_scope.files)
                    .map(|path| path.display().to_string())
                    .collect(),
                evidence_refs: Vec::new(),
            })
        })
    }
}

#[tokio::test]
async fn relative_parent_and_child_scopes_share_workspace_authority() {
    let workspace = std::env::temp_dir().canonicalize().expect("workspace");
    let relative_root = PathBuf::from(
        unique_staging_path("uncreated-scope")
            .file_name()
            .expect("unique name"),
    );
    let relative_file = relative_root.join("src/new.rs");
    assert!(!workspace.join(&relative_root).exists());
    let file_scope = WriteScope {
        roots: Vec::new(),
        files: vec![relative_file.clone()],
    };
    let root_scope = WriteScope {
        roots: vec![relative_root.clone()],
        files: Vec::new(),
    };
    let cases = [
        // Existing absolute authority, matching nonexistent relative files and roots,
        // and inherited authority must all use the same workspace base.
        (
            WriteScope {
                roots: vec![workspace.clone()],
                files: Vec::new(),
            },
            file_scope.clone(),
            Some(relative_file.clone()),
        ),
        (
            file_scope.clone(),
            file_scope.clone(),
            Some(relative_file.clone()),
        ),
        (
            root_scope.clone(),
            root_scope.clone(),
            Some(relative_root.clone()),
        ),
        (
            file_scope.clone(),
            WriteScope::default(),
            Some(relative_file),
        ),
        (
            root_scope.clone(),
            WriteScope::default(),
            Some(relative_root),
        ),
        (file_scope, root_scope, None),
    ];
    for (parent_scope, child_scope, expected) in cases {
        let mut request = delegation_request("implement the parser");
        request.agents[0].write_scope = child_scope;
        let backend = Arc::new(RecordingBackend {
            inner: ScriptedBackend::new(vec![
                vec![
                    Ok(ModelEvent::Delegation { request }),
                    Ok(ModelEvent::ResponseCompleted {
                        cursor: None,
                        finish_reason: FinishReason::ToolCalls,
                    }),
                ],
                vec![Ok(ModelEvent::ResponseCompleted {
                    cursor: None,
                    finish_reason: FinishReason::Stop,
                })],
            ]),
            requests: Mutex::new(Vec::new()),
        });
        let store = Arc::new(MemoryStore::default());
        let mut config = delegation_config("relative-scope", backend.clone(), store.clone());
        config.workspace_root = workspace.clone();
        config.session.project_root = workspace.display().to_string();
        let orchestration = config.orchestration.as_mut().expect("orchestration");
        orchestration.context.parent_write_scope = parent_scope;
        orchestration.runner = Arc::new(ScopeReportingRunner);
        let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("engine");
        handle.submit("implement this", true).await.expect("submit");
        let accepted = loop {
            match control_event(&mut events).await {
                RuntimeEvent::TurnCompleted => break true,
                RuntimeEvent::Error { .. } => break false,
                _ => {}
            }
        };
        assert_eq!(accepted, expected.is_some());
        if let Some(relative) = expected {
            let expected = vec![workspace.join(relative).display().to_string()];
            assert!(backend.requests.lock().last().expect("parent request").items.iter().any(|item| matches!(item,
                ModelItem::AgentResult { summary, changed_files, .. } if summary == "scope accepted" && changed_files == &expected
            )));
        } else {
            assert!(
                !store
                    .events("relative-scope")
                    .iter()
                    .any(|event| matches!(event.event, SessionEvent::AgentQueued { .. }))
            );
        }
        handle.shutdown().await.expect("shutdown");
    }
}

#[tokio::test]
async fn explicit_submit_and_idle_steering_authorization_survive_durable_replay() {
    for idle_steering in [false, true] {
        let backend = Arc::new(RecordingBackend {
            inner: ScriptedBackend::new(vec![vec![Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            })]]),
            requests: Mutex::new(Vec::new()),
        });
        let store = Arc::new(MemoryStore::default());
        let (handle, mut events) = Engine::spawn(
            delegation_config("resume", backend.clone(), store.clone()),
            Vec::new(),
        )
        .expect("engine");
        if idle_steering {
            handle
                .steer("inspect the parser", true)
                .await
                .expect("steer");
        } else {
            handle
                .submit("inspect the parser", true)
                .await
                .expect("submit");
        }
        loop {
            match control_event(&mut events).await {
                RuntimeEvent::TurnCompleted => break,
                RuntimeEvent::Error { message } => panic!("{message}"),
                _ => {}
            }
        }
        assert!(backend.requests.lock()[0].delegation.is_some());
        handle.shutdown().await.expect("shutdown");
        // Snapshot immediately before terminal persistence, as after an interrupted turn.
        let replay: Vec<_> = store
            .replay(&"resume".into())
            .expect("log")
            .into_iter()
            .take_while(|event| !matches!(event.event, SessionEvent::TurnCompleted))
            .collect();
        let replay = serde_json::from_str::<Vec<EventEnvelope>>(
            &serde_json::to_string(&replay).expect("persist"),
        )
        .expect("load");
        let resumed_store = Arc::new(MemoryStore::default());
        seed_replay(&resumed_store, &replay);
        let resumed_backend = Arc::new(RecordingBackend {
            inner: ScriptedBackend::new(vec![vec![Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            })]]),
            requests: Mutex::new(Vec::new()),
        });
        let (handle, mut events) = Engine::spawn(
            delegation_config("resume", resumed_backend.clone(), resumed_store),
            replay,
        )
        .expect("resume engine");
        loop {
            match control_event(&mut events).await {
                RuntimeEvent::TurnCompleted => break,
                RuntimeEvent::Error { message } => panic!("{message}"),
                _ => {}
            }
        }
        assert!(resumed_backend.requests.lock()[0].delegation.is_some());
        handle.shutdown().await.expect("shutdown");
    }
}

#[tokio::test]
async fn goal_continuation_keeps_explicit_authorization_from_the_replayed_user_turn() {
    let replay = vec![
        replay_event(
            0,
            SessionEvent::GoalUpdated {
                goal: kurama_protocol::session::SessionGoal::new("inspect parser").expect("goal"),
            },
        ),
        replay_event(
            1,
            SessionEvent::UserMessage {
                text: "inspect parser".into(),
                explicit_delegation: true,
            },
        ),
    ];
    let store = Arc::new(MemoryStore::default());
    seed_replay(&store, &replay);
    let backend = Arc::new(RecordingBackend {
        inner: ScriptedBackend::new(vec![
            vec![Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            })],
            vec![
                Ok(ModelEvent::ToolCall {
                    call_id: "goal-done".into(),
                    name: "update_goal".into(),
                    arguments: serde_json::json!({"status":"complete", "reason":"parser inspected"}),
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
        ]),
        requests: Mutex::new(Vec::new()),
    });
    let (handle, mut events) =
        Engine::spawn(delegation_config("resume", backend.clone(), store), replay).expect("engine");
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    {
        let requests = backend.requests.lock();
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().all(|request| request.delegation.is_some()));
    }
    handle.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn delegation_can_run_two_waves_then_stop() {
    let backend: Arc<dyn ModelBackend> = Arc::new(ScriptedBackend::new(vec![
        vec![
            Ok(ModelEvent::Delegation {
                request: delegation_request("investigate first"),
            }),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::ToolCalls,
            }),
        ],
        vec![
            Ok(ModelEvent::Delegation {
                request: delegation_request("investigate second"),
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
    let (handle, mut events) = Engine::spawn(
        delegation_config("two-delegation-waves", backend, Arc::clone(&store)),
        Vec::new(),
    )
    .expect("spawn engine");

    handle.submit("delegate", true).await.expect("submit");
    let mut completed_agents = 0;
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::AgentUpdated { snapshot } if snapshot.state == AgentState::Completed => {
                completed_agents += 1;
            }
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("engine error: {message}"),
            _ => {}
        }
    }

    assert_eq!(completed_agents, 2);
    assert_eq!(
        store
            .events("two-delegation-waves")
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::AgentCompleted { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn fourth_delegation_wave_is_refused() {
    let streams = (0..4)
        .map(|wave| {
            vec![
                Ok(ModelEvent::Delegation {
                    request: delegation_request(&format!("investigate wave {wave}")),
                }),
                Ok(ModelEvent::ResponseCompleted {
                    cursor: None,
                    finish_reason: FinishReason::ToolCalls,
                }),
            ]
        })
        .collect();
    let backend: Arc<dyn ModelBackend> = Arc::new(ScriptedBackend::new(streams));
    let store = Arc::new(MemoryStore::default());
    let (handle, mut events) = Engine::spawn(
        delegation_config("fourth-delegation-wave", backend, Arc::clone(&store)),
        Vec::new(),
    )
    .expect("spawn engine");

    handle.submit("delegate", true).await.expect("submit");
    let message = loop {
        match control_event(&mut events).await {
            RuntimeEvent::Error { message } => break message,
            RuntimeEvent::TurnCompleted => panic!("fourth delegation completed the turn"),
            _ => {}
        }
    };

    assert!(message.contains("capability"));
    assert_eq!(
        store
            .events("fourth-delegation-wave")
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::AgentCompleted { .. }))
            .count(),
        3
    );
}

#[tokio::test]
async fn stream_failure_flushes_and_persists_sub_threshold_assistant_text() {
    let store = Arc::new(MemoryStore::default());
    let backend = Arc::new(ScriptedBackend::new(vec![vec![
        Ok(ModelEvent::TextDelta {
            text: "partial failure".into(),
        }),
        Err(kurama_protocol::KuramaError::Model(
            "provider disconnected".into(),
        )),
    ]]));
    let (handle, mut events) = Engine::spawn(
        partial_stream_config("partial-failure", backend, Arc::clone(&store)),
        Vec::new(),
    )
    .expect("spawn engine");

    handle.submit("inspect", false).await.expect("submit");
    let mut streamed = String::new();
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::AssistantDelta { text } => streamed.push_str(&text),
            RuntimeEvent::Error { message } => {
                assert!(message.contains("provider disconnected"));
                break;
            }
            _ => {}
        }
    }

    assert_eq!(streamed, "partial failure");
    let assistant_messages = store
        .events("partial-failure")
        .into_iter()
        .filter_map(|event| match event.event {
            SessionEvent::AssistantMessage { text } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(assistant_messages, ["partial failure"]);
}

#[tokio::test]
async fn observed_usage_is_durable_even_when_text_delivery_fails() {
    struct FailingTextSink;
    impl kurama_protocol::traits::EventSink for FailingTextSink {
        fn emit(&self, event: RuntimeEvent) -> Result<(), kurama_protocol::KuramaError> {
            if matches!(event, RuntimeEvent::AssistantDelta { .. }) {
                return Err(kurama_protocol::KuramaError::Cancelled);
            }
            Ok(())
        }
    }
    let usage = kurama_protocol::model::Usage {
        input_tokens: 123,
        output_tokens: 7,
        cached_input_tokens: 0,
    };
    let store = Arc::new(MemoryStore::default());
    let backend = Arc::new(ScriptedBackend::new(vec![vec![
        Ok(ModelEvent::TextDelta {
            text: "pending".into(),
        }),
        Ok(ModelEvent::Usage { usage }),
    ]]));
    let mut config = partial_stream_config("usage-delivery-failure", backend, store.clone());
    config.sink = Arc::new(FailingTextSink);
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("engine");
    handle.submit("inspect", false).await.expect("submit");
    assert!(matches!(
        control_event(&mut events).await,
        RuntimeEvent::Error { .. }
    ));
    let recorded_usage = store
        .events("usage-delivery-failure")
        .into_iter()
        .filter_map(|event| match event.event {
            SessionEvent::ModelUsage { usage } => Some(usage),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(recorded_usage, [usage]);
}

#[tokio::test]
async fn cancellation_flushes_and_persists_sub_threshold_assistant_text() {
    let store = Arc::new(MemoryStore::default());
    let blocked = Arc::new(Notify::new());
    let backend = Arc::new(BlockingPartialBackend {
        blocked: Arc::clone(&blocked),
    });
    let (handle, mut events) = Engine::spawn(
        partial_stream_config("partial-cancel", backend, Arc::clone(&store)),
        Vec::new(),
    )
    .expect("spawn engine");

    handle.submit("inspect", false).await.expect("submit");
    blocked.notified().await;
    handle.cancel_turn().await.expect("cancel turn");
    let mut streamed = String::new();
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::AssistantDelta { text } => streamed.push_str(&text),
            RuntimeEvent::Error { message } => {
                assert_eq!(message, "cancelled");
                break;
            }
            _ => {}
        }
    }

    assert_eq!(streamed, "partial cancellation");
    let assistant_messages = store
        .events("partial-cancel")
        .into_iter()
        .filter_map(|event| match event.event {
            SessionEvent::AssistantMessage { text } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(assistant_messages, ["partial cancellation"]);
}

#[tokio::test]
async fn short_delta_is_visible_before_provider_completion() {
    let blocked = Arc::new(Notify::new());
    let complete = Arc::new(Notify::new());
    let backend = Arc::new(GatedTextBackend {
        text: "first token".into(),
        blocked: Arc::clone(&blocked),
        complete: Arc::clone(&complete),
    });
    let (handle, mut events) = Engine::spawn(
        partial_stream_config("short-delta", backend, Arc::new(MemoryStore::default())),
        Vec::new(),
    )
    .expect("spawn engine");

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        handle.submit("inspect", false).await.expect("submit");
        blocked.notified().await;
        match control_event(&mut events).await {
            RuntimeEvent::AssistantDelta { text } => assert_eq!(text, "first token"),
            event => panic!("expected text while completion remained gated, got {event:?}"),
        }
        complete.notify_one();
        match control_event(&mut events).await {
            RuntimeEvent::TurnCompleted => {}
            event => panic!("unexpected event after releasing completion: {event:?}"),
        }
    })
    .await
    .expect("short text remained buffered behind provider completion");
}

#[tokio::test]
async fn cancellation_flushes_pending_tail_after_full_chunk() {
    let store = Arc::new(MemoryStore::default());
    let blocked = Arc::new(Notify::new());
    let text = format!("{}pending 𝄞", "x".repeat(4_096));
    let backend = Arc::new(GatedTextBackend {
        text: text.clone(),
        blocked: Arc::clone(&blocked),
        complete: Arc::new(Notify::new()),
    });
    let mut config = partial_stream_config("pending-tail-cancel", backend, Arc::clone(&store));
    config.event_capacity = 1;
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn engine");

    let streamed = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        assert!(matches!(events.recv().await, Some(RuntimeEvent::Ready)));
        handle.submit("inspect", false).await.expect("submit");
        blocked.notified().await;
        handle.cancel_turn().await.expect("cancel turn");
        let mut streamed = String::new();
        loop {
            match control_event(&mut events).await {
                RuntimeEvent::AssistantDelta { text } => streamed.push_str(&text),
                RuntimeEvent::Error { .. } => break streamed,
                event => panic!("unexpected cancellation event: {event:?}"),
            }
        }
    })
    .await
    .expect("pending-tail cancellation timed out");

    assert_eq!(streamed, text);
    let assistant_messages = store
        .events("pending-tail-cancel")
        .into_iter()
        .filter_map(|event| match event.event {
            SessionEvent::AssistantMessage { text } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(assistant_messages, [text]);
}

#[tokio::test]
async fn large_utf8_deltas_preserve_bytes_and_usage_order_under_backpressure() {
    let store = Arc::new(MemoryStore::default());
    let first = format!("{}{}tail", "x".repeat(4_095), "é界𝄞".repeat(65_536));
    let last = "after usage 𝄞";
    let expected = format!("{first}{last}");
    let backend = Arc::new(ScriptedBackend::new(vec![vec![
        Ok(ModelEvent::TextDelta {
            text: first.clone(),
        }),
        Ok(ModelEvent::Usage {
            usage: Default::default(),
        }),
        Ok(ModelEvent::TextDelta { text: last.into() }),
        Ok(ModelEvent::ResponseCompleted {
            cursor: None,
            finish_reason: FinishReason::Stop,
        }),
    ]]));
    let mut config = partial_stream_config("large-utf8", backend, Arc::clone(&store));
    config.event_capacity = 1;
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn engine");

    let streamed = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        handle.submit("inspect", false).await.expect("submit");
        let mut streamed = String::new();
        let mut saw_usage = false;
        loop {
            match control_event(&mut events).await {
                RuntimeEvent::AssistantDelta { text } => {
                    assert!(!text.is_empty() && text.len() <= 4_096);
                    streamed.push_str(&text);
                }
                RuntimeEvent::Usage { .. } => {
                    assert_eq!(streamed, first);
                    saw_usage = true;
                }
                RuntimeEvent::TurnCompleted => {
                    assert!(saw_usage);
                    break streamed;
                }
                event => panic!("unexpected streaming event: {event:?}"),
            }
        }
    })
    .await
    .expect("large UTF-8 response stalled under backpressure");

    assert_eq!(streamed.as_bytes(), expected.as_bytes());
    let assistant_messages = store
        .events("large-utf8")
        .into_iter()
        .filter_map(|event| match event.event {
            SessionEvent::AssistantMessage { text } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(assistant_messages, [expected]);
}

#[tokio::test]
async fn shutdown_during_streaming_is_acknowledged_without_an_error() {
    let blocked = Arc::new(Notify::new());
    let backend = Arc::new(BlockingPartialBackend {
        blocked: Arc::clone(&blocked),
    });
    let (handle, mut events) = Engine::spawn(
        partial_stream_config("shutdown-stream", backend, Arc::new(MemoryStore::default())),
        Vec::new(),
    )
    .expect("spawn engine");

    handle.submit("inspect", false).await.expect("submit");
    blocked.notified().await;
    handle.shutdown().await.expect("request shutdown");

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match control_event(&mut events).await {
                RuntimeEvent::Shutdown => break,
                RuntimeEvent::Error { message } => {
                    panic!("shutdown surfaced an error: {message}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("shutdown acknowledgement timed out");
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

fn approval_config(
    session_id: &str,
    backend: Arc<dyn ModelBackend>,
    tool: Arc<dyn Tool>,
    policy: Arc<dyn ApprovalPolicy>,
    store: Arc<MemoryStore>,
) -> EngineConfig {
    EngineConfig {
        session: SessionMetadata {
            id: session_id.into(),
            created_at_ms: 0,
            project_root: ".".into(),
            profile: "test".into(),
            mode: ExecutionMode::Supervised,
            redaction_best_effort: false,
        },
        profile: ModelProfile::new("test", "frontier", 4_000, 500),
        backend,
        tools: vec![tool],
        policy,
        store,
        sink: Arc::new(CollectingSink::default()),
        orchestrator: Arc::new(NoDelegation),
        ids: Arc::new(SequenceIds::new(1)),
        context_policy: ContextPolicy::default(),
        workspace_root: PathBuf::from("."),
        write_scope: WriteScope::default(),
        auto: AutoBoundaries::default(),
        agent_id: None,
        orchestration: None,
        provider_retry_delays_ms: Vec::new(),
        command_capacity: 32,
        event_capacity: 128,
    }
}

#[derive(Clone, Default)]
struct ArgumentTool {
    executed: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl Tool for ArgumentTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "edit".into(),
            description: "record arguments".into(),
            parameters: serde_json::json!({"type":"object"}),
        }
    }

    fn classify(
        &self,
        _context: &ToolContext,
        invocation: &ToolInvocation,
    ) -> Result<Operation, kurama_protocol::KuramaError> {
        Ok(Operation::Write {
            paths: vec![PathBuf::from(
                invocation.arguments["path"].as_str().unwrap_or("missing"),
            )],
            destructive: false,
            external: false,
        })
    }

    fn execute<'a>(
        &'a self,
        _context: ToolContext,
        invocation: ToolInvocation,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, kurama_protocol::KuramaError>> {
        Box::pin(async move {
            self.executed.lock().push(invocation.arguments);
            Ok(ToolResult::success(invocation.call_id, "ok"))
        })
    }
}

#[derive(Clone, Default)]
struct ReadArgumentTool {
    executed: Arc<Mutex<Vec<serde_json::Value>>>,
    blocked: Option<Arc<Notify>>,
}

impl Tool for ReadArgumentTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "edit".into(),
            description: "record read arguments".into(),
            parameters: serde_json::json!({"type":"object"}),
        }
    }

    fn classify(
        &self,
        _context: &ToolContext,
        invocation: &ToolInvocation,
    ) -> Result<Operation, kurama_protocol::KuramaError> {
        Ok(Operation::Read {
            paths: vec![PathBuf::from(
                invocation.arguments["path"].as_str().unwrap_or("missing"),
            )],
            external: false,
        })
    }

    fn execute<'a>(
        &'a self,
        _context: ToolContext,
        invocation: ToolInvocation,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, kurama_protocol::KuramaError>> {
        Box::pin(async move {
            let call_id = invocation.call_id;
            self.executed.lock().push(invocation.arguments);
            if let Some(blocked) = &self.blocked {
                blocked.notify_one();
                pending().await
            } else {
                Ok(ToolResult::success(call_id, "ok"))
            }
        })
    }
}

#[tokio::test]
async fn approval_edit_is_reclassified_before_execution() {
    let backend: Arc<dyn ModelBackend> = Arc::new(ScriptedBackend::new(vec![
        vec![
            Ok(ModelEvent::ToolCall {
                call_id: "call_1".into(),
                name: "edit".into(),
                arguments: serde_json::json!({"path":"unsafe.txt"}),
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
    let tool = ArgumentTool::default();
    let executed = tool.executed.clone();
    let config = approval_config(
        "approval",
        backend,
        Arc::new(tool),
        Arc::new(AskPolicy),
        Arc::new(MemoryStore::default()),
    );
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn");
    handle.submit("edit", false).await.expect("submit");
    let mut approvals = 0;
    let mut stale_rejections = 0;
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::ApprovalRequired { request } if approvals == 0 => {
                approvals += 1;
                assert_eq!(request.arguments, serde_json::json!({"path":"unsafe.txt"}));
                handle
                    .resolve_approval(
                        OperationId::from("stale-operation"),
                        ApprovalResponse::ApproveOnce,
                    )
                    .await
                    .expect("submit stale approval");
                handle
                    .resolve_approval(
                        request.operation_id,
                        ApprovalResponse::Edit {
                            arguments: serde_json::json!({"path":"safe.txt"}),
                        },
                    )
                    .await
                    .expect("edit");
            }
            RuntimeEvent::ApprovalRequired { request } => {
                approvals += 1;
                assert_eq!(request.arguments, serde_json::json!({"path":"safe.txt"}));
                handle
                    .resolve_approval(request.operation_id, ApprovalResponse::ApproveOnce)
                    .await
                    .expect("approve");
            }
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } if message.contains("no longer pending") => {
                stale_rejections += 1;
            }
            RuntimeEvent::Error { message } => panic!("engine error: {message}"),
            _ => {}
        }
    }
    assert_eq!(approvals, 2);
    assert_eq!(stale_rejections, 1);
    assert_eq!(
        executed.lock().as_slice(),
        &[serde_json::json!({"path":"safe.txt"})]
    );
}

#[tokio::test]
async fn approval_edit_remains_resumable_after_a_crash() {
    let backend: Arc<dyn ModelBackend> = Arc::new(ScriptedBackend::new(vec![vec![
        Ok(ModelEvent::ToolCall {
            call_id: "edited_call".into(),
            name: "edit".into(),
            arguments: serde_json::json!({"path":"unsafe.txt"}),
        }),
        Ok(ModelEvent::ResponseCompleted {
            cursor: None,
            finish_reason: FinishReason::ToolCalls,
        }),
    ]]));
    let store = Arc::new(MemoryStore::default());
    let started = Arc::new(Notify::new());
    let config = approval_config(
        "approval-crash",
        backend,
        Arc::new(ReadArgumentTool {
            executed: Arc::default(),
            blocked: Some(started.clone()),
        }),
        Arc::new(AskPolicy),
        store.clone(),
    );
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn engine");
    handle.submit("edit", false).await.expect("submit");

    let mut approvals = 0;
    while approvals < 2 {
        match control_event(&mut events).await {
            RuntimeEvent::ApprovalRequired { request } if approvals == 0 => {
                approvals += 1;
                handle
                    .resolve_approval(
                        request.operation_id,
                        ApprovalResponse::Edit {
                            arguments: serde_json::json!({"path":"safe.txt"}),
                        },
                    )
                    .await
                    .expect("edit approval");
            }
            RuntimeEvent::ApprovalRequired { request } => {
                approvals += 1;
                assert_eq!(request.arguments, serde_json::json!({"path":"safe.txt"}));
                handle
                    .resolve_approval(request.operation_id, ApprovalResponse::ApproveOnce)
                    .await
                    .expect("approve edited invocation");
            }
            RuntimeEvent::Error { message } => panic!("engine error: {message}"),
            _ => {}
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), started.notified())
        .await
        .expect("edited tool did not start");

    let replay = store.events("approval-crash");

    drop(handle);
    drop(events);

    let resume_store = Arc::new(MemoryStore::default());
    seed_replay(&resume_store, &replay);
    let resume_tool = ReadArgumentTool::default();
    let resumed_arguments = resume_tool.executed.clone();
    let (resume_handle, mut resume_events) = Engine::spawn(
        approval_config(
            "approval-crash",
            Arc::new(ScriptedBackend::new(vec![vec![Ok(
                ModelEvent::ResponseCompleted {
                    cursor: None,
                    finish_reason: FinishReason::Stop,
                },
            )]])),
            Arc::new(resume_tool),
            Arc::new(AllowAllPolicy),
            resume_store,
        ),
        replay,
    )
    .expect("resume edited approval");

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match control_event(&mut resume_events).await {
                RuntimeEvent::TurnCompleted => break,
                RuntimeEvent::Error { message } => panic!("resume error: {message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("resumed turn timed out");
    assert_eq!(
        resumed_arguments.lock().as_slice(),
        &[serde_json::json!({"path":"safe.txt"})]
    );
    drop(resume_handle);
}

#[derive(Clone, Default)]
struct CountingTool {
    executions: Arc<AtomicUsize>,
}

fn counting_config(
    session_id: &str,
    backend: Arc<dyn ModelBackend>,
    tool: Arc<CountingTool>,
    store: Arc<MemoryStore>,
) -> EngineConfig {
    EngineConfig {
        session: SessionMetadata {
            id: session_id.into(),
            created_at_ms: 0,
            project_root: ".".into(),
            profile: "test".into(),
            mode: ExecutionMode::Supervised,
            redaction_best_effort: false,
        },
        profile: ModelProfile::new("test", "frontier", 4_000, 500),
        backend,
        tools: vec![tool],
        policy: Arc::new(AllowAllPolicy),
        store,
        sink: Arc::new(CollectingSink::default()),
        orchestrator: Arc::new(NoDelegation),
        ids: Arc::new(SequenceIds::new(1)),
        context_policy: ContextPolicy::default(),
        workspace_root: PathBuf::from("."),
        write_scope: WriteScope::default(),
        auto: AutoBoundaries::default(),
        agent_id: None,
        orchestration: None,
        provider_retry_delays_ms: Vec::new(),
        command_capacity: 32,
        event_capacity: 128,
    }
}

impl Tool for CountingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "count".into(),
            description: "count executions".into(),
            parameters: serde_json::json!({"type":"object"}),
        }
    }

    fn classify(
        &self,
        context: &ToolContext,
        _invocation: &ToolInvocation,
    ) -> Result<Operation, kurama_protocol::KuramaError> {
        Ok(Operation::Read {
            paths: vec![context.cwd.clone()],
            external: false,
        })
    }

    fn execute<'a>(
        &'a self,
        _context: ToolContext,
        invocation: ToolInvocation,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, kurama_protocol::KuramaError>> {
        Box::pin(async move {
            self.executions.fetch_add(1, Ordering::Relaxed);
            Ok(ToolResult::success(invocation.call_id, "counted"))
        })
    }
}

#[tokio::test]
async fn repeated_model_call_id_does_not_repeat_the_side_effect() {
    let call = || ModelEvent::ToolCall {
        call_id: "same_call".into(),
        name: "count".into(),
        arguments: serde_json::json!({}),
    };
    let backend: Arc<dyn ModelBackend> = Arc::new(ScriptedBackend::new(vec![
        vec![
            Ok(call()),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::ToolCalls,
            }),
        ],
        vec![
            Ok(call()),
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
    let tool = Arc::new(CountingTool::default());
    let executions = tool.executions.clone();
    let config = counting_config("dedupe", backend, tool, Arc::new(MemoryStore::default()));
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn");
    handle.submit("count", false).await.expect("submit");
    while !matches!(
        events.recv().await.expect("event"),
        RuntimeEvent::TurnCompleted
    ) {}
    assert_eq!(executions.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn duplicate_model_call_ids_in_one_round_are_rejected_before_execution() {
    let backend: Arc<dyn ModelBackend> = Arc::new(ScriptedBackend::new(vec![vec![
        Ok(ModelEvent::ToolCall {
            call_id: "duplicate".into(),
            name: "count".into(),
            arguments: serde_json::json!({"value": 1}),
        }),
        Ok(ModelEvent::ToolCall {
            call_id: "duplicate".into(),
            name: "count".into(),
            arguments: serde_json::json!({"value": 2}),
        }),
        Ok(ModelEvent::ResponseCompleted {
            cursor: None,
            finish_reason: FinishReason::ToolCalls,
        }),
    ]]));
    let tool = Arc::new(CountingTool::default());
    let executions = tool.executions.clone();
    let config = counting_config(
        "duplicate-round",
        backend,
        tool,
        Arc::new(MemoryStore::default()),
    );
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn");
    handle.submit("count twice", false).await.expect("submit");

    let message = loop {
        match control_event(&mut events).await {
            RuntimeEvent::Error { message } => break message,
            RuntimeEvent::TurnCompleted => panic!("duplicate call ids completed the turn"),
            _ => {}
        }
    };

    assert!(message.contains("duplicate tool call id"));
    assert_eq!(executions.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn reused_model_call_id_is_remapped_instead_of_failing_the_turn() {
    let backend: Arc<dyn ModelBackend> = Arc::new(ScriptedBackend::new(vec![
        vec![
            Ok(ModelEvent::ToolCall {
                call_id: "reused".into(),
                name: "count".into(),
                arguments: serde_json::json!({"value": 1}),
            }),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::ToolCalls,
            }),
        ],
        vec![
            Ok(ModelEvent::ToolCall {
                call_id: "reused".into(),
                name: "count".into(),
                arguments: serde_json::json!({"value": 2}),
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
    let tool = Arc::new(CountingTool::default());
    let executions = tool.executions.clone();
    let config = counting_config(
        "conflicting-reuse",
        backend,
        tool,
        Arc::new(MemoryStore::default()),
    );
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn");
    handle.submit("count twice", false).await.expect("submit");

    loop {
        match control_event(&mut events).await {
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("reused call id failed the turn: {message}"),
            _ => {}
        }
    }

    assert_eq!(executions.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn model_call_ids_are_reusable_after_turn_completion() {
    let call = || ModelEvent::ToolCall {
        call_id: "same_call".into(),
        name: "count".into(),
        arguments: serde_json::json!({}),
    };
    let backend: Arc<dyn ModelBackend> = Arc::new(ScriptedBackend::new(vec![
        vec![
            Ok(call()),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::ToolCalls,
            }),
        ],
        vec![Ok(ModelEvent::ResponseCompleted {
            cursor: None,
            finish_reason: FinishReason::Stop,
        })],
        vec![
            Ok(call()),
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
    let tool = Arc::new(CountingTool::default());
    let executions = tool.executions.clone();
    let (handle, mut events) = Engine::spawn(
        counting_config(
            "call-id-turns",
            backend,
            tool,
            Arc::new(MemoryStore::default()),
        ),
        Vec::new(),
    )
    .expect("spawn");

    for prompt in ["first", "second"] {
        handle.submit(prompt, false).await.expect("submit");
        while !matches!(
            events.recv().await.expect("event"),
            RuntimeEvent::TurnCompleted
        ) {}
    }

    assert_eq!(executions.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn resumed_turn_ignores_call_ids_from_completed_turns() {
    let operation_id = OperationId::from("old-operation");
    let call_id = kurama_protocol::id::CallId::from("same_call");
    let operation = Operation::Read {
        paths: vec![".".into()],
        external: false,
    };
    let replay = vec![
        replay_event(
            0,
            SessionEvent::UserMessage {
                text: "old turn".into(),
                explicit_delegation: false,
            },
        ),
        replay_event(
            1,
            SessionEvent::ToolProposed {
                operation_id: operation_id.clone(),
                call_id: call_id.clone(),
                operation,
            },
        ),
        replay_event(
            2,
            SessionEvent::ToolCompleted {
                operation_id,
                result: ToolResult::success(call_id.clone(), "old result"),
            },
        ),
        replay_event(3, SessionEvent::TurnCompleted),
        replay_event(
            4,
            SessionEvent::UserMessage {
                text: "resumed turn".into(),
                explicit_delegation: false,
            },
        ),
    ];
    let backend: Arc<dyn ModelBackend> = Arc::new(ScriptedBackend::new(vec![
        vec![
            Ok(ModelEvent::ToolCall {
                call_id,
                name: "count".into(),
                arguments: serde_json::json!({}),
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
    seed_replay(&store, &replay);
    let tool = Arc::new(CountingTool::default());
    let executions = tool.executions.clone();
    let (_handle, mut events) =
        Engine::spawn(counting_config("resume", backend, tool, store), replay).expect("spawn");

    while !matches!(
        events.recv().await.expect("event"),
        RuntimeEvent::TurnCompleted
    ) {}

    assert_eq!(executions.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn resume_retries_interrupted_read_and_continues_turn() {
    let operation_id = OperationId::from("operation");
    let invocation = ToolInvocation {
        call_id: "call".into(),
        name: "count".into(),
        arguments: serde_json::json!({}),
    };
    let operation = Operation::Read {
        paths: vec![".".into()],
        external: false,
    };
    let replay = vec![
        replay_event(
            0,
            SessionEvent::UserMessage {
                text: "inspect".into(),
                explicit_delegation: false,
            },
        ),
        replay_event(
            1,
            SessionEvent::ToolProposed {
                operation_id: operation_id.clone(),
                call_id: invocation.call_id.clone(),
                operation: operation.clone(),
            },
        ),
        replay_event(
            2,
            SessionEvent::ToolInvocationRecorded {
                operation_id: operation_id.clone(),
                invocation,
            },
        ),
        replay_event(
            3,
            SessionEvent::ToolStarted {
                operation_id: operation_id.clone(),
            },
        ),
    ];
    let store = Arc::new(MemoryStore::default());
    seed_replay(&store, &replay);
    let tool = CountingTool::default();
    let executions = tool.executions.clone();
    let (_handle, mut events) = Engine::spawn(
        resume_config(
            store.clone(),
            vec![Arc::new(tool)],
            Arc::new(AllowAllPolicy),
        ),
        replay,
    )
    .expect("resume engine");

    let mut completed_operation = None;
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::ToolCompleted {
                operation_id,
                result,
            } => {
                assert_eq!(result.call_id.as_ref(), "call");
                completed_operation = Some(operation_id);
            }
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("engine error: {message}"),
            _ => {}
        }
    }

    assert_eq!(executions.load(Ordering::Relaxed), 1);
    assert_eq!(completed_operation.as_ref(), Some(&operation_id));
    assert_eq!(store.operation_completion_count("resume", operation_id), 1);
}

#[tokio::test]
async fn resume_restores_pending_approval_before_write() {
    let operation_id = OperationId::from("operation");
    let path = std::env::temp_dir().join(format!("kurama-approval-{}", std::process::id()));
    let invocation = ToolInvocation {
        call_id: "call".into(),
        name: "edit".into(),
        arguments: serde_json::json!({"path": path}),
    };
    let operation = Operation::Write {
        paths: vec![path.clone()],
        destructive: false,
        external: false,
    };
    let replay = vec![
        replay_event(
            0,
            SessionEvent::UserMessage {
                text: "edit".into(),
                explicit_delegation: false,
            },
        ),
        replay_event(
            1,
            SessionEvent::ToolProposed {
                operation_id: operation_id.clone(),
                call_id: invocation.call_id.clone(),
                operation: operation.clone(),
            },
        ),
        replay_event(
            2,
            SessionEvent::ToolInvocationRecorded {
                operation_id: operation_id.clone(),
                invocation,
            },
        ),
        replay_event(
            3,
            SessionEvent::ApprovalRequested {
                operation_id: operation_id.clone(),
                summary: "write one path".into(),
            },
        ),
    ];
    let store = Arc::new(MemoryStore::default());
    seed_replay(&store, &replay);
    let tool = ArgumentTool::default();
    let executed = tool.executed.clone();
    let (handle, mut events) = Engine::spawn(
        resume_config(store, vec![Arc::new(tool)], Arc::new(AskPolicy)),
        replay,
    )
    .expect("resume engine");

    match control_event(&mut events).await {
        RuntimeEvent::ApprovalRequired { request } => {
            assert_eq!(request.operation_id, operation_id);
            assert_eq!(request.operation, operation);
            assert_eq!(request.arguments, serde_json::json!({"path": path}));
        }
        event => panic!("expected approval, got {event:?}"),
    }
    handle
        .resolve_approval(operation_id, ApprovalResponse::ApproveOnce)
        .await
        .expect("approve recovery");
    while !matches!(
        events.recv().await.expect("recovery event"),
        RuntimeEvent::TurnCompleted
    ) {}

    assert_eq!(executed.lock().len(), 1);
}

#[tokio::test]
async fn resume_approval_uses_empty_arguments_without_durable_invocation() {
    let operation_id = OperationId::from("operation");
    let path = std::env::temp_dir().join(format!(
        "kurama-missing-approval-invocation-{}",
        std::process::id()
    ));
    let operation = Operation::Write {
        paths: vec![path],
        destructive: false,
        external: false,
    };
    let replay = vec![
        replay_event(
            0,
            SessionEvent::UserMessage {
                text: "edit".into(),
                explicit_delegation: false,
            },
        ),
        replay_event(
            1,
            SessionEvent::ToolProposed {
                operation_id: operation_id.clone(),
                call_id: "call".into(),
                operation: operation.clone(),
            },
        ),
        replay_event(
            2,
            SessionEvent::ApprovalRequested {
                operation_id: operation_id.clone(),
                summary: "write one path".into(),
            },
        ),
    ];
    let store = Arc::new(MemoryStore::default());
    seed_replay(&store, &replay);
    let (handle, mut events) = Engine::spawn(
        resume_config(
            store,
            vec![Arc::new(ArgumentTool::default())],
            Arc::new(AskPolicy),
        ),
        replay,
    )
    .expect("resume engine");

    match control_event(&mut events).await {
        RuntimeEvent::ApprovalRequired { request } => {
            assert_eq!(request.operation_id, operation_id);
            assert_eq!(request.operation, operation);
            assert_eq!(request.arguments, serde_json::json!({}));
        }
        event => panic!("expected approval, got {event:?}"),
    }
    handle
        .resolve_approval(operation_id, ApprovalResponse::Deny)
        .await
        .expect("deny recovery");
    while !matches!(
        events.recv().await.expect("recovery event"),
        RuntimeEvent::TurnCompleted
    ) {}
}

#[tokio::test]
async fn resume_records_applied_write_without_repeating_it() {
    let operation_id = OperationId::from("operation");
    let path = PathBuf::from("Cargo.toml");
    let invocation = ToolInvocation {
        call_id: "call".into(),
        name: "edit".into(),
        arguments: serde_json::json!({"path": path}),
    };
    let operation = Operation::Write {
        paths: vec![path.clone()],
        destructive: true,
        external: false,
    };
    let result = ToolResult::success(invocation.call_id.clone(), "already applied");
    let store = Arc::new(MemoryStore::default());
    let postcondition = store
        .put_blob(&std::fs::read(&path).expect("read fixture"))
        .expect("store postcondition");
    let replay = vec![
        replay_event(
            0,
            SessionEvent::UserMessage {
                text: "edit".into(),
                explicit_delegation: false,
            },
        ),
        replay_event(
            1,
            SessionEvent::ToolProposed {
                operation_id: operation_id.clone(),
                call_id: invocation.call_id.clone(),
                operation,
            },
        ),
        replay_event(
            2,
            SessionEvent::ToolInvocationRecorded {
                operation_id: operation_id.clone(),
                invocation,
            },
        ),
        replay_event(
            3,
            SessionEvent::WritePrepared {
                operation_id: operation_id.clone(),
                files: vec![FileCheckpoint {
                    path: path.clone(),
                    content: None,
                }],
            },
        ),
        replay_event(
            4,
            SessionEvent::ToolStarted {
                operation_id: operation_id.clone(),
            },
        ),
        replay_event(
            5,
            SessionEvent::WriteApplied {
                operation_id: operation_id.clone(),
                files: vec![FileCheckpoint {
                    path,
                    content: Some(postcondition),
                }],
                result,
            },
        ),
    ];
    seed_replay(&store, &replay);
    let tool = ArgumentTool::default();
    let executed = tool.executed.clone();
    let (_handle, mut events) = Engine::spawn(
        resume_config(
            store.clone(),
            vec![Arc::new(tool)],
            Arc::new(AllowAllPolicy),
        ),
        replay,
    )
    .expect("resume engine");

    while !matches!(
        events.recv().await.expect("recovery event"),
        RuntimeEvent::TurnCompleted
    ) {}

    assert!(executed.lock().is_empty());
    assert_eq!(store.operation_completion_count("resume", operation_id), 1);
}

#[derive(Clone, Default)]
struct BashCountingTool {
    executions: Arc<AtomicUsize>,
}

impl Tool for BashCountingTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "bash".into(),
            description: "count bash executions".into(),
            parameters: serde_json::json!({"type":"object"}),
        }
    }

    fn classify(
        &self,
        _context: &ToolContext,
        _invocation: &ToolInvocation,
    ) -> Result<Operation, kurama_protocol::KuramaError> {
        Ok(Operation::Bash {
            command: "make install".into(),
            cwd: ".".into(),
            class: CommandClass::Unknown,
            timeout_ms: 1_000,
        })
    }

    fn execute<'a>(
        &'a self,
        _context: ToolContext,
        invocation: ToolInvocation,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, kurama_protocol::KuramaError>> {
        Box::pin(async move {
            self.executions.fetch_add(1, Ordering::Relaxed);
            let mut result = ToolResult::success(invocation.call_id, "ran");
            result.metadata["exit_code"] = serde_json::json!(0);
            Ok(result)
        })
    }
}

#[tokio::test]
async fn resume_requires_a_decision_before_retrying_unknown_bash() {
    let operation_id = OperationId::from("operation");
    let invocation = ToolInvocation {
        call_id: "call".into(),
        name: "bash".into(),
        arguments: serde_json::json!({"command":"make install"}),
    };
    let operation = Operation::Bash {
        command: "make install".into(),
        cwd: ".".into(),
        class: CommandClass::Unknown,
        timeout_ms: 1_000,
    };
    let replay = vec![
        replay_event(
            0,
            SessionEvent::UserMessage {
                text: "install".into(),
                explicit_delegation: false,
            },
        ),
        replay_event(
            1,
            SessionEvent::ToolProposed {
                operation_id: operation_id.clone(),
                call_id: invocation.call_id.clone(),
                operation: operation.clone(),
            },
        ),
        replay_event(
            2,
            SessionEvent::ToolInvocationRecorded {
                operation_id: operation_id.clone(),
                invocation,
            },
        ),
        replay_event(
            3,
            SessionEvent::ToolStarted {
                operation_id: operation_id.clone(),
            },
        ),
    ];
    let store = Arc::new(MemoryStore::default());
    seed_replay(&store, &replay);
    let tool = BashCountingTool::default();
    let executions = tool.executions.clone();
    let (handle, mut events) = Engine::spawn(
        resume_config(store, vec![Arc::new(tool)], Arc::new(AllowAllPolicy)),
        replay,
    )
    .expect("resume engine");

    match control_event(&mut events).await {
        RuntimeEvent::ApprovalRequired { request } => {
            assert_eq!(request.operation_id, operation_id);
            assert_eq!(request.operation, operation);
            assert_eq!(
                request.arguments,
                serde_json::json!({"command":"make install"})
            );
            assert!(request.summary.contains("outcome is unknown"));
        }
        event => panic!("expected recovery decision, got {event:?}"),
    }
    assert_eq!(executions.load(Ordering::Relaxed), 0);
    handle
        .resolve_approval(operation_id, ApprovalResponse::Deny)
        .await
        .expect("skip retry");
    while !matches!(
        events.recv().await.expect("recovery event"),
        RuntimeEvent::TurnCompleted
    ) {}
}

#[tokio::test]
async fn resume_requires_a_new_decision_after_an_authorized_bash_retry_is_interrupted() {
    let operation_id = OperationId::from("operation");
    let invocation = ToolInvocation {
        call_id: "call".into(),
        name: "bash".into(),
        arguments: serde_json::json!({"command":"make install"}),
    };
    let operation = Operation::Bash {
        command: "make install".into(),
        cwd: ".".into(),
        class: CommandClass::Unknown,
        timeout_ms: 1_000,
    };
    let replay = vec![
        replay_event(
            0,
            SessionEvent::UserMessage {
                text: "install".into(),
                explicit_delegation: false,
            },
        ),
        replay_event(
            1,
            SessionEvent::ToolProposed {
                operation_id: operation_id.clone(),
                call_id: invocation.call_id.clone(),
                operation: operation.clone(),
            },
        ),
        replay_event(
            2,
            SessionEvent::ToolInvocationRecorded {
                operation_id: operation_id.clone(),
                invocation,
            },
        ),
        replay_event(
            3,
            SessionEvent::ToolStarted {
                operation_id: operation_id.clone(),
            },
        ),
        replay_event(
            4,
            SessionEvent::ApprovalRequested {
                operation_id: operation_id.clone(),
                summary: "interrupted Bash outcome is unknown".into(),
            },
        ),
        replay_event(
            5,
            SessionEvent::ApprovalResolved {
                operation_id: operation_id.clone(),
                response: ApprovalResponse::ApproveOnce,
            },
        ),
        replay_event(
            6,
            SessionEvent::RecoveryDecision {
                operation_id: operation_id.clone(),
                action: "retry_once".into(),
            },
        ),
        replay_event(
            7,
            SessionEvent::ToolStarted {
                operation_id: operation_id.clone(),
            },
        ),
    ];
    let store = Arc::new(MemoryStore::default());
    seed_replay(&store, &replay);
    let tool = BashCountingTool::default();
    let executions = tool.executions.clone();
    let (handle, mut events) = Engine::spawn(
        resume_config(
            store.clone(),
            vec![Arc::new(tool)],
            Arc::new(AllowAllPolicy),
        ),
        replay,
    )
    .expect("resume engine");

    match control_event(&mut events).await {
        RuntimeEvent::ApprovalRequired { request } => {
            assert_eq!(request.operation_id, operation_id);
            assert_eq!(request.operation, operation);
            assert_eq!(
                request.arguments,
                serde_json::json!({"command":"make install"})
            );
            assert!(request.summary.contains("outcome is unknown"));
        }
        event => panic!("expected recovery decision, got {event:?}"),
    }
    assert_eq!(executions.load(Ordering::Relaxed), 0);
    handle
        .resolve_approval(operation_id, ApprovalResponse::ApproveOnce)
        .await
        .expect("approve retry");
    while !matches!(
        events.recv().await.expect("recovery event"),
        RuntimeEvent::TurnCompleted
    ) {}
    assert_eq!(executions.load(Ordering::Relaxed), 1);
    assert!(store.events("resume").iter().any(|event| matches!(
        &event.event,
        SessionEvent::RecoveryDecision { action, .. } if action == "retry_once"
    )));
}

#[tokio::test]
async fn resume_marks_interrupted_children_failed() {
    let snapshot = AgentSnapshot {
        id: "child".into(),
        role: "worker".into(),
        objective: "inspect".into(),
        profile: "test".into(),
        state: AgentState::Running,
        phase: Some("working".into()),
        active_operation: Some("read".into()),
        changed_files: Vec::new(),
        last_error: None,
    };
    let replay = vec![
        replay_event(
            0,
            SessionEvent::UserMessage {
                text: "delegate".into(),
                explicit_delegation: false,
            },
        ),
        replay_event(
            1,
            SessionEvent::AgentStarted {
                snapshot: snapshot.clone(),
            },
        ),
    ];
    let store = Arc::new(MemoryStore::default());
    seed_replay(&store, &replay);
    let (_handle, mut events) = Engine::spawn(
        resume_config(store.clone(), Vec::new(), Arc::new(AllowAllPolicy)),
        replay,
    )
    .expect("resume engine");

    match control_event(&mut events).await {
        RuntimeEvent::AgentUpdated { snapshot } => {
            assert_eq!(snapshot.id.as_ref(), "child");
            assert_eq!(snapshot.state, AgentState::Failed);
        }
        event => panic!("expected interrupted agent update, got {event:?}"),
    }
    while !matches!(
        events.recv().await.expect("recovery event"),
        RuntimeEvent::TurnCompleted
    ) {}
    assert!(store.events("resume").iter().any(|event| matches!(
        &event.event,
        SessionEvent::AgentFailed { snapshot, .. }
            if snapshot.id.as_ref() == "child"
                && snapshot.state == AgentState::Failed
    )));
}

struct GatedCountTool {
    inner: CountingTool,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

impl Tool for GatedCountTool {
    fn descriptor(&self) -> ToolDescriptor {
        self.inner.descriptor()
    }

    fn classify(
        &self,
        context: &ToolContext,
        invocation: &ToolInvocation,
    ) -> Result<Operation, kurama_protocol::KuramaError> {
        self.inner.classify(context, invocation)
    }

    fn execute<'a>(
        &'a self,
        context: ToolContext,
        invocation: ToolInvocation,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, kurama_protocol::KuramaError>> {
        Box::pin(async move {
            self.started.notify_one();
            self.release.notified().await;
            self.inner.execute(context, invocation, cancel).await
        })
    }
}

#[tokio::test]
async fn steering_waits_for_the_entire_approved_tool_batch() {
    let backend = Arc::new(RecordingBackend {
        inner: ScriptedBackend::new(vec![
            vec![
                Ok(ModelEvent::ToolCall {
                    call_id: "first".into(),
                    name: "count".into(),
                    arguments: serde_json::json!({}),
                }),
                Ok(ModelEvent::ToolCall {
                    call_id: "second".into(),
                    name: "count".into(),
                    arguments: serde_json::json!({}),
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
        ]),
        requests: Mutex::new(Vec::new()),
    });
    let tool = Arc::new(GatedCountTool {
        inner: CountingTool::default(),
        started: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    });
    let store = Arc::new(MemoryStore::default());
    let (handle, mut events) = Engine::spawn(
        approval_config(
            "steer-tools",
            backend.clone(),
            tool.clone(),
            Arc::new(AskPolicy),
            store.clone(),
        ),
        Vec::new(),
    )
    .expect("spawn");
    handle.submit("count twice", false).await.expect("submit");
    let first = loop {
        match control_event(&mut events).await {
            RuntimeEvent::ApprovalRequired { request } => break request.operation_id,
            RuntimeEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    };
    handle
        .steer("during approval", false)
        .await
        .expect("approval steering");
    handle.inspect_context().await.expect("approval inspection");
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::ContextInspected { .. } => break,
            RuntimeEvent::SteeringApplied { .. }
            | RuntimeEvent::TurnCompleted
            | RuntimeEvent::Error { .. } => panic!("inspection or steering resolved approval"),
            _ => {}
        }
    }
    handle
        .resolve_approval(first, ApprovalResponse::ApproveOnce)
        .await
        .expect("approve first");
    tool.started.notified().await;
    handle
        .steer("during execution", false)
        .await
        .expect("tool steering");
    handle.inspect_context().await.expect("tool inspection");
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::ContextInspected { .. } => break,
            RuntimeEvent::SteeringApplied { .. }
            | RuntimeEvent::TurnCompleted
            | RuntimeEvent::Error { .. } => panic!("steering interrupted execution"),
            _ => {}
        }
    }
    tool.release.notify_one();
    let second = loop {
        match control_event(&mut events).await {
            RuntimeEvent::ApprovalRequired { request } => break request.operation_id,
            RuntimeEvent::SteeringApplied { .. } => panic!("steering split a tool batch"),
            RuntimeEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    };
    handle
        .resolve_approval(second, ApprovalResponse::ApproveOnce)
        .await
        .expect("approve second");
    tool.started.notified().await;
    tool.release.notify_one();
    let mut applied = Vec::new();
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::SteeringApplied { text } => applied.push(text),
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    assert_eq!(applied, ["during approval", "during execution"]);
    assert_eq!(tool.inner.executions.load(Ordering::Relaxed), 2);
    {
        let requests = backend.requests.lock();
        assert_eq!(requests.len(), 2);
        let items = &requests[1].items;
        let last_tool = items
            .iter()
            .rposition(|item| matches!(item, ModelItem::ToolResult { .. }))
            .expect("tool results");
        let first_steer = items
            .iter()
            .position(|item| matches!(item, ModelItem::User { text } if text == "during approval"))
            .expect("steering input");
        assert!(last_tool < first_steer);
        assert_eq!(
            items
                .iter()
                .filter(|item| matches!(item, ModelItem::ToolResult { .. }))
                .count(),
            2
        );
    }
    let durable = store.events("steer-tools");
    let last_result = durable
        .iter()
        .rposition(|event| matches!(event.event, SessionEvent::ToolCompleted { .. }))
        .expect("completed tool");
    let first_steer = durable
        .iter()
        .position(|event| matches!(event.event, SessionEvent::UserSteered { .. }))
        .expect("durable steering");
    assert!(last_result < first_steer);
    handle.shutdown().await.expect("shutdown");
    assert!(matches!(
        control_event(&mut events).await,
        RuntimeEvent::Shutdown
    ));
}

#[tokio::test]
async fn recovered_steering_retains_history_and_does_not_repeat_completed_tools() {
    let invocation = ToolInvocation {
        call_id: "old-call".into(),
        name: "count".into(),
        arguments: serde_json::json!({}),
    };
    let operation_id = OperationId::from("old-operation");
    let replay = vec![
        replay_event(
            0,
            SessionEvent::UserMessage {
                text: "original request".into(),
                explicit_delegation: false,
            },
        ),
        replay_event(
            1,
            SessionEvent::ToolProposed {
                operation_id: operation_id.clone(),
                call_id: invocation.call_id.clone(),
                operation: Operation::Read {
                    paths: vec![PathBuf::from(".")],
                    external: false,
                },
            },
        ),
        replay_event(
            2,
            SessionEvent::ToolInvocationRecorded {
                operation_id: operation_id.clone(),
                invocation: invocation.clone(),
            },
        ),
        replay_event(
            3,
            SessionEvent::ToolCompleted {
                operation_id,
                result: ToolResult::success(invocation.call_id.clone(), "earlier result"),
            },
        ),
        replay_event(
            4,
            SessionEvent::ModelCursor {
                cursor: kurama_protocol::model::BackendCursor {
                    backend: "scripted".into(),
                    value: "before-steering".into(),
                },
            },
        ),
        replay_event(
            5,
            SessionEvent::UserSteered {
                text: "preserve new direction".into(),
                explicit_delegation: false,
            },
        ),
    ];
    let store = Arc::new(MemoryStore::default());
    seed_replay(&store, &replay);
    let backend = Arc::new(RecordingBackend {
        inner: ScriptedBackend::new(vec![
            vec![
                Ok(ModelEvent::ToolCall {
                    call_id: invocation.call_id,
                    name: invocation.name,
                    arguments: invocation.arguments,
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
        ]),
        requests: Mutex::new(Vec::new()),
    });
    let tool = Arc::new(CountingTool::default());
    let mut config = resume_config(store, vec![tool.clone()], Arc::new(AllowAllPolicy));
    config.backend = backend.clone();
    let (handle, mut events) = Engine::spawn(config, replay).expect("resume");
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    assert_eq!(tool.executions.load(Ordering::Relaxed), 0);
    {
        let requests = backend.requests.lock();
        assert!(requests[0].continuation.is_none());
        let users: Vec<_> = requests[0]
            .items
            .iter()
            .filter_map(|item| match item {
                ModelItem::User { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(users, ["original request", "preserve new direction"]);
        assert!(requests[0].items.iter().any(|item| matches!(item, ModelItem::ToolResult { content, .. } if content == "earlier result")));
    }
    handle.shutdown().await.expect("shutdown");
    assert!(matches!(
        control_event(&mut events).await,
        RuntimeEvent::Shutdown
    ));
}

#[tokio::test]
async fn idle_steering_starts_a_visible_normal_turn() {
    let store = Arc::new(MemoryStore::default());
    let (handle, mut events) = Engine::spawn(
        resume_config(store.clone(), Vec::new(), Arc::new(AllowAllPolicy)),
        Vec::new(),
    )
    .expect("spawn");
    handle.steer("idle request", false).await.expect("steer");
    assert!(
        matches!(control_event(&mut events).await, RuntimeEvent::SteeringApplied { text } if text == "idle request")
    );
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    let durable = store.events("resume");
    assert!(durable.iter().any(
        |event| matches!(&event.event, SessionEvent::UserMessage { text, .. } if text == "idle request")
    ));
    assert!(
        !durable
            .iter()
            .any(|event| matches!(event.event, SessionEvent::UserSteered { .. }))
    );
    handle.shutdown().await.expect("shutdown");
    assert!(matches!(
        control_event(&mut events).await,
        RuntimeEvent::Shutdown
    ));
}

#[tokio::test]
async fn buffered_cancel_rejects_steering_and_preserves_later_submit_cancel_order() {
    let store = Arc::new(MemoryStore::default());
    let backend = Arc::new(RecordingBackend {
        inner: ScriptedBackend::new(vec![vec![Ok(ModelEvent::ResponseCompleted {
            cursor: None,
            finish_reason: FinishReason::Stop,
        })]]),
        requests: Mutex::new(Vec::new()),
    });
    let (handle, mut events) = Engine::spawn(
        partial_stream_config("buffered-cancel", backend.clone(), store.clone()),
        Vec::new(),
    )
    .expect("spawn");
    // These sends all fit in the channel without yielding on this current-thread
    // runtime. Both cancellation batches are ready before the actor starts.
    handle.submit("first", false).await.expect("first submit");
    handle.cancel_turn().await.expect("first cancel");
    handle
        .steer("first draft", false)
        .await
        .expect("first steer");
    handle.inspect_context().await.expect("inspection");
    handle.submit("second", false).await.expect("second submit");
    handle.cancel_turn().await.expect("second cancel");
    handle
        .steer("second draft", false)
        .await
        .expect("second steer");
    handle.submit("third", false).await.expect("third submit");

    let mut rejected = Vec::new();
    let mut failures = 0;
    let mut inspected = false;
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::SteeringRejected { text, .. } => rejected.push(text),
            RuntimeEvent::Error { .. } => failures += 1,
            RuntimeEvent::ContextInspected { .. } => inspected = true,
            RuntimeEvent::SteeringApplied { .. } => panic!("cancelled input was applied"),
            RuntimeEvent::TurnCompleted => break,
            _ => {}
        }
    }
    assert_eq!(rejected, ["first draft", "second draft"]);
    assert_eq!(failures, 2);
    assert!(inspected);
    {
        let requests = backend.requests.lock();
        assert_eq!(
            requests.len(),
            1,
            "both cancelled turns must stay cancelled"
        );
        let users: Vec<_> = requests[0]
            .items
            .iter()
            .filter_map(|item| match item {
                ModelItem::User { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(users, ["first", "second", "third"]);
    }
    handle.shutdown().await.expect("shutdown");
    assert!(matches!(
        control_event(&mut events).await,
        RuntimeEvent::Shutdown
    ));
}

struct ReadySteeringFailureBackend {
    handle: Mutex<Option<kurama_core::engine::EngineHandle>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ModelBackend for ReadySteeringFailureBackend {
    fn backend_name(&self) -> &'static str {
        "ready-steering-failure"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::remote_default()
    }

    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ModelStream, kurama_protocol::KuramaError>> {
        Box::pin(async move {
            self.requests.lock().push(request);
            let handle = self.handle.lock().take();
            if let Some(handle) = handle {
                // Queue commands in the same poll that returns the provider
                // error, so the provider branch necessarily wins this select.
                handle
                    .steer("failed draft", false)
                    .await
                    .expect("failed steer");
                handle.inspect_context().await.expect("inspection");
                handle
                    .submit("explicit next", false)
                    .await
                    .expect("next submit");
                handle
                    .steer("next direction", false)
                    .await
                    .expect("next steer");
                return Err(kurama_protocol::KuramaError::Model(
                    "provider failed".into(),
                ));
            }
            Ok(Box::pin(stream::iter([Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            })])) as ModelStream)
        })
    }
}

#[tokio::test]
async fn provider_failure_rejects_ready_steering_without_eating_the_next_explicit_batch() {
    let backend = Arc::new(ReadySteeringFailureBackend {
        handle: Mutex::new(None),
        requests: Mutex::new(Vec::new()),
    });
    let (handle, mut events) = Engine::spawn(
        partial_stream_config(
            "ready-failure",
            backend.clone(),
            Arc::new(MemoryStore::default()),
        ),
        Vec::new(),
    )
    .expect("spawn");
    *backend.handle.lock() = Some(handle.clone());
    handle.submit("original", false).await.expect("submit");
    let mut rejected = Vec::new();
    let mut applied = Vec::new();
    let mut failures = 0;
    let mut inspected = false;
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::SteeringRejected { text, .. } => rejected.push(text),
            RuntimeEvent::SteeringApplied { text } => applied.push(text),
            RuntimeEvent::Error { .. } => failures += 1,
            RuntimeEvent::ContextInspected { .. } => inspected = true,
            RuntimeEvent::TurnCompleted => break,
            _ => {}
        }
    }
    assert_eq!(rejected, ["failed draft"]);
    assert_eq!(applied, ["next direction"]);
    assert_eq!(failures, 1);
    assert!(inspected);
    {
        let requests = backend.requests.lock();
        assert_eq!(requests.len(), 2);
        let users: Vec<_> = requests[1]
            .items
            .iter()
            .filter_map(|item| match item {
                ModelItem::User { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(users, ["original", "explicit next", "next direction"]);
    }
    handle.shutdown().await.expect("shutdown");
    assert!(matches!(
        control_event(&mut events).await,
        RuntimeEvent::Shutdown
    ));
}

#[tokio::test]
async fn startup_only_recovery_rejects_steering_and_becomes_idle_without_leaking_input() {
    let operation_id = OperationId::from("interrupted-read");
    let invocation = ToolInvocation {
        call_id: "interrupted-call".into(),
        name: "count".into(),
        arguments: serde_json::json!({}),
    };
    let replay = vec![
        replay_event(
            0,
            SessionEvent::UserMessage {
                text: "old request".into(),
                explicit_delegation: false,
            },
        ),
        replay_event(
            1,
            SessionEvent::ToolProposed {
                operation_id: operation_id.clone(),
                call_id: invocation.call_id.clone(),
                operation: Operation::Read {
                    paths: vec![".".into()],
                    external: false,
                },
            },
        ),
        replay_event(
            2,
            SessionEvent::ToolInvocationRecorded {
                operation_id: operation_id.clone(),
                invocation,
            },
        ),
        replay_event(3, SessionEvent::ToolStarted { operation_id }),
        replay_event(
            4,
            SessionEvent::TurnFailed {
                error: "cancelled".into(),
            },
        ),
    ];
    let store = Arc::new(MemoryStore::default());
    seed_replay(&store, &replay);
    let tool = Arc::new(GatedCountTool {
        inner: CountingTool::default(),
        started: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    });
    let backend = Arc::new(RecordingBackend {
        inner: ScriptedBackend::new(vec![vec![Ok(ModelEvent::ResponseCompleted {
            cursor: None,
            finish_reason: FinishReason::Stop,
        })]]),
        requests: Mutex::new(Vec::new()),
    });
    let mut config = resume_config(store.clone(), vec![tool.clone()], Arc::new(AllowAllPolicy));
    config.backend = backend.clone();
    let (handle, mut events) = Engine::spawn(config, replay).expect("spawn recovery");
    tool.started.notified().await;
    handle.steer("recovery draft", false).await.expect("steer");
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::SteeringQueued { .. } => break,
            RuntimeEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    tool.release.notify_one();
    let mut rejected = Vec::new();
    let mut tool_completed = false;
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::ToolCompleted { .. } => tool_completed = true,
            RuntimeEvent::SteeringRejected { text, .. } => rejected.push(text),
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::SteeringApplied { .. } => panic!("terminal recovery applied steering"),
            RuntimeEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    assert!(tool_completed);
    assert_eq!(rejected, ["recovery draft"]);
    assert!(backend.requests.lock().is_empty());
    assert!(
        !store
            .events("resume")
            .iter()
            .any(|event| matches!(event.event, SessionEvent::TurnCompleted))
    );
    handle.submit("new request", false).await.expect("submit");
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::SteeringApplied { .. } => {
                panic!("recovery steering leaked into next turn")
            }
            RuntimeEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    {
        let requests = backend.requests.lock();
        let users: Vec<_> = requests[0]
            .items
            .iter()
            .filter_map(|item| match item {
                ModelItem::User { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(users, ["old request", "new request"]);
    }
    handle.shutdown().await.expect("shutdown");
    assert!(matches!(
        control_event(&mut events).await,
        RuntimeEvent::Shutdown
    ));

    // Completed tool history is not startup recovery work. On a later clean
    // restart, idle steering must still start a visible turn rather than being
    // rejected by terminal recovery cleanup.
    let replay = store.events("resume");
    let (handle, mut events) = Engine::spawn(
        resume_config(store, vec![tool], Arc::new(AllowAllPolicy)),
        replay,
    )
    .expect("restart completed history");
    handle
        .steer("intentional idle request", false)
        .await
        .expect("idle steer");
    assert!(matches!(
        control_event(&mut events).await,
        RuntimeEvent::SteeringApplied { text } if text == "intentional idle request"
    ));
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::SteeringRejected { .. } => panic!("clean restart rejected idle input"),
            RuntimeEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    handle.shutdown().await.expect("shutdown clean restart");
    assert!(matches!(
        control_event(&mut events).await,
        RuntimeEvent::Shutdown
    ));
}

#[tokio::test]
async fn replay_recovers_same_turn_delegation_without_importing_earlier_authorization() {
    let legacy_steering: SessionEvent =
        serde_json::from_str(r#"{"type":"user_steered","text":"focus on error handling"}"#)
            .expect("legacy steering event");
    let histories = [
        (
            vec![
                SessionEvent::UserMessage {
                    text: "review the parser".into(),
                    explicit_delegation: true,
                },
                legacy_steering.clone(),
            ],
            true,
        ),
        (
            vec![
                SessionEvent::UserMessage {
                    text: "review the earlier parser".into(),
                    explicit_delegation: true,
                },
                SessionEvent::TurnCompleted,
                SessionEvent::UserMessage {
                    text: "review the current parser".into(),
                    explicit_delegation: false,
                },
            ],
            false,
        ),
        (
            vec![
                SessionEvent::UserMessage {
                    text: "delegate the parser review".into(),
                    explicit_delegation: false,
                },
                legacy_steering.clone(),
            ],
            true,
        ),
        (
            vec![
                SessionEvent::UserMessage {
                    text: "review the parser".into(),
                    explicit_delegation: false,
                },
                SessionEvent::UserSteered {
                    text: "use helpers".into(),
                    explicit_delegation: true,
                },
                legacy_steering.clone(),
            ],
            true,
        ),
        (
            vec![
                SessionEvent::UserMessage {
                    text: "delegate an earlier review".into(),
                    explicit_delegation: false,
                },
                SessionEvent::TurnCompleted,
                SessionEvent::UserMessage {
                    text: "review the parser".into(),
                    explicit_delegation: false,
                },
                legacy_steering.clone(),
            ],
            false,
        ),
        (
            vec![
                SessionEvent::UserMessage {
                    text: "delegate an earlier review".into(),
                    explicit_delegation: false,
                },
                SessionEvent::TurnCompleted,
                legacy_steering,
            ],
            false,
        ),
    ];
    for (history, expected) in histories {
        let replay: Vec<_> = history
            .into_iter()
            .enumerate()
            .map(|(sequence, event)| replay_event(sequence as u64, event))
            .collect();
        let store = Arc::new(MemoryStore::default());
        seed_replay(&store, &replay);
        let backend = Arc::new(RecordingBackend {
            inner: ScriptedBackend::new(vec![vec![Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            })]]),
            requests: Mutex::new(Vec::new()),
        });
        let (handle, mut events) =
            Engine::spawn(delegation_config("resume", backend.clone(), store), replay)
                .expect("resume");
        loop {
            match control_event(&mut events).await {
                RuntimeEvent::TurnCompleted => break,
                RuntimeEvent::Error { message } => panic!("{message}"),
                _ => {}
            }
        }
        assert_eq!(backend.requests.lock()[0].delegation.is_some(), expected);
        handle.shutdown().await.expect("shutdown");
        assert!(matches!(
            control_event(&mut events).await,
            RuntimeEvent::Shutdown
        ));
    }
}

fn verification_recipe() -> kurama_protocol::verification::VerificationRecipe {
    kurama_protocol::verification::VerificationRecipe {
        command: "make check".into(),
        cwd: ".".into(),
        timeout_ms: 60_000,
    }
}

async fn verification_report(
    events: &mut kurama_core::engine::RuntimeEvents,
) -> kurama_protocol::verification::VerificationReport {
    let mut report = None;
    loop {
        match control_event(events).await {
            RuntimeEvent::VerificationUpdated { report: update } => report = Some(update),
            RuntimeEvent::TurnCompleted => return report.expect("verification report"),
            RuntimeEvent::Error { message } => panic!("verification runtime error: {message}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn verification_is_model_free_and_last_run_survives_resume_but_not_recipe_changes() {
    use kurama_protocol::verification::VerificationStatus;
    let store = Arc::new(MemoryStore::default());
    let backend = Arc::new(RecordingBackend {
        inner: ScriptedBackend::new(vec![]),
        requests: Mutex::new(Vec::new()),
    });
    let mut config = resume_config(
        store.clone(),
        vec![Arc::new(BashCountingTool::default())],
        Arc::new(AllowAllPolicy),
    );
    config.backend = backend.clone();
    let (handle, mut events) = Engine::spawn(config, vec![]).unwrap();
    handle.verify("quick", verification_recipe()).await.unwrap();
    let report = verification_report(&mut events).await;
    assert_eq!(report.status, VerificationStatus::Passed);
    assert_eq!(report.exit_code, Some(0));
    let replay = store.events("resume");
    assert!(replay.iter().any(|event| matches!(&event.event, SessionEvent::ToolCompleted { operation_id, result } if Some(operation_id) == report.operation_id.as_ref() && result.output == "ran")));
    assert!(backend.requests.lock().is_empty());
    handle.shutdown().await.unwrap();
    assert!(matches!(
        control_event(&mut events).await,
        RuntimeEvent::Shutdown
    ));
    let (handle, mut events) = Engine::spawn(
        resume_config(store, vec![], Arc::new(AllowAllPolicy)),
        replay,
    )
    .unwrap();
    handle
        .inspect_verifications([("quick".into(), verification_recipe())].into())
        .await
        .unwrap();
    match control_event(&mut events).await {
        RuntimeEvent::VerificationsInspected { reports } => assert_eq!(reports, vec![report]),
        event => panic!("unexpected event: {event:?}"),
    }
    let mut changed = verification_recipe();
    changed.timeout_ms += 1;
    handle
        .inspect_verifications(
            [
                ("quick".into(), changed),
                ("new".into(), verification_recipe()),
            ]
            .into(),
        )
        .await
        .unwrap();
    match control_event(&mut events).await {
        RuntimeEvent::VerificationsInspected { reports } => {
            assert!(
                reports
                    .iter()
                    .all(|report| report.status == VerificationStatus::NotRun
                        && report.operation_id.is_none())
            );
            assert!(
                reports
                    .iter()
                    .find(|report| report.name == "quick")
                    .unwrap()
                    .message
                    .is_some()
            );
        }
        event => panic!("unexpected event: {event:?}"),
    }
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn verification_approval_edit_cannot_certify_original_recipe_and_denial_does_not_execute() {
    use kurama_protocol::verification::VerificationStatus;
    for edited in [false, true] {
        let store = Arc::new(MemoryStore::default());
        let tool = BashCountingTool::default();
        let executions = tool.executions.clone();
        let (handle, mut events) = Engine::spawn(
            resume_config(store.clone(), vec![Arc::new(tool)], Arc::new(AskPolicy)),
            vec![],
        )
        .unwrap();
        handle.verify("quick", verification_recipe()).await.unwrap();
        let mut approvals = 0;
        let mut report = None;
        loop {
            match control_event(&mut events).await {
                RuntimeEvent::ApprovalRequired { request } => {
                    let response = if !edited {
                        ApprovalResponse::Deny
                    } else if approvals == 0 {
                        ApprovalResponse::Edit {
                            arguments: serde_json::json!({"command":"true", "cwd":".", "timeout_ms":12_345}),
                        }
                    } else {
                        ApprovalResponse::ApproveOnce
                    };
                    approvals += 1;
                    handle
                        .resolve_approval(request.operation_id, response)
                        .await
                        .unwrap();
                }
                RuntimeEvent::VerificationUpdated { report: update } => report = Some(update),
                RuntimeEvent::TurnCompleted => break,
                RuntimeEvent::Error { message } => panic!("{message}"),
                _ => {}
            }
        }
        let report = report.unwrap();
        assert_eq!(
            report.status,
            if edited {
                VerificationStatus::Failed
            } else {
                VerificationStatus::Denied
            }
        );
        assert_eq!(executions.load(Ordering::Relaxed), usize::from(edited));
        if edited {
            assert_eq!(report.command, "true");
            assert_eq!(report.timeout_ms, 12_345);
            assert_eq!(report.exit_code, Some(0));
            let replay = store.events("resume");
            assert!(replay.iter().any(|event| matches!(&event.event, SessionEvent::ToolInvocationRecorded { operation_id, invocation } if Some(operation_id) == report.operation_id.as_ref() && invocation.arguments["command"] == "true")));
        }
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn interrupted_verification_is_reported_without_approval_or_tool_retry() {
    use kurama_protocol::verification::{VerificationReport, VerificationStatus};
    let recipe = verification_recipe();
    let mut report = VerificationReport::not_run("quick".into(), &recipe);
    report.status = VerificationStatus::Running;
    report.started_at_ms = Some(1);
    let replay = vec![
        replay_event(
            0,
            SessionEvent::VerificationStarted {
                recipe: recipe.clone(),
                report,
            },
        ),
        replay_event(
            1,
            SessionEvent::ToolProposed {
                operation_id: "check".into(),
                call_id: "call".into(),
                operation: Operation::Bash {
                    command: recipe.command.clone(),
                    cwd: ".".into(),
                    class: CommandClass::Unknown,
                    timeout_ms: recipe.timeout_ms,
                },
            },
        ),
        replay_event(
            2,
            SessionEvent::ApprovalRequested {
                operation_id: "check".into(),
                summary: "check".into(),
            },
        ),
    ];
    let store = Arc::new(MemoryStore::default());
    seed_replay(&store, &replay);
    let tool = BashCountingTool::default();
    let executions = tool.executions.clone();
    let (handle, mut events) = Engine::spawn(
        resume_config(store.clone(), vec![Arc::new(tool)], Arc::new(AskPolicy)),
        replay,
    )
    .unwrap();
    handle
        .inspect_verifications([("quick".into(), recipe)].into())
        .await
        .unwrap();
    loop {
        match control_event(&mut events).await {
            RuntimeEvent::VerificationUpdated { report } => {
                assert_eq!(report.status, VerificationStatus::Interrupted)
            }
            RuntimeEvent::VerificationsInspected { reports } => {
                assert_eq!(reports[0].status, VerificationStatus::Interrupted);
                break;
            }
            event => panic!("interrupted verification resumed work: {event:?}"),
        }
    }
    assert_eq!(executions.load(Ordering::Relaxed), 0);
    assert!(store.events("resume").iter().any(|event| matches!(&event.event, SessionEvent::VerificationCompleted { report } if report.status == VerificationStatus::Interrupted)));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn verification_inspection_works_during_approval_and_cancel_is_not_a_pass() {
    use kurama_protocol::verification::VerificationStatus;
    let (handle, mut events) = Engine::spawn(
        resume_config(
            Arc::new(MemoryStore::default()),
            vec![Arc::new(BashCountingTool::default())],
            Arc::new(AskPolicy),
        ),
        vec![],
    )
    .unwrap();
    handle.verify("quick", verification_recipe()).await.unwrap();
    loop {
        if matches!(
            control_event(&mut events).await,
            RuntimeEvent::ApprovalRequired { .. }
        ) {
            break;
        }
    }
    assert!(handle.verify("other", verification_recipe()).await.is_err());
    handle
        .inspect_verifications([("quick".into(), verification_recipe())].into())
        .await
        .unwrap();
    match control_event(&mut events).await {
        RuntimeEvent::VerificationsInspected { reports } => {
            assert_eq!(reports[0].status, VerificationStatus::Running)
        }
        event => panic!("unexpected event: {event:?}"),
    }
    handle.cancel_turn().await.unwrap();
    assert_eq!(
        verification_report(&mut events).await.status,
        VerificationStatus::Cancelled
    );
    handle.shutdown().await.unwrap();
}
