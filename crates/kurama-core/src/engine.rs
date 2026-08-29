use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use kurama_protocol::{
    KuramaError,
    agent::{AgentResult, OrchestrationContext, WriteScope},
    id::{AgentId, OperationId},
    model::{BackendCursor, FinishReason, ModelEvent, ModelItem, ModelProfile, ModelRequest},
    policy::{
        ApprovalRequest, ApprovalResponse, AutoBoundaries, ExecutionMode, PolicyContext,
        PolicyDecision,
    },
    runtime::{AgentCommand, EngineCommand, RuntimeEvent},
    session::{EventEnvelope, FileCheckpoint, SessionEvent, SessionMetadata},
    tool::{Operation, ToolContext, ToolInvocation, ToolLimits, ToolResult},
    traits::{
        ApprovalPolicy, EventSink, IdGenerator, ModelBackend, Orchestrator, SessionStore, Tool,
    },
};
use tokio::sync::mpsc;

use crate::{
    agent_manager::{AgentManager, ChildRunner},
    cancel::CancelToken,
    context::{ContextManager, ContextPolicy, estimate_text, normalize_compaction_json},
    recovery::{RecoveryAction, RecoveryPlanner, SessionRecoveryProbe, StreamRecovery},
};

pub type RuntimeEvents = mpsc::Receiver<RuntimeEvent>;

#[derive(Clone)]
pub struct EngineHandle {
    commands: mpsc::Sender<EngineCommand>,
}

impl EngineHandle {
    pub async fn submit(
        &self,
        text: impl Into<String>,
        explicit_delegation: bool,
    ) -> Result<(), KuramaError> {
        self.send(EngineCommand::SubmitTurn {
            text: text.into(),
            explicit_delegation,
        })
        .await
    }

    pub async fn resolve_approval(
        &self,
        operation_id: OperationId,
        response: ApprovalResponse,
    ) -> Result<(), KuramaError> {
        self.send(EngineCommand::ResolveApproval {
            operation_id,
            response,
        })
        .await
    }

    pub async fn cancel_turn(&self) -> Result<(), KuramaError> {
        self.send(EngineCommand::CancelTurn).await
    }

    pub async fn compact(&self) -> Result<(), KuramaError> {
        self.send(EngineCommand::Compact).await
    }

    pub async fn set_mode(&self, mode: ExecutionMode) -> Result<(), KuramaError> {
        self.send(EngineCommand::SetMode(mode)).await
    }

    pub async fn agent_command(&self, command: AgentCommand) -> Result<(), KuramaError> {
        self.send(EngineCommand::Agent(command)).await
    }

    pub async fn shutdown(&self) -> Result<(), KuramaError> {
        self.send(EngineCommand::Shutdown).await
    }

    async fn send(&self, command: EngineCommand) -> Result<(), KuramaError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| KuramaError::Cancelled)
    }
}

pub struct EngineOrchestration {
    pub context: OrchestrationContext,
    pub runner: Arc<dyn ChildRunner>,
}

pub struct EngineConfig {
    pub session: SessionMetadata,
    pub profile: ModelProfile,
    pub backend: Arc<dyn ModelBackend>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub policy: Arc<dyn ApprovalPolicy>,
    pub store: Arc<dyn SessionStore>,
    pub sink: Arc<dyn EventSink>,
    pub orchestrator: Arc<dyn Orchestrator>,
    pub ids: Arc<dyn IdGenerator>,
    pub context_policy: ContextPolicy,
    pub workspace_root: PathBuf,
    pub write_scope: WriteScope,
    pub auto: AutoBoundaries,
    pub agent_id: Option<AgentId>,
    pub orchestration: Option<EngineOrchestration>,
    pub provider_retry_delays_ms: Vec<u64>,
    pub command_capacity: usize,
    pub event_capacity: usize,
}

pub struct Engine;

impl Engine {
    pub fn spawn(
        config: EngineConfig,
        mut replay: Vec<EventEnvelope>,
    ) -> Result<(EngineHandle, RuntimeEvents), KuramaError> {
        let mut tools = BTreeMap::new();
        for tool in &config.tools {
            let name = tool.descriptor().name;
            if tools.insert(name.clone(), tool.clone()).is_some() {
                return Err(KuramaError::Configuration(format!("duplicate tool {name}")));
            }
        }

        let mut sequence = replay.last().map_or(0, |event| event.sequence + 1);
        if config.command_capacity == 0 || config.event_capacity == 0 {
            return Err(KuramaError::Configuration(
                "engine channel capacities must be non-zero".into(),
            ));
        }

        if replay.is_empty() && config.agent_id.is_none() {
            config.store.create(&config.session)?;
            let event = EventEnvelope::new(
                sequence,
                now_ms(),
                config.session.id.clone(),
                config.agent_id.clone(),
                SessionEvent::SessionStarted {
                    metadata: config.session.clone(),
                },
            );
            config.store.append(&event)?;
            sequence += 1;
            replay.push(event);
        }

        let recovery_probe = SessionRecoveryProbe::new(&replay, config.store.as_ref());
        let recovery = RecoveryPlanner::new().plan_for_backend(
            &replay,
            &recovery_probe,
            config.backend.backend_name(),
            config.backend.capabilities(),
        )?;
        let resume_incomplete_turn = !matches!(recovery.stream, StreamRecovery::None);
        let recovery_continuation = match recovery.stream {
            StreamRecovery::Continue(cursor) => Some(cursor),
            StreamRecovery::None | StreamRecovery::RestartFromBoundary => None,
        };

        let (command_tx, command_rx) = mpsc::channel(config.command_capacity);
        let (runtime_tx, runtime_rx) = mpsc::channel(config.event_capacity);
        let completed_tool_calls = replay
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::ToolCompleted {
                    operation_id,
                    result,
                } => Some((
                    result.call_id.clone(),
                    (operation_id.clone(), result.clone()),
                )),
                _ => None,
            })
            .collect();
        let mut context = ContextManager::new(config.context_policy);
        context.replay(replay);
        let agent_manager = config.orchestration.as_ref().map(|orchestration| {
            Arc::new(
                AgentManager::new(
                    config.session.id.clone(),
                    config.agent_id.clone(),
                    orchestration.context.max_concurrency,
                    config.store.clone(),
                    config.sink.clone(),
                )
                .with_profiles(orchestration.context.profiles.clone())
                .with_runtime_sender(runtime_tx.clone()),
            )
        });
        let actor = EngineActor {
            session_id: config.session.id,
            profile: config.profile,
            backend: config.backend,
            tools,
            tool_descriptors: config.tools.iter().map(|tool| tool.descriptor()).collect(),
            policy: config.policy,
            store: config.store,
            sink: config.sink,
            orchestrator: config.orchestrator,
            ids: config.ids,
            workspace_root: config.workspace_root,
            write_scope: config.write_scope,
            auto: config.auto,
            agent_id: config.agent_id,
            orchestration: config.orchestration,
            agent_manager,
            mode: config.session.mode,
            retry_delays: config
                .provider_retry_delays_ms
                .into_iter()
                .map(Duration::from_millis)
                .collect(),
            context,
            sequence,
            command_rx,
            runtime_tx,
            session_approvals: BTreeSet::new(),
            completed_tool_calls,
            recovery_continuation,
            recovery_operations: recovery.operations.into_iter().collect(),
            interrupted_agents: recovery.interrupted_agents,
            resume_incomplete_turn,
        };
        tokio::spawn(actor.run());
        Ok((
            EngineHandle {
                commands: command_tx,
            },
            runtime_rx,
        ))
    }
}

struct EngineActor {
    session_id: kurama_protocol::id::SessionId,
    profile: ModelProfile,
    backend: Arc<dyn ModelBackend>,
    tools: BTreeMap<String, Arc<dyn Tool>>,
    tool_descriptors: Vec<kurama_protocol::tool::ToolDescriptor>,
    policy: Arc<dyn ApprovalPolicy>,
    store: Arc<dyn SessionStore>,
    sink: Arc<dyn EventSink>,
    orchestrator: Arc<dyn Orchestrator>,
    ids: Arc<dyn IdGenerator>,
    workspace_root: PathBuf,
    write_scope: WriteScope,
    auto: AutoBoundaries,
    agent_id: Option<AgentId>,
    orchestration: Option<EngineOrchestration>,
    agent_manager: Option<Arc<AgentManager>>,
    mode: ExecutionMode,
    retry_delays: Vec<Duration>,
    context: ContextManager,
    sequence: u64,
    command_rx: mpsc::Receiver<EngineCommand>,
    runtime_tx: mpsc::Sender<RuntimeEvent>,
    session_approvals: BTreeSet<String>,
    completed_tool_calls: BTreeMap<kurama_protocol::id::CallId, (OperationId, ToolResult)>,
    recovery_continuation: Option<BackendCursor>,
    recovery_operations: Vec<(OperationId, RecoveryAction)>,
    interrupted_agents: Vec<kurama_protocol::agent::AgentSnapshot>,
    resume_incomplete_turn: bool,
}

impl EngineActor {
    async fn run(mut self) {
        let recovered = match self.recover_startup().await {
            Ok(()) => true,
            Err(error) => {
                let _ = self
                    .emit(RuntimeEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                false
            }
        };
        if recovered && self.resume_incomplete_turn {
            self.resume_incomplete_turn = false;
            if let Err(error) = self.resume_turn().await {
                let _ = self
                    .emit(RuntimeEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
            }
        }
        while let Some(command) = self.command_rx.recv().await {
            let result = match command {
                EngineCommand::SubmitTurn {
                    text,
                    explicit_delegation,
                } => self.run_turn(text, explicit_delegation).await,
                EngineCommand::ResolveApproval { .. } => {
                    self.emit(RuntimeEvent::Error {
                        message: "there is no pending approval".into(),
                    })
                    .await
                }
                EngineCommand::CancelTurn => {
                    self.emit(RuntimeEvent::Status {
                        message: "there is no active turn".into(),
                    })
                    .await
                }
                EngineCommand::Compact => self.compact_context().await,
                EngineCommand::SetMode(mode) => self.change_mode(mode).await,
                EngineCommand::Agent(command) => self.agent_command(command).await,
                EngineCommand::Shutdown => {
                    let _ = self.emit(RuntimeEvent::Shutdown).await;
                    break;
                }
            };
            if let Err(error) = result {
                let _ = self
                    .emit(RuntimeEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
            }
        }
    }

    async fn recover_startup(&mut self) -> Result<(), KuramaError> {
        for snapshot in std::mem::take(&mut self.interrupted_agents) {
            let error = snapshot
                .last_error
                .clone()
                .unwrap_or_else(|| "interrupted during previous process".into());
            self.append(SessionEvent::AgentFailed {
                snapshot: snapshot.clone(),
                error,
            })?;
            self.emit(RuntimeEvent::AgentUpdated { snapshot }).await?;
        }

        let cancel = CancelToken::new();
        for (operation_id, action) in std::mem::take(&mut self.recovery_operations) {
            match action {
                RecoveryAction::Completed { .. } => {}
                RecoveryAction::RecordCompleted { result, .. } => {
                    self.record_recovery_decision(&operation_id, "record_completed")?;
                    self.complete_tool(operation_id, result).await?;
                }
                RecoveryAction::DiscardDenied { tool } => {
                    self.record_recovery_decision(&operation_id, "discard_denied")?;
                    let result = error_result(
                        tool.call_id,
                        "operation was not resumed after recovery".into(),
                        tool.invocation.as_ref().map_or_else(
                            || operation_tool_name(&tool.operation),
                            |value| value.name.as_str(),
                        ),
                    );
                    self.complete_tool(operation_id, result).await?;
                }
                RecoveryAction::RetrySafe { tool, .. } => {
                    self.record_recovery_decision(&operation_id, "retry_safe")?;
                    self.execute_recovered_operation(
                        operation_id,
                        tool.operation,
                        tool.invocation,
                        tool.call_id,
                        &cancel,
                    )
                    .await?;
                }
                RecoveryAction::Reevaluate { tool } => {
                    self.record_recovery_decision(&operation_id, "reevaluate")?;
                    self.reevaluate_recovered_operation(
                        operation_id,
                        tool.operation,
                        tool.invocation,
                        tool.call_id,
                        &cancel,
                    )
                    .await?;
                }
                RecoveryAction::RestoreApproval { tool, summary } => {
                    self.record_recovery_decision(&operation_id, "restore_approval")?;
                    self.restore_recovered_approval(
                        operation_id,
                        tool.operation,
                        tool.invocation,
                        tool.call_id,
                        summary,
                        &cancel,
                    )
                    .await?;
                }
                RecoveryAction::RequireDecision {
                    tool,
                    reason,
                    pending,
                } => {
                    self.require_recovery_decision(
                        operation_id,
                        tool.operation,
                        tool.invocation,
                        tool.call_id,
                        reason,
                        pending,
                        &cancel,
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    async fn reevaluate_recovered_operation(
        &mut self,
        operation_id: OperationId,
        operation: Operation,
        invocation: Option<ToolInvocation>,
        call_id: kurama_protocol::id::CallId,
        cancel: &CancelToken,
    ) -> Result<ToolResult, KuramaError> {
        let Some(invocation) = invocation else {
            return self
                .complete_tool(
                    operation_id,
                    error_result(
                        call_id,
                        "durable tool invocation is unavailable".into(),
                        operation_tool_name(&operation),
                    ),
                )
                .await;
        };
        let policy_context = PolicyContext {
            mode: self.mode,
            workspace_root: self.workspace_root.clone(),
            write_scope: self.write_scope.clone(),
            auto: self.auto.clone(),
        };
        match self.policy.decide(&policy_context, &operation) {
            PolicyDecision::Allow => {
                self.execute_recovered_operation(
                    operation_id,
                    operation,
                    Some(invocation),
                    call_id,
                    cancel,
                )
                .await
            }
            PolicyDecision::Deny { reason } => {
                let tool_name = invocation.name.clone();
                self.complete_tool(operation_id, error_result(call_id, reason, &tool_name))
                    .await
            }
            PolicyDecision::Ask { reason } => {
                let summary = operation_summary(&operation);
                self.append(SessionEvent::ApprovalRequested {
                    operation_id: operation_id.clone(),
                    summary: summary.clone(),
                })?;
                self.emit(RuntimeEvent::ApprovalRequired {
                    request: ApprovalRequest {
                        operation_id: operation_id.clone(),
                        operation: operation.clone(),
                        summary: format!("{summary}: {reason}"),
                        arguments: invocation.arguments.clone(),
                    },
                })
                .await?;
                let response = self.await_approval(&operation_id, cancel).await?;
                self.append(SessionEvent::ApprovalResolved {
                    operation_id: operation_id.clone(),
                    response: response.clone(),
                })?;
                self.apply_recovered_approval(
                    operation_id,
                    operation,
                    invocation,
                    response,
                    true,
                    cancel,
                )
                .await
            }
        }
    }

    async fn restore_recovered_approval(
        &mut self,
        operation_id: OperationId,
        operation: Operation,
        invocation: Option<ToolInvocation>,
        call_id: kurama_protocol::id::CallId,
        summary: String,
        cancel: &CancelToken,
    ) -> Result<ToolResult, KuramaError> {
        let arguments = invocation
            .as_ref()
            .map_or_else(empty_arguments, |invocation| invocation.arguments.clone());
        self.emit(RuntimeEvent::ApprovalRequired {
            request: ApprovalRequest {
                operation_id: operation_id.clone(),
                operation: operation.clone(),
                summary,
                arguments,
            },
        })
        .await?;
        let response = self.await_approval(&operation_id, cancel).await?;
        self.append(SessionEvent::ApprovalResolved {
            operation_id: operation_id.clone(),
            response: response.clone(),
        })?;
        let Some(invocation) = invocation else {
            return self
                .complete_tool(
                    operation_id,
                    error_result(
                        call_id,
                        "durable tool invocation is unavailable".into(),
                        operation_tool_name(&operation),
                    ),
                )
                .await;
        };
        self.apply_recovered_approval(operation_id, operation, invocation, response, true, cancel)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn require_recovery_decision(
        &mut self,
        operation_id: OperationId,
        operation: Operation,
        invocation: Option<ToolInvocation>,
        call_id: kurama_protocol::id::CallId,
        reason: String,
        pending: bool,
        cancel: &CancelToken,
    ) -> Result<ToolResult, KuramaError> {
        let arguments = invocation
            .as_ref()
            .map_or_else(empty_arguments, |invocation| invocation.arguments.clone());
        if !pending {
            self.append(SessionEvent::ApprovalRequested {
                operation_id: operation_id.clone(),
                summary: reason.clone(),
            })?;
        }
        self.emit(RuntimeEvent::ApprovalRequired {
            request: ApprovalRequest {
                operation_id: operation_id.clone(),
                operation: operation.clone(),
                summary: format!("Retry interrupted operation? {reason}"),
                arguments,
            },
        })
        .await?;
        let response = self.await_approval(&operation_id, cancel).await?;
        self.append(SessionEvent::ApprovalResolved {
            operation_id: operation_id.clone(),
            response: response.clone(),
        })?;
        match response {
            ApprovalResponse::Deny => {
                self.record_recovery_decision(&operation_id, "skip")?;
                self.complete_tool(
                    operation_id,
                    error_result(
                        call_id,
                        "interrupted operation was not retried".into(),
                        invocation.as_ref().map_or_else(
                            || operation_tool_name(&operation),
                            |value| value.name.as_str(),
                        ),
                    ),
                )
                .await
            }
            ApprovalResponse::ApproveOnce | ApprovalResponse::ApproveSession => {
                self.record_recovery_decision(&operation_id, "retry_once")?;
                self.execute_recovered_operation(
                    operation_id,
                    operation,
                    invocation,
                    call_id,
                    cancel,
                )
                .await
            }
            ApprovalResponse::Edit { arguments } => {
                self.record_recovery_decision(&operation_id, "edit")?;
                let Some(mut invocation) = invocation else {
                    return self
                        .complete_tool(
                            operation_id,
                            error_result(
                                call_id,
                                "durable tool invocation is unavailable".into(),
                                operation_tool_name(&operation),
                            ),
                        )
                        .await;
                };
                let replaced = error_result(
                    call_id,
                    "operation replaced by edited arguments".into(),
                    &invocation.name,
                );
                self.append(SessionEvent::ToolCompleted {
                    operation_id,
                    result: replaced,
                })?;
                invocation.arguments = arguments;
                self.execute_tool(invocation, cancel).await
            }
        }
    }

    async fn apply_recovered_approval(
        &mut self,
        operation_id: OperationId,
        operation: Operation,
        mut invocation: ToolInvocation,
        response: ApprovalResponse,
        remember_session: bool,
        cancel: &CancelToken,
    ) -> Result<ToolResult, KuramaError> {
        match response {
            ApprovalResponse::Deny => {
                let tool_name = invocation.name.clone();
                self.complete_tool(
                    operation_id,
                    error_result(
                        invocation.call_id,
                        "operation denied by user".into(),
                        &tool_name,
                    ),
                )
                .await
            }
            ApprovalResponse::ApproveSession => {
                if remember_session {
                    let approval_key = serde_json::to_string(&operation)
                        .map_err(|error| KuramaError::Protocol(error.to_string()))?;
                    self.session_approvals.insert(approval_key);
                }
                let call_id = invocation.call_id.clone();
                self.execute_recovered_operation(
                    operation_id,
                    operation,
                    Some(invocation),
                    call_id,
                    cancel,
                )
                .await
            }
            ApprovalResponse::ApproveOnce => {
                let call_id = invocation.call_id.clone();
                self.execute_recovered_operation(
                    operation_id,
                    operation,
                    Some(invocation),
                    call_id,
                    cancel,
                )
                .await
            }
            ApprovalResponse::Edit { arguments } => {
                let result = error_result(
                    invocation.call_id.clone(),
                    "operation replaced by edited arguments".into(),
                    &invocation.name,
                );
                self.append(SessionEvent::ToolCompleted {
                    operation_id,
                    result,
                })?;
                invocation.arguments = arguments;
                self.execute_tool(invocation, cancel).await
            }
        }
    }

    async fn execute_recovered_operation(
        &mut self,
        operation_id: OperationId,
        operation: Operation,
        invocation: Option<ToolInvocation>,
        call_id: kurama_protocol::id::CallId,
        cancel: &CancelToken,
    ) -> Result<ToolResult, KuramaError> {
        let Some(invocation) = invocation else {
            return self
                .complete_tool(
                    operation_id,
                    error_result(
                        call_id,
                        "durable tool invocation is unavailable".into(),
                        operation_tool_name(&operation),
                    ),
                )
                .await;
        };
        let Some(tool) = self.tools.get(&invocation.name).cloned() else {
            let tool_name = invocation.name.clone();
            return self
                .complete_tool(
                    operation_id,
                    error_result(
                        invocation.call_id,
                        format!("unknown tool {tool_name}"),
                        &tool_name,
                    ),
                )
                .await;
        };
        let tool_context = self.tool_context();
        match tool.classify(&tool_context, &invocation) {
            Ok(current) if current == operation => {
                self.execute_authorized_tool(
                    tool,
                    tool_context,
                    operation_id,
                    operation,
                    invocation,
                    cancel,
                )
                .await
            }
            Ok(_) => {
                let tool_name = invocation.name.clone();
                self.complete_tool(
                    operation_id,
                    error_result(
                        invocation.call_id,
                        "recovered tool operation no longer matches its durable proposal".into(),
                        &tool_name,
                    ),
                )
                .await
            }
            Err(error) => {
                let tool_name = invocation.name.clone();
                self.complete_tool(
                    operation_id,
                    error_result(invocation.call_id, error.to_string(), &tool_name),
                )
                .await
            }
        }
    }

    fn record_recovery_decision(
        &mut self,
        operation_id: &OperationId,
        action: &str,
    ) -> Result<(), KuramaError> {
        self.append(SessionEvent::RecoveryDecision {
            operation_id: operation_id.clone(),
            action: action.into(),
        })?;
        Ok(())
    }

    async fn run_turn(
        &mut self,
        text: String,
        explicit_delegation: bool,
    ) -> Result<(), KuramaError> {
        self.append(SessionEvent::UserMessage { text })?;
        self.continue_turn(explicit_delegation).await
    }

    async fn resume_turn(&mut self) -> Result<(), KuramaError> {
        self.continue_turn(false).await
    }

    async fn continue_turn(&mut self, explicit_delegation: bool) -> Result<(), KuramaError> {
        let cancel = CancelToken::new();
        let capability_enabled = explicit_delegation
            || self
                .orchestrator
                .explicit_delegation(self.latest_user_text().unwrap_or_default().as_str());
        let outcome = self.drive_turn(capability_enabled, &cancel).await;

        match outcome {
            Ok(()) => {
                self.append(SessionEvent::TurnCompleted)?;
                self.emit(RuntimeEvent::TurnCompleted).await
            }
            Err(error) => {
                self.append(SessionEvent::TurnFailed {
                    error: error.to_string(),
                })?;
                Err(error)
            }
        }
    }

    async fn drive_turn(
        &mut self,
        capability_enabled: bool,
        cancel: &CancelToken,
    ) -> Result<(), KuramaError> {
        loop {
            let mut assembled = self.context.assemble(
                &self.profile,
                self.tool_descriptors.clone(),
                capability_enabled && self.agent_id.is_none(),
            )?;
            if assembled.request.continuation.is_none() {
                assembled.request.continuation = self.recovery_continuation.take();
            }
            let round = self.stream_round(assembled.request, cancel).await?;
            if !round.text.is_empty() {
                self.append(SessionEvent::AssistantMessage { text: round.text })?;
            }
            for invocation in round.tool_calls {
                self.execute_tool(invocation, cancel).await?;
            }
            for request in round.delegations {
                self.execute_delegation(request, capability_enabled, cancel)
                    .await?;
            }
            if !round.tool_calls_empty || !round.delegations_empty {
                continue;
            }
            match round.finish_reason.unwrap_or(FinishReason::Stop) {
                FinishReason::Stop => return Ok(()),
                FinishReason::ToolCalls => continue,
                FinishReason::Length => {
                    return Err(KuramaError::Model(
                        "model stopped at its output limit".into(),
                    ));
                }
                FinishReason::Cancelled => return Err(KuramaError::Cancelled),
            }
        }
    }

    async fn stream_round(
        &mut self,
        request: ModelRequest,
        cancel: &CancelToken,
    ) -> Result<ModelRound, KuramaError> {
        let mut attempt = 0;
        loop {
            let stream = self.open_stream(request.clone(), cancel).await;
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(error) if is_transient(&error) && attempt < self.retry_delays.len() => {
                    self.wait_retry(attempt, cancel).await?;
                    attempt += 1;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let mut round = ModelRound::default();
            let mut runtime_buffer = String::new();
            let mut progressed = false;
            loop {
                tokio::select! {
                    item = stream.next() => {
                        match item {
                            Some(Ok(event)) => {
                                progressed = true;
                                match event {
                                    ModelEvent::ResponseStarted { .. } => {}
                                    ModelEvent::TextDelta { text } => {
                                        round.text.push_str(&text);
                                        runtime_buffer.push_str(&text);
                                        self.flush_deltas(&mut runtime_buffer, false).await?;
                                    }
                                    ModelEvent::ToolCall { call_id, name, arguments } => {
                                        round.tool_calls.push(ToolInvocation { call_id, name, arguments });
                                    }
                                    ModelEvent::Delegation { request } => round.delegations.push(request),
                                    ModelEvent::Usage { usage } => {
                                        self.append(SessionEvent::ModelUsage { usage })?;
                                    }
                                    ModelEvent::ResponseCompleted { cursor, finish_reason } => {
                                        if let Some(cursor) = cursor {
                                            self.append(SessionEvent::ModelCursor { cursor })?;
                                        }
                                        round.finish_reason = Some(finish_reason);
                                    }
                                }
                            }
                            Some(Err(error)) if !progressed && is_transient(&error) && attempt < self.retry_delays.len() => {
                                self.wait_retry(attempt, cancel).await?;
                                attempt += 1;
                                break;
                            }
                            Some(Err(error)) => return Err(error),
                            None => {
                                self.flush_deltas(&mut runtime_buffer, true).await?;
                                round.tool_calls_empty = round.tool_calls.is_empty();
                                round.delegations_empty = round.delegations.is_empty();
                                return Ok(round);
                            }
                        }
                    }
                    command = self.command_rx.recv() => {
                        self.handle_turn_command(command, cancel).await?;
                    }
                }
            }
        }
    }

    async fn open_stream(
        &mut self,
        request: ModelRequest,
        cancel: &CancelToken,
    ) -> Result<kurama_protocol::traits::ModelStream, KuramaError> {
        let backend = self.backend.clone();
        let future = backend.stream(request, cancel);
        tokio::pin!(future);
        loop {
            tokio::select! {
                result = &mut future => return result,
                command = self.command_rx.recv() => self.handle_turn_command(command, cancel).await?,
            }
        }
    }

    async fn execute_tool(
        &mut self,
        invocation: ToolInvocation,
        cancel: &CancelToken,
    ) -> Result<ToolResult, KuramaError> {
        if let Some((operation_id, result)) =
            self.completed_tool_calls.get(&invocation.call_id).cloned()
        {
            self.emit(RuntimeEvent::ToolCompleted {
                operation_id,
                result: result.clone(),
            })
            .await?;
            return Ok(result);
        }
        let mut invocation = invocation;
        let Some(tool) = self.tools.get(&invocation.name).cloned() else {
            let operation_id = self.ids.operation_id();
            self.append(SessionEvent::ToolUnknown {
                operation_id: operation_id.clone(),
                reason: format!("unknown tool {}", invocation.name),
            })?;
            let result = error_result(
                invocation.call_id,
                format!("unknown tool {}", invocation.name),
                &invocation.name,
            );
            return self.complete_tool(operation_id, result).await;
        };
        loop {
            let operation_id = self.ids.operation_id();
            let tool_context = self.tool_context();
            let operation = match tool.classify(&tool_context, &invocation) {
                Ok(operation) => operation,
                Err(error) => {
                    self.append(SessionEvent::ToolUnknown {
                        operation_id: operation_id.clone(),
                        reason: error.to_string(),
                    })?;
                    let result =
                        error_result(invocation.call_id, error.to_string(), &invocation.name);
                    return self.complete_tool(operation_id, result).await;
                }
            };
            self.append(SessionEvent::ToolProposed {
                operation_id: operation_id.clone(),
                call_id: invocation.call_id.clone(),
                operation: operation.clone(),
            })?;
            self.append(SessionEvent::ToolInvocationRecorded {
                operation_id: operation_id.clone(),
                invocation: invocation.clone(),
            })?;

            let policy_context = PolicyContext {
                mode: self.mode,
                workspace_root: self.workspace_root.clone(),
                write_scope: self.write_scope.clone(),
                auto: self.auto.clone(),
            };
            let approval_key = serde_json::to_string(&operation)
                .map_err(|error| KuramaError::Protocol(error.to_string()))?;
            let decision = if self.session_approvals.contains(&approval_key) {
                PolicyDecision::Allow
            } else {
                self.policy.decide(&policy_context, &operation)
            };
            match decision {
                PolicyDecision::Deny { reason } => {
                    let result = error_result(invocation.call_id, reason, &invocation.name);
                    return self.complete_tool(operation_id, result).await;
                }
                PolicyDecision::Ask { reason } => {
                    let summary = operation_summary(&operation);
                    self.append(SessionEvent::ApprovalRequested {
                        operation_id: operation_id.clone(),
                        summary: summary.clone(),
                    })?;
                    self.emit(RuntimeEvent::ApprovalRequired {
                        request: ApprovalRequest {
                            operation_id: operation_id.clone(),
                            operation: operation.clone(),
                            summary: format!("{summary}: {reason}"),
                            arguments: invocation.arguments.clone(),
                        },
                    })
                    .await?;
                    let response = self.await_approval(&operation_id, cancel).await?;
                    self.append(SessionEvent::ApprovalResolved {
                        operation_id: operation_id.clone(),
                        response: response.clone(),
                    })?;
                    match response {
                        ApprovalResponse::Deny => {
                            let result = error_result(
                                invocation.call_id,
                                "operation denied by user".into(),
                                &invocation.name,
                            );
                            return self.complete_tool(operation_id, result).await;
                        }
                        ApprovalResponse::ApproveSession => {
                            self.session_approvals.insert(approval_key);
                        }
                        ApprovalResponse::ApproveOnce => {}
                        ApprovalResponse::Edit { arguments } => {
                            let result = error_result(
                                invocation.call_id.clone(),
                                "operation replaced by edited arguments".into(),
                                &invocation.name,
                            );
                            self.append(SessionEvent::ToolCompleted {
                                operation_id,
                                result,
                            })?;
                            invocation.arguments = arguments;
                            continue;
                        }
                    }
                }
                PolicyDecision::Allow => {}
            }

            return self
                .execute_authorized_tool(
                    tool,
                    tool_context,
                    operation_id,
                    operation,
                    invocation,
                    cancel,
                )
                .await;
        }
    }

    async fn execute_authorized_tool(
        &mut self,
        tool: Arc<dyn Tool>,
        tool_context: ToolContext,
        operation_id: OperationId,
        operation: Operation,
        invocation: ToolInvocation,
        cancel: &CancelToken,
    ) -> Result<ToolResult, KuramaError> {
        if let Operation::Write { paths, .. } = &operation {
            let files = self.checkpoint_files(paths)?;
            self.append(SessionEvent::WritePrepared {
                operation_id: operation_id.clone(),
                files,
            })?;
        }
        self.append(SessionEvent::ToolPrepared {
            operation_id: operation_id.clone(),
        })?;
        self.append(SessionEvent::ToolStarted {
            operation_id: operation_id.clone(),
        })?;
        self.emit(RuntimeEvent::ToolStarted {
            operation_id: operation_id.clone(),
            name: invocation.name.clone(),
        })
        .await?;
        let call_id = invocation.call_id.clone();
        let tool_name = invocation.name.clone();
        let execution = tool.execute(tool_context, invocation, cancel);
        tokio::pin!(execution);
        let mut result = loop {
            tokio::select! {
                result = &mut execution => {
                    break result.unwrap_or_else(|error| error_result(call_id.clone(), error.to_string(), &tool_name));
                }
                command = self.command_rx.recv() => self.handle_turn_command(command, cancel).await?,
            }
        };
        attach_tool_name(&mut result, &tool_name);
        if !result.is_error
            && let Operation::Write { paths, .. } = &operation
        {
            let files = self.checkpoint_files(paths)?;
            self.append(SessionEvent::WriteApplied {
                operation_id: operation_id.clone(),
                files,
                result: result.clone(),
            })?;
        }
        self.complete_tool(operation_id, result).await
    }

    fn checkpoint_files(&self, paths: &[PathBuf]) -> Result<Vec<FileCheckpoint>, KuramaError> {
        paths
            .iter()
            .map(|path| {
                let content = match std::fs::read(path) {
                    Ok(bytes) => Some(self.store.put_blob(&bytes)?),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error.into()),
                };
                Ok(FileCheckpoint {
                    path: path.clone(),
                    content,
                })
            })
            .collect()
    }

    async fn complete_tool(
        &mut self,
        operation_id: OperationId,
        result: ToolResult,
    ) -> Result<ToolResult, KuramaError> {
        self.append(SessionEvent::ToolCompleted {
            operation_id: operation_id.clone(),
            result: result.clone(),
        })?;
        self.completed_tool_calls.insert(
            result.call_id.clone(),
            (operation_id.clone(), result.clone()),
        );
        self.emit(RuntimeEvent::ToolCompleted {
            operation_id,
            result: result.clone(),
        })
        .await?;
        Ok(result)
    }

    async fn execute_delegation(
        &mut self,
        request: kurama_protocol::agent::DelegationRequest,
        capability_enabled: bool,
        cancel: &CancelToken,
    ) -> Result<Vec<AgentResult>, KuramaError> {
        if !capability_enabled || self.agent_id.is_some() {
            return Err(KuramaError::Protocol(
                "model emitted delegation without an enabled parent capability".into(),
            ));
        }
        let orchestration = self.orchestration.as_ref().ok_or_else(|| {
            KuramaError::Configuration("delegation runtime is not configured".into())
        })?;
        let manager =
            self.agent_manager.as_ref().cloned().ok_or_else(|| {
                KuramaError::Configuration("agent manager is not configured".into())
            })?;
        let plan = self.orchestrator.resolve(request, &orchestration.context)?;
        let execution = manager.execute(
            plan,
            self.context.project_summary(),
            orchestration.runner.clone(),
        );
        tokio::pin!(execution);
        let results = loop {
            tokio::select! {
                result = &mut execution => break result?,
                command = self.command_rx.recv() => {
                    match command {
                        Some(EngineCommand::Agent(command)) => manager.command(command).await?,
                        Some(EngineCommand::ResolveApproval {
                            operation_id,
                            response,
                        }) => {
                            manager.resolve_approval(&operation_id, response).await?
                        }
                        Some(EngineCommand::CancelTurn) => {
                            cancel.cancel();
                            manager.cancel_all().await;
                            return Err(KuramaError::Cancelled);
                        }
                        Some(EngineCommand::Shutdown) | None => {
                            cancel.cancel();
                            manager.cancel_all().await;
                            return Err(KuramaError::Cancelled);
                        }
                        Some(_) => self.emit(RuntimeEvent::Error { message: "command is unavailable while agents are running".into() }).await?,
                    }
                }
            }
        };
        let snapshots = manager.snapshots().await;
        for result in &results {
            if let Some(snapshot) = snapshots
                .iter()
                .find(|snapshot| snapshot.id == result.agent_id)
            {
                self.append(SessionEvent::AgentCompleted {
                    snapshot: snapshot.clone(),
                    summary: result.summary.clone(),
                })?;
            }
        }
        Ok(results)
    }

    async fn await_approval(
        &mut self,
        expected_operation_id: &OperationId,
        cancel: &CancelToken,
    ) -> Result<ApprovalResponse, KuramaError> {
        loop {
            match self.command_rx.recv().await {
                Some(EngineCommand::ResolveApproval {
                    operation_id,
                    response,
                }) if &operation_id == expected_operation_id => return Ok(response),
                Some(EngineCommand::ResolveApproval { .. }) => {
                    self.emit(RuntimeEvent::Error {
                        message: "approval request is no longer pending".into(),
                    })
                    .await?;
                }
                Some(EngineCommand::CancelTurn) => {
                    cancel.cancel();
                    return Err(KuramaError::Cancelled);
                }
                Some(EngineCommand::Agent(command)) => self.agent_command(command).await?,
                Some(EngineCommand::Shutdown) | None => {
                    cancel.cancel();
                    return Err(KuramaError::Cancelled);
                }
                Some(_) => {
                    self.emit(RuntimeEvent::Error {
                        message: "command is unavailable while approval is pending".into(),
                    })
                    .await?;
                }
            }
        }
    }

    async fn handle_turn_command(
        &mut self,
        command: Option<EngineCommand>,
        cancel: &CancelToken,
    ) -> Result<(), KuramaError> {
        match command {
            Some(EngineCommand::CancelTurn) | Some(EngineCommand::Shutdown) | None => {
                cancel.cancel();
                if let Some(manager) = &self.agent_manager {
                    manager.cancel_all().await;
                }
                Err(KuramaError::Cancelled)
            }
            Some(EngineCommand::Agent(command)) => self.agent_command(command).await,
            Some(EngineCommand::ResolveApproval { .. }) => {
                self.emit(RuntimeEvent::Error {
                    message: "there is no pending approval".into(),
                })
                .await
            }
            Some(_) => {
                self.emit(RuntimeEvent::Error {
                    message: "another command cannot start during an active turn".into(),
                })
                .await
            }
        }
    }

    async fn wait_retry(
        &mut self,
        attempt: usize,
        cancel: &CancelToken,
    ) -> Result<(), KuramaError> {
        let delay = tokio::time::sleep(self.retry_delays[attempt]);
        tokio::pin!(delay);
        loop {
            tokio::select! {
                () = &mut delay => return Ok(()),
                command = self.command_rx.recv() => self.handle_turn_command(command, cancel).await?,
            }
        }
    }

    async fn flush_deltas(
        &mut self,
        buffer: &mut String,
        flush_all: bool,
    ) -> Result<(), KuramaError> {
        while buffer.len() >= 4_096 || (flush_all && !buffer.is_empty()) {
            let requested = if flush_all {
                buffer.len().min(4_096)
            } else {
                4_096
            };
            let mut boundary = requested;
            while !buffer.is_char_boundary(boundary) {
                boundary -= 1;
            }
            let remainder = buffer.split_off(boundary);
            let chunk = std::mem::replace(buffer, remainder);
            self.emit(RuntimeEvent::AssistantDelta { text: chunk })
                .await?;
        }
        Ok(())
    }

    async fn compact_context(&mut self) -> Result<(), KuramaError> {
        let Some(compaction) = self.context.compaction_request() else {
            return self
                .emit(RuntimeEvent::Status {
                    message: "context does not need compaction".into(),
                })
                .await;
        };
        let input = serde_json::to_string(&compaction.events)
            .map_err(|error| KuramaError::Protocol(error.to_string()))?;
        let request = ModelRequest {
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            profile: self.profile.clone(),
            system: compaction.prompt,
            items: vec![ModelItem::User { text: input }],
            tools: Vec::new(),
            delegation: None,
            continuation: None,
        };
        let cancel = CancelToken::new();
        let round = self.stream_round(request, &cancel).await?;
        let summary = normalize_compaction_json(&round.text)?;
        let tokens = estimate_text(&summary);
        self.append(SessionEvent::ContextCompacted {
            covered_through_sequence: compaction.covered_through_sequence,
            summary,
            tokens,
        })?;
        self.emit(RuntimeEvent::Status {
            message: "context compacted".into(),
        })
        .await
    }

    async fn change_mode(&mut self, mode: ExecutionMode) -> Result<(), KuramaError> {
        if mode == ExecutionMode::Yolo && self.mode != ExecutionMode::Yolo {
            return Err(KuramaError::Policy(
                "YOLO can only be enabled by the launch flag".into(),
            ));
        }
        self.mode = mode;
        self.append(SessionEvent::ModeSelected { mode })?;
        self.emit(RuntimeEvent::Status {
            message: format!("mode set to {mode:?}"),
        })
        .await
    }

    async fn agent_command(&self, command: AgentCommand) -> Result<(), KuramaError> {
        let manager = self
            .agent_manager
            .as_ref()
            .ok_or_else(|| KuramaError::Configuration("agent manager is not configured".into()))?;
        manager.command(command).await
    }

    fn append(&mut self, event: SessionEvent) -> Result<EventEnvelope, KuramaError> {
        let envelope = EventEnvelope::new(
            self.sequence,
            now_ms(),
            self.session_id.clone(),
            self.agent_id.clone(),
            event,
        );
        self.store.append(&envelope)?;
        self.sequence += 1;
        self.context.record(envelope.clone());
        Ok(envelope)
    }

    async fn emit(&self, event: RuntimeEvent) -> Result<(), KuramaError> {
        self.sink.emit(event.clone())?;
        self.runtime_tx
            .send(event)
            .await
            .map_err(|_| KuramaError::Cancelled)
    }

    fn tool_context(&self) -> ToolContext {
        ToolContext {
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            cwd: self.workspace_root.clone(),
            workspace_root: self.workspace_root.clone(),
            mode: self.mode,
            limits: ToolLimits::default(),
            write_scope: self.write_scope.clone(),
        }
    }

    fn latest_user_text(&self) -> Option<String> {
        self.store
            .replay(&self.session_id)
            .ok()?
            .into_iter()
            .rev()
            .find_map(|event| {
                if let SessionEvent::UserMessage { text } = event.event {
                    Some(text)
                } else {
                    None
                }
            })
    }
}

fn empty_arguments() -> serde_json::Value {
    serde_json::Value::Object(Default::default())
}

#[derive(Default)]
struct ModelRound {
    text: String,
    tool_calls: Vec<ToolInvocation>,
    delegations: Vec<kurama_protocol::agent::DelegationRequest>,
    finish_reason: Option<FinishReason>,
    tool_calls_empty: bool,
    delegations_empty: bool,
}

fn error_result(
    call_id: kurama_protocol::id::CallId,
    message: String,
    tool_name: &str,
) -> ToolResult {
    ToolResult {
        call_id,
        output: message,
        is_error: true,
        metadata: serde_json::json!({"tool_name": tool_name}),
        truncated: false,
        blob_refs: Vec::new(),
    }
}

fn attach_tool_name(result: &mut ToolResult, tool_name: &str) {
    if !result.metadata.is_object() {
        result.metadata = serde_json::json!({});
    }
    if let Some(metadata) = result.metadata.as_object_mut() {
        metadata.insert("tool_name".into(), tool_name.into());
    }
}

fn operation_summary(operation: &Operation) -> String {
    match operation {
        Operation::Read { path, .. } => format!("read {}", path.display()),
        Operation::Write { paths, .. } => format!("write {} path(s)", paths.len()),
        Operation::Bash { command, .. } => format!("run {command}"),
        Operation::WebSearch { query, .. } => format!("search for {query}"),
        Operation::WebOpen { url, .. } => format!("open {url}"),
    }
}

fn operation_tool_name(operation: &Operation) -> &'static str {
    match operation {
        Operation::Read { .. } => "read",
        Operation::Write { .. } => "write",
        Operation::Bash { .. } => "bash",
        Operation::WebSearch { .. } => "web-search",
        Operation::WebOpen { .. } => "web-open",
    }
}

fn is_transient(error: &KuramaError) -> bool {
    let KuramaError::Model(message) = error else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    message.starts_with("transient:")
        || message.contains("rate limit")
        || message.contains("timeout")
        || message.contains("temporarily unavailable")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
