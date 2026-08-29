use std::{collections::BTreeMap, fs, io::ErrorKind, path::PathBuf};

use kurama_protocol::{
    KuramaError,
    agent::{AgentSnapshot, AgentState},
    id::{CallId, OperationId},
    model::{BackendCapabilities, BackendCursor},
    policy::ApprovalResponse,
    session::{EventEnvelope, FileCheckpoint, SessionEvent},
    tool::{Operation, ToolInvocation, ToolResult},
    traits::SessionStore,
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

pub struct SessionRecoveryProbe<'a> {
    store: &'a dyn SessionStore,
    checkpoints: BTreeMap<OperationId, WriteCheckpoints>,
}

impl<'a> SessionRecoveryProbe<'a> {
    pub fn new(events: &[EventEnvelope], store: &'a dyn SessionStore) -> Self {
        let mut checkpoints: BTreeMap<OperationId, WriteCheckpoints> = BTreeMap::new();
        for envelope in events {
            match &envelope.event {
                SessionEvent::WritePrepared {
                    operation_id,
                    files,
                } => checkpoints.entry(operation_id.clone()).or_default().before = files.clone(),
                SessionEvent::WriteApplied {
                    operation_id,
                    files,
                    ..
                } => {
                    checkpoints.entry(operation_id.clone()).or_default().after = Some(files.clone())
                }
                _ => {}
            }
        }
        Self { store, checkpoints }
    }

    fn matches(
        &self,
        paths: &[PathBuf],
        checkpoints: &[FileCheckpoint],
    ) -> Result<bool, KuramaError> {
        if paths.len() != checkpoints.len()
            || !paths.iter().all(|path| {
                checkpoints
                    .iter()
                    .any(|checkpoint| checkpoint.path == *path)
            })
        {
            return Ok(false);
        }
        for checkpoint in checkpoints {
            match (&checkpoint.content, fs::read(&checkpoint.path)) {
                (None, Err(error)) if error.kind() == ErrorKind::NotFound => {}
                (None, _) => return Ok(false),
                (Some(reference), Ok(current)) => {
                    if current != self.store.get_blob(reference)? {
                        return Ok(false);
                    }
                }
                (Some(_), Err(error)) if error.kind() == ErrorKind::NotFound => return Ok(false),
                (Some(_), Err(error)) => return Err(error.into()),
            }
        }
        Ok(true)
    }
}

impl RecoveryProbe for SessionRecoveryProbe<'_> {
    fn inspect_write(
        &self,
        operation_id: &OperationId,
        paths: &[PathBuf],
    ) -> Result<WriteRecoveryState, KuramaError> {
        let Some(checkpoints) = self.checkpoints.get(operation_id) else {
            return Ok(WriteRecoveryState::Unknown);
        };
        if let Some(after) = &checkpoints.after
            && self.matches(paths, after)?
        {
            return Ok(WriteRecoveryState::MatchesPostcondition);
        }
        if self.matches(paths, &checkpoints.before)? {
            return Ok(WriteRecoveryState::MatchesPrecondition);
        }
        Ok(WriteRecoveryState::Conflict {
            details: "current content matches neither durable write checkpoint".into(),
        })
    }
}

#[derive(Debug, Clone, Default)]
struct WriteCheckpoints {
    before: Vec<FileCheckpoint>,
    after: Option<Vec<FileCheckpoint>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecoverableTool {
    pub operation: Operation,
    pub invocation: Option<ToolInvocation>,
    pub call_id: CallId,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryAction {
    Reevaluate {
        tool: RecoverableTool,
    },
    RestoreApproval {
        tool: RecoverableTool,
        summary: String,
    },
    DiscardDenied {
        tool: RecoverableTool,
    },
    RetrySafe {
        tool: RecoverableTool,
        reason: String,
    },
    RecordCompleted {
        result: ToolResult,
        reason: String,
    },
    RequireDecision {
        tool: RecoverableTool,
        reason: String,
        pending: bool,
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
                    call_id,
                    operation,
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
                    if state
                        .call_id
                        .as_ref()
                        .is_some_and(|existing| existing != call_id)
                    {
                        return Err(KuramaError::Session(format!(
                            "operation {operation_id} has conflicting call ids"
                        )));
                    }
                    state.call_id = Some(call_id.clone());
                }
                SessionEvent::ToolInvocationRecorded {
                    operation_id,
                    invocation,
                } => {
                    let state = operations.entry(operation_id.clone()).or_default();
                    if state
                        .invocation
                        .as_ref()
                        .is_some_and(|existing| existing != invocation)
                    {
                        return Err(KuramaError::Session(format!(
                            "operation {operation_id} has conflicting invocations"
                        )));
                    }
                    state.invocation = Some(invocation.clone());
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
                SessionEvent::WriteApplied {
                    operation_id,
                    result,
                    ..
                } => {
                    let state = operations.entry(operation_id.clone()).or_default();
                    if state
                        .applied
                        .as_ref()
                        .is_some_and(|existing| existing != result)
                    {
                        return Err(KuramaError::Session(format!(
                            "operation {operation_id} has conflicting applied results"
                        )));
                    }
                    state.applied = Some(result.clone());
                }
                SessionEvent::RecoveryDecision {
                    operation_id,
                    action,
                } => {
                    operations
                        .entry(operation_id.clone())
                        .or_default()
                        .recovery_decision = Some(action.clone());
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
            let call_id = state.call_id.ok_or_else(|| {
                KuramaError::Session(format!("operation {operation_id} is missing its call id"))
            })?;
            if state
                .invocation
                .as_ref()
                .is_some_and(|invocation| invocation.call_id != call_id)
            {
                return Err(KuramaError::Session(format!(
                    "operation {operation_id} invocation has a conflicting call id"
                )));
            }
            let invocation = state.invocation;
            let tool = RecoverableTool {
                operation,
                invocation,
                call_id,
            };
            let action = if let Some(result) = state.completed {
                RecoveryAction::Completed { result }
            } else if state.recovery_decision.as_deref() == Some("skip")
                || state.approval == Some(ApprovalResponse::Deny)
            {
                RecoveryAction::DiscardDenied { tool }
            } else if !state.started && state.approval_summary.is_some() && state.approval.is_none()
            {
                RecoveryAction::RestoreApproval {
                    tool,
                    summary: state.approval_summary.unwrap_or_default(),
                }
            } else if state.started && state.approval_summary.is_some() && state.approval.is_none()
            {
                RecoveryAction::RequireDecision {
                    tool,
                    reason: state.approval_summary.unwrap_or_default(),
                    pending: true,
                }
            } else if !state.started && state.approval.is_some() {
                RecoveryAction::RetrySafe {
                    tool,
                    reason: "approved operation did not start".into(),
                }
            } else if !state.started {
                RecoveryAction::Reevaluate { tool }
            } else {
                match &tool.operation {
                    Operation::Read { .. }
                    | Operation::WebSearch { .. }
                    | Operation::WebOpen { .. } => RecoveryAction::RetrySafe {
                        tool,
                        reason: "idempotent operation was interrupted".into(),
                    },
                    Operation::Bash { .. } => RecoveryAction::RequireDecision {
                        tool,
                        reason: "interrupted Bash outcome is unknown".into(),
                        pending: false,
                    },
                    Operation::Write { paths, .. } => match probe
                        .inspect_write(&operation_id, paths)?
                    {
                        WriteRecoveryState::MatchesPostcondition => {
                            RecoveryAction::RecordCompleted {
                                result: state.applied.ok_or_else(|| {
                                    KuramaError::Session(format!(
                                        "operation {operation_id} is missing its applied result"
                                    ))
                                })?,
                                reason: "current content matches the durable postcondition".into(),
                            }
                        }
                        WriteRecoveryState::MatchesPrecondition => RecoveryAction::RetrySafe {
                            tool,
                            reason: "current content still matches the durable precondition".into(),
                        },
                        WriteRecoveryState::Conflict { details } => {
                            RecoveryAction::RequireDecision {
                                tool,
                                reason: details,
                                pending: false,
                            }
                        }
                        WriteRecoveryState::Unknown => RecoveryAction::RequireDecision {
                            tool,
                            reason: "write fingerprints are unavailable".into(),
                            pending: false,
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
    invocation: Option<ToolInvocation>,
    call_id: Option<CallId>,
    approval_summary: Option<String>,
    approval: Option<ApprovalResponse>,
    prepared: bool,
    started: bool,
    unknown: bool,
    completed: Option<ToolResult>,
    applied: Option<ToolResult>,
    recovery_decision: Option<String>,
}
