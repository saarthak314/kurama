use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use kurama_adapters::{
    AppPaths, BashTool, ConfigRepository, CredentialResolver, FsSessionStore, HttpClient,
    JsonSearchBackend, OpenAiNativeSearch, ProviderFactory, RandomIds, ReadTool, SearchBackend,
    SecretValue, SessionSecrets, WebSearchTool, WriteTool,
};
use kurama_core::{
    engine::{EngineHandle, RuntimeEvents},
    orchestrator::SmartOrchestrator,
    policy::DefaultPolicy,
};
use kurama_protocol::{
    KuramaError,
    agent::{OrchestrationContext, WriteScope},
    config::{
        AuthRef, KuramaConfig, OrchestrationConfig, ProfileConfig, ProfileKind, SearchConfig,
    },
    id::SessionId,
    model::ModelProfile,
    policy::{ApprovalResponse, AutoBoundaries, ExecutionMode},
    runtime::{EngineCommand, RuntimeEvent},
    session::{EventEnvelope, SessionEvent, SessionMetadata},
    traits::{EventSink, IdGenerator, Orchestrator, SessionStore, Tool},
};
use kurama_sdk::AgentBuilder;
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend},
};
use tokio::sync::mpsc;

use crate::{
    args::{Args, ResumeChoice},
    commands::{Command, parse_command},
    tui::{OnboardingSubmission, Overlay, TerminalGuard, TuiState, render, spawn_input_thread},
};

pub struct App {
    pub state: TuiState,
    engine: Option<EngineHandle>,
    runtime_events: Option<RuntimeEvents>,
    tool_events: Option<mpsc::UnboundedReceiver<RuntimeEvent>>,
    orchestrator: Option<Arc<dyn Orchestrator>>,
    session_id: Option<SessionId>,
    restart_args: Option<Args>,
    control: Option<AppControl>,
}

struct AppControl {
    project: PathBuf,
    paths: AppPaths,
    session_secrets: Arc<Mutex<SessionSecrets>>,
    repository: ConfigRepository,
    store: Arc<FsSessionStore>,
    profiles: Vec<String>,
    max_input_tokens: u64,
    launch_args: Args,
}

impl App {
    pub fn bootstrap(args: &Args, cwd: PathBuf) -> Result<Self, String> {
        Self::bootstrap_with_paths(
            args,
            cwd,
            AppPaths::discover().map_err(|error| error.to_string())?,
            SessionSecrets::default(),
        )
    }

    pub fn bootstrap_with_paths(
        args: &Args,
        cwd: PathBuf,
        paths: AppPaths,
        session_secrets: SessionSecrets,
    ) -> Result<Self, String> {
        Self::bootstrap_with_shared_paths(args, cwd, paths, Arc::new(Mutex::new(session_secrets)))
    }

    fn bootstrap_with_shared_paths(
        args: &Args,
        cwd: PathBuf,
        paths: AppPaths,
        session_secrets: Arc<Mutex<SessionSecrets>>,
    ) -> Result<Self, String> {
        let project = cwd
            .canonicalize()
            .map_err(|error| format!("canonicalize project: {error}"))?;
        let repository =
            ConfigRepository::open(paths.clone()).map_err(|error| error.to_string())?;
        let store = Arc::new(
            FsSessionStore::open(paths.root().to_path_buf()).map_err(|error| error.to_string())?,
        );
        let Some(config) = repository
            .read_config()
            .map_err(|error| error.to_string())?
        else {
            if args.profile.is_some() || args.resume.is_some() {
                return Err("Kurama is not configured; create ~/.kurama/config.toml first".into());
            }
            return Ok(Self::disconnected(
                TuiState::onboarding(project.display().to_string()),
                AppControl {
                    project,
                    paths,
                    session_secrets,
                    repository,
                    store,
                    profiles: Vec::new(),
                    max_input_tokens: 0,
                    launch_args: args.clone(),
                },
            ));
        };
        if config.profiles.is_empty() {
            return Ok(Self::disconnected(
                TuiState::onboarding(project.display().to_string()),
                AppControl {
                    project,
                    paths,
                    session_secrets,
                    repository,
                    store,
                    profiles: Vec::new(),
                    max_input_tokens: 0,
                    launch_args: args.clone(),
                },
            ));
        }

        let resume_id = resolve_resume(args, &repository, store.as_ref(), &project)?;
        let mut replay = resume_id
            .as_ref()
            .map(|session_id| store.replay(session_id))
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_default();
        let previous_metadata = replay.iter().find_map(|event| {
            if let SessionEvent::SessionStarted { metadata } = &event.event {
                Some(metadata.clone())
            } else {
                None
            }
        });
        if resume_id.is_some() && previous_metadata.is_none() {
            return Err("resumed session has no durable session metadata".into());
        }
        if let Some(metadata) = &previous_metadata {
            let recorded = PathBuf::from(&metadata.project_root);
            if recorded.canonicalize().ok().as_ref() != Some(&project) {
                return Err(format!(
                    "session {} belongs to another project",
                    metadata.id
                ));
            }
            if args
                .profile
                .as_deref()
                .is_some_and(|profile| profile != metadata.profile)
            {
                return Err(format!(
                    "session {} is pinned to profile {}; start a new session to switch profiles",
                    metadata.id, metadata.profile
                ));
            }
        }
        let active_profile = if let Some(metadata) = &previous_metadata {
            metadata.profile.clone()
        } else {
            repository
                .resolve_profile(&project, args.profile.as_deref())
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "no active profile is configured".to_owned())?
        };
        let active = config
            .profiles
            .get(&active_profile)
            .ok_or_else(|| format!("unknown active profile: {active_profile}"))?;
        let mutable_state = repository.read_state().map_err(|error| error.to_string())?;
        let mode = if args.yolo {
            ExecutionMode::Yolo
        } else if let Some(metadata) = &previous_metadata {
            if metadata.mode == ExecutionMode::Yolo {
                mutable_state.last_mode.unwrap_or(ExecutionMode::Supervised)
            } else {
                metadata.mode
            }
        } else {
            mutable_state.last_mode.unwrap_or(config.default_mode)
        };
        let profile_names: Vec<_> = config.profiles.keys().cloned().collect();
        let missing_session_profile = {
            let secrets = session_secrets
                .lock()
                .map_err(|_| "session credential store is unavailable".to_owned())?;
            config
                .profiles
                .iter()
                .find(|(name, profile)| {
                    matches!(profile.auth, Some(AuthRef::Session)) && !secrets.contains(name)
                })
                .map(|(name, _)| name.clone())
        };
        if let Some(profile) = missing_session_profile {
            let mut state = TuiState::credential(project.display().to_string(), profile);
            state.mode = mode;
            return Ok(Self::disconnected(
                state,
                AppControl {
                    project,
                    paths,
                    session_secrets,
                    repository,
                    store,
                    profiles: profile_names,
                    max_input_tokens: active.max_input_tokens,
                    launch_args: args.clone(),
                },
            ));
        }

        let http = HttpClient::try_new().map_err(|error| error.to_string())?;
        let credentials = CredentialResolver;
        let provider_factory = ProviderFactory::new(http.clone(), paths.clone(), credentials);
        let mut profiles = BTreeMap::new();
        let mut builder = AgentBuilder::new();
        let secrets = session_secrets
            .lock()
            .map_err(|_| "session credential store is unavailable".to_owned())?;
        for (name, profile) in &config.profiles {
            let model_profile = ModelProfile::new(
                name.clone(),
                profile.model.clone(),
                profile.max_input_tokens,
                profile.max_output_tokens,
            );
            let backend = provider_factory
                .build(name, profile, &secrets)
                .map_err(|error| error.to_string())?;
            profiles.insert(name.clone(), model_profile.clone());
            builder = builder.profile(model_profile, backend);
        }

        let ids = Arc::new(RandomIds);
        let orchestrator = Arc::new(SmartOrchestrator::new(ids.clone()));
        let write_scope = WriteScope {
            roots: vec![project.clone()],
            files: Vec::new(),
        };
        let orchestration = orchestration_context(
            &config,
            active_profile.as_str(),
            &profiles,
            write_scope.clone(),
            mode,
        )?;
        let search_backend = search_backend(
            &config,
            active_profile.as_str(),
            active,
            &secrets,
            credentials,
            http.clone(),
        )?;
        drop(secrets);
        let (tool_tx, tool_rx) = mpsc::unbounded_channel();
        let tool_sink: Arc<dyn EventSink> = Arc::new(ToolEventSink { sender: tool_tx });
        let tools = standard_tools(http, search_backend, Some(tool_sink));
        let sink: Arc<dyn EventSink> = Arc::new(NoopSink);
        builder = builder
            .active_profile(active_profile.clone())
            .policy(Arc::new(DefaultPolicy::new(mode, config.auto.clone())))
            .store(store.clone())
            .sink(sink)
            .orchestrator(orchestrator.clone())
            .ids(ids.clone())
            .write_scope(write_scope)
            .auto_boundaries(config.auto.clone())
            .orchestration_context(orchestration);
        for tool in tools {
            builder = builder.tool(tool);
        }
        let runtime = builder.build().map_err(|error| error.to_string())?;

        let session_id = resume_id.unwrap_or_else(|| ids.session_id());
        let created_at_ms = previous_metadata
            .as_ref()
            .map_or_else(now_ms, |metadata| metadata.created_at_ms);
        let resumed_yolo = previous_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.mode == ExecutionMode::Yolo)
            && mode != ExecutionMode::Yolo;
        if previous_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.mode != mode)
        {
            let event = EventEnvelope::new(
                replay.last().map_or(0, |event| event.sequence + 1),
                now_ms(),
                session_id.clone(),
                None,
                SessionEvent::ModeSelected { mode },
            );
            store.append(&event).map_err(|error| error.to_string())?;
            replay.push(event);
        }
        let metadata = SessionMetadata {
            id: session_id.clone(),
            created_at_ms,
            project_root: project.display().to_string(),
            profile: active_profile.clone(),
            mode,
            redaction_best_effort: mode == ExecutionMode::Yolo,
        };
        let (engine, runtime_events) = runtime
            .start(metadata, replay)
            .map_err(|error| error.to_string())?;

        repository
            .remember_project_profile(&project, &active_profile)
            .map_err(|error| error.to_string())?;
        repository
            .remember_latest_session(&project, &session_id)
            .map_err(|error| error.to_string())?;
        if mode != ExecutionMode::Yolo {
            repository
                .remember_mode(mode)
                .map_err(|error| error.to_string())?;
        }

        let mut state = TuiState::new(
            active_profile,
            active.model.clone(),
            project.display().to_string(),
            mode,
        );
        if resumed_yolo {
            state.status = "Previous run used YOLO; resumed in supervised mode".into();
        }
        Ok(Self {
            state,
            engine: Some(engine),
            runtime_events: Some(runtime_events),
            tool_events: Some(tool_rx),
            orchestrator: Some(orchestrator),
            session_id: Some(session_id),
            restart_args: None,
            control: Some(AppControl {
                project,
                paths,
                session_secrets,
                repository,
                store,
                profiles: profile_names,
                max_input_tokens: active.max_input_tokens,
                launch_args: args.clone(),
            }),
        })
    }

    pub fn from_runtime(
        state: TuiState,
        engine: EngineHandle,
        orchestrator: Arc<dyn Orchestrator>,
        session_id: SessionId,
    ) -> Self {
        Self {
            state,
            engine: Some(engine),
            runtime_events: None,
            tool_events: None,
            orchestrator: Some(orchestrator),
            session_id: Some(session_id),
            restart_args: None,
            control: None,
        }
    }

    fn disconnected(state: TuiState, control: AppControl) -> Self {
        Self {
            state,
            engine: None,
            runtime_events: None,
            tool_events: None,
            orchestrator: None,
            session_id: None,
            restart_args: None,
            control: Some(control),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.engine.is_some()
    }

    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    pub fn restart_args(&self) -> Option<&Args> {
        self.restart_args.as_ref()
    }

    fn request_restart(&mut self, args: Args, status: impl Into<String>) {
        self.restart_args = Some(args);
        self.state.status = status.into();
        if self.engine.is_some() {
            self.state.queue_command(EngineCommand::Shutdown);
        }
    }

    pub const fn tool_names() -> [&'static str; 4] {
        ["bash", "read", "web-search", "write"]
    }

    pub async fn run(mut self) -> Result<(), String> {
        let _guard = TerminalGuard::enter().map_err(|error| error.to_string())?;
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend).map_err(|error| error.to_string())?;
        let mut input = spawn_input_thread(32);
        loop {
            let runtime_events = self.runtime_events.take();
            let tool_events = self.tool_events.take();
            run_loop(
                &mut self,
                &mut terminal,
                &mut input,
                runtime_events,
                tool_events,
            )
            .await?;
            let Some(args) = self.restart_args.take() else {
                return Ok(());
            };
            let control = self
                .control
                .as_ref()
                .ok_or_else(|| "runtime cannot restart without bootstrap context".to_owned())?;
            self = Self::bootstrap_with_shared_paths(
                &args,
                control.project.clone(),
                control.paths.clone(),
                control.session_secrets.clone(),
            )?;
        }
    }

    pub fn handle_event(&mut self, event: Event) -> Result<bool, String> {
        let Event::Key(key) = event else {
            return Ok(false);
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.state.queue_command(EngineCommand::Shutdown);
            return Ok(true);
        }

        match self.state.overlay {
            Overlay::Onboarding => self.handle_onboarding_key(key),
            Overlay::Approval | Overlay::ApprovalEdit => self.handle_approval_key(key),
            Overlay::Agents
            | Overlay::AgentInspect
            | Overlay::AgentMessage
            | Overlay::ConfirmAgentCancel => self.handle_agents_key(key),
            Overlay::None => self.handle_main_key(key)?,
        }
        Ok(self.restart_args.is_some() && self.engine.is_none())
    }

    fn handle_main_key(&mut self, key: KeyEvent) -> Result<(), String> {
        match key.code {
            KeyCode::Char(character) => {
                self.state.composer.insert(self.state.cursor, character);
                self.state.cursor += character.len_utf8();
            }
            KeyCode::Backspace if self.state.cursor > 0 => {
                let previous = self.state.composer[..self.state.cursor]
                    .char_indices()
                    .last()
                    .map(|(index, _)| index)
                    .unwrap_or(0);
                self.state.composer.drain(previous..self.state.cursor);
                self.state.cursor = previous;
            }
            KeyCode::Delete if self.state.cursor < self.state.composer.len() => {
                let next = self.state.cursor
                    + self.state.composer[self.state.cursor..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                self.state.composer.drain(self.state.cursor..next);
            }
            KeyCode::Left => {
                self.state.cursor = self.state.composer[..self.state.cursor]
                    .char_indices()
                    .last()
                    .map(|(index, _)| index)
                    .unwrap_or(0);
            }
            KeyCode::Right if self.state.cursor < self.state.composer.len() => {
                self.state.cursor += self.state.composer[self.state.cursor..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(0);
            }
            KeyCode::PageUp => self.state.scroll = self.state.scroll.saturating_add(5),
            KeyCode::PageDown => self.state.scroll = self.state.scroll.saturating_sub(5),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.state.composer.insert(self.state.cursor, '\n');
                self.state.cursor += 1;
            }
            KeyCode::Enter => self.submit_composer()?,
            _ => {}
        }
        Ok(())
    }

    fn submit_composer(&mut self) -> Result<(), String> {
        let text = std::mem::take(&mut self.state.composer);
        self.state.cursor = 0;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        if trimmed.starts_with('/') {
            let command = match parse_command(trimmed) {
                Ok(command) => command,
                Err(error) => {
                    self.state.status = error;
                    return Ok(());
                }
            };
            match command {
                Command::Agents => self.state.open_agents(),
                Command::Model(profile) => {
                    if let Some(profile) = profile {
                        let known = self
                            .control
                            .as_ref()
                            .is_some_and(|control| control.profiles.contains(&profile));
                        if !known {
                            self.state.status = format!("unknown profile: {profile}");
                        } else if profile == self.state.profile {
                            self.state.status = format!("profile {profile} is already active");
                        } else {
                            self.request_restart(
                                Args {
                                    profile: Some(profile.clone()),
                                    yolo: self.state.mode == ExecutionMode::Yolo,
                                    ..Args::default()
                                },
                                format!("switching to profile {profile}"),
                            );
                        }
                    } else {
                        self.state.status = self.control.as_ref().map_or_else(
                            || "profile selection is unavailable".into(),
                            |control| format!("profiles: {}", control.profiles.join(", ")),
                        );
                    }
                }
                Command::Connect => self.state.overlay = Overlay::Onboarding,
                Command::Sessions => self.show_sessions()?,
                Command::Resume(session) => {
                    self.request_restart(
                        Args {
                            resume: Some(ResumeChoice::Id(session.to_string())),
                            yolo: self.state.mode == ExecutionMode::Yolo,
                            ..Args::default()
                        },
                        format!("resuming session {session}"),
                    );
                }
                Command::New => {
                    self.request_restart(
                        Args {
                            profile: self.is_connected().then(|| self.state.profile.clone()),
                            yolo: self.state.mode == ExecutionMode::Yolo,
                            ..Args::default()
                        },
                        "starting a new session",
                    );
                }
                Command::Context => {
                    self.state.status = self.control.as_ref().map_or_else(
                        || "context details are unavailable".into(),
                        |control| {
                            format!(
                                "context: {} token input limit; automatic compaction; session {}",
                                control.max_input_tokens,
                                self.session_id.as_ref().map_or("none", AsRef::as_ref)
                            )
                        },
                    )
                }
                Command::Compact => self.state.queue_command(EngineCommand::Compact),
                Command::Mode(mode) => {
                    if let Some(control) = &self.control {
                        control
                            .repository
                            .remember_mode(mode)
                            .map_err(|error| error.to_string())?;
                    }
                    self.state.mode = mode;
                    self.state.queue_command(EngineCommand::SetMode(mode));
                }
            }
        } else if self.engine.is_none() {
            self.state.status = "not connected; configure ~/.kurama/config.toml".into();
        } else {
            self.state.push_user(trimmed);
            let explicit_delegation = self
                .orchestrator
                .as_ref()
                .is_some_and(|orchestrator| orchestrator.explicit_delegation(trimmed));
            self.state.queue_command(EngineCommand::SubmitTurn {
                text: trimmed.to_owned(),
                explicit_delegation,
            });
            self.state.status = "thinking".into();
        }
        Ok(())
    }

    fn show_sessions(&mut self) -> Result<(), String> {
        let Some(control) = &self.control else {
            self.state.status = "session listing is unavailable".into();
            return Ok(());
        };
        let project = control.project.display().to_string();
        let sessions: Vec<_> = control
            .store
            .list()
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter(|session| session.project_root == project)
            .take(4)
            .collect();
        self.state.status = if sessions.is_empty() {
            "no saved sessions for this project".into()
        } else {
            let summaries = sessions
                .iter()
                .map(|session| format!("{} ({})", session.id, session.profile))
                .collect::<Vec<_>>()
                .join(", ");
            format!("sessions: {summaries}")
        };
        Ok(())
    }

    fn handle_onboarding_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.state.onboarding.select_previous(),
            KeyCode::Down => self.state.onboarding.select_next(),
            KeyCode::Char(character) => self.state.onboarding.push(character),
            KeyCode::Backspace => self.state.onboarding.backspace(),
            KeyCode::Enter => match self.state.onboarding.submit() {
                Ok(Some(submission)) => {
                    if let Err(error) = self.apply_onboarding_submission(submission) {
                        self.state.status = error;
                    }
                }
                Ok(None) => self.state.status = self.state.onboarding.prompt().to_lowercase(),
                Err(error) => self.state.status = error,
            },
            KeyCode::Esc => self.state.overlay = Overlay::None,
            _ => {}
        }
    }

    fn apply_onboarding_submission(
        &mut self,
        submission: OnboardingSubmission,
    ) -> Result<(), String> {
        let control = self
            .control
            .as_ref()
            .ok_or_else(|| "connection setup is unavailable".to_owned())?;
        let repository = control.repository.clone();
        let project = control.project.clone();
        let session_secrets = control.session_secrets.clone();
        let launch_args = control.launch_args.clone();
        match submission {
            OnboardingSubmission::Profile {
                name,
                profile,
                secret,
            } => {
                let mut config = repository
                    .read_config()
                    .map_err(|error| error.to_string())?
                    .unwrap_or_else(empty_config);
                if config.profiles.contains_key(&name) {
                    return Err(format!("profile {name} already exists"));
                }
                config.profiles.insert(name.clone(), profile);
                if config.default_profile.is_none() {
                    config.default_profile = Some(name.clone());
                }
                repository
                    .write_config(&config)
                    .map_err(|error| error.to_string())?;
                repository
                    .remember_project_profile(&project, &name)
                    .map_err(|error| error.to_string())?;
                if let Some(secret) = secret {
                    session_secrets
                        .lock()
                        .map_err(|_| "session credential store is unavailable".to_owned())?
                        .insert(name.clone(), SecretValue::new(secret));
                }
                self.request_restart(
                    Args {
                        profile: Some(name.clone()),
                        yolo: self.state.mode == ExecutionMode::Yolo,
                        ..Args::default()
                    },
                    format!("connecting profile {name}"),
                );
            }
            OnboardingSubmission::Credential { profile, secret } => {
                session_secrets
                    .lock()
                    .map_err(|_| "session credential store is unavailable".to_owned())?
                    .insert(profile.clone(), SecretValue::new(secret));
                self.request_restart(launch_args, format!("connecting profile {profile}"));
            }
        }
        Ok(())
    }

    fn handle_approval_key(&mut self, key: KeyEvent) {
        match (self.state.overlay, key.code) {
            (Overlay::Approval, KeyCode::Char('a')) => {
                self.state.resolve_approval(ApprovalResponse::ApproveOnce)
            }
            (Overlay::Approval, KeyCode::Char('d')) => {
                self.state.resolve_approval(ApprovalResponse::Deny)
            }
            (Overlay::Approval, KeyCode::Char('e')) => self.state.begin_approval_edit(),
            (Overlay::ApprovalEdit, KeyCode::Char(character)) => {
                if let Some(approval) = &mut self.state.approval {
                    approval.editor.push(character);
                }
            }
            (Overlay::ApprovalEdit, KeyCode::Backspace) => {
                if let Some(approval) = &mut self.state.approval {
                    approval.editor.pop();
                }
            }
            (Overlay::ApprovalEdit, KeyCode::Enter) => {
                let _ = self.state.submit_approval_edit();
            }
            (_, KeyCode::Esc) => self.state.close_overlay(),
            _ => {}
        }
    }

    fn handle_agents_key(&mut self, key: KeyEvent) {
        match (self.state.overlay, key.code) {
            (Overlay::Agents, KeyCode::Up) => self.state.select_previous_agent(),
            (Overlay::Agents, KeyCode::Down) => self.state.select_next_agent(),
            (Overlay::Agents, KeyCode::Enter) => self.state.inspect_selected_agent(),
            (Overlay::Agents | Overlay::AgentInspect, KeyCode::Char('m')) => {
                self.state.begin_agent_message()
            }
            (Overlay::AgentMessage, KeyCode::Char(character)) => {
                self.state.agent_message.push(character);
            }
            (Overlay::AgentMessage, KeyCode::Backspace) => {
                self.state.agent_message.pop();
            }
            (Overlay::AgentMessage, KeyCode::Enter) => self.state.submit_agent_message(),
            (Overlay::Agents | Overlay::AgentInspect, KeyCode::Char('x')) => {
                self.state.request_agent_cancel()
            }
            (Overlay::ConfirmAgentCancel, KeyCode::Char('y')) => self.state.confirm_agent_cancel(),
            (Overlay::ConfirmAgentCancel, KeyCode::Char('n')) | (_, KeyCode::Esc) => {
                self.state.close_overlay()
            }
            _ => {}
        }
    }

    async fn flush_commands(&mut self) -> Result<(), String> {
        let commands = self.state.take_commands();
        let Some(engine) = &self.engine else {
            if !commands.is_empty() {
                self.state.status = "not connected; configure ~/.kurama/config.toml".into();
            }
            return Ok(());
        };
        for command in commands {
            match command {
                EngineCommand::SubmitTurn {
                    text,
                    explicit_delegation,
                } => engine.submit(text, explicit_delegation).await,
                EngineCommand::ResolveApproval(response) => engine.resolve_approval(response).await,
                EngineCommand::CancelTurn => engine.cancel_turn().await,
                EngineCommand::Compact => engine.compact().await,
                EngineCommand::SetMode(mode) => engine.set_mode(mode).await,
                EngineCommand::Agent(command) => engine.agent_command(command).await,
                EngineCommand::Shutdown => engine.shutdown().await,
            }
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

pub async fn run(args: Args) -> Result<(), String> {
    App::bootstrap(
        &args,
        std::env::current_dir().map_err(|error| error.to_string())?,
    )?
    .run()
    .await
}

pub fn prompt_bundle() -> String {
    let descriptors: Vec<_> = standard_tools(HttpClient::new(), None, None)
        .into_iter()
        .map(|tool| tool.descriptor())
        .collect();
    format!(
        "{}\n{}",
        kurama_core::prompts::SYSTEM_PROMPT,
        serde_json::to_string(&descriptors).expect("serialize standard tool schemas")
    )
}

pub async fn run_with<B>(
    mut app: App,
    terminal: &mut Terminal<B>,
    mut input: mpsc::Receiver<Event>,
    runtime_events: mpsc::Receiver<RuntimeEvent>,
) -> Result<App, String>
where
    B: Backend,
{
    run_loop(&mut app, terminal, &mut input, Some(runtime_events), None).await?;
    Ok(app)
}

async fn run_loop<B>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    input: &mut mpsc::Receiver<Event>,
    runtime_events: Option<mpsc::Receiver<RuntimeEvent>>,
    tool_events: Option<mpsc::UnboundedReceiver<RuntimeEvent>>,
) -> Result<(), String>
where
    B: Backend,
{
    let (runtime_tx, mut runtime_receiver) = mpsc::channel(1);
    let mut runtime_open = if let Some(events) = runtime_events {
        runtime_receiver = events;
        true
    } else {
        drop(runtime_tx);
        false
    };
    let (tool_tx, mut tool_receiver) = mpsc::unbounded_channel();
    let mut tool_open = if let Some(events) = tool_events {
        tool_receiver = events;
        true
    } else {
        drop(tool_tx);
        false
    };
    let mut input_open = true;
    terminal
        .draw(|frame| render(frame, &app.state))
        .map_err(|error| error.to_string())?;

    while input_open || runtime_open || tool_open {
        let mut exit = false;
        tokio::select! {
            event = input.recv(), if input_open => {
                match event {
                    Some(event) => {
                        exit = app.handle_event(event)?;
                        app.flush_commands().await?;
                    }
                    None => input_open = false,
                }
            }
            event = runtime_receiver.recv(), if runtime_open => {
                match event {
                    Some(event) => {
                        exit = matches!(event, RuntimeEvent::Shutdown);
                        app.state.apply_runtime_event(event);
                    }
                    None => runtime_open = false,
                }
            }
            event = tool_receiver.recv(), if tool_open => {
                match event {
                    Some(event) => app.state.apply_runtime_event(event),
                    None => tool_open = false,
                }
            }
        }
        terminal
            .draw(|frame| render(frame, &app.state))
            .map_err(|error| error.to_string())?;
        if exit {
            break;
        }
    }
    Ok(())
}

fn standard_tools(
    http: HttpClient,
    search_backend: Option<Arc<dyn SearchBackend>>,
    bash_sink: Option<Arc<dyn EventSink>>,
) -> Vec<Arc<dyn Tool>> {
    let bash: Arc<dyn Tool> = match bash_sink {
        Some(sink) => Arc::new(BashTool::with_event_sink("/bin/bash", sink)),
        None => Arc::new(BashTool::default()),
    };
    vec![
        bash,
        Arc::new(ReadTool::default()),
        Arc::new(WebSearchTool::new(http, search_backend)),
        Arc::new(WriteTool::default()),
    ]
}

fn search_backend(
    config: &KuramaConfig,
    active_profile: &str,
    active: &ProfileConfig,
    session_secrets: &SessionSecrets,
    credentials: CredentialResolver,
    http: HttpClient,
) -> Result<Option<Arc<dyn SearchBackend>>, String> {
    match config.search.as_ref() {
        None => Ok(None),
        Some(SearchConfig::Json { endpoint, auth }) => {
            let secret = credentials
                .resolve_optional("search", auth.as_ref(), session_secrets)
                .map_err(|error| error.to_string())?;
            Ok(Some(Arc::new(JsonSearchBackend::with_client(
                http,
                endpoint.clone(),
                secret,
            ))))
        }
        Some(SearchConfig::Provider) if active.kind == ProfileKind::OpenAi => {
            let auth = active
                .auth
                .as_ref()
                .ok_or_else(|| "OpenAI native search requires profile auth".to_owned())?;
            let secret = credentials
                .resolve(active_profile, auth, session_secrets)
                .map_err(|error| error.to_string())?;
            Ok(Some(Arc::new(OpenAiNativeSearch::new(
                http,
                active
                    .endpoint
                    .clone()
                    .unwrap_or_else(|| "https://api.openai.com/v1".into()),
                secret,
                active.model.clone(),
            ))))
        }
        Some(SearchConfig::Provider) => Ok(None),
    }
}

fn orchestration_context(
    config: &KuramaConfig,
    active_profile: &str,
    profiles: &BTreeMap<String, ModelProfile>,
    parent_write_scope: WriteScope,
    mode: ExecutionMode,
) -> Result<OrchestrationContext, String> {
    let parent_profile = profiles
        .get(active_profile)
        .cloned()
        .ok_or_else(|| format!("unknown active profile: {active_profile}"))?;
    Ok(OrchestrationContext {
        parent_profile,
        profiles: profiles.clone(),
        role_routes: config
            .roles
            .iter()
            .map(|(role, route)| (role.clone(), route.profile.clone()))
            .collect(),
        role_escalations: config
            .roles
            .iter()
            .map(|(role, route)| (role.clone(), route.escalation_profiles.clone()))
            .collect(),
        profile_escalations: config
            .profiles
            .iter()
            .map(|(name, profile)| (name.clone(), profile.escalation_profiles.clone()))
            .collect(),
        parent_write_scope,
        max_concurrency: config.orchestration.max_concurrency,
        depth: 0,
        yolo: mode == ExecutionMode::Yolo,
    })
}

fn empty_config() -> KuramaConfig {
    KuramaConfig {
        version: 1,
        default_profile: None,
        default_mode: ExecutionMode::Supervised,
        profiles: BTreeMap::new(),
        roles: BTreeMap::new(),
        orchestration: OrchestrationConfig::default(),
        auto: AutoBoundaries::default(),
        search: None,
    }
}

fn resolve_resume(
    args: &Args,
    repository: &ConfigRepository,
    store: &dyn SessionStore,
    project: &Path,
) -> Result<Option<SessionId>, String> {
    match &args.resume {
        None => Ok(None),
        Some(ResumeChoice::Id(session_id)) => Ok(Some(SessionId::from(session_id.clone()))),
        Some(ResumeChoice::Continue) => {
            let state = repository.read_state().map_err(|error| error.to_string())?;
            if let Some(session_id) = state.latest_sessions.get(project) {
                return Ok(Some(session_id.clone()));
            }
            let session = store
                .list()
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|summary| {
                    PathBuf::from(&summary.project_root)
                        .canonicalize()
                        .ok()
                        .as_ref()
                        == Some(&project.to_path_buf())
                })
                .map(|summary| summary.id)
                .ok_or_else(|| "no resumable session exists for this project".to_owned())?;
            Ok(Some(session))
        }
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

struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&self, _event: RuntimeEvent) -> Result<(), KuramaError> {
        Ok(())
    }
}

struct ToolEventSink {
    sender: mpsc::UnboundedSender<RuntimeEvent>,
}

impl EventSink for ToolEventSink {
    fn emit(&self, event: RuntimeEvent) -> Result<(), KuramaError> {
        if matches!(event, RuntimeEvent::ToolOutputDelta { .. }) {
            self.sender
                .send(event)
                .map_err(|_| KuramaError::Cancelled)?;
        }
        Ok(())
    }
}
