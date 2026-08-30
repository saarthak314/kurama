use std::{
    collections::BTreeMap,
    fmt,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
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
    model::{ModelProfile, Usage},
    policy::{ApprovalResponse, AutoBoundaries, ExecutionMode},
    runtime::{EngineCommand, RuntimeEvent},
    session::{BlobRef, EventEnvelope, SessionEvent, SessionMetadata},
    traits::{EventSink, IdGenerator, Orchestrator, SessionStore, Tool},
};
use kurama_sdk::AgentBuilder;
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::{Backend, ClearType, CrosstermBackend},
    layout::{Position, Rect},
    style::Style,
    widgets::{Block, Padding, Paragraph, Widget},
};
use tokio::sync::mpsc;

use crate::{
    args::{Args, ResumeChoice},
    commands::{Command, parse_command},
    tui::{
        CursorTrackingBackend, OnboardingState, OnboardingSubmission, Overlay, SURFACE,
        SharedBackend, TerminalGuard, TranscriptDetail, TuiState, approval_height, composer_height,
        cursor_position, main_area, render, spawn_input_thread, transcript_lines,
        visible_activity_rect,
    },
};

const TRANSCRIPT_HORIZONTAL_PADDING: usize = 2;
const MAX_TRANSCRIPT_INSERT_HEIGHT: usize = 1_024;
const INLINE_VIEWPORT_MAX_HEIGHT: u16 = 12;
const ACTIVITY_FRAME_INTERVAL: Duration = Duration::from_millis(100);

pub struct App {
    pub state: TuiState,
    engine: Option<EngineHandle>,
    runtime_events: Option<RuntimeEvents>,
    tool_events: Option<mpsc::UnboundedReceiver<RuntimeEvent>>,
    orchestrator: Option<Arc<dyn Orchestrator>>,
    session_id: Option<SessionId>,
    restart_args: Option<Args>,
    exit_requested: bool,
    control: Option<AppControl>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitSummary {
    session_id: SessionId,
    usage: Usage,
}

impl fmt::Display for ExitSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total = self
            .usage
            .input_tokens
            .saturating_add(self.usage.output_tokens);
        write!(
            formatter,
            "Token usage: total={total} input={}",
            self.usage.input_tokens
        )?;
        if self.usage.cached_input_tokens > 0 {
            write!(formatter, " (+ {} cached)", self.usage.cached_input_tokens)?;
        }
        write!(formatter, " output={}", self.usage.output_tokens)?;
        write!(
            formatter,
            "\nTo continue this session, run kurama resume {}\nSession ID: {}",
            self.session_id, self.session_id
        )
    }
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
        let active_session_missing = {
            let secrets = session_secrets
                .lock()
                .map_err(|_| "session credential store is unavailable".to_owned())?;
            matches!(active.auth, Some(AuthRef::Session)) && !secrets.contains(&active_profile)
        };
        if active_session_missing {
            let mut state =
                TuiState::credential(project.display().to_string(), active_profile.clone());
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
            if matches!(profile.auth, Some(AuthRef::Session)) && !secrets.contains(name) {
                continue;
            }
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
        let transcript_replay = replay_for_transcript(&replay, store.as_ref());
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
        state.hydrate_replay(&transcript_replay);
        if resumed_yolo {
            state.push_notice(
                Some("MODE".into()),
                "Previous run used YOLO; resumed in supervised mode",
            );
        }
        Ok(Self {
            state,
            engine: Some(engine),
            runtime_events: Some(runtime_events),
            tool_events: Some(tool_rx),
            orchestrator: Some(orchestrator),
            session_id: Some(session_id),
            restart_args: None,
            exit_requested: false,
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
            exit_requested: false,
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
            exit_requested: false,
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

    fn request_restart(&mut self, args: Args, notice: impl Into<String>) {
        self.restart_args = Some(args);
        self.state.push_notice(Some("RESTART".into()), notice);
        if self.engine.is_some() {
            self.state.queue_command(EngineCommand::Shutdown);
        }
    }

    pub const fn tool_names() -> [&'static str; 4] {
        ["bash", "read", "web-search", "write"]
    }

    pub async fn run(mut self) -> Result<Option<ExitSummary>, String> {
        let _guard = TerminalGuard::enter().map_err(|error| error.to_string())?;
        let initial_cursor = cursor_position(Duration::from_millis(100)).unwrap_or_default();
        let backend = SharedBackend::new(CursorTrackingBackend::with_cursor_position(
            CrosstermBackend::new(io::stdout()),
            initial_cursor,
        ));
        let mut terminal =
            initialize_inline_terminal(backend).map_err(|error| error.to_string())?;
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
                true,
            )
            .await?;
            let Some(args) = self.restart_args.take() else {
                clear_inline_terminal(&mut terminal).map_err(|error| error.to_string())?;
                return Ok(self.exit_requested.then(|| self.exit_summary()).flatten());
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
            self.exit_requested = true;
            self.state.queue_command(EngineCommand::Shutdown);
            return Ok(true);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.state.toggle_transcript_view();
            return Ok(false);
        }
        if self.state.transcript_view_expanded() {
            match key.code {
                KeyCode::Esc => self.state.toggle_transcript_view(),
                KeyCode::Up => self.state.scroll = self.state.scroll.saturating_add(1),
                KeyCode::Down => self.state.scroll = self.state.scroll.saturating_sub(1),
                KeyCode::PageUp => self.state.scroll = self.state.scroll.saturating_add(5),
                KeyCode::PageDown => self.state.scroll = self.state.scroll.saturating_sub(5),
                _ => {}
            }
            return Ok(false);
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
        Ok(self.exit_requested || (self.restart_args.is_some() && self.engine.is_none()))
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
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.state.composer.insert(self.state.cursor, '\n');
                self.state.cursor += 1;
            }
            KeyCode::Enter => self.submit_composer()?,
            KeyCode::Esc => {
                self.state.interrupt_active();
            }
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
                    self.state.push_error(error);
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
                            self.state.push_error(format!("unknown profile: {profile}"));
                        } else if profile == self.state.profile {
                            self.state
                                .push_error(format!("profile {profile} is already active"));
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
                        if let Some(control) = &self.control {
                            self.state
                                .push_notice(Some("PROFILES".into()), control.profiles.join(", "));
                        } else {
                            self.state.push_error("profile selection is unavailable");
                        }
                    }
                }
                Command::Connect => {
                    self.state.onboarding = OnboardingState::new();
                    self.state.overlay = Overlay::Onboarding;
                }
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
                    if let Some(control) = &self.control {
                        self.state.push_notice(
                            Some("CONTEXT".into()),
                            format!(
                                "{} token input limit; automatic compaction; session {}",
                                control.max_input_tokens,
                                self.session_id.as_ref().map_or("none", AsRef::as_ref)
                            ),
                        );
                    } else {
                        self.state.push_error("context details are unavailable");
                    }
                }
                Command::Compact => {
                    self.state.queue_command(EngineCommand::Compact);
                    self.state
                        .push_notice(Some("CONTEXT".into()), "compaction requested");
                }
                Command::Mode(mode) => {
                    if let Some(control) = &self.control {
                        control
                            .repository
                            .remember_mode(mode)
                            .map_err(|error| error.to_string())?;
                    }
                    self.state.mode = mode;
                    self.state.queue_command(EngineCommand::SetMode(mode));
                    self.state.push_notice(
                        Some("MODE".into()),
                        format!("mode {}", execution_mode_label(mode)),
                    );
                }
                Command::Exit => {
                    self.exit_requested = true;
                    self.state.queue_command(EngineCommand::Shutdown);
                }
            }
        } else if self.engine.is_none() {
            self.state
                .push_error("not connected; configure ~/.kurama/config.toml");
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
            self.state.set_thinking();
        }
        Ok(())
    }

    fn show_sessions(&mut self) -> Result<(), String> {
        let Some(control) = &self.control else {
            self.state.push_error("session listing is unavailable");
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
        let body = if sessions.is_empty() {
            "no saved sessions for this project".into()
        } else {
            let summaries = sessions
                .iter()
                .map(|session| format!("{} ({})", session.id, session.profile))
                .collect::<Vec<_>>()
                .join(", ");
            format!("sessions: {summaries}")
        };
        self.state.push_notice(Some("SESSIONS".into()), body);
        Ok(())
    }

    fn exit_summary(&self) -> Option<ExitSummary> {
        let session_id = self.session_id.clone()?;
        let usage = self
            .control
            .as_ref()
            .and_then(|control| control.store.replay(&session_id).ok())
            .map(|events| {
                events
                    .into_iter()
                    .fold(Usage::default(), |mut total, event| {
                        if let SessionEvent::ModelUsage { usage } = event.event {
                            total.input_tokens =
                                total.input_tokens.saturating_add(usage.input_tokens);
                            total.output_tokens =
                                total.output_tokens.saturating_add(usage.output_tokens);
                            total.cached_input_tokens = total
                                .cached_input_tokens
                                .saturating_add(usage.cached_input_tokens);
                        }
                        total
                    })
            })
            .unwrap_or_default();
        Some(ExitSummary { session_id, usage })
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
                        self.state.onboarding = OnboardingState::new();
                        self.state.overlay = Overlay::None;
                        self.state.push_error(error);
                    }
                }
                Ok(None) => {}
                Err(error) => self.state.push_error(error),
            },
            KeyCode::Esc => {
                self.state.onboarding = OnboardingState::new();
                self.state.overlay = Overlay::None;
            }
            _ => {}
        }
    }

    fn apply_onboarding_submission(
        &mut self,
        submission: OnboardingSubmission,
    ) -> Result<(), String> {
        let connected = self.is_connected();
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
                let http = HttpClient::try_new().map_err(|error| error.to_string())?;
                let factory = ProviderFactory::new(http, control.paths.clone(), CredentialResolver);
                let mut secrets = session_secrets
                    .lock()
                    .map_err(|_| "session credential store is unavailable".to_owned())?;
                if let Some(secret) = secret {
                    secrets.insert(name.clone(), SecretValue::new(secret));
                }
                let validation = factory.build(&name, &profile, &secrets);
                let secret = secrets.remove(&name);
                validation.map_err(|error| error.to_string())?;
                drop(secrets);
                config.profiles.insert(name.clone(), profile);
                if config.default_profile.is_none() {
                    config.default_profile = Some(name.clone());
                }
                repository
                    .write_config(&config)
                    .map_err(|error| error.to_string())?;
                if let Some(secret) = secret {
                    session_secrets
                        .lock()
                        .map_err(|_| "session credential store is unavailable".to_owned())?
                        .insert(name.clone(), secret);
                }
                if connected {
                    if let Some(control) = self.control.as_mut() {
                        control.profiles = config.profiles.keys().cloned().collect();
                    }
                    self.state.onboarding = OnboardingState::new();
                    self.state.overlay = Overlay::None;
                    self.state
                        .push_notice(Some("PROFILE".into()), format!("added profile {name}"));
                } else {
                    repository
                        .remember_project_profile(&project, &name)
                        .map_err(|error| error.to_string())?;
                    self.request_restart(
                        Args {
                            profile: Some(name.clone()),
                            yolo: self.state.mode == ExecutionMode::Yolo,
                            ..Args::default()
                        },
                        format!("connecting profile {name}"),
                    );
                }
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
            if commands
                .iter()
                .any(|command| !matches!(command, EngineCommand::Shutdown))
            {
                self.state
                    .push_error("not connected; configure ~/.kurama/config.toml");
            }
            return Ok(());
        };
        for command in commands {
            match command {
                EngineCommand::SubmitTurn {
                    text,
                    explicit_delegation,
                } => engine.submit(text, explicit_delegation).await,
                EngineCommand::ResolveApproval {
                    operation_id,
                    response,
                } => engine.resolve_approval(operation_id, response).await,
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

fn replay_for_transcript(replay: &[EventEnvelope], store: &dyn SessionStore) -> Vec<EventEnvelope> {
    let mut transcript_replay = replay.to_vec();
    for event in &mut transcript_replay {
        let SessionEvent::ToolCompleted { result, .. } = &mut event.event else {
            continue;
        };
        let Some(display_blobs) = result
            .metadata
            .get("display_blobs")
            .and_then(serde_json::Value::as_object)
        else {
            continue;
        };
        let Ok(Some(display_output)) = display_output_from_blobs(display_blobs, store) else {
            continue;
        };
        result.metadata["display_output"] = serde_json::Value::String(display_output);
    }
    transcript_replay
}

fn display_output_from_blobs(
    display_blobs: &serde_json::Map<String, serde_json::Value>,
    store: &dyn SessionStore,
) -> Result<Option<String>, String> {
    if let Some(output) = display_blob_text(display_blobs.get("output"), store)? {
        return Ok(Some(output));
    }
    let stdout = display_blob_text(display_blobs.get("stdout"), store)?;
    let stderr = display_blob_text(display_blobs.get("stderr"), store)?;
    if stdout.is_none() && stderr.is_none() {
        return Ok(None);
    }
    Ok(Some(combined_tool_output(
        stdout.as_deref().unwrap_or_default(),
        stderr.as_deref().unwrap_or_default(),
    )))
}

fn display_blob_text(
    value: Option<&serde_json::Value>,
    store: &dyn SessionStore,
) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let reference: BlobRef = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid display blob reference: {error}"))?;
    let bytes = store
        .get_blob(&reference)
        .map_err(|error| error.to_string())?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

fn combined_tool_output(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, true) => stdout.to_owned(),
        (true, false) => stderr.to_owned(),
        (true, true) => String::new(),
        (false, false) => format!("{stdout}\n[stderr]\n{stderr}"),
    }
}

fn initialize_inline_terminal<B>(mut backend: B) -> Result<Terminal<B>, B::Error>
where
    B: Backend,
{
    let rows = backend.size()?.height;
    let viewport_height = rows.min(INLINE_VIEWPORT_MAX_HEIGHT);
    let viewport_top = backend.get_cursor_position()?.y.min(rows.saturating_sub(1));
    backend.set_cursor_position(Position::new(0, viewport_top))?;
    backend.clear_region(ClearType::AfterCursor)?;
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(viewport_height),
        },
    )
}

fn resize_inline_terminal<B>(
    terminal: &mut Terminal<B>,
    width: u16,
    height: u16,
) -> Result<(), B::Error>
where
    B: Backend + Clone,
{
    let viewport_height = height.min(INLINE_VIEWPORT_MAX_HEIGHT);
    let viewport_top = height.saturating_sub(viewport_height);
    terminal.backend_mut().clear_region(ClearType::All)?;
    terminal.backend_mut().flush()?;
    replace_inline_terminal(
        terminal,
        Rect::new(0, 0, width, height),
        viewport_top,
        viewport_height,
    )
}

fn prepare_inline_frame<B>(state: &mut TuiState, terminal: &mut Terminal<B>) -> Result<(), String>
where
    B: Backend + Clone,
{
    if !state.stable_transcript().is_empty() {
        let size = terminal.size().map_err(|error| error.to_string())?;
        set_inline_viewport_height(terminal, size.height.min(INLINE_VIEWPORT_MAX_HEIGHT))
            .map_err(|error| error.to_string())?;
        commit_stable_transcript(state, terminal)?;
    }

    let size = terminal.size().map_err(|error| error.to_string())?;
    let viewport_height = desired_inline_viewport_height(state, size.width, size.height);
    set_inline_viewport_height(terminal, viewport_height).map_err(|error| error.to_string())
}

fn desired_inline_viewport_height(state: &TuiState, width: u16, height: u16) -> u16 {
    if height == 0 {
        return 0;
    }
    if state.transcript_view_expanded()
        || matches!(
            state.overlay(),
            Overlay::Onboarding
                | Overlay::Agents
                | Overlay::AgentInspect
                | Overlay::AgentMessage
                | Overlay::ConfirmAgentCancel
        )
    {
        return height.min(INLINE_VIEWPORT_MAX_HEIGHT);
    }

    let area = main_area(Rect::new(0, 0, width, height));
    let approval_visible = matches!(state.overlay(), Overlay::Approval | Overlay::ApprovalEdit);
    let input_height = if approval_visible {
        approval_height(state, area.width)
    } else {
        composer_height(state, area.width)
    }
    .max(1)
    .min(height);
    let activity_height = u16::from(
        state.overlay() == Overlay::None && state.activity().is_animated() && input_height < height,
    );
    let footer_height = u16::from(input_height.saturating_add(activity_height) < height);
    let transcript_capacity = height.saturating_sub(
        input_height
            .saturating_add(activity_height)
            .saturating_add(footer_height),
    );
    let transcript_height = transcript_lines(
        state.live_transcript(),
        area.width as usize,
        TranscriptDetail::Compact,
    )
    .len()
    .min(transcript_capacity as usize) as u16;

    input_height
        .saturating_add(activity_height)
        .saturating_add(footer_height)
        .saturating_add(transcript_height)
        .min(height)
        .min(INLINE_VIEWPORT_MAX_HEIGHT)
}

fn set_inline_viewport_height<B>(
    terminal: &mut Terminal<B>,
    viewport_height: u16,
) -> Result<(), B::Error>
where
    B: Backend + Clone,
{
    let current_area = terminal.get_frame().area();
    if current_area.height == viewport_height {
        return Ok(());
    }

    let size = terminal.size()?;
    let viewport_height = viewport_height.min(size.height);
    let viewport_top = current_area
        .top()
        .min(size.height.saturating_sub(viewport_height));
    terminal.clear()?;
    terminal.backend_mut().flush()?;
    replace_inline_terminal(
        terminal,
        Rect::new(0, 0, size.width, size.height),
        viewport_top,
        viewport_height,
    )
}

fn replace_inline_terminal<B>(
    terminal: &mut Terminal<B>,
    terminal_area: Rect,
    viewport_top: u16,
    viewport_height: u16,
) -> Result<(), B::Error>
where
    B: Backend + Clone,
{
    let mut backend = terminal.backend().clone();
    backend.set_cursor_position(Position::new(
        0,
        viewport_top.min(terminal_area.height.saturating_sub(1)),
    ))?;
    let replacement = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(viewport_height),
        },
    )?;
    *terminal = replacement;
    Ok(())
}

fn clear_inline_terminal<B>(terminal: &mut Terminal<B>) -> Result<(), B::Error>
where
    B: Backend,
{
    let viewport_top = terminal.get_frame().area().as_position();
    terminal.clear()?;
    terminal.set_cursor_position(viewport_top)?;
    terminal.backend_mut().flush()
}

fn purge_terminal_history() -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(b"\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[H")?;
    stdout.flush()
}

pub async fn run(args: Args) -> Result<Option<ExitSummary>, String> {
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
    B: Backend + Clone,
{
    run_loop(
        &mut app,
        terminal,
        &mut input,
        Some(runtime_events),
        None,
        false,
    )
    .await?;
    Ok(app)
}

fn apply_runtime_event_in_order(
    state: &mut TuiState,
    event: RuntimeEvent,
    tool_receiver: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
) -> bool {
    let mut tool_open = true;
    if matches!(
        &event,
        RuntimeEvent::ToolCompleted { .. }
            | RuntimeEvent::TurnCompleted
            | RuntimeEvent::Error { .. }
            | RuntimeEvent::Shutdown
    ) {
        loop {
            match tool_receiver.try_recv() {
                Ok(event) => state.apply_runtime_event(event),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    tool_open = false;
                    break;
                }
            }
        }
    }
    state.apply_runtime_event(event);
    tool_open
}

async fn run_loop<B>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    input: &mut mpsc::Receiver<Event>,
    runtime_events: Option<mpsc::Receiver<RuntimeEvent>>,
    tool_events: Option<mpsc::UnboundedReceiver<RuntimeEvent>>,
    commit_to_scrollback: bool,
) -> Result<(), String>
where
    B: Backend + Clone,
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
    if commit_to_scrollback {
        prepare_inline_frame(&mut app.state, terminal)?;
    }
    terminal
        .draw(|frame| render(frame, &app.state))
        .map_err(|error| error.to_string())?;

    while input_open || runtime_open || tool_open {
        let mut exit = false;
        let mut animation_tick = false;
        let terminal_area = terminal.get_frame().area();
        let animate_activity = !visible_activity_rect(terminal_area, &app.state).is_empty();
        let animation = async move {
            if animate_activity {
                tokio::time::sleep(ACTIVITY_FRAME_INTERVAL).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::pin!(animation);
        tokio::select! {
            _ = &mut animation => animation_tick = true,
            event = input.recv(), if input_open => {
                match event {
                    Some(Event::Resize(width, height)) if commit_to_scrollback => {
                        purge_terminal_history().map_err(|error| error.to_string())?;
                        app.state.reset_transcript_commit();
                        resize_inline_terminal(terminal, width, height)
                            .map_err(|error| error.to_string())?;
                    }
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
                        if tool_open {
                            tool_open = apply_runtime_event_in_order(
                                &mut app.state,
                                event,
                                &mut tool_receiver,
                            );
                        } else {
                            app.state.apply_runtime_event(event);
                        }
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
        if commit_to_scrollback && !animation_tick {
            prepare_inline_frame(&mut app.state, terminal)?;
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

fn commit_stable_transcript<B>(
    state: &mut TuiState,
    terminal: &mut Terminal<B>,
) -> Result<(), String>
where
    B: Backend,
{
    terminal.autoresize().map_err(|error| error.to_string())?;
    let committed_end = state.stable_transcript_end();
    if state.stable_transcript().is_empty() {
        return Ok(());
    }

    let terminal_width = terminal.get_frame().area().width as usize;
    if terminal_width <= TRANSCRIPT_HORIZONTAL_PADDING * 2 {
        return Ok(());
    }
    let content_width = terminal_width
        .saturating_sub(TRANSCRIPT_HORIZONTAL_PADDING * 2)
        .max(1);
    let mut lines = transcript_lines(
        state.stable_transcript(),
        content_width,
        TranscriptDetail::Compact,
    );
    if state.transcript.len() > state.live_transcript().len()
        && matches!(
            state.stable_transcript().first(),
            Some(crate::tui::TranscriptEntry::UserTurn { .. })
        )
    {
        lines.insert(0, ratatui::text::Line::from(""));
    }

    for chunk in lines.chunks(MAX_TRANSCRIPT_INSERT_HEIGHT) {
        terminal
            .insert_before(chunk.len() as u16, |buffer| {
                buffer.set_style(*buffer.area(), Style::default().bg(SURFACE));
                Paragraph::new(chunk.to_vec())
                    .block(Block::default().padding(Padding::new(
                        TRANSCRIPT_HORIZONTAL_PADDING as u16,
                        TRANSCRIPT_HORIZONTAL_PADDING as u16,
                        0,
                        0,
                    )))
                    .render(*buffer.area(), buffer);
            })
            .map_err(|error| error.to_string())?;
    }

    state.mark_transcript_committed(committed_end);
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
            .filter(|(_, route)| profiles.contains_key(&route.profile))
            .map(|(role, route)| (role.clone(), route.profile.clone()))
            .collect(),
        role_escalations: config
            .roles
            .iter()
            .filter(|(_, route)| profiles.contains_key(&route.profile))
            .map(|(role, route)| {
                (
                    role.clone(),
                    route
                        .escalation_profiles
                        .iter()
                        .filter(|profile| profiles.contains_key(*profile))
                        .cloned()
                        .collect(),
                )
            })
            .collect(),
        profile_escalations: config
            .profiles
            .iter()
            .filter(|(name, _)| profiles.contains_key(*name))
            .map(|(name, profile)| {
                (
                    name.clone(),
                    profile
                        .escalation_profiles
                        .iter()
                        .filter(|profile| profiles.contains_key(*profile))
                        .cloned()
                        .collect(),
                )
            })
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

fn execution_mode_label(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Supervised => "supervised",
        ExecutionMode::Auto => "auto",
        ExecutionMode::Yolo => "yolo",
    }
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

#[cfg(test)]
mod tests {
    use crossterm::event::{MouseEvent, MouseEventKind};
    use kurama_protocol::{
        id::{CallId, OperationId},
        tool::ToolResult,
    };
    use ratatui::{
        TerminalOptions, Viewport,
        backend::TestBackend,
        layout::{Position, Rect},
        style::Color,
    };

    use super::*;
    use crate::tui::{ActivityState, TranscriptEntry, visible_activity_rect};

    fn test_app() -> App {
        App {
            state: TuiState::new("fixture", "frontier", ".", ExecutionMode::Supervised),
            engine: None,
            runtime_events: None,
            tool_events: None,
            orchestrator: None,
            session_id: None,
            restart_args: None,
            exit_requested: false,
            control: None,
        }
    }

    #[test]
    fn exit_command_requests_shutdown_and_returns_resume_details() {
        let mut app = test_app();
        app.session_id = Some(SessionId::from("s_exit"));
        app.state.composer = "/exit".into();
        app.state.cursor = app.state.composer.len();

        let exit = app
            .handle_event(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("submit exit command");

        assert!(exit);
        assert!(matches!(
            app.state.sent_commands().last(),
            Some(EngineCommand::Shutdown)
        ));
        assert_eq!(
            app.exit_summary().expect("exit summary").to_string(),
            "Token usage: total=0 input=0 output=0\n\
To continue this session, run kurama resume s_exit\n\
Session ID: s_exit"
        );
    }

    #[test]
    fn exit_summary_includes_cached_usage() {
        let summary = ExitSummary {
            session_id: SessionId::from("s_cached"),
            usage: Usage {
                input_tokens: 120,
                output_tokens: 30,
                cached_input_tokens: 80,
            },
        };

        assert_eq!(
            summary.to_string(),
            "Token usage: total=150 input=120 (+ 80 cached) output=30\n\
To continue this session, run kurama resume s_cached\n\
Session ID: s_cached"
        );
    }

    #[test]
    fn ctrl_o_toggles_expanded_transcript_view_and_escape_closes_it() {
        let mut app = test_app();

        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
        )))
        .expect("open transcript view");
        assert!(app.state.transcript_view_expanded());
        assert!(app.state.composer.is_empty());

        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .expect("close transcript view");
        assert!(!app.state.transcript_view_expanded());
    }

    #[test]
    fn expanded_transcript_view_uses_arrow_and_page_scrolling() {
        let mut app = test_app();
        app.state.toggle_transcript_view();

        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)))
            .expect("scroll transcript up");
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page transcript up");
        assert_eq!(app.state.scroll, 6);

        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
            .expect("scroll transcript down");
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::PageDown,
            KeyModifiers::NONE,
        )))
        .expect("page transcript down");
        assert_eq!(app.state.scroll, 0);
    }

    #[test]
    fn disconnected_submit_is_a_transcript_error() {
        let mut app = test_app();
        app.state.composer = "inspect".into();
        app.state.cursor = app.state.composer.len();

        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("submit disconnected turn");

        assert_eq!(app.state.activity(), &ActivityState::Idle);
        assert!(app.state.transcript.iter().any(
            |entry| matches!(entry, TranscriptEntry::Error { body } if body.contains("not connected"))
        ));
    }

    #[test]
    fn escape_interrupts_visible_active_work() {
        let mut app = test_app();
        app.state.set_thinking();

        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .expect("interrupt active turn");

        assert_eq!(app.state.activity(), &ActivityState::Interrupted);
        assert!(matches!(
            app.state.sent_commands().last(),
            Some(EngineCommand::CancelTurn)
        ));
    }

    #[test]
    fn higher_priority_surface_consumes_escape_without_interrupting() {
        let mut app = test_app();
        app.state.set_thinking();
        app.state.toggle_transcript_view();

        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .expect("close transcript view");

        assert!(matches!(
            app.state.activity(),
            ActivityState::Thinking { .. }
        ));
        assert!(app.state.sent_commands().is_empty());
    }

    #[test]
    fn animation_wakes_only_for_visible_active_work() {
        let mut app = test_app();
        assert_eq!(ACTIVITY_FRAME_INTERVAL, Duration::from_millis(100));
        let normal_area = Rect::new(0, 0, 80, 24);
        assert!(visible_activity_rect(normal_area, &app.state).is_empty());

        app.state.set_thinking();
        assert!(visible_activity_rect(Rect::new(0, 0, 80, 0), &app.state).is_empty());
        assert!(visible_activity_rect(Rect::new(0, 0, 80, 1), &app.state).is_empty());
        assert!(!visible_activity_rect(normal_area, &app.state).is_empty());

        app.state.toggle_transcript_view();
        assert!(visible_activity_rect(normal_area, &app.state).is_empty());

        app.state.toggle_transcript_view();
        app.state.overlay = Overlay::Approval;
        assert!(visible_activity_rect(normal_area, &app.state).is_empty());

        app.state.overlay = Overlay::Agents;
        assert!(visible_activity_rect(normal_area, &app.state).is_empty());
    }

    #[test]
    fn queued_tool_delta_is_applied_before_completion() {
        let mut state = TuiState::new("fixture", "frontier", ".", ExecutionMode::Supervised);
        let (tool_sender, mut tool_receiver) = mpsc::unbounded_channel();
        tool_sender
            .send(RuntimeEvent::ToolOutputDelta {
                call_id: CallId::from("call_1"),
                stream: "stdout".into(),
                chunk: "partial output".into(),
            })
            .expect("queue tool delta");

        let mut result = ToolResult::success(CallId::from("call_1"), "final output");
        result.metadata = serde_json::json!({"tool_name": "bash"});
        apply_runtime_event_in_order(
            &mut state,
            RuntimeEvent::ToolCompleted {
                operation_id: OperationId::from("operation_1"),
                result,
            },
            &mut tool_receiver,
        );

        while let Ok(event) = tool_receiver.try_recv() {
            state.apply_runtime_event(event);
        }

        assert_eq!(state.transcript.len(), 1);
        assert!(matches!(
            &state.transcript[0],
            TranscriptEntry::ToolCall(tool) if tool.output == "final output"
        ));
    }

    #[test]
    fn queued_tool_delta_is_applied_before_runtime_error() {
        let mut state = TuiState::new("fixture", "frontier", ".", ExecutionMode::Supervised);
        let (tool_sender, mut tool_receiver) = mpsc::unbounded_channel();
        tool_sender
            .send(RuntimeEvent::ToolOutputDelta {
                call_id: CallId::from("call_1"),
                stream: "stdout".into(),
                chunk: "partial output".into(),
            })
            .expect("queue tool delta");

        apply_runtime_event_in_order(
            &mut state,
            RuntimeEvent::Error {
                message: "cancelled".into(),
            },
            &mut tool_receiver,
        );

        assert!(tool_receiver.try_recv().is_err());
        assert_eq!(state.transcript.len(), 2);
        assert!(matches!(
            &state.transcript[..],
            [TranscriptEntry::ToolCall(tool), TranscriptEntry::Error { body }]
                if tool.output == "partial output" && body == "cancelled"
        ));
        assert_eq!(state.stable_transcript_end(), 2);
    }

    #[test]
    fn normal_view_leaves_page_keys_to_native_scrollback() {
        let mut app = App {
            state: TuiState::new("fixture", "frontier", ".", ExecutionMode::Supervised),
            engine: None,
            runtime_events: None,
            tool_events: None,
            orchestrator: None,
            session_id: None,
            restart_args: None,
            exit_requested: false,
            control: None,
        };
        app.state.scroll = usize::from(u16::MAX);

        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .expect("page up");

        assert_eq!(app.state.scroll, usize::from(u16::MAX));
    }

    #[test]
    fn mouse_events_preserve_native_terminal_selection() {
        let mut app = App {
            state: TuiState::new("fixture", "frontier", ".", ExecutionMode::Supervised),
            engine: None,
            runtime_events: None,
            tool_events: None,
            orchestrator: None,
            session_id: None,
            restart_args: None,
            exit_requested: false,
            control: None,
        };

        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }))
        .expect("scroll up");
        assert_eq!(app.state.scroll, 0);

        app.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }))
        .expect("scroll down");
        assert_eq!(app.state.scroll, 0);
    }

    #[test]
    fn inline_terminal_initialization_starts_at_the_invocation_cursor() {
        let mut lines = vec![" ".repeat(80); 40];
        lines[0] = "stale shell prompt".into();
        lines[4] = "stale viewport content".into();
        lines[30] = "stale lower content".into();
        let mut backend = TestBackend::with_lines(lines);
        backend
            .set_cursor_position(Position::new(0, 4))
            .expect("position inline viewport");

        let mut terminal = initialize_inline_terminal(backend).expect("initialize terminal");
        let visible = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert_eq!(terminal.get_frame().area(), Rect::new(0, 4, 80, 12));
        assert!(visible.contains("stale shell prompt"));
        assert!(!visible.contains("stale viewport content"));
        assert!(!visible.contains("stale lower content"));
    }

    #[test]
    fn committed_history_sits_directly_above_the_idle_composer() {
        let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        state.push_user("hi");
        state.push_assistant("hello from kurama");
        let mut terminal = initialize_inline_terminal(TestBackend::new(80, 24))
            .expect("initialize inline terminal");

        prepare_inline_frame(&mut state, &mut terminal).expect("prepare inline frame");
        terminal
            .draw(|frame| render(frame, &state))
            .expect("draw idle frame");

        assert_eq!(terminal.get_frame().area().height, 2);
        let rows = terminal
            .backend()
            .buffer()
            .content()
            .chunks(80)
            .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let answer_row = rows
            .iter()
            .position(|row| row.contains("hello from kurama"))
            .expect("committed answer row");
        let composer_row = rows
            .iter()
            .position(|row| row.contains("Ask Kurama"))
            .expect("composer row");

        assert_eq!(composer_row, answer_row + 1, "{rows:#?}");
    }

    #[test]
    fn inline_resize_does_not_commit_live_viewport_to_scrollback() {
        let state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        let mut terminal = initialize_inline_terminal(TestBackend::new(80, 24))
            .expect("initialize inline terminal");
        terminal
            .draw(|frame| render(frame, &state))
            .expect("draw initial viewport");

        terminal.backend_mut().resize(52, 12);
        resize_inline_terminal(&mut terminal, 52, 12).expect("shrink inline terminal");
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 0, 52, 12));
        terminal
            .draw(|frame| render(frame, &state))
            .expect("draw narrow viewport");

        terminal.backend_mut().resize(100, 30);
        resize_inline_terminal(&mut terminal, 100, 30).expect("grow inline terminal");
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 18, 100, 12));
        terminal
            .draw(|frame| render(frame, &state))
            .expect("draw wide viewport");

        let text = terminal
            .backend()
            .scrollback()
            .content()
            .iter()
            .chain(terminal.backend().buffer().content().iter())
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(text.matches("Ask Kurama").count(), 1, "{text}");
        assert_eq!(text.matches("work/model").count(), 1, "{text}");
    }

    #[test]
    fn clearing_inline_terminal_removes_the_live_ui_before_exit_output() {
        let state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        let mut terminal = initialize_inline_terminal(TestBackend::new(80, 24))
            .expect("initialize inline terminal");
        terminal
            .draw(|frame| render(frame, &state))
            .expect("draw live viewport");

        clear_inline_terminal(&mut terminal).expect("clear live viewport");

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!text.contains("Ask Kurama"), "{text}");
        assert_eq!(
            terminal
                .backend_mut()
                .get_cursor_position()
                .expect("exit cursor"),
            Position::ORIGIN
        );
    }

    #[tokio::test]
    async fn completed_transcript_is_inserted_above_the_inline_viewport() {
        let mut app = App {
            state: TuiState::new("fixture", "frontier", ".", ExecutionMode::Supervised),
            engine: None,
            runtime_events: None,
            tool_events: None,
            orchestrator: None,
            session_id: None,
            restart_args: None,
            exit_requested: false,
            control: None,
        };
        app.state.push_user("committed question");

        let mut backend = TestBackend::new(80, 16);
        backend
            .set_cursor_position(Position::new(0, 4))
            .expect("position inline viewport");
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(8),
            },
        )
        .expect("inline terminal");
        let (input_sender, mut input) = mpsc::channel(1);
        let (runtime_sender, runtime_events) = mpsc::channel(1);
        drop(input_sender);
        drop(runtime_sender);

        run_loop(
            &mut app,
            &mut terminal,
            &mut input,
            Some(runtime_events),
            None,
            true,
        )
        .await
        .expect("run inline terminal");
        let inserted_row = (0..16)
            .find(|y| {
                (0..80)
                    .map(|x| {
                        terminal
                            .backend()
                            .buffer()
                            .cell((x, *y))
                            .expect("inserted cell")
                            .symbol()
                    })
                    .collect::<String>()
                    .contains("› committed question")
            })
            .expect("committed transcript row");

        assert!((0..80).all(|x| {
            terminal
                .backend()
                .buffer()
                .cell((x, inserted_row))
                .expect("inserted background")
                .bg
                == Color::Reset
        }));
        assert!(app.state.live_transcript().is_empty());
    }

    #[tokio::test]
    async fn fullscreen_run_with_keeps_transcript_in_the_live_view() {
        let mut app = App {
            state: TuiState::new("fixture", "frontier", ".", ExecutionMode::Supervised),
            engine: None,
            runtime_events: None,
            tool_events: None,
            orchestrator: None,
            session_id: None,
            restart_args: None,
            exit_requested: false,
            control: None,
        };
        app.state.push_user("visible fullscreen question");
        let mut terminal = Terminal::new(TestBackend::new(80, 16)).expect("fullscreen terminal");
        let (input_sender, input) = mpsc::channel(1);
        let (runtime_sender, runtime_events) = mpsc::channel(1);
        drop(input_sender);
        drop(runtime_sender);

        let app = run_with(app, &mut terminal, input, runtime_events)
            .await
            .expect("run fullscreen terminal");
        let visible = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(visible.contains("visible fullscreen question"));
        assert_eq!(app.state.live_transcript().len(), 1);
    }

    #[test]
    fn transcript_commit_uses_current_terminal_width_after_resize() {
        let mut backend = TestBackend::new(30, 16);
        backend
            .set_cursor_position(Position::new(0, 4))
            .expect("position inline viewport");
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(8),
            },
        )
        .expect("inline terminal");
        terminal.backend_mut().resize(60, 16);
        let mut state = TuiState::new("fixture", "frontier", ".", ExecutionMode::Supervised);
        state.push_user("123456789012345678901234567890");

        commit_stable_transcript(&mut state, &mut terminal).expect("commit transcript");
        let visible = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(visible.contains("› 123456789012345678901234567890"));
    }

    #[test]
    fn tiny_inline_terminal_defers_invisible_transcript_commit() {
        let mut backend = TestBackend::new(4, 8);
        backend
            .set_cursor_position(Position::new(0, 2))
            .expect("position inline viewport");
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(4),
            },
        )
        .expect("inline terminal");
        let mut state = TuiState::new("fixture", "frontier", ".", ExecutionMode::Supervised);
        state.push_user("defer me");

        commit_stable_transcript(&mut state, &mut terminal).expect("defer transcript");

        assert_eq!(state.live_transcript().len(), 1);
    }
}
