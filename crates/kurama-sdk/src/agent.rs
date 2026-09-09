use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use kurama_core::{
    engine::{EngineHandle, RuntimeEvents},
    ids::RandomIds,
    orchestrator::{NoDelegation, SmartOrchestrator},
    policy::DefaultPolicy,
    sink::NoopSink,
    store::MemoryStore,
};
use kurama_protocol::{
    KuramaError,
    agent::{AgentSnapshot, OrchestrationContext, WriteScope},
    config::{KuramaConfig, RoleConfig},
    id::{CallId, OperationId, SessionId},
    model::ModelProfile,
    policy::{ApprovalRequest, ApprovalResponse, AutoBoundaries, ExecutionMode},
    runtime::RuntimeEvent,
    session::{EventEnvelope, SessionEvent, SessionMetadata},
    tool::ToolResult,
    traits::{
        ApprovalPolicy, EventSink, IdGenerator, ModelBackend, Orchestrator, SessionStore, Tool,
    },
};

use crate::{AgentBuilder, AgentRuntime};

const DEFAULT_INPUT_TOKENS: u64 = 32_000;
const DEFAULT_OUTPUT_TOKENS: u64 = 4_000;

pub type Events = RuntimeEvents;

#[derive(Clone)]
pub struct Handle {
    inner: EngineHandle,
}

impl Handle {
    pub(crate) fn from_engine(inner: EngineHandle) -> Self {
        Self { inner }
    }

    pub async fn prompt(&self, text: impl Into<String>) -> Result<(), KuramaError> {
        self.submit(text, false).await
    }

    pub async fn delegate(&self, text: impl Into<String>) -> Result<(), KuramaError> {
        self.submit(text, true).await
    }

    pub async fn submit(
        &self,
        text: impl Into<String>,
        explicit_delegation: bool,
    ) -> Result<(), KuramaError> {
        self.inner.submit(text, explicit_delegation).await
    }

    pub async fn resolve_approval(
        &self,
        operation_id: OperationId,
        response: ApprovalResponse,
    ) -> Result<(), KuramaError> {
        self.inner.resolve_approval(operation_id, response).await
    }

    pub async fn cancel_turn(&self) -> Result<(), KuramaError> {
        self.inner.cancel_turn().await
    }

    pub async fn compact(&self) -> Result<(), KuramaError> {
        self.inner.compact().await
    }

    pub async fn set_mode(&self, mode: ExecutionMode) -> Result<(), KuramaError> {
        self.inner.set_mode(mode).await
    }

    pub async fn set_goal(&self, objective: impl Into<String>) -> Result<(), KuramaError> {
        self.inner.set_goal(objective).await
    }

    pub async fn edit_goal(&self, objective: impl Into<String>) -> Result<(), KuramaError> {
        self.inner.edit_goal(objective).await
    }

    pub async fn pause_goal(&self) -> Result<(), KuramaError> {
        self.inner.pause_goal().await
    }

    pub async fn resume_goal(&self) -> Result<(), KuramaError> {
        self.inner.resume_goal().await
    }

    pub async fn clear_goal(&self) -> Result<(), KuramaError> {
        self.inner.clear_goal().await
    }

    pub async fn agent_command(
        &self,
        command: kurama_protocol::runtime::AgentCommand,
    ) -> Result<(), KuramaError> {
        self.inner.agent_command(command).await
    }

    pub async fn shutdown(&self) -> Result<(), KuramaError> {
        self.inner.shutdown().await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    pub text: String,
    pub session_id: SessionId,
}

impl fmt::Display for TurnOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Status(String),
    Text(String),
    Approval(ApprovalRequest),
    ToolStarted {
        operation_id: OperationId,
        name: String,
        context: String,
    },
    ToolOutput {
        call_id: CallId,
        stream: String,
        chunk: String,
    },
    ToolCompleted {
        operation_id: OperationId,
        result: ToolResult,
    },
    AgentUpdated(AgentSnapshot),
    AgentInspection {
        snapshot: AgentSnapshot,
        transcript: Vec<String>,
    },
    Done(TurnOutcome),
    Error(String),
    Shutdown,
}

pub struct Turn<'a> {
    handle: Handle,
    events: &'a mut Events,
    text: String,
    session_id: SessionId,
    finished: bool,
}

impl Turn<'_> {
    pub async fn next(&mut self) -> Result<Option<Event>, KuramaError> {
        if self.finished {
            return Ok(None);
        }
        loop {
            let Some(event) = self.events.recv().await else {
                self.finished = true;
                return Ok(None);
            };
            let event = match event {
                RuntimeEvent::Usage { .. }
                | RuntimeEvent::GoalUpdated { .. }
                | RuntimeEvent::GoalCleared => continue,
                RuntimeEvent::Status { message } => Event::Status(message),
                RuntimeEvent::AssistantDelta { text } => {
                    self.text.push_str(&text);
                    Event::Text(text)
                }
                RuntimeEvent::ApprovalRequired { request } => Event::Approval(request),
                RuntimeEvent::ToolStarted {
                    operation_id,
                    name,
                    context,
                } => Event::ToolStarted {
                    operation_id,
                    name,
                    context,
                },
                RuntimeEvent::ToolOutputDelta {
                    call_id,
                    stream,
                    chunk,
                } => Event::ToolOutput {
                    call_id,
                    stream,
                    chunk,
                },
                RuntimeEvent::ToolCompleted {
                    operation_id,
                    result,
                } => Event::ToolCompleted {
                    operation_id,
                    result,
                },
                RuntimeEvent::AgentUpdated { snapshot } => Event::AgentUpdated(snapshot),
                RuntimeEvent::AgentInspection {
                    snapshot,
                    transcript,
                } => Event::AgentInspection {
                    snapshot,
                    transcript,
                },
                RuntimeEvent::TurnCompleted => Event::Done(TurnOutcome {
                    text: self.text.clone(),
                    session_id: self.session_id.clone(),
                }),
                RuntimeEvent::Error { message } => Event::Error(message),
                RuntimeEvent::Shutdown => Event::Shutdown,
            };
            if matches!(event, Event::Done(_) | Event::Error(_) | Event::Shutdown) {
                self.finished = true;
            }
            return Ok(Some(event));
        }
    }

    pub async fn approve_once(&self, operation_id: OperationId) -> Result<(), KuramaError> {
        self.handle
            .resolve_approval(operation_id, ApprovalResponse::ApproveOnce)
            .await
    }

    pub async fn deny(&self, operation_id: OperationId) -> Result<(), KuramaError> {
        self.handle
            .resolve_approval(operation_id, ApprovalResponse::Deny)
            .await
    }
}

struct Live {
    handle: Handle,
    events: Events,
    metadata: SessionMetadata,
}

pub struct Agent {
    runtime: AgentRuntime,
    workspace: PathBuf,
    mode: ExecutionMode,
    live: Option<Live>,
}

pub struct AgentSetup {
    profiles: BTreeMap<String, (ModelProfile, Arc<dyn ModelBackend>)>,
    active_profile: Option<String>,
    tools: BTreeMap<String, Arc<dyn Tool>>,
    policy: Option<Arc<dyn ApprovalPolicy>>,
    store: Option<Arc<dyn SessionStore>>,
    sink: Option<Arc<dyn EventSink>>,
    orchestrator: Option<Arc<dyn Orchestrator>>,
    ids: Option<Arc<dyn IdGenerator>>,
    write_scope: Option<WriteScope>,
    auto: AutoBoundaries,
    workspace: PathBuf,
    mode: ExecutionMode,
    orchestrate: bool,
    roles: BTreeMap<String, RoleConfig>,
    profile_escalations: BTreeMap<String, Vec<String>>,
    max_concurrency: usize,
    pending_model: Option<String>,
    pending_limits: Option<(u64, u64)>,
    error: Option<String>,
}

impl Agent {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> AgentSetup {
        AgentSetup::default()
    }

    pub fn active_profile(&self) -> &str {
        self.runtime.active_profile()
    }

    pub fn registered_tools(&self) -> Vec<&str> {
        self.runtime.registered_tools()
    }

    pub fn orchestrator(&self) -> Arc<dyn Orchestrator> {
        self.runtime.parts().orchestrator.clone()
    }

    pub fn allocate_session_id(&self) -> SessionId {
        self.runtime.parts().ids.session_id()
    }

    pub fn session_id(&self) -> Option<&SessionId> {
        self.live.as_ref().map(|live| &live.metadata.id)
    }

    pub fn launch(
        &self,
        metadata: SessionMetadata,
        replay: Vec<EventEnvelope>,
    ) -> Result<(Handle, Events), KuramaError> {
        let (handle, events) = self.runtime.start(metadata, replay)?;
        Ok((Handle::from_engine(handle), events))
    }

    pub async fn prompt(&mut self, text: impl Into<String>) -> Result<TurnOutcome, KuramaError> {
        self.run_turn(text, false).await
    }

    pub async fn delegate(&mut self, text: impl Into<String>) -> Result<TurnOutcome, KuramaError> {
        if self.runtime.parts().orchestration.is_none() {
            return Err(KuramaError::Configuration(
                "delegate() requires .orchestrate()".into(),
            ));
        }
        self.run_turn(text, true).await
    }

    pub async fn resume(&mut self, id: impl Into<String>) -> Result<(), KuramaError> {
        let id = SessionId::from(id.into());
        let replay = self.runtime.parts().store.replay(&id)?;
        if replay.is_empty() {
            return Err(KuramaError::NotFound(format!("session {id}")));
        }
        let mut metadata = replay
            .iter()
            .find_map(|event| match &event.event {
                SessionEvent::SessionStarted { metadata } => Some(metadata.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                KuramaError::Session("resumed session has no durable session metadata".into())
            })?;
        if !same_workspace(Path::new(&metadata.project_root), &self.workspace) {
            return Err(KuramaError::Session(format!(
                "session {id} belongs to another project"
            )));
        }
        if metadata.profile != self.runtime.active_profile() {
            return Err(KuramaError::Session(format!(
                "session {id} is pinned to profile {}; start a new session to switch profiles",
                metadata.profile
            )));
        }
        metadata.mode = self.mode;
        metadata.redaction_best_effort = self.mode == ExecutionMode::Yolo;
        metadata.project_root = self.workspace.display().to_string();
        let (handle, events) = self.runtime.start(metadata.clone(), replay)?;
        self.live = Some(Live {
            handle: Handle::from_engine(handle),
            events,
            metadata,
        });
        Ok(())
    }

    pub async fn turn(&mut self, text: impl Into<String>) -> Result<Turn<'_>, KuramaError> {
        let handle = self.live_handle()?;
        handle.prompt(text).await?;
        let live = self.live.as_mut().expect("live session");
        let session_id = live.metadata.id.clone();
        Ok(Turn {
            handle: live.handle.clone(),
            events: &mut live.events,
            text: String::new(),
            session_id,
            finished: false,
        })
    }

    async fn run_turn(
        &mut self,
        text: impl Into<String>,
        explicit_delegation: bool,
    ) -> Result<TurnOutcome, KuramaError> {
        let handle = self.live_handle()?;
        handle.submit(text, explicit_delegation).await?;
        let live = self.live.as_mut().expect("live session");
        collect_turn(&handle, &mut live.events, live.metadata.id.clone()).await
    }

    fn live_handle(&mut self) -> Result<Handle, KuramaError> {
        if self.live.is_none() {
            let metadata = self.new_session();
            let (handle, events) = self.runtime.start(metadata.clone(), Vec::new())?;
            self.live = Some(Live {
                handle: Handle::from_engine(handle),
                events,
                metadata,
            });
        }
        Ok(self.live.as_ref().expect("live session").handle.clone())
    }

    fn new_session(&self) -> SessionMetadata {
        SessionMetadata {
            id: self.runtime.parts().ids.session_id(),
            created_at_ms: now_ms(),
            project_root: self.workspace.display().to_string(),
            profile: self.runtime.active_profile().to_owned(),
            mode: self.mode,
            redaction_best_effort: self.mode == ExecutionMode::Yolo,
        }
    }
}

impl Default for AgentSetup {
    fn default() -> Self {
        Self {
            profiles: BTreeMap::new(),
            active_profile: None,
            tools: BTreeMap::new(),
            policy: None,
            store: None,
            sink: None,
            orchestrator: None,
            ids: None,
            write_scope: None,
            auto: AutoBoundaries::default(),
            workspace: PathBuf::from("."),
            mode: ExecutionMode::Supervised,
            orchestrate: false,
            roles: BTreeMap::new(),
            profile_escalations: BTreeMap::new(),
            max_concurrency: 4,
            pending_model: None,
            pending_limits: None,
            error: None,
        }
    }
}

impl AgentSetup {
    pub fn backend(self, backend: impl ModelBackend + 'static) -> Self {
        let backend = Arc::new(backend);
        let name = backend.backend_name().to_string();
        self.backend_as(name, backend)
    }

    pub fn backend_as(mut self, name: impl Into<String>, backend: Arc<dyn ModelBackend>) -> Self {
        let name = name.into();
        let model = self
            .pending_model
            .take()
            .unwrap_or_else(|| backend.backend_name().to_string());
        let (max_input_tokens, max_output_tokens) = self
            .pending_limits
            .take()
            .unwrap_or((DEFAULT_INPUT_TOKENS, DEFAULT_OUTPUT_TOKENS));
        self.profile(
            ModelProfile::new(name, model, max_input_tokens, max_output_tokens),
            backend,
        )
    }

    pub fn profile(mut self, profile: ModelProfile, backend: Arc<dyn ModelBackend>) -> Self {
        if self.profiles.contains_key(&profile.name) {
            self.error = Some(format!("duplicate profile: {}", profile.name));
            return self;
        }
        if self.active_profile.is_none() {
            self.active_profile = Some(profile.name.clone());
        }
        self.profiles
            .insert(profile.name.clone(), (profile, backend));
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        let model = model.into();
        if let Some(name) = self.active_profile.clone()
            && let Some((profile, _)) = self.profiles.get_mut(&name)
        {
            profile.model = model;
            return self;
        }
        self.pending_model = Some(model);
        self
    }

    pub fn limits(mut self, max_input_tokens: u64, max_output_tokens: u64) -> Self {
        self.pending_limits = Some((max_input_tokens, max_output_tokens));
        self
    }

    pub fn active_profile(mut self, name: impl Into<String>) -> Self {
        self.active_profile = Some(name.into());
        self
    }

    pub fn tool(self, tool: impl Tool + 'static) -> Self {
        self.tool_arc(Arc::new(tool))
    }

    pub fn tool_arc(mut self, tool: Arc<dyn Tool>) -> Self {
        let name = tool.descriptor().name;
        if self.tools.contains_key(&name) {
            self.error = Some(format!("duplicate tool: {name}"));
            return self;
        }
        self.tools.insert(name, tool);
        self
    }

    pub fn tools(mut self, tools: impl IntoIterator<Item = Arc<dyn Tool>>) -> Self {
        for tool in tools {
            self = self.tool_arc(tool);
        }
        self
    }

    pub fn policy(mut self, policy: Arc<dyn ApprovalPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    pub fn orchestrator(mut self, orchestrator: Arc<dyn Orchestrator>) -> Self {
        self.orchestrator = Some(orchestrator);
        self
    }

    pub fn ids(mut self, ids: Arc<dyn IdGenerator>) -> Self {
        self.ids = Some(ids);
        self
    }

    pub fn workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.workspace = workspace.into();
        self
    }

    pub fn mode(mut self, mode: ExecutionMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn write_scope(mut self, write_scope: WriteScope) -> Self {
        self.write_scope = Some(write_scope);
        self
    }

    pub fn auto_boundaries(mut self, auto: AutoBoundaries) -> Self {
        self.auto = auto;
        self
    }

    pub fn allow_writes(mut self, roots: impl IntoIterator<Item = impl AsRef<Path>>) -> Self {
        self.auto
            .write_roots
            .extend(roots.into_iter().map(|root| root.as_ref().to_path_buf()));
        self
    }

    pub fn allow_commands(mut self, commands: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.auto
            .allowed_commands
            .extend(commands.into_iter().map(Into::into));
        self
    }

    pub fn allow_hosts(mut self, hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.auto
            .allowed_hosts
            .extend(hosts.into_iter().map(Into::into));
        self
    }

    pub fn config(mut self, config: &KuramaConfig) -> Self {
        self.roles = config.roles.clone();
        self.profile_escalations = config
            .profiles
            .iter()
            .map(|(name, profile)| (name.clone(), profile.escalation_profiles.clone()))
            .collect();
        self.max_concurrency = config.orchestration.max_concurrency;
        self.auto = config.auto.clone();
        self
    }

    pub fn max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = max_concurrency;
        self
    }

    pub fn orchestrate(mut self) -> Self {
        self.orchestrate = true;
        self
    }

    pub fn build(self) -> Result<Agent, KuramaError> {
        if let Some(error) = self.error {
            return Err(KuramaError::Configuration(error));
        }

        let ids = self
            .ids
            .unwrap_or_else(|| Arc::new(RandomIds) as Arc<dyn IdGenerator>);
        let store = self
            .store
            .unwrap_or_else(|| Arc::new(MemoryStore::default()) as Arc<dyn SessionStore>);
        let sink = self
            .sink
            .unwrap_or_else(|| Arc::new(NoopSink) as Arc<dyn EventSink>);
        let policy = self.policy.unwrap_or_else(|| {
            Arc::new(DefaultPolicy::new(self.mode, self.auto.clone())) as Arc<dyn ApprovalPolicy>
        });
        let write_scope = self.write_scope.unwrap_or_else(|| WriteScope {
            roots: vec![self.workspace.clone()],
            files: Vec::new(),
        });
        let orchestrator = if self.orchestrate {
            Arc::new(SmartOrchestrator::new(ids.clone())) as Arc<dyn Orchestrator>
        } else {
            self.orchestrator
                .unwrap_or_else(|| Arc::new(NoDelegation) as Arc<dyn Orchestrator>)
        };

        let mut builder = AgentBuilder::new()
            .policy(policy)
            .store(store)
            .sink(sink)
            .orchestrator(orchestrator)
            .ids(ids)
            .write_scope(write_scope.clone())
            .auto_boundaries(self.auto);
        if let Some(name) = &self.active_profile {
            builder = builder.active_profile(name.clone());
        }
        for (profile, backend) in self.profiles.values() {
            builder = builder.profile(profile.clone(), backend.clone());
        }
        for tool in self.tools.into_values() {
            builder = builder.tool(tool);
        }
        if self.orchestrate {
            builder = builder.orchestration_context(orchestration_context(
                self.active_profile.as_deref(),
                &self.profiles,
                &self.roles,
                &self.profile_escalations,
                write_scope,
                self.max_concurrency,
                self.mode,
            )?);
        }

        Ok(Agent {
            runtime: builder.build()?,
            workspace: self.workspace,
            mode: self.mode,
            live: None,
        })
    }
}

fn orchestration_context(
    active_profile: Option<&str>,
    profiles: &BTreeMap<String, (ModelProfile, Arc<dyn ModelBackend>)>,
    roles: &BTreeMap<String, RoleConfig>,
    profile_escalations: &BTreeMap<String, Vec<String>>,
    parent_write_scope: WriteScope,
    max_concurrency: usize,
    mode: ExecutionMode,
) -> Result<OrchestrationContext, KuramaError> {
    let active_profile = active_profile
        .ok_or_else(|| KuramaError::Configuration("an active model profile is required".into()))?;
    let parent_profile = profiles
        .get(active_profile)
        .map(|(profile, _)| profile.clone())
        .ok_or_else(|| {
            KuramaError::Configuration(format!("unknown active profile: {active_profile}"))
        })?;
    let registered = profiles.keys().cloned().collect::<Vec<_>>();
    let model_profiles = profiles
        .iter()
        .map(|(name, (profile, _))| (name.clone(), profile.clone()))
        .collect::<BTreeMap<_, _>>();
    Ok(OrchestrationContext {
        parent_profile,
        profiles: model_profiles,
        role_routes: roles
            .iter()
            .filter(|(_, route)| registered.iter().any(|name| name == &route.profile))
            .map(|(role, route)| (role.clone(), route.profile.clone()))
            .collect(),
        role_escalations: roles
            .iter()
            .filter(|(_, route)| registered.iter().any(|name| name == &route.profile))
            .map(|(role, route)| {
                (
                    role.clone(),
                    route
                        .escalation_profiles
                        .iter()
                        .filter(|profile| registered.iter().any(|name| name == *profile))
                        .cloned()
                        .collect(),
                )
            })
            .collect(),
        profile_escalations: profile_escalations
            .iter()
            .filter(|(name, _)| registered.iter().any(|registered| registered == *name))
            .map(|(name, profiles)| {
                (
                    name.clone(),
                    profiles
                        .iter()
                        .filter(|profile| registered.iter().any(|name| name == *profile))
                        .cloned()
                        .collect(),
                )
            })
            .collect(),
        parent_write_scope,
        max_concurrency,
        depth: 0,
        yolo: mode == ExecutionMode::Yolo,
    })
}

async fn collect_turn(
    handle: &Handle,
    events: &mut Events,
    session_id: SessionId,
) -> Result<TurnOutcome, KuramaError> {
    let mut text = String::new();
    loop {
        match events.recv().await.ok_or(KuramaError::Cancelled)? {
            RuntimeEvent::AssistantDelta { text: delta } => text.push_str(&delta),
            RuntimeEvent::TurnCompleted => {
                return Ok(TurnOutcome { text, session_id });
            }
            RuntimeEvent::Error { message } => return Err(KuramaError::Model(message)),
            RuntimeEvent::ApprovalRequired { request } => {
                let _ = handle.cancel_turn().await;
                drain_until_terminal(events).await;
                return Err(KuramaError::Policy(format!(
                    "approval required: {}",
                    request.summary
                )));
            }
            RuntimeEvent::Shutdown => return Err(KuramaError::Cancelled),
            _ => {}
        }
    }
}

async fn drain_until_terminal(events: &mut Events) {
    while let Some(event) = events.recv().await {
        if matches!(
            event,
            RuntimeEvent::Error { .. } | RuntimeEvent::TurnCompleted | RuntimeEvent::Shutdown
        ) {
            break;
        }
    }
}

fn same_workspace(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

impl fmt::Debug for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Agent")
            .field("runtime", &self.runtime)
            .field("workspace", &self.workspace)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}
