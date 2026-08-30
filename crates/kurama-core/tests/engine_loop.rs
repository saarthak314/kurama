use std::{
    future::pending,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    sync::{Arc, Mutex},
};

use futures_util::{StreamExt as _, stream};
use kurama_core::{
    context::{ContextManager, ContextPolicy},
    engine::{Engine, EngineConfig},
    testing::{
        AllowAllPolicy, CollectingSink, EchoTool, MemoryStore, NoDelegation, ScriptedBackend,
        SequenceIds,
    },
};
use kurama_protocol::{
    agent::{AgentSnapshot, AgentState, WriteScope},
    id::{CallId, OperationId, SessionId},
    model::{BackendCapabilities, FinishReason, ModelEvent, ModelProfile, ModelRequest},
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
            path: PathBuf::from("."),
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
            *self.observed.lock().expect("observed limits lock") = Some(context.limits);
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
            path: PathBuf::from("."),
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
        match events.recv().await.expect("runtime event") {
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
        match events.recv().await.expect("runtime event") {
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
        *observed.lock().expect("observed limits lock"),
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
            match events.recv().await.expect("runtime event") {
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
            match events.recv().await.expect("runtime event") {
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
            match events.recv().await.expect("runtime event") {
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
        match events.recv().await.expect("runtime event") {
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
        match events.recv().await.expect("runtime event") {
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
            match events.recv().await.expect("runtime event") {
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
            self.executed
                .lock()
                .expect("executed lock")
                .push(invocation.arguments);
            Ok(ToolResult::success(invocation.call_id, "ok"))
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
    let config = EngineConfig {
        session: SessionMetadata {
            id: "approval".into(),
            created_at_ms: 0,
            project_root: ".".into(),
            profile: "test".into(),
            mode: ExecutionMode::Supervised,
            redaction_best_effort: false,
        },
        profile: ModelProfile::new("test", "frontier", 4_000, 500),
        backend,
        tools: vec![Arc::new(tool)],
        policy: Arc::new(AskPolicy),
        store: Arc::new(MemoryStore::default()),
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
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn");
    handle.submit("edit", false).await.expect("submit");
    let mut approvals = 0;
    let mut stale_rejections = 0;
    loop {
        match events.recv().await.expect("event") {
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
        executed.lock().expect("executed lock").as_slice(),
        &[serde_json::json!({"path":"safe.txt"})]
    );
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
            path: context.cwd.clone(),
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
        match events.recv().await.expect("event") {
            RuntimeEvent::Error { message } => break message,
            RuntimeEvent::TurnCompleted => panic!("duplicate call ids completed the turn"),
            _ => {}
        }
    };

    assert!(message.contains("duplicate tool call id"));
    assert_eq!(executions.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn reused_model_call_id_cannot_alias_a_different_invocation() {
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

    let message = loop {
        match events.recv().await.expect("event") {
            RuntimeEvent::Error { message } => break message,
            RuntimeEvent::TurnCompleted => panic!("conflicting call id completed the turn"),
            _ => {}
        }
    };

    assert!(message.contains("different invocation"));
    assert_eq!(executions.load(Ordering::Relaxed), 1);
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
        path: ".".into(),
        external: false,
    };
    let replay = vec![
        replay_event(
            0,
            SessionEvent::UserMessage {
                text: "old turn".into(),
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
        path: ".".into(),
        external: false,
    };
    let replay = vec![
        replay_event(
            0,
            SessionEvent::UserMessage {
                text: "inspect".into(),
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
        match events.recv().await.expect("recovery event") {
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

    match events.recv().await.expect("approval event") {
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

    assert_eq!(executed.lock().expect("executed lock").len(), 1);
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

    match events.recv().await.expect("approval event") {
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

    assert!(executed.lock().expect("executed lock").is_empty());
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
            Ok(ToolResult::success(invocation.call_id, "ran"))
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

    match events.recv().await.expect("decision event") {
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

    match events.recv().await.expect("decision event") {
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

    match events.recv().await.expect("agent recovery event") {
        RuntimeEvent::AgentUpdated { snapshot } => {
            assert_eq!(snapshot.id.as_ref(), "child");
            assert_eq!(snapshot.state, AgentState::Failed);
            assert_eq!(
                snapshot.last_error.as_deref(),
                Some("interrupted during previous process")
            );
        }
        event => panic!("expected interrupted agent update, got {event:?}"),
    }
    while !matches!(
        events.recv().await.expect("recovery event"),
        RuntimeEvent::TurnCompleted
    ) {}
    assert!(store.events("resume").iter().any(|event| matches!(
        &event.event,
        SessionEvent::AgentFailed { snapshot, error }
            if snapshot.id.as_ref() == "child"
                && snapshot.state == AgentState::Failed
                && error == "interrupted during previous process"
    )));
}
