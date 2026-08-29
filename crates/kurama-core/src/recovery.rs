use std::{collections::BTreeMap, path::PathBuf};

use kurama_protocol::{
    KuramaError,
    agent::{AgentSnapshot, AgentState},
    id::OperationId,
    model::{BackendCapabilities, BackendCursor},
    policy::ApprovalResponse,
    session::{EventEnvelope, SessionEvent},
    tool::{Operation, ToolResult},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteRecoveryState {
    MatchesPrecondition,
    MatchesPostcondition,
    Conflict { details: String },
    Unknown,
}

pub trait RecoveryProbe {
    fn inspect_write(
        &self,
        operation_id: &OperationId,
        paths: &[PathBuf],
    ) -> Result<WriteRecoveryState, KuramaError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopRecoveryProbe;

impl RecoveryProbe for NoopRecoveryProbe {
    fn inspect_write(
        &self,
        _operation_id: &OperationId,
        _paths: &[PathBuf],
    ) -> Result<WriteRecoveryState, KuramaError> {
        Ok(WriteRecoveryState::Unknown)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryAction {
    Reevaluate {
        operation: Operation,
    },
    RestoreApproval {
        operation: Operation,
        summary: String,
    },
    DiscardDenied,
    RetrySafe {
        operation: Operation,
        reason: String,
    },
    RecordCompleted {
        operation: Operation,
        reason: String,
    },
    RequireDecision {
        operation: Operation,
        reason: String,
    },
    Completed {
        result: ToolResult,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StreamRecovery {
    #[default]
    None,
    Continue(BackendCursor),
    RestartFromBoundary,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryPlan {
    pub operations: BTreeMap<OperationId, RecoveryAction>,
    pub stream: StreamRecovery,
    pub interrupted_agents: Vec<AgentSnapshot>,
}

#[derive(Debug, Clone, Default)]
pub struct RecoveryPlanner;

impl RecoveryPlanner {
    pub fn new() -> Self {
        Self
    }

    pub fn plan(
        &self,
        events: &[EventEnvelope],
        probe: &dyn RecoveryProbe,
        capabilities: BackendCapabilities,
    ) -> Result<RecoveryPlan, KuramaError> {
        self.plan_inner(events, probe, None, capabilities)
    }

    pub fn plan_for_backend(
        &self,
        events: &[EventEnvelope],
        probe: &dyn RecoveryProbe,
        backend: &str,
        capabilities: BackendCapabilities,
    ) -> Result<RecoveryPlan, KuramaError> {
        self.plan_inner(events, probe, Some(backend), capabilities)
    }

    fn plan_inner(
        &self,
        events: &[EventEnvelope],
        probe: &dyn RecoveryProbe,
        backend: Option<&str>,
        capabilities: BackendCapabilities,
    ) -> Result<RecoveryPlan, KuramaError> {
        let mut operations: BTreeMap<OperationId, OperationState> = BTreeMap::new();
        let mut agents: BTreeMap<_, AgentSnapshot> = BTreeMap::new();
        let mut cursor = None;
        let mut turn_terminal = false;

        for envelope in events {
            match &envelope.event {
                SessionEvent::ToolProposed {
                    operation_id,
                    operation,
                    ..
                } => {
                    let state = operations.entry(operation_id.clone()).or_default();
                    if state
                        .operation
                        .as_ref()
                        .is_some_and(|existing| existing != operation)
                    {
                        return Err(KuramaError::Session(format!(
                            "operation {operation_id} has conflicting proposals"
                        )));
                    }
                    state.operation = Some(operation.clone());
                }
                SessionEvent::ApprovalRequested {
                    operation_id,
                    summary,
                } => {
                    let state = operations.entry(operation_id.clone()).or_default();
                    state.approval_summary = Some(summary.clone());
                }
                SessionEvent::ApprovalResolved {
                    operation_id,
                    response,
                } => {
                    operations.entry(operation_id.clone()).or_default().approval =
                        Some(response.clone());
                }
                SessionEvent::ToolPrepared { operation_id } => {
                    operations.entry(operation_id.clone()).or_default().prepared = true;
                }
                SessionEvent::ToolStarted { operation_id } => {
                    operations.entry(operation_id.clone()).or_default().started = true;
                }
                SessionEvent::ToolUnknown { operation_id, .. } => {
                    operations.entry(operation_id.clone()).or_default().unknown = true;
                }
                SessionEvent::ToolCompleted {
                    operation_id,
                    result,
                } => {
                    let state = operations.entry(operation_id.clone()).or_default();
                    if state
                        .completed
                        .as_ref()
                        .is_some_and(|existing| existing != result)
                    {
                        return Err(KuramaError::Session(format!(
                            "operation {operation_id} has conflicting completions"
                        )));
                    }
                    state.completed = Some(result.clone());
                }
                SessionEvent::ModelCursor { cursor: value } => cursor = Some(value.clone()),
                SessionEvent::TurnCompleted | SessionEvent::TurnFailed { .. } => {
                    turn_terminal = true;
                }
                SessionEvent::UserMessage { .. } => turn_terminal = false,
                SessionEvent::AgentStarted { snapshot }
                | SessionEvent::AgentProgress { snapshot } => {
                    agents.insert(snapshot.id.clone(), snapshot.clone());
                }
                SessionEvent::AgentCompleted { snapshot, .. }
                | SessionEvent::AgentFailed { snapshot, .. }
                | SessionEvent::AgentCancelled { snapshot } => {
                    agents.remove(&snapshot.id);
                }
                _ => {}
            }
        }

        let mut planned = BTreeMap::new();
        for (operation_id, state) in operations {
            let Some(operation) = state.operation else {
                if state.unknown {
                    if let Some(result) = state.completed {
                        planned.insert(operation_id, RecoveryAction::Completed { result });
                    }
                    continue;
                }
                return Err(KuramaError::Session(format!(
                    "operation {operation_id} has lifecycle events without a proposal"
                )));
            };
            let action = if let Some(result) = state.completed {
                RecoveryAction::Completed { result }
            } else if state.approval == Some(ApprovalResponse::Deny) {
                RecoveryAction::DiscardDenied
            } else if !state.started && state.approval_summary.is_some() && state.approval.is_none()
            {
                RecoveryAction::RestoreApproval {
                    operation,
                    summary: state.approval_summary.unwrap_or_default(),
                }
            } else if !state.started {
                RecoveryAction::Reevaluate { operation }
            } else {
                match &operation {
                    Operation::Read { .. }
                    | Operation::WebSearch { .. }
                    | Operation::WebOpen { .. } => RecoveryAction::RetrySafe {
                        operation,
                        reason: "idempotent operation was interrupted".into(),
                    },
                    Operation::Bash { .. } => RecoveryAction::RequireDecision {
                        operation,
                        reason: "interrupted Bash outcome is unknown".into(),
                    },
                    Operation::Write { paths, .. } => match probe
                        .inspect_write(&operation_id, paths)?
                    {
                        WriteRecoveryState::MatchesPostcondition => {
                            RecoveryAction::RecordCompleted {
                                operation,
                                reason: "current content matches the durable postcondition".into(),
                            }
                        }
                        WriteRecoveryState::MatchesPrecondition => RecoveryAction::RetrySafe {
                            operation,
                            reason: "current content still matches the durable precondition".into(),
                        },
                        WriteRecoveryState::Conflict { details } => {
                            RecoveryAction::RequireDecision {
                                operation,
                                reason: details,
                            }
                        }
                        WriteRecoveryState::Unknown => RecoveryAction::RequireDecision {
                            operation,
                            reason: "write fingerprints are unavailable".into(),
                        },
                    },
                }
            };
            planned.insert(operation_id, action);
        }

        let stream = if turn_terminal {
            StreamRecovery::None
        } else if let Some(cursor) = cursor {
            if capabilities.resumable
                && backend.is_none_or(|active_backend| cursor.backend == active_backend)
            {
                StreamRecovery::Continue(cursor)
            } else {
                StreamRecovery::RestartFromBoundary
            }
        } else if events
            .iter()
            .any(|event| matches!(event.event, SessionEvent::UserMessage { .. }))
        {
            StreamRecovery::RestartFromBoundary
        } else {
            StreamRecovery::None
        };

        let interrupted_agents = agents
            .into_values()
            .map(|mut snapshot| {
                snapshot.state = AgentState::Failed;
                snapshot.last_error = Some("interrupted during previous process".into());
                snapshot
            })
            .collect();

        Ok(RecoveryPlan {
            operations: planned,
            stream,
            interrupted_agents,
        })
    }
}

#[derive(Debug, Clone, Default)]
struct OperationState {
    operation: Option<Operation>,
    approval_summary: Option<String>,
    approval: Option<ApprovalResponse>,
    prepared: bool,
    started: bool,
    unknown: bool,
    completed: Option<ToolResult>,
}
