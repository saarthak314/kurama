use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use futures_util::{StreamExt, stream::FuturesUnordered};
use kurama_protocol::{
    KuramaError,
    agent::{
        AgentResult, AgentSnapshot, AgentState, ChildBrief, ChildLaunch, ResolvedAgentSpec,
        SchedulePlan,
    },
    id::{AgentId, OperationId, SessionId},
    model::Usage,
    policy::{ApprovalRequest, ApprovalResponse},
    runtime::{AgentCommand, RuntimeEvent},
    session::{EventEnvelope, SessionEvent},
    traits::{BoxFuture, EventSink, SessionStore},
};
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    time::Instant,
};

use crate::{cancel::CancelToken, orchestrator::scopes_overlap};

#[derive(Debug, Clone, Default)]
pub struct ChildProgress {
    pub phase: Option<String>,
    pub active_operation: Option<String>,
    pub changed_files: Vec<String>,
    pub last_error: Option<String>,
    pub transcript_line: Option<String>,
    pub usage: Usage,
    pub completed_turns: u32,
}

pub struct ChildRunContext {
    pub agent_id: AgentId,
    pub launch: ChildLaunch,
    pub cancel: CancelToken,
    pub messages: mpsc::Receiver<String>,
    pub progress: mpsc::Sender<ChildProgress>,
    pub approvals: mpsc::Sender<ChildApproval>,
}

pub struct ChildApproval {
    pub agent_id: AgentId,
    pub request: ApprovalRequest,
    pub response: oneshot::Sender<ApprovalResponse>,
}

pub trait ChildRunner: Send + Sync {
    fn run(&self, context: ChildRunContext)
    -> BoxFuture<'static, Result<AgentResult, KuramaError>>;
}

#[derive(Debug, Clone)]
pub struct AgentInspection {
    pub snapshot: AgentSnapshot,
    pub transcript: Vec<String>,
}

pub struct AgentManager {
    session_id: SessionId,
    parent_agent_id: Option<AgentId>,
    max_concurrency: usize,
    store: Arc<dyn SessionStore>,
    sink: Arc<dyn EventSink>,
    profiles: BTreeMap<String, kurama_protocol::model::ModelProfile>,
    runtime_tx: Option<mpsc::Sender<RuntimeEvent>>,
    state: Mutex<ManagerState>,
}

#[derive(Default)]
struct ManagerState {
    agents: BTreeMap<AgentId, ManagedAgent>,
    active_approval: Option<ChildApproval>,
    queued_approvals: VecDeque<ChildApproval>,
}

struct ManagedAgent {
    spec: ResolvedAgentSpec,
    snapshot: AgentSnapshot,
    transcript: Vec<String>,
    cancel: CancelToken,
    messages: Option<mpsc::Sender<String>>,
    pending_messages: Vec<String>,
    profile_attempts: u8,
    next_escalation: usize,
    execution_deadline: Option<Instant>,
}

impl AgentManager {
    pub fn new(
        session_id: SessionId,
        parent_agent_id: Option<AgentId>,
        max_concurrency: usize,
        store: Arc<dyn SessionStore>,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        Self {
            session_id,
            parent_agent_id,
            max_concurrency: max_concurrency.clamp(1, 8),
            store,
            sink,
            profiles: BTreeMap::new(),
            runtime_tx: None,
            state: Mutex::new(ManagerState::default()),
        }
    }

    pub fn with_runtime_sender(mut self, runtime_tx: mpsc::Sender<RuntimeEvent>) -> Self {
        self.runtime_tx = Some(runtime_tx);
        self
    }

    pub fn with_profiles(
        mut self,
        profiles: BTreeMap<String, kurama_protocol::model::ModelProfile>,
    ) -> Self {
        self.profiles = profiles;
        self
    }

    pub async fn execute(
        &self,
        plan: SchedulePlan,
        project_summary: String,
        runner: Arc<dyn ChildRunner>,
    ) -> Result<Vec<AgentResult>, KuramaError> {
        let specs: Vec<_> = plan
            .ready
            .into_iter()
            .chain(plan.queued)
            .chain(plan.blocked)
            .collect();
        self.register(&specs).await?;

        let (progress_tx, mut progress_rx) = mpsc::channel(64);
        let (approval_tx, mut approval_rx) = mpsc::channel(32);
        let mut running = FuturesUnordered::new();
        let mut results = Vec::new();

        loop {
            while let Ok((agent_id, progress)) = progress_rx.try_recv() {
                self.apply_progress(&agent_id, progress).await?;
            }
            self.launch_ready(
                &project_summary,
                runner.clone(),
                progress_tx.clone(),
                approval_tx.clone(),
                &mut running,
            )
            .await?;

            if self.all_terminal().await {
                break;
            }
            if running.is_empty() {
                self.fail_unrunnable().await?;
                if self.all_terminal().await {
                    break;
                }
            }

            tokio::select! {
                progress = progress_rx.recv() => {
                    if let Some((agent_id, progress)) = progress {
                        self.apply_progress(&agent_id, progress).await?;
                    }
                }
                approval = approval_rx.recv() => {
                    if let Some(approval) = approval {
                        self.queue_approval(approval).await?;
                    }
                }
                completed = running.next(), if !running.is_empty() => {
                    while let Ok((agent_id, progress)) = progress_rx.try_recv() {
                        self.apply_progress(&agent_id, progress).await?;
                    }
                    if let Some((agent_id, outcome)) = completed
                        && let Some(result) = self.finish(&agent_id, outcome).await?
                    {
                        results.push(result);
                    }
                }
            }
        }
        Ok(results)
    }

    pub async fn command(&self, command: AgentCommand) -> Result<(), KuramaError> {
        match command {
            AgentCommand::Inspect { agent_id } => {
                let inspection = self.inspect(&agent_id).await?;
                self.emit(RuntimeEvent::AgentInspection {
                    snapshot: inspection.snapshot,
                    transcript: inspection.transcript,
                })
                .await
            }
            AgentCommand::Message { agent_id, text } => self.message(&agent_id, text).await,
            AgentCommand::Cancel { agent_id } => self.cancel(&agent_id).await,
        }
    }

    pub async fn resolve_approval(
        &self,
        operation_id: &OperationId,
        response: ApprovalResponse,
    ) -> Result<(), KuramaError> {
        let next_request = {
            let mut state = self.state.lock().await;
            let approval = state
                .active_approval
                .take()
                .ok_or_else(|| KuramaError::Protocol("no child approval is pending".into()))?;
            if &approval.request.operation_id != operation_id {
                state.active_approval = Some(approval);
                return Err(KuramaError::Protocol(
                    "child approval request is no longer pending".into(),
                ));
            }
            let _ = approval.response.send(response);
            promote_approval(&mut state)
        };
        if let Some(request) = next_request {
            self.emit(RuntimeEvent::ApprovalRequired { request })
                .await?;
        }
        Ok(())
    }

    pub async fn inspect(&self, agent_id: &AgentId) -> Result<AgentInspection, KuramaError> {
        let state = self.state.lock().await;
        let agent = state
            .agents
            .get(agent_id)
            .ok_or_else(|| KuramaError::NotFound(agent_id.to_string()))?;
        Ok(AgentInspection {
            snapshot: agent.snapshot.clone(),
            transcript: agent.transcript.clone(),
        })
    }

    pub async fn snapshots(&self) -> Vec<AgentSnapshot> {
        self.state
            .lock()
            .await
            .agents
            .values()
            .map(|agent| agent.snapshot.clone())
            .collect()
    }

    pub async fn message(&self, agent_id: &AgentId, text: String) -> Result<(), KuramaError> {
        let sender = {
            let mut state = self.state.lock().await;
            let agent = state
                .agents
                .get_mut(agent_id)
                .ok_or_else(|| KuramaError::NotFound(agent_id.to_string()))?;
            agent.transcript.push(format!("parent: {text}"));
            if agent.messages.is_none() {
                agent.pending_messages.push(text.clone());
                self.append_agent_event(
                    agent,
                    SessionEvent::AgentMessage {
                        agent_id: agent_id.clone(),
                        text: text.clone(),
                    },
                )?;
            }
            agent.messages.clone()
        };
        if let Some(sender) = sender {
            sender
                .send(text)
                .await
                .map_err(|_| KuramaError::Cancelled)?;
        }
        Ok(())
    }

    pub async fn cancel(&self, agent_id: &AgentId) -> Result<(), KuramaError> {
        let (terminal_snapshot, next_request) = {
            let mut state = self.state.lock().await;
            let terminal_snapshot = {
                let agent = state
                    .agents
                    .get_mut(agent_id)
                    .ok_or_else(|| KuramaError::NotFound(agent_id.to_string()))?;
                agent.cancel.cancel();
                if agent.snapshot.state == AgentState::Queued {
                    agent.snapshot.state = AgentState::Cancelled;
                    let snapshot = agent.snapshot.clone();
                    self.append_agent_event(
                        agent,
                        SessionEvent::AgentCancelled {
                            snapshot: snapshot.clone(),
                        },
                    )?;
                    Some(snapshot)
                } else {
                    None
                }
            };
            let next_request = remove_agent_approvals(&mut state, agent_id);
            (terminal_snapshot, next_request)
        };
        if let Some(snapshot) = terminal_snapshot {
            self.emit(RuntimeEvent::AgentUpdated { snapshot }).await?;
        }
        if let Some(request) = next_request {
            self.emit(RuntimeEvent::ApprovalRequired { request })
                .await?;
        }
        Ok(())
    }

    pub async fn cancel_all(&self) {
        let state = self.state.lock().await;
        for agent in state.agents.values() {
            agent.cancel.cancel();
        }
    }

    async fn register(&self, specs: &[ResolvedAgentSpec]) -> Result<(), KuramaError> {
        for spec in specs {
            let snapshot = AgentSnapshot {
                id: spec.id.clone(),
                role: spec.role.clone(),
                objective: spec.objective.clone(),
                profile: spec.profile.name.clone(),
                state: AgentState::Queued,
                phase: None,
                active_operation: None,
                changed_files: Vec::new(),
                last_error: None,
            };
            {
                let mut state = self.state.lock().await;
                if state.agents.contains_key(&spec.id) {
                    return Err(KuramaError::Protocol(format!(
                        "duplicate agent id {}",
                        spec.id
                    )));
                }
                let agent = ManagedAgent {
                    spec: spec.clone(),
                    snapshot: snapshot.clone(),
                    transcript: Vec::new(),
                    cancel: CancelToken::new(),
                    messages: None,
                    pending_messages: Vec::new(),
                    profile_attempts: 0,
                    next_escalation: 0,
                    execution_deadline: None,
                };
                self.append_agent_event(
                    &agent,
                    SessionEvent::AgentQueued {
                        snapshot: snapshot.clone(),
                    },
                )?;
                state.agents.insert(spec.id.clone(), agent);
            }
            self.emit(RuntimeEvent::AgentUpdated { snapshot }).await?;
        }
        Ok(())
    }

    async fn launch_ready(
        &self,
        project_summary: &str,
        runner: Arc<dyn ChildRunner>,
        progress_tx: mpsc::Sender<(AgentId, ChildProgress)>,
        approval_tx: mpsc::Sender<ChildApproval>,
        running: &mut FuturesUnordered<
            BoxFuture<'static, (AgentId, Result<AgentResult, KuramaError>)>,
        >,
    ) -> Result<(), KuramaError> {
        loop {
            let launch = {
                let mut state = self.state.lock().await;
                let active: Vec<_> = state
                    .agents
                    .values()
                    .filter(|agent| agent.snapshot.state == AgentState::Running)
                    .map(|agent| agent.spec.write_scope.clone())
                    .collect();
                if active.len() >= self.max_concurrency {
                    return Ok(());
                }
                let completed: Vec<_> = state
                    .agents
                    .values()
                    .filter(|agent| agent.snapshot.state == AgentState::Completed)
                    .flat_map(agent_dependency_keys)
                    .collect();
                let candidate_id = state
                    .agents
                    .values()
                    .find(|agent| {
                        agent.snapshot.state == AgentState::Queued
                            && agent
                                .spec
                                .depends_on
                                .iter()
                                .all(|dependency| completed.contains(dependency))
                            && active
                                .iter()
                                .all(|scope| !scopes_overlap(scope, &agent.spec.write_scope))
                    })
                    .map(|agent| agent.spec.id.clone());
                let Some(candidate_id) = candidate_id else {
                    return Ok(());
                };
                let agent = state.agents.get_mut(&candidate_id).expect("candidate");
                let (message_tx, message_rx) = mpsc::channel(16);
                for message in agent.pending_messages.drain(..) {
                    message_tx.try_send(message).map_err(|_| {
                        KuramaError::Protocol("too many queued child messages".into())
                    })?;
                }
                agent.messages = Some(message_tx);
                agent.profile_attempts = agent.profile_attempts.saturating_add(1);
                let execution_deadline = *agent.execution_deadline.get_or_insert_with(|| {
                    Instant::now() + Duration::from_secs(agent.spec.budget.max_seconds)
                });
                agent.snapshot.state = AgentState::Running;
                agent.snapshot.phase = Some("starting".into());
                let snapshot = agent.snapshot.clone();
                self.append_agent_event(
                    agent,
                    SessionEvent::AgentStarted {
                        snapshot: snapshot.clone(),
                    },
                )?;
                (
                    agent.spec.clone(),
                    agent.cancel.clone(),
                    message_rx,
                    snapshot,
                    execution_deadline,
                )
            };
            self.emit(RuntimeEvent::AgentUpdated {
                snapshot: launch.3.clone(),
            })
            .await?;

            let (spec, cancel, messages, _, execution_deadline) = launch;
            let (child_progress_tx, mut child_progress_rx) = mpsc::channel(32);
            let forwarding = progress_tx.clone();
            let forwarding_id = spec.id.clone();
            let forward = async move {
                while let Some(progress) = child_progress_rx.recv().await {
                    if forwarding
                        .send((forwarding_id.clone(), progress))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            };
            let forward_task = tokio::spawn(forward);
            let context = ChildRunContext {
                agent_id: spec.id.clone(),
                launch: ChildLaunch {
                    parent_session_id: self.session_id.clone(),
                    parent_agent_id: self.parent_agent_id.clone(),
                    depth: 1,
                    brief: ChildBrief {
                        role: spec.role.clone(),
                        objective: spec.objective.clone(),
                        project_summary: project_summary.into(),
                        context_refs: spec.context_refs.clone(),
                    },
                    profile: spec.profile.clone(),
                    budget: spec.budget.clone(),
                    write_scope: spec.write_scope.clone(),
                    delegation_enabled: false,
                },
                cancel: cancel.clone(),
                messages,
                progress: child_progress_tx,
                approvals: approval_tx.clone(),
            };
            let agent_id = spec.id.clone();
            let future = runner.run(context);
            running.push(Box::pin(async move {
                let result = tokio::time::timeout_at(execution_deadline, future)
                    .await
                    .unwrap_or(Err(KuramaError::Cancelled));
                let _ = forward_task.await;
                (agent_id, result)
            }));
        }
    }

    async fn apply_progress(
        &self,
        agent_id: &AgentId,
        progress: ChildProgress,
    ) -> Result<(), KuramaError> {
        let snapshot = {
            let mut state = self.state.lock().await;
            let agent = state
                .agents
                .get_mut(agent_id)
                .ok_or_else(|| KuramaError::NotFound(agent_id.to_string()))?;
            if let Some(line) = progress.transcript_line {
                agent.transcript.push(line);
            }
            agent.snapshot.phase = progress.phase;
            agent.snapshot.active_operation = progress.active_operation;
            agent.snapshot.changed_files = progress.changed_files;
            agent.snapshot.last_error = progress.last_error;
            if progress.usage.input_tokens > agent.spec.budget.max_input_tokens
                || progress.usage.output_tokens > agent.spec.budget.max_output_tokens
                || progress.completed_turns > agent.spec.budget.max_turns
            {
                agent.cancel.cancel();
                agent.snapshot.last_error = Some("child budget exhausted".into());
            }
            agent.snapshot.clone()
        };
        self.emit(RuntimeEvent::AgentUpdated { snapshot }).await
    }

    async fn queue_approval(&self, approval: ChildApproval) -> Result<(), KuramaError> {
        let request = {
            let mut state = self.state.lock().await;
            let agent = state
                .agents
                .get(&approval.agent_id)
                .ok_or_else(|| KuramaError::NotFound(approval.agent_id.to_string()))?;
            if agent.snapshot.state != AgentState::Running || approval.response.is_closed() {
                return Ok(());
            }
            state.queued_approvals.push_back(approval);
            if state
                .active_approval
                .as_ref()
                .is_some_and(|approval| approval_is_live(&state.agents, approval))
            {
                None
            } else {
                state.active_approval.take();
                promote_approval(&mut state)
            }
        };
        if let Some(request) = request {
            self.emit(RuntimeEvent::ApprovalRequired { request })
                .await?;
        }
        Ok(())
    }

    async fn finish(
        &self,
        agent_id: &AgentId,
        outcome: Result<AgentResult, KuramaError>,
    ) -> Result<Option<AgentResult>, KuramaError> {
        let (snapshot, result, next_request) = {
            let mut state = self.state.lock().await;
            let next_request = remove_agent_approvals(&mut state, agent_id);
            let agent = state
                .agents
                .get_mut(agent_id)
                .ok_or_else(|| KuramaError::NotFound(agent_id.to_string()))?;
            agent.messages = None;
            if agent.cancel.is_cancelled() {
                agent.snapshot.state = AgentState::Cancelled;
                let snapshot = agent.snapshot.clone();
                self.append_agent_event(
                    agent,
                    SessionEvent::AgentCancelled {
                        snapshot: snapshot.clone(),
                    },
                )?;
                (snapshot, None, next_request)
            } else {
                match outcome {
                    Ok(result) => {
                        agent.snapshot.state = AgentState::Completed;
                        agent.snapshot.changed_files = result.changed_files.clone();
                        let snapshot = agent.snapshot.clone();
                        self.append_agent_event(
                            agent,
                            SessionEvent::AgentCompleted {
                                snapshot: snapshot.clone(),
                                summary: result.summary.clone(),
                            },
                        )?;
                        (snapshot, Some(result), next_request)
                    }
                    Err(error @ KuramaError::Model(_)) if agent.profile_attempts < 2 => {
                        agent.snapshot.state = AgentState::Queued;
                        agent.snapshot.phase = Some("retrying".into());
                        agent.snapshot.last_error = Some(error.to_string());
                        let snapshot = agent.snapshot.clone();
                        self.append_agent_event(
                            agent,
                            SessionEvent::AgentProgress {
                                snapshot: snapshot.clone(),
                            },
                        )?;
                        (snapshot, None, next_request)
                    }
                    Err(error @ KuramaError::Model(_))
                        if agent.next_escalation < agent.spec.escalation_profiles.len() =>
                    {
                        let profile_name = &agent.spec.escalation_profiles[agent.next_escalation];
                        let profile =
                            self.profiles.get(profile_name).cloned().ok_or_else(|| {
                                KuramaError::Configuration(format!(
                                    "unknown escalation profile {profile_name}"
                                ))
                            })?;
                        agent.next_escalation += 1;
                        agent.profile_attempts = 0;
                        agent.spec.profile = profile;
                        agent.snapshot.profile = profile_name.clone();
                        agent.snapshot.state = AgentState::Queued;
                        agent.snapshot.phase = Some("escalating".into());
                        agent.snapshot.last_error = Some(error.to_string());
                        let snapshot = agent.snapshot.clone();
                        self.append_agent_event(
                            agent,
                            SessionEvent::AgentProgress {
                                snapshot: snapshot.clone(),
                            },
                        )?;
                        (snapshot, None, next_request)
                    }
                    Err(error) => {
                        agent.snapshot.state = AgentState::Failed;
                        agent.snapshot.last_error = Some(error.to_string());
                        let snapshot = agent.snapshot.clone();
                        self.append_agent_event(
                            agent,
                            SessionEvent::AgentFailed {
                                snapshot: snapshot.clone(),
                                error: error.to_string(),
                            },
                        )?;
                        (snapshot, None, next_request)
                    }
                }
            }
        };
        self.emit(RuntimeEvent::AgentUpdated { snapshot }).await?;
        if let Some(request) = next_request {
            self.emit(RuntimeEvent::ApprovalRequired { request })
                .await?;
        }
        Ok(result)
    }

    async fn fail_unrunnable(&self) -> Result<(), KuramaError> {
        let failed_keys: Vec<_> = self
            .state
            .lock()
            .await
            .agents
            .values()
            .filter(|agent| {
                matches!(
                    agent.snapshot.state,
                    AgentState::Failed | AgentState::Cancelled
                )
            })
            .flat_map(agent_dependency_keys)
            .collect();
        let snapshots = {
            let mut state = self.state.lock().await;
            let mut snapshots = Vec::new();
            for agent in state.agents.values_mut().filter(|agent| {
                agent.snapshot.state == AgentState::Queued
                    && agent
                        .spec
                        .depends_on
                        .iter()
                        .any(|dependency| failed_keys.contains(dependency))
            }) {
                agent.snapshot.state = AgentState::Failed;
                agent.snapshot.last_error = Some("dependency did not complete".into());
                let snapshot = agent.snapshot.clone();
                self.append_agent_event(
                    agent,
                    SessionEvent::AgentFailed {
                        snapshot: snapshot.clone(),
                        error: "dependency did not complete".into(),
                    },
                )?;
                snapshots.push(snapshot);
            }
            snapshots
        };
        for snapshot in snapshots {
            self.emit(RuntimeEvent::AgentUpdated { snapshot }).await?;
        }
        Ok(())
    }

    async fn all_terminal(&self) -> bool {
        self.state.lock().await.agents.values().all(|agent| {
            matches!(
                agent.snapshot.state,
                AgentState::Completed | AgentState::Failed | AgentState::Cancelled
            )
        })
    }

    fn append_agent_event(
        &self,
        agent: &ManagedAgent,
        event: SessionEvent,
    ) -> Result<(), KuramaError> {
        let next_sequence = self
            .store
            .replay_agent(&self.session_id, &agent.spec.id)?
            .last()
            .map_or(0, |event| event.sequence + 1);
        let envelope = EventEnvelope::new(
            next_sequence,
            0,
            self.session_id.clone(),
            Some(agent.spec.id.clone()),
            event,
        );
        self.store.append(&envelope)?;
        Ok(())
    }

    async fn emit(&self, event: RuntimeEvent) -> Result<(), KuramaError> {
        self.sink.emit(event.clone())?;
        if let Some(sender) = &self.runtime_tx {
            sender
                .send(event)
                .await
                .map_err(|_| KuramaError::Cancelled)?;
        }
        Ok(())
    }
}

fn promote_approval(state: &mut ManagerState) -> Option<ApprovalRequest> {
    while let Some(approval) = state.queued_approvals.pop_front() {
        if !approval_is_live(&state.agents, &approval) {
            continue;
        }
        let request = approval.request.clone();
        state.active_approval = Some(approval);
        return Some(request);
    }
    None
}

fn remove_agent_approvals(state: &mut ManagerState, agent_id: &AgentId) -> Option<ApprovalRequest> {
    let remove_active = state.active_approval.as_ref().is_some_and(|approval| {
        &approval.agent_id == agent_id || !approval_is_live(&state.agents, approval)
    });
    if remove_active {
        state.active_approval.take();
    }
    let agents = &state.agents;
    state
        .queued_approvals
        .retain(|approval| &approval.agent_id != agent_id && approval_is_live(agents, approval));
    if state.active_approval.is_none() {
        promote_approval(state)
    } else {
        None
    }
}

fn approval_is_live(agents: &BTreeMap<AgentId, ManagedAgent>, approval: &ChildApproval) -> bool {
    !approval.response.is_closed()
        && agents.get(&approval.agent_id).is_some_and(|agent| {
            agent.snapshot.state == AgentState::Running && !agent.cancel.is_cancelled()
        })
}

fn agent_dependency_keys(agent: &ManagedAgent) -> [String; 2] {
    [
        agent.spec.role.clone(),
        format!("{}:{}", agent.spec.role, agent.spec.objective),
    ]
}
