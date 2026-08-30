use kurama_core::recovery::{NoopRecoveryProbe, RecoveryAction, RecoveryPlanner, StreamRecovery};
use kurama_protocol::{
    agent::{AgentSnapshot, AgentState},
    id::{OperationId, SessionId},
    model::{BackendCapabilities, BackendCursor},
    policy::ApprovalResponse,
    session::{EventEnvelope, SessionEvent},
    tool::{CommandClass, Operation},
};

fn queued_agent() -> AgentSnapshot {
    AgentSnapshot {
        id: "child".into(),
        role: "worker".into(),
        objective: "inspect".into(),
        profile: "test".into(),
        state: AgentState::Queued,
        phase: None,
        active_operation: None,
        changed_files: Vec::new(),
        last_error: None,
    }
}

fn event(sequence: u64, event: SessionEvent) -> EventEnvelope {
    EventEnvelope::new(sequence, sequence, SessionId::from("session"), None, event)
}

#[test]
fn recovery_never_blindly_reexecutes_unknown_bash() {
    let operation_id = OperationId::from("operation");
    let events = vec![
        event(
            0,
            SessionEvent::ToolProposed {
                operation_id: operation_id.clone(),
                call_id: "call".into(),
                operation: Operation::Bash {
                    command: "make install".into(),
                    cwd: ".".into(),
                    class: CommandClass::Unknown,
                    timeout_ms: 1_000,
                },
            },
        ),
        event(
            1,
            SessionEvent::ToolStarted {
                operation_id: operation_id.clone(),
            },
        ),
    ];
    let plan = RecoveryPlanner::new()
        .plan(
            &events,
            &NoopRecoveryProbe,
            BackendCapabilities::remote_default(),
        )
        .expect("plan");
    assert!(matches!(
        plan.operations.get(&operation_id),
        Some(RecoveryAction::RequireDecision { .. })
    ));
}

#[test]
fn recovery_marks_a_durable_queued_child_as_interrupted() {
    let plan = RecoveryPlanner::new()
        .plan(
            &[event(
                0,
                SessionEvent::AgentQueued {
                    snapshot: queued_agent(),
                },
            )],
            &NoopRecoveryProbe,
            BackendCapabilities::remote_default(),
        )
        .expect("plan");

    assert_eq!(plan.interrupted_agents.len(), 1);
    assert_eq!(plan.interrupted_agents[0].state, AgentState::Failed);
    assert_eq!(
        plan.interrupted_agents[0].last_error.as_deref(),
        Some("interrupted during previous process")
    );
}

#[test]
fn recovery_requires_a_new_decision_after_an_authorized_bash_retry_is_interrupted() {
    for action in ["retry", "retry_once"] {
        let operation_id = OperationId::from("operation");
        let operation = Operation::Bash {
            command: "make install".into(),
            cwd: ".".into(),
            class: CommandClass::Unknown,
            timeout_ms: 1_000,
        };
        let events = vec![
            event(
                0,
                SessionEvent::ToolProposed {
                    operation_id: operation_id.clone(),
                    call_id: "call".into(),
                    operation,
                },
            ),
            event(
                1,
                SessionEvent::ToolStarted {
                    operation_id: operation_id.clone(),
                },
            ),
            event(
                2,
                SessionEvent::ApprovalRequested {
                    operation_id: operation_id.clone(),
                    summary: "interrupted Bash outcome is unknown".into(),
                },
            ),
            event(
                3,
                SessionEvent::ApprovalResolved {
                    operation_id: operation_id.clone(),
                    response: ApprovalResponse::ApproveOnce,
                },
            ),
            event(
                4,
                SessionEvent::RecoveryDecision {
                    operation_id: operation_id.clone(),
                    action: action.into(),
                },
            ),
            event(
                5,
                SessionEvent::ToolStarted {
                    operation_id: operation_id.clone(),
                },
            ),
        ];

        let plan = RecoveryPlanner::new()
            .plan(
                &events,
                &NoopRecoveryProbe,
                BackendCapabilities::remote_default(),
            )
            .expect("plan");

        assert!(
            matches!(
                plan.operations.get(&operation_id),
                Some(RecoveryAction::RequireDecision { .. })
            ),
            "{action} must not authorize another retry after a crash"
        );
    }
}

#[test]
fn recovery_retries_interrupted_reads_with_same_operation_id() {
    let operation_id = OperationId::from("operation");
    let events = vec![
        event(
            0,
            SessionEvent::ToolProposed {
                operation_id: operation_id.clone(),
                call_id: "call".into(),
                operation: Operation::Read {
                    path: "Cargo.toml".into(),
                    external: false,
                },
            },
        ),
        event(
            1,
            SessionEvent::ToolStarted {
                operation_id: operation_id.clone(),
            },
        ),
    ];
    let plan = RecoveryPlanner::new()
        .plan(
            &events,
            &NoopRecoveryProbe,
            BackendCapabilities::remote_default(),
        )
        .expect("plan");
    assert!(matches!(
        plan.operations.get(&operation_id),
        Some(RecoveryAction::RetrySafe { .. })
    ));
}

#[test]
fn recovery_uses_matching_resumable_cursor() {
    let cursor = BackendCursor {
        backend: "test".into(),
        value: "cursor".into(),
    };
    let events = vec![event(
        0,
        SessionEvent::ModelCursor {
            cursor: cursor.clone(),
        },
    )];
    let plan = RecoveryPlanner::new()
        .plan(
            &events,
            &NoopRecoveryProbe,
            BackendCapabilities {
                streaming: true,
                tool_calls: true,
                native_web_search: false,
                resumable: true,
            },
        )
        .expect("plan");
    assert_eq!(plan.stream, StreamRecovery::Continue(cursor));
}

#[test]
fn recovery_restarts_when_cursor_belongs_to_another_backend() {
    let events = vec![event(
        0,
        SessionEvent::ModelCursor {
            cursor: BackendCursor {
                backend: "other".into(),
                value: "cursor".into(),
            },
        },
    )];
    let plan = RecoveryPlanner::new()
        .plan_for_backend(
            &events,
            &NoopRecoveryProbe,
            "active",
            BackendCapabilities {
                streaming: true,
                tool_calls: true,
                native_web_search: false,
                resumable: true,
            },
        )
        .expect("plan");
    assert_eq!(plan.stream, StreamRecovery::RestartFromBoundary);
}
