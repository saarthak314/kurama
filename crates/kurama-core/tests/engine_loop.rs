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
    agent::{AgentSnapshot, AgentState, WriteScope},
    id::{OperationId, SessionId},
    model::{FinishReason, ModelEvent, ModelProfile},
    policy::{ApprovalResponse, AutoBoundaries, ExecutionMode, PolicyContext, PolicyDecision},
    runtime::RuntimeEvent,
    session::{EventEnvelope, FileCheckpoint, SessionEvent, SessionMetadata},
    tool::{CommandClass, Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolResult},
    traits::{ApprovalPolicy, BoxFuture, CancelSignal, ModelBackend, SessionStore, Tool},
};

fn replay_event(sequence: u64, event: SessionEvent) -> EventEnvelope {
    EventEnvelope::new(sequence, sequence, SessionId::from("resume"), None, event)
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
        command_capacity: 32,
        event_capacity: 128,
    };
    let (handle, mut events) = Engine::spawn(config, Vec::new()).expect("spawn");
    handle.submit("count", false).await.expect("submit");
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
