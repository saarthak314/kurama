use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    sync::{Arc, Mutex},
};

use kurama_core::{
    context::ContextPolicy,
    engine::{Engine, EngineConfig},
    testing::{
        AllowAllPolicy, CollectingSink, EchoTool, MemoryStore, NoDelegation, ScriptedBackend,
        SequenceIds,
    },
};
use kurama_protocol::{
    agent::WriteScope,
    model::{FinishReason, ModelEvent, ModelProfile},
    policy::{ApprovalResponse, AutoBoundaries, ExecutionMode, PolicyContext, PolicyDecision},
    runtime::RuntimeEvent,
    session::SessionMetadata,
    tool::{Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolResult},
    traits::{ApprovalPolicy, BoxFuture, CancelSignal, ModelBackend, Tool},
};

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
    };
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn engine");
    handle.submit("inspect", false).await.expect("submit");

    let mut saw_tool = false;
    let mut text = String::new();
    loop {
        match events.recv().await.expect("runtime event") {
            RuntimeEvent::ToolCompleted { .. } => saw_tool = true,
            RuntimeEvent::AssistantDelta { text: delta } => text.push_str(&delta),
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("engine error: {message}"),
            _ => {}
        }
    }
    assert!(saw_tool);
    assert_eq!(text, "Done.");
    assert_eq!(store.operation_completion_count("session", "o_1"), 1);
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
    };
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn");
    handle.submit("edit", false).await.expect("submit");
    let mut approvals = 0;
    loop {
        match events.recv().await.expect("event") {
            RuntimeEvent::ApprovalRequired { .. } if approvals == 0 => {
                approvals += 1;
                handle
                    .resolve_approval(ApprovalResponse::Edit {
                        arguments: serde_json::json!({"path":"safe.txt"}),
                    })
                    .await
                    .expect("edit");
            }
            RuntimeEvent::ApprovalRequired { .. } => {
                approvals += 1;
                handle
                    .resolve_approval(ApprovalResponse::ApproveOnce)
                    .await
                    .expect("approve");
            }
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("engine error: {message}"),
            _ => {}
        }
    }
    assert_eq!(approvals, 2);
    assert_eq!(
        executed.lock().expect("executed lock").as_slice(),
        &[serde_json::json!({"path":"safe.txt"})]
    );
}

#[derive(Clone, Default)]
struct CountingTool {
    executions: Arc<AtomicUsize>,
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
    let tool = CountingTool::default();
    let executions = tool.executions.clone();
    let config = EngineConfig {
        session: SessionMetadata {
            id: "dedupe".into(),
            created_at_ms: 0,
            project_root: ".".into(),
            profile: "test".into(),
            mode: ExecutionMode::Supervised,
            redaction_best_effort: false,
        },
        profile: ModelProfile::new("test", "frontier", 4_000, 500),
        backend,
        tools: vec![Arc::new(tool)],
        policy: Arc::new(AllowAllPolicy),
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
    };
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn");
    handle.submit("count", false).await.expect("submit");
    while !matches!(
        events.recv().await.expect("event"),
        RuntimeEvent::TurnCompleted
    ) {}
    assert_eq!(executions.load(Ordering::Relaxed), 1);
}
