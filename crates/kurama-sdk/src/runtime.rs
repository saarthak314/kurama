use std::{collections::BTreeMap, fmt, sync::Arc};

use kurama_core::{
    agent_manager::{ChildProgress, ChildRunContext, ChildRunner},
    engine::{EngineConfig, EngineOrchestration},
};
use kurama_protocol::{
    KuramaError,
    agent::{AgentResult, OrchestrationContext, WriteScope},
    model::ModelProfile,
    policy::AutoBoundaries,
    runtime::RuntimeEvent,
    session::{EventEnvelope, SessionEvent, SessionMetadata},
    traits::{
        ApprovalPolicy, EventSink, IdGenerator, ModelBackend, Orchestrator, SessionStore, Tool,
    },
};

pub struct RuntimeParts {
    pub profiles: BTreeMap<String, (ModelProfile, Arc<dyn ModelBackend>)>,
    pub active_profile: String,
    pub tools: BTreeMap<String, Arc<dyn Tool>>,
    pub policy: Arc<dyn ApprovalPolicy>,
    pub store: Arc<dyn SessionStore>,
    pub sink: Arc<dyn EventSink>,
    pub orchestrator: Arc<dyn Orchestrator>,
    pub ids: Arc<dyn IdGenerator>,
    pub command_capacity: usize,
    pub event_capacity: usize,
    pub write_scope: Option<WriteScope>,
    pub auto: AutoBoundaries,
    pub orchestration: Option<OrchestrationContext>,
    pub provider_retry_delays_ms: Vec<u64>,
}

pub struct AgentRuntime {
    parts: RuntimeParts,
}

impl AgentRuntime {
    pub(crate) fn new(parts: RuntimeParts) -> Self {
        Self { parts }
    }

    pub fn active_profile(&self) -> &str {
        &self.parts.active_profile
    }

    pub fn registered_tools(&self) -> Vec<&str> {
        self.parts.tools.keys().map(String::as_str).collect()
    }

    pub fn parts(&self) -> &RuntimeParts {
        &self.parts
    }

    pub fn into_parts(self) -> RuntimeParts {
        self.parts
    }

    pub fn start(
        &self,
        session: SessionMetadata,
        replay: Vec<EventEnvelope>,
    ) -> Result<
        (
            kurama_core::engine::EngineHandle,
            kurama_core::engine::RuntimeEvents,
        ),
        KuramaError,
    > {
        let (profile, backend) = self
            .parts
            .profiles
            .get(&self.parts.active_profile)
            .ok_or_else(|| KuramaError::Configuration("active profile disappeared".into()))?;
        let workspace_root = std::path::PathBuf::from(&session.project_root);
        let write_scope = self
            .parts
            .write_scope
            .clone()
            .unwrap_or_else(|| WriteScope {
                roots: vec![workspace_root.clone()],
                files: Vec::new(),
            });
        let orchestration = self.parts.orchestration.clone().map(|context| {
            let runner = Arc::new(RuntimeChildRunner {
                profiles: self.parts.profiles.clone(),
                tools: self.parts.tools.values().cloned().collect(),
                policy: self.parts.policy.clone(),
                store: self.parts.store.clone(),
                sink: self.parts.sink.clone(),
                orchestrator: self.parts.orchestrator.clone(),
                ids: self.parts.ids.clone(),
                session: session.clone(),
                workspace_root: workspace_root.clone(),
                auto: self.parts.auto.clone(),
                provider_retry_delays_ms: self.parts.provider_retry_delays_ms.clone(),
                command_capacity: self.parts.command_capacity,
                event_capacity: self.parts.event_capacity,
            });
            EngineOrchestration { context, runner }
        });
        kurama_core::engine::Engine::spawn(
            EngineConfig {
                session,
                profile: profile.clone(),
                backend: backend.clone(),
                tools: self.parts.tools.values().cloned().collect(),
                policy: self.parts.policy.clone(),
                store: self.parts.store.clone(),
                sink: self.parts.sink.clone(),
                orchestrator: self.parts.orchestrator.clone(),
                ids: self.parts.ids.clone(),
                context_policy: kurama_core::context::ContextPolicy {
                    max_input_tokens: profile.max_input_tokens,
                    reserve_output_tokens: profile.max_output_tokens,
                    ..kurama_core::context::ContextPolicy::default()
                },
                workspace_root: workspace_root.clone(),
                write_scope,
                auto: self.parts.auto.clone(),
                agent_id: None,
                orchestration,
                provider_retry_delays_ms: self.parts.provider_retry_delays_ms.clone(),
                command_capacity: self.parts.command_capacity,
                event_capacity: self.parts.event_capacity,
            },
            replay,
        )
    }
}

struct RuntimeChildRunner {
    profiles: BTreeMap<String, (ModelProfile, Arc<dyn ModelBackend>)>,
    tools: Vec<Arc<dyn Tool>>,
    policy: Arc<dyn ApprovalPolicy>,
    store: Arc<dyn SessionStore>,
    sink: Arc<dyn EventSink>,
    orchestrator: Arc<dyn Orchestrator>,
    ids: Arc<dyn IdGenerator>,
    session: SessionMetadata,
    workspace_root: std::path::PathBuf,
    auto: AutoBoundaries,
    provider_retry_delays_ms: Vec<u64>,
    command_capacity: usize,
    event_capacity: usize,
}

impl ChildRunner for RuntimeChildRunner {
    fn run(
        &self,
        mut context: ChildRunContext,
    ) -> kurama_protocol::traits::BoxFuture<'static, Result<AgentResult, KuramaError>> {
        let profiles = self.profiles.clone();
        let tools = self.tools.clone();
        let policy = self.policy.clone();
        let store = self.store.clone();
        let sink = self.sink.clone();
        let orchestrator = self.orchestrator.clone();
        let ids = self.ids.clone();
        let session = self.session.clone();
        let workspace_root = self.workspace_root.clone();
        let auto = self.auto.clone();
        let provider_retry_delays_ms = self.provider_retry_delays_ms.clone();
        let command_capacity = self.command_capacity;
        let event_capacity = self.event_capacity;

        Box::pin(async move {
            let (_, backend) = profiles.get(&context.launch.profile.name).ok_or_else(|| {
                KuramaError::Configuration(format!(
                    "unknown child profile {}",
                    context.launch.profile.name
                ))
            })?;
            let replay = store.replay_agent(&session.id, &context.agent_id)?;
            let (handle, mut events) = kurama_core::engine::Engine::spawn(
                EngineConfig {
                    session: session.clone(),
                    profile: context.launch.profile.clone(),
                    backend: backend.clone(),
                    tools,
                    policy,
                    store: store.clone(),
                    sink,
                    orchestrator,
                    ids,
                    context_policy: kurama_core::context::ContextPolicy {
                        max_input_tokens: context.launch.profile.max_input_tokens,
                        reserve_output_tokens: context.launch.profile.max_output_tokens,
                        ..kurama_core::context::ContextPolicy::default()
                    },
                    workspace_root,
                    write_scope: context.launch.write_scope.clone(),
                    auto,
                    agent_id: Some(context.agent_id.clone()),
                    orchestration: None,
                    provider_retry_delays_ms,
                    command_capacity,
                    event_capacity,
                },
                replay,
            )?;
            handle.submit(child_prompt(&context), false).await?;

            let mut summary = String::new();
            let mut pending_messages = Vec::new();
            let mut messages_open = true;
            let mut completed_turns = 0;
            loop {
                tokio::select! {
                    () = context.cancel.cancelled() => {
                        let _ = handle.cancel_turn().await;
                        return Err(KuramaError::Cancelled);
                    }
                    message = context.messages.recv(), if messages_open => {
                        match message {
                            Some(message) => pending_messages.push(message),
                            None => messages_open = false,
                        }
                    }
                    event = events.recv() => {
                        match event.ok_or(KuramaError::Cancelled)? {
                            RuntimeEvent::AssistantDelta { text } => {
                                summary.push_str(&text);
                                let _ = context.progress.send(ChildProgress {
                                    phase: Some("responding".into()),
                                    transcript_line: Some(text),
                                    completed_turns,
                                    ..ChildProgress::default()
                                }).await;
                            }
                            RuntimeEvent::ToolStarted { name, .. } => {
                                let _ = context.progress.send(ChildProgress {
                                    phase: Some("working".into()),
                                    active_operation: Some(name),
                                    completed_turns,
                                    ..ChildProgress::default()
                                }).await;
                            }
                            RuntimeEvent::ToolCompleted { .. } => {
                                let _ = context.progress.send(ChildProgress {
                                    phase: Some("working".into()),
                                    completed_turns,
                                    ..ChildProgress::default()
                                }).await;
                            }
                            RuntimeEvent::TurnCompleted => {
                                completed_turns += 1;
                                if pending_messages.is_empty() {
                                    let _ = handle.shutdown().await;
                                    let (changed_files, evidence_refs) = child_artifacts(
                                        store.as_ref(),
                                        &session.id,
                                        &context.agent_id,
                                    )?;
                                    return Ok(AgentResult {
                                        agent_id: context.agent_id,
                                        summary: if summary.trim().is_empty() {
                                            "completed without a textual summary".into()
                                        } else {
                                            summary
                                        },
                                        changed_files,
                                        evidence_refs,
                                    });
                                }
                                let message = pending_messages.join("\n");
                                pending_messages.clear();
                                handle.submit(message, false).await?;
                            }
                            RuntimeEvent::Error { message } => {
                                return Err(KuramaError::Model(message));
                            }
                            RuntimeEvent::Shutdown => return Err(KuramaError::Cancelled),
                            RuntimeEvent::ApprovalRequired { .. } => {
                                handle
                                    .resolve_approval(kurama_protocol::policy::ApprovalResponse::Deny)
                                    .await?;
                            }
                            RuntimeEvent::Status { .. }
                            | RuntimeEvent::ToolOutputDelta { .. }
                            | RuntimeEvent::AgentUpdated { .. }
                            | RuntimeEvent::AgentInspection { .. } => {}
                        }
                    }
                }
            }
        })
    }
}

fn child_prompt(context: &ChildRunContext) -> String {
    let references = if context.launch.brief.context_refs.is_empty() {
        "none".into()
    } else {
        context.launch.brief.context_refs.join(", ")
    };
    format!(
        "Role: {}\nObjective: {}\nProject context: {}\nContext references: {}",
        context.launch.brief.role,
        context.launch.brief.objective,
        context.launch.brief.project_summary,
        references
    )
}

fn child_artifacts(
    store: &dyn SessionStore,
    session_id: &kurama_protocol::id::SessionId,
    agent_id: &kurama_protocol::id::AgentId,
) -> Result<(Vec<String>, Vec<kurama_protocol::session::BlobRef>), KuramaError> {
    let mut changed_files = Vec::new();
    let mut evidence_refs = Vec::new();
    for event in store.replay_agent(session_id, agent_id)? {
        match event.event {
            SessionEvent::ToolProposed {
                operation: kurama_protocol::tool::Operation::Write { paths, .. },
                ..
            } => {
                changed_files.extend(paths.into_iter().map(|path| path.display().to_string()));
            }
            SessionEvent::ToolCompleted { result, .. } => {
                evidence_refs.extend(result.blob_refs);
            }
            _ => {}
        }
    }
    changed_files.sort();
    changed_files.dedup();
    evidence_refs.sort_by(|left, right| left.sha256.cmp(&right.sha256));
    evidence_refs.dedup();
    Ok((changed_files, evidence_refs))
}

impl fmt::Debug for AgentRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentRuntime")
            .field("active_profile", &self.parts.active_profile)
            .field("profiles", &self.parts.profiles.keys().collect::<Vec<_>>())
            .field("tools", &self.parts.tools.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}
