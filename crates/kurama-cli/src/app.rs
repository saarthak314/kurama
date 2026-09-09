use std::{
    collections::BTreeMap,
    fmt,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crossterm::{
    event::{Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use kurama_adapters::{
    AppPaths, BashTool, ConfigRepository, CredentialResolver, FsSessionStore, HttpClient,
    JsonSearchBackend, OpenAiNativeSearch, ProviderFactory, ReadTool, SearchBackend, SecretValue,
    SessionSecrets, WebSearchTool, WriteTool,
};
use kurama_protocol::{
    KuramaError,
    config::{
        AuthRef, KuramaConfig, OrchestrationConfig, ProfileConfig, ProfileKind, SearchConfig,
    },
    id::SessionId,
    model::{ModelProfile, Usage},
    policy::{ApprovalResponse, AutoBoundaries, ExecutionMode},
    runtime::{EngineCommand, RuntimeEvent},
    session::{BlobRef, EventEnvelope, SessionEvent, SessionMetadata},
    traits::{EventSink, Orchestrator, SessionStore, Tool},
};
use kurama_sdk::{Agent, Events, Handle};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::{Backend, ClearType, CrosstermBackend},
    layout::{Position, Rect},
    style::Style,
    text::Line,
    widgets::{Block, Padding, Paragraph, Widget},
};
use tokio::sync::mpsc;

use crate::{
    args::{Args, ResumeChoice},
    commands::{Command, command_missing_required_arguments, parse_command},
    tui::{
        CursorTrackingBackend, OnboardingState, OnboardingSubmission, Overlay, SURFACE,
        SharedBackend, TerminalGuard, TranscriptDetail, TuiState, approval_height,
        command_palette_height, composer_cursor_vertical, composer_height, main_area, queue_height,
        render_with_transcript, spawn_input_thread, transcript_lines, visible_activity_rect,
    },
};

#[cfg(test)]
use crate::tui::render;

const TRANSCRIPT_HORIZONTAL_PADDING: usize = 2;
const MAX_TRANSCRIPT_INSERT_HEIGHT: usize = 1_024;
const INLINE_VIEWPORT_MAX_HEIGHT: u16 = 12;
const TOOL_EVENT_CAPACITY: usize = 64;
const MAX_PASTE_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const READY_EVENT_BATCH_LIMIT: usize = 128;
const STREAM_REDRAW_INTERVAL: Duration = Duration::from_millis(33);
const ACTIVITY_FRAME_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Default)]
struct TranscriptRenderCache {
    width: Option<usize>,
    lines: Vec<Line<'static>>,
}

impl TranscriptRenderCache {
    fn invalidate(&mut self) {
        self.width = None;
        self.lines.clear();
    }

    fn prepare(&mut self, state: &TuiState, frame_area: Rect) {
        if state.transcript_view_expanded() {
            self.invalidate();
            return;
        }

        let width = main_area(frame_area).width as usize;
        if self.width == Some(width) {
            return;
        }
        self.lines = transcript_lines(state.live_transcript(), width, TranscriptDetail::Compact);
        self.width = Some(width);
    }

    fn lines(&self) -> Option<&[Line<'static>]> {
        self.width.map(|_| self.lines.as_slice())
    }
}

#[derive(Clone, Copy)]
struct ResizeMode {
    replay: bool,
    purge_history: bool,
}

#[derive(Default)]
struct AltOverlay {
    active: bool,
    saved: Option<Rect>,
}

impl ResizeMode {
    const PRESERVE: Self = Self {
        replay: false,
        purge_history: false,
    };
    const PURGE_AND_REPLAY: Self = Self {
        replay: true,
        purge_history: true,
    };
    #[cfg(test)]
    const REPLAY: Self = Self {
        replay: true,
        purge_history: false,
    };
}

pub struct App {
    pub state: TuiState,
    engine: Option<Handle>,
    runtime_events: Option<Events>,
    tool_events: Option<mpsc::Receiver<RuntimeEvent>>,
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
            "\nResume with kurama resume {}\nSession ID: {}",
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
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        let startup_project = project_display_path(&project, home.as_deref());
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
            let mut state = TuiState::onboarding(project.display().to_string());
            state.prepend_startup(env!("CARGO_PKG_VERSION"), startup_project.clone());
            return Ok(Self::disconnected(
                state,
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
            let mut state = TuiState::onboarding(project.display().to_string());
            state.prepend_startup(env!("CARGO_PKG_VERSION"), startup_project.clone());
            return Ok(Self::disconnected(
                state,
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
            state.prepend_startup(env!("CARGO_PKG_VERSION"), startup_project.clone());
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
        let mut agent = Agent::new()
            .workspace(project.clone())
            .mode(mode)
            .active_profile(active_profile.clone())
            .store(store.clone())
            .config(&config)
            .orchestrate();
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
            agent = agent.profile(model_profile, backend);
        }
        let search_backend = search_backend(
            &config,
            active_profile.as_str(),
            active,
            &secrets,
            credentials,
            http.clone(),
        )?;
        drop(secrets);
        let (tool_tx, tool_rx) = mpsc::channel(TOOL_EVENT_CAPACITY);
        let tool_sink: Arc<dyn EventSink> = Arc::new(ToolEventSink { sender: tool_tx });
        let agent = agent
            .tools(standard_tools(http, search_backend, Some(tool_sink)))
            .build()
            .map_err(|error| error.to_string())?;
        let orchestrator = agent.orchestrator();

        let session_id = resume_id.unwrap_or_else(|| agent.allocate_session_id());
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
        let (engine, runtime_events) = agent
            .launch(metadata, replay)
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
        state.max_input_tokens = active.max_input_tokens;
        state.hydrate_replay(&transcript_replay);
        state.refresh_git_branch();
        state.prepend_startup(env!("CARGO_PKG_VERSION"), startup_project);
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
        engine: Handle,
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
        purge_terminal_history().map_err(|error| error.to_string())?;
        let backend = SharedBackend::new(CursorTrackingBackend::new(CrosstermBackend::new(
            io::stdout(),
        )));
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
                ResizeMode::PURGE_AND_REPLAY,
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
        let key = match event {
            Event::Paste(text) => {
                match self.state.overlay {
                    Overlay::None if self.state.history_search_active() => {
                        for character in text.chars() {
                            if !character.is_control() {
                                self.state.push_history_search_char(character);
                            }
                        }
                    }
                    Overlay::None if !self.state.transcript_view_expanded() => {
                        self.ingest_composer_paste(&text);
                    }
                    Overlay::ApprovalEdit => self.state.insert_approval_text(&text),
                    Overlay::Onboarding => self.state.onboarding.insert_str(&text),
                    Overlay::AgentMessage => self.state.insert_agent_message(&text),
                    _ => {}
                }
                return Ok(false);
            }
            Event::Key(key) => key,
            _ => return Ok(false),
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(self.handle_ctrl_c());
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            if self.state.overlay() != Overlay::None {
                return Ok(false);
            }
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
                KeyCode::Home => {
                    let width = self.state.composer_inner_width.get().max(8) as usize;
                    let rendered = crate::tui::transcript_lines(
                        &self.state.transcript,
                        width,
                        crate::tui::TranscriptDetail::Expanded,
                    );
                    let viewport = self.state.viewport_height.get().max(1) as usize;
                    self.state.scroll = rendered.len().saturating_sub(viewport);
                }
                KeyCode::End => self.state.scroll = 0,
                KeyCode::Char('{') => self
                    .state
                    .jump_user_turn(-1, self.state.composer_inner_width.get().max(8) as usize),
                KeyCode::Char('}') => self
                    .state
                    .jump_user_turn(1, self.state.composer_inner_width.get().max(8) as usize),
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
            Overlay::Todos => {
                if key.code == KeyCode::Esc {
                    self.state.close_overlay();
                }
            }
            Overlay::Shortcuts => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter) {
                    self.state.close_overlay();
                }
            }
            Overlay::None => self.handle_main_key(key)?,
        }
        Ok(self.exit_requested || (self.restart_args.is_some() && self.engine.is_none()))
    }

    fn handle_ctrl_c(&mut self) -> bool {
        if self.state.overlay() == Overlay::Onboarding {
            self.state.onboarding = OnboardingState::new();
            self.state.overlay = Overlay::None;
            return false;
        }
        if matches!(
            self.state.overlay(),
            Overlay::Shortcuts
                | Overlay::Agents
                | Overlay::Todos
                | Overlay::AgentInspect
                | Overlay::AgentMessage
                | Overlay::ConfirmAgentCancel
        ) {
            self.state.close_overlay();
            return false;
        }
        if self.state.interrupt_active() {
            return false;
        }
        if !self.state.composer.is_empty() {
            self.state.clear_composer();
            return false;
        }
        self.exit_requested = true;
        self.state.queue_command(EngineCommand::Shutdown);
        true
    }

    fn handle_main_key(&mut self, key: KeyEvent) -> Result<(), String> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.state.history_search_active() {
                if key.code == KeyCode::Char('r') {
                    self.state.start_history_search();
                }
                return Ok(());
            }
            match key.code {
                KeyCode::Char('a') => self.state.cursor_home(),
                KeyCode::Char('e') => self.state.cursor_end(),
                KeyCode::Char('k') => self.state.kill_to_end(),
                KeyCode::Char('u') => self.state.kill_to_start(),
                KeyCode::Char('w') => self.state.kill_previous_word(),
                KeyCode::Char('j') => {
                    self.state.composer.insert(self.state.cursor, '\n');
                    self.state.cursor += 1;
                    self.state.composer_edited();
                }
                KeyCode::Char('l') => self.state.scroll = 0,
                KeyCode::Char('t') => self.state.toggle_todos(),
                KeyCode::Char('r') => self.state.start_history_search(),
                KeyCode::Char('d') => {
                    if self.state.composer.is_empty() {
                        self.exit_requested = true;
                        self.state.queue_command(EngineCommand::Shutdown);
                    } else if self.state.cursor < self.state.composer.len() {
                        let next = self.state.cursor
                            + self.state.composer[self.state.cursor..]
                                .chars()
                                .next()
                                .map(char::len_utf8)
                                .unwrap_or(0);
                        self.state.composer.drain(self.state.cursor..next);
                        self.state.composer_edited();
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        if self.state.history_search_active() {
            self.handle_history_search_key(key);
            return Ok(());
        }
        match key.code {
            KeyCode::BackTab => self.cycle_mode()?,
            KeyCode::Home => self.state.cursor_home(),
            KeyCode::End => self.state.cursor_end(),
            KeyCode::Char('?') if self.state.composer.is_empty() => {
                self.state.open_shortcuts();
            }
            KeyCode::Char(character) => {
                self.state.composer.insert(self.state.cursor, character);
                self.state.cursor += character.len_utf8();
                self.state.composer_edited();
            }
            KeyCode::Backspace if self.state.cursor > 0 => {
                let previous = self.state.composer[..self.state.cursor]
                    .char_indices()
                    .last()
                    .map(|(index, _)| index)
                    .unwrap_or(0);
                self.state.composer.drain(previous..self.state.cursor);
                self.state.cursor = previous;
                self.state.composer_edited();
            }
            KeyCode::Delete if self.state.cursor < self.state.composer.len() => {
                let next = self.state.cursor
                    + self.state.composer[self.state.cursor..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                self.state.composer.drain(self.state.cursor..next);
                self.state.composer_edited();
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
                self.state.composer_edited();
            }
            KeyCode::Up if self.state.select_previous_file() => {}
            KeyCode::Up if self.state.select_previous_command() => {}
            KeyCode::Up => {
                if let Some(cursor) = composer_cursor_vertical(
                    &self.state.composer,
                    self.state.cursor,
                    self.state.composer_inner_width.get().max(4) as usize,
                    -1,
                ) {
                    self.state.cursor = cursor;
                } else {
                    self.state.history_previous();
                }
            }
            KeyCode::Down if self.state.select_next_file() => {}
            KeyCode::Down if self.state.select_next_command() => {}
            KeyCode::Down => {
                if let Some(cursor) = composer_cursor_vertical(
                    &self.state.composer,
                    self.state.cursor,
                    self.state.composer_inner_width.get().max(4) as usize,
                    1,
                ) {
                    self.state.cursor = cursor;
                } else {
                    self.state.history_next();
                }
            }
            KeyCode::Tab if self.state.complete_selected_file() => {}
            KeyCode::Tab if self.state.selected_command().is_some() => {
                self.state.complete_selected_command();
            }
            KeyCode::Enter if self.state.complete_selected_file() => {}
            KeyCode::Enter => {
                if let Some(selected) = self.state.selected_command() {
                    self.state.complete_selected_command();
                    if selected.requires_arguments
                        && command_missing_required_arguments(&self.state.composer)
                    {
                        return Ok(());
                    }
                }
                self.submit_composer()?;
            }
            KeyCode::Esc if self.state.cancel_history_search() => {}
            KeyCode::Esc if !self.state.dismiss_command_palette() => {
                if !self.state.interrupt_active() {
                    self.state.pop_queued_follow_up();
                }
            }
            KeyCode::Esc => {}
            _ => {}
        }
        Ok(())
    }

    fn submit_composer(&mut self) -> Result<(), String> {
        if command_missing_required_arguments(&self.state.composer) {
            return Ok(());
        }
        let text = std::mem::take(&mut self.state.composer);
        self.state.cursor = 0;
        self.state.composer_edited();
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
                Command::Todo => self.state.open_todos(),
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
                Command::Status => self.push_status_notice(),
                Command::Copy => self.copy_last_assistant(),
                Command::Diff => self.push_diff_notice(),
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
                Command::Help => {
                    let body = crate::commands::COMMAND_SPECS
                        .iter()
                        .map(|spec| format!("/{} — {}", spec.name, spec.description))
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.state.push_notice(Some("HELP".into()), body);
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
            let explicit_delegation = self
                .orchestrator
                .as_ref()
                .is_some_and(|orchestrator| orchestrator.explicit_delegation(trimmed));
            self.state.remember_prompt(trimmed);
            self.state.submit_turn(trimmed, explicit_delegation);
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
            KeyCode::Char(digit)
                if self.state.onboarding.is_selecting_connection() && digit.is_ascii_digit() =>
            {
                if let Some(index) = digit.to_digit(10) {
                    self.state
                        .onboarding
                        .select_index(index.saturating_sub(1) as usize);
                }
            }
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
                Err(error) => self.state.onboarding.set_error(error),
            },
            KeyCode::Esc if !self.state.onboarding.go_back() && self.is_connected() => {
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
        let unmodified = key.modifiers.is_empty();
        match (self.state.overlay, key.code) {
            (Overlay::Approval, KeyCode::Char('a')) if unmodified => {
                self.state.resolve_approval(ApprovalResponse::ApproveOnce)
            }
            (Overlay::Approval, KeyCode::Char('s')) if unmodified => self
                .state
                .resolve_approval(ApprovalResponse::ApproveSession),
            (Overlay::Approval, KeyCode::Char('d')) if unmodified => {
                self.state.resolve_approval(ApprovalResponse::Deny)
            }
            (Overlay::Approval, KeyCode::Char('e')) if unmodified => {
                self.state.begin_approval_edit()
            }
            (Overlay::Approval, KeyCode::Up) => {
                if let Some(approval) = &mut self.state.approval {
                    approval.select_previous();
                }
            }
            (Overlay::Approval, KeyCode::Down) => {
                if let Some(approval) = &mut self.state.approval {
                    approval.select_next();
                }
            }
            (Overlay::Approval, KeyCode::Enter) if unmodified => {
                match self
                    .state
                    .approval
                    .as_ref()
                    .map(|approval| approval.selected)
                {
                    Some(0) => self.state.resolve_approval(ApprovalResponse::ApproveOnce),
                    Some(1) => self
                        .state
                        .resolve_approval(ApprovalResponse::ApproveSession),
                    Some(2) => self.state.resolve_approval(ApprovalResponse::Deny),
                    Some(3) => self.state.begin_approval_edit(),
                    _ => {}
                }
            }
            (Overlay::ApprovalEdit, KeyCode::Char(character))
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(approval) = &mut self.state.approval {
                    let mut encoded = [0_u8; 4];
                    approval.insert_str(character.encode_utf8(&mut encoded));
                }
            }
            (Overlay::ApprovalEdit, KeyCode::Backspace) => {
                if let Some(approval) = &mut self.state.approval {
                    approval.backspace();
                }
            }
            (Overlay::ApprovalEdit, KeyCode::Delete) => {
                if let Some(approval) = &mut self.state.approval {
                    approval.delete();
                }
            }
            (Overlay::ApprovalEdit, KeyCode::Left) => {
                if let Some(approval) = &mut self.state.approval {
                    approval.move_left();
                }
            }
            (Overlay::ApprovalEdit, KeyCode::Right) => {
                if let Some(approval) = &mut self.state.approval {
                    approval.move_right();
                }
            }
            (Overlay::ApprovalEdit, KeyCode::Up) => {
                if let Some(approval) = &mut self.state.approval {
                    approval.move_up();
                }
            }
            (Overlay::ApprovalEdit, KeyCode::Down) => {
                if let Some(approval) = &mut self.state.approval {
                    approval.move_down();
                }
            }
            (Overlay::ApprovalEdit, KeyCode::Home) => {
                if let Some(approval) = &mut self.state.approval {
                    approval.move_home();
                }
            }
            (Overlay::ApprovalEdit, KeyCode::End) => {
                if let Some(approval) = &mut self.state.approval {
                    approval.move_end();
                }
            }
            (Overlay::ApprovalEdit, KeyCode::Enter)
                if key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                if let Some(approval) = &mut self.state.approval {
                    approval.insert_str("\n");
                }
            }
            (Overlay::ApprovalEdit, KeyCode::Enter) => {
                let _ = self.state.submit_approval_edit();
            }
            (Overlay::Approval, KeyCode::Esc) => {
                self.state.resolve_approval(ApprovalResponse::Deny)
            }
            (Overlay::ApprovalEdit, KeyCode::Esc) => self.state.close_overlay(),
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
            (Overlay::AgentMessage, KeyCode::Char(character))
                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let mut encoded = [0_u8; 4];
                self.state
                    .insert_agent_message(character.encode_utf8(&mut encoded));
            }
            (Overlay::AgentMessage, KeyCode::Backspace) => {
                if self.state.agent_message_cursor > 0 {
                    let previous = self.state.agent_message[..self.state.agent_message_cursor]
                        .char_indices()
                        .next_back()
                        .map(|(index, _)| index)
                        .unwrap_or(0);
                    self.state
                        .agent_message
                        .drain(previous..self.state.agent_message_cursor);
                    self.state.agent_message_cursor = previous;
                }
            }
            (Overlay::AgentMessage, KeyCode::Left) => {
                if let Some((index, _)) = self.state.agent_message
                    [..self.state.agent_message_cursor]
                    .char_indices()
                    .next_back()
                {
                    self.state.agent_message_cursor = index;
                }
            }
            (Overlay::AgentMessage, KeyCode::Right)
                if self.state.agent_message_cursor < self.state.agent_message.len() =>
            {
                let next = self.state.agent_message[self.state.agent_message_cursor..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(0);
                self.state.agent_message_cursor += next;
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

    fn handle_history_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.state.cancel_history_search();
            }
            KeyCode::Enter => {
                self.state.accept_history_search();
            }
            KeyCode::Up => {
                self.state.select_previous_history_match();
            }
            KeyCode::Down => {
                self.state.select_next_history_match();
            }
            KeyCode::Backspace => self.state.pop_history_search_char(),
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.state.push_history_search_char(character);
            }
            _ => {}
        }
    }

    fn ingest_composer_paste(&mut self, text: &str) {
        if let Some((bytes, ext)) = crate::tui::decode_pasted_image(text) {
            if bytes.len() > MAX_PASTE_IMAGE_BYTES {
                self.state.push_error("pasted image is too large");
                return;
            }
            match save_pasted_image(&self.state.project, &bytes, ext) {
                Ok(path) => {
                    self.state.insert_mention(&path);
                    self.state
                        .push_notice(Some("PASTE".into()), format!("attached {path}"));
                }
                Err(error) => self.state.push_error(error),
            }
            return;
        }
        self.state.composer.insert_str(self.state.cursor, text);
        self.state.cursor = self.state.cursor.saturating_add(text.len());
        self.state.composer_edited();
    }

    fn cycle_mode(&mut self) -> Result<(), String> {
        if self.state.mode == ExecutionMode::Yolo {
            self.state
                .push_notice(Some("MODE".into()), "YOLO is launch-only");
            return Ok(());
        }
        let mode = if self.state.mode == ExecutionMode::Supervised {
            ExecutionMode::Auto
        } else {
            ExecutionMode::Supervised
        };
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
        Ok(())
    }

    fn push_status_notice(&mut self) {
        let session = self.session_id.as_ref().map_or("none", AsRef::as_ref);
        let todos = self.state.todos.len();
        let branch = self.state.git_branch.as_deref().unwrap_or("-");
        let max_input = self
            .control
            .as_ref()
            .map(|control| control.max_input_tokens)
            .unwrap_or(self.state.max_input_tokens);
        let usage = if max_input == 0 {
            "n/a".into()
        } else if self.state.usage.input_tokens == 0 {
            format!("{max_input} input")
        } else {
            format!(
                "{}% of {max_input}",
                (self.state.usage.input_tokens.saturating_mul(100) / max_input.max(1)).min(100)
            )
        };
        self.state.push_notice(
            Some("STATUS".into()),
            format!(
                "{}/{}  {}  session {session}  todos {todos}  git {branch}  context {usage}",
                self.state.profile,
                self.state.model,
                execution_mode_label(self.state.mode),
            ),
        );
    }

    fn copy_last_assistant(&mut self) {
        let Some(text) = self.state.last_assistant_text().map(str::to_owned) else {
            self.state.push_error("no assistant reply to copy");
            return;
        };
        match copy_to_clipboard(&text) {
            Ok(()) => self.state.push_notice(
                Some("COPY".into()),
                format!("copied {} characters", text.chars().count()),
            ),
            Err(error) => self.state.push_error(error),
        }
    }

    fn push_diff_notice(&mut self) {
        match git_diff_stat(&self.state.project) {
            Ok(diff) if diff.trim().is_empty() => self
                .state
                .push_notice(Some("DIFF".into()), "working tree is clean"),
            Ok(diff) => self.state.push_notice(Some("DIFF".into()), diff),
            Err(error) => self.state.push_error(error),
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
    backend.clear_region(ClearType::All)?;
    backend.set_cursor_position(Position::new(0, rows.saturating_sub(viewport_height)))?;
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
    reset_origin: bool,
) -> Result<(), B::Error>
where
    B: Backend + Clone,
{
    let viewport_height = height.min(INLINE_VIEWPORT_MAX_HEIGHT);
    let current_viewport_top = terminal.get_frame().area().top();
    let viewport_top = height.saturating_sub(viewport_height);
    let clear_top = if reset_origin {
        0
    } else {
        current_viewport_top.min(viewport_top)
    };
    terminal
        .backend_mut()
        .set_cursor_position(Position::new(0, clear_top))?;
    terminal
        .backend_mut()
        .clear_region(ClearType::AfterCursor)?;
    terminal.backend_mut().flush()?;
    replace_inline_terminal(
        terminal,
        Rect::new(0, 0, width, height),
        viewport_top,
        viewport_height,
    )
}

fn prepare_inline_frame<B>(
    state: &mut TuiState,
    terminal: &mut Terminal<B>,
    transcript_cache: &mut TranscriptRenderCache,
) -> Result<(), String>
where
    B: Backend + Clone,
{
    if !state.stable_transcript().is_empty() && !uses_full_inline_viewport(state) {
        let size = terminal.size().map_err(|error| error.to_string())?;
        let viewport_height =
            desired_inline_viewport_height_for_transcript(state, size.width, size.height, 0);
        set_inline_viewport_height(terminal, viewport_height).map_err(|error| error.to_string())?;
        commit_stable_transcript(state, terminal)?;
        transcript_cache.invalidate();
    }

    let size = terminal.size().map_err(|error| error.to_string())?;
    let viewport_height = if uses_full_inline_viewport(state) {
        size.height
    } else {
        transcript_cache.prepare(state, Rect::new(0, 0, size.width, size.height));
        desired_inline_viewport_height_for_transcript(
            state,
            size.width,
            size.height,
            transcript_cache.lines.len(),
        )
    };
    set_inline_viewport_height(terminal, viewport_height).map_err(|error| error.to_string())?;
    Ok(())
}

fn prepare_fullscreen_frame<B>(
    state: &TuiState,
    terminal: &mut Terminal<B>,
    transcript_cache: &mut TranscriptRenderCache,
) -> Result<(), String>
where
    B: Backend,
{
    terminal.autoresize().map_err(|error| error.to_string())?;
    transcript_cache.prepare(state, terminal.get_frame().area());
    Ok(())
}

#[cfg(test)]
fn desired_inline_viewport_height(state: &TuiState, width: u16, height: u16) -> u16 {
    if height == 0 {
        return 0;
    }
    if uses_full_inline_viewport(state) {
        return height;
    }

    let area = main_area(Rect::new(0, 0, width, height));
    let transcript_height = transcript_lines(
        state.live_transcript(),
        area.width as usize,
        TranscriptDetail::Compact,
    )
    .len();
    desired_inline_viewport_height_for_transcript(state, width, height, transcript_height)
}

fn uses_full_inline_viewport(state: &TuiState) -> bool {
    state.transcript_view_expanded()
        || matches!(
            state.overlay(),
            Overlay::Onboarding
                | Overlay::Agents
                | Overlay::Todos
                | Overlay::AgentInspect
                | Overlay::AgentMessage
                | Overlay::ConfirmAgentCancel
        )
}

fn desired_inline_viewport_height_for_transcript(
    state: &TuiState,
    width: u16,
    height: u16,
    transcript_height: usize,
) -> u16 {
    if height == 0 {
        return 0;
    }

    let area = main_area(Rect::new(0, 0, width, height));
    let approval_visible = matches!(state.overlay(), Overlay::Approval | Overlay::ApprovalEdit);
    let shortcuts_visible = state.overlay() == Overlay::Shortcuts;
    let input_height = if approval_visible {
        approval_height(state, area.width)
    } else if shortcuts_visible {
        10.min(height).max(5)
    } else {
        composer_height(state, area.width)
    }
    .max(1)
    .min(height);
    let queue = if approval_visible {
        0
    } else {
        queue_height(state, area.width)
    };
    let activity_height = u16::from(
        state.overlay() == Overlay::None
            && (state.activity().is_animated() || state.last_turn_elapsed().is_some())
            && input_height < height,
    );
    let footer_height = u16::from(input_height.saturating_add(activity_height) < height);
    let chrome_height = input_height
        .saturating_add(activity_height)
        .saturating_add(queue)
        .saturating_add(footer_height);
    let palette_height = if approval_visible || shortcuts_visible {
        0
    } else {
        command_palette_height(state, height.saturating_sub(chrome_height))
    };
    let chrome_height = chrome_height.saturating_add(palette_height);
    let transcript_capacity = height.saturating_sub(chrome_height);
    let transcript_height = transcript_height.min(transcript_capacity as usize) as u16;

    input_height
        .saturating_add(activity_height)
        .saturating_add(queue)
        .saturating_add(footer_height)
        .saturating_add(palette_height)
        .saturating_add(transcript_height)
        .min(height)
}

fn sync_alt_overlay<B>(
    want: bool,
    alt: &mut AltOverlay,
    terminal: &mut Terminal<B>,
) -> Result<(), String>
where
    B: Backend + Clone,
{
    if want == alt.active {
        return Ok(());
    }
    if want {
        alt.saved = Some(terminal.get_frame().area());
        terminal
            .backend_mut()
            .flush()
            .map_err(|error| error.to_string())?;
        execute!(io::stdout(), EnterAlternateScreen).map_err(|error| error.to_string())?;
        alt.active = true;
        let size = terminal.size().map_err(|error| error.to_string())?;
        replace_inline_terminal(
            terminal,
            Rect::new(0, 0, size.width, size.height),
            0,
            size.height,
        )
        .map_err(|error| error.to_string())?;
    } else {
        terminal
            .backend_mut()
            .flush()
            .map_err(|error| error.to_string())?;
        execute!(io::stdout(), LeaveAlternateScreen).map_err(|error| error.to_string())?;
        alt.active = false;
        let size = terminal.size().map_err(|error| error.to_string())?;
        let saved = alt.saved.take().unwrap_or_else(|| {
            let height = 8.min(size.height).max(1);
            Rect::new(0, size.height.saturating_sub(height), size.width, height)
        });
        replace_inline_terminal(
            terminal,
            Rect::new(0, 0, size.width, size.height),
            saved.y.min(size.height.saturating_sub(1)),
            saved.height.max(1).min(size.height),
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn set_inline_viewport_height<B>(
    terminal: &mut Terminal<B>,
    viewport_height: u16,
) -> Result<(), B::Error>
where
    B: Backend + Clone,
{
    let mut current_area = terminal.get_frame().area();
    if current_area.height == viewport_height {
        return Ok(());
    }

    let size = terminal.size()?;
    let viewport_height = viewport_height.min(size.height);
    let growth = viewport_height.saturating_sub(current_area.height);
    if growth > 0 {
        terminal.insert_before(growth, |_| {})?;
        current_area = terminal.get_frame().area();
    }
    let viewport_top = size.height.saturating_sub(viewport_height);
    let clear_top = current_area.top().min(viewport_top);
    terminal
        .backend_mut()
        .set_cursor_position(Position::new(0, clear_top))?;
    terminal
        .backend_mut()
        .clear_region(ClearType::AfterCursor)?;
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

fn project_display_path(project: &Path, home: Option<&Path>) -> String {
    let Some(relative) = home.and_then(|home| {
        project
            .strip_prefix(home)
            .ok()
            .map(Path::to_path_buf)
            .or_else(|| {
                home.canonicalize()
                    .ok()
                    .and_then(|home| project.strip_prefix(home).ok().map(Path::to_path_buf))
            })
    }) else {
        return project.display().to_string();
    };
    if relative.as_os_str().is_empty() {
        "~".into()
    } else {
        format!("~/{}", relative.display())
    }
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
        ResizeMode::PRESERVE,
    )
    .await?;
    Ok(app)
}

fn apply_runtime_event_in_order(
    state: &mut TuiState,
    event: RuntimeEvent,
    tool_receiver: &mut mpsc::Receiver<RuntimeEvent>,
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

fn requires_immediate_redraw(event: &RuntimeEvent) -> bool {
    matches!(
        event,
        RuntimeEvent::ApprovalRequired { .. }
            | RuntimeEvent::ToolCompleted { .. }
            | RuntimeEvent::TurnCompleted
            | RuntimeEvent::Usage { .. }
            | RuntimeEvent::Error { .. }
            | RuntimeEvent::Shutdown
    )
}

fn stream_redraw_due(
    last_draw: tokio::time::Instant,
    now: tokio::time::Instant,
    redraw_pending: bool,
    channels_closed: bool,
) -> bool {
    redraw_pending && (channels_closed || now.duration_since(last_draw) >= STREAM_REDRAW_INTERVAL)
}

fn apply_runtime_channel_event(
    state: &mut TuiState,
    event: RuntimeEvent,
    tool_receiver: &mut mpsc::Receiver<RuntimeEvent>,
    tool_open: &mut bool,
) -> (bool, bool) {
    let immediate_redraw = requires_immediate_redraw(&event);
    let exit = matches!(event, RuntimeEvent::Shutdown);
    if *tool_open {
        *tool_open = apply_runtime_event_in_order(state, event, tool_receiver);
    } else {
        state.apply_runtime_event(event);
    }
    (immediate_redraw, exit)
}

fn apply_tool_channel_event(state: &mut TuiState, event: RuntimeEvent) -> (bool, bool) {
    let immediate_redraw = requires_immediate_redraw(&event);
    let exit = matches!(event, RuntimeEvent::Shutdown);
    state.apply_runtime_event(event);
    (immediate_redraw, exit)
}

fn drain_ready_events(
    state: &mut TuiState,
    runtime_receiver: &mut mpsc::Receiver<RuntimeEvent>,
    runtime_open: &mut bool,
    tool_receiver: &mut mpsc::Receiver<RuntimeEvent>,
    tool_open: &mut bool,
) -> (bool, bool) {
    let mut processed = 0;
    let mut immediate_redraw = false;
    let mut exit = false;
    while processed < READY_EVENT_BATCH_LIMIT {
        let mut progressed = false;
        if *runtime_open {
            match runtime_receiver.try_recv() {
                Ok(event) => {
                    processed += 1;
                    progressed = true;
                    let outcome =
                        apply_runtime_channel_event(state, event, tool_receiver, tool_open);
                    immediate_redraw |= outcome.0;
                    exit |= outcome.1;
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => *runtime_open = false,
            }
        }
        if immediate_redraw || processed == READY_EVENT_BATCH_LIMIT {
            break;
        }
        if *tool_open {
            match tool_receiver.try_recv() {
                Ok(event) => {
                    processed += 1;
                    progressed = true;
                    let outcome = apply_tool_channel_event(state, event);
                    immediate_redraw |= outcome.0;
                    exit |= outcome.1;
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => *tool_open = false,
            }
        }
        if immediate_redraw || !progressed {
            break;
        }
    }
    (immediate_redraw, exit)
}

async fn run_loop<B>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    input: &mut mpsc::Receiver<Event>,
    runtime_events: Option<mpsc::Receiver<RuntimeEvent>>,
    tool_events: Option<mpsc::Receiver<RuntimeEvent>>,
    commit_to_scrollback: bool,
    resize_mode: ResizeMode,
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
    let (tool_tx, mut tool_receiver) = mpsc::channel(1);
    let mut tool_open = if let Some(events) = tool_events {
        tool_receiver = events;
        true
    } else {
        drop(tool_tx);
        false
    };
    let mut input_open = true;
    let mut transcript_cache = TranscriptRenderCache::default();
    let mut alt_overlay = AltOverlay::default();
    if commit_to_scrollback {
        sync_alt_overlay(
            uses_full_inline_viewport(&app.state),
            &mut alt_overlay,
            terminal,
        )?;
        prepare_inline_frame(&mut app.state, terminal, &mut transcript_cache)?;
    } else {
        prepare_fullscreen_frame(&app.state, terminal, &mut transcript_cache)?;
    }
    terminal
        .draw(|frame| render_with_transcript(frame, &app.state, transcript_cache.lines()))
        .map_err(|error| error.to_string())?;
    let mut last_draw = tokio::time::Instant::now();
    let mut next_activity_frame = last_draw + ACTIVITY_FRAME_INTERVAL;
    let mut redraw_pending = false;

    while input_open || runtime_open || tool_open {
        let mut exit = false;
        let mut animation_tick = false;
        let mut force_redraw = false;
        let mut state_changed = false;
        let mut transcript_changed = false;
        let transcript_len_before = app.state.transcript.len();
        let terminal_area = terminal.get_frame().area();
        let animate_activity = !visible_activity_rect(terminal_area, &app.state).is_empty();
        let activity_deadline =
            std::cmp::max(next_activity_frame, last_draw + STREAM_REDRAW_INTERVAL);
        let animation = async move {
            if animate_activity {
                tokio::time::sleep_until(activity_deadline).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let redraw_deadline = last_draw + STREAM_REDRAW_INTERVAL;
        let stream_redraw = async move {
            if redraw_pending {
                tokio::time::sleep_until(redraw_deadline).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::pin!(animation);
        tokio::pin!(stream_redraw);
        tokio::select! {
            biased;
            event = input.recv(), if input_open => {
                match event {
                    Some(Event::Resize(width, height)) if commit_to_scrollback => {
                        if resize_mode.purge_history {
                            purge_terminal_history().map_err(|error| error.to_string())?;
                        }
                        if resize_mode.replay {
                            app.state.reset_transcript_commit();
                        }
                        resize_inline_terminal(terminal, width, height, resize_mode.replay)
                            .map_err(|error| error.to_string())?;
                        state_changed = true;
                        transcript_changed = true;
                        force_redraw = true;
                    }
                    Some(event) => {
                        exit = app.handle_event(event)?;
                        state_changed = true;
                        transcript_changed = app.state.transcript.len() != transcript_len_before;
                        force_redraw = true;
                    }
                    None => input_open = false,
                }
            }
            event = runtime_receiver.recv(), if runtime_open => {
                match event {
                    Some(event) => {
                        state_changed = true;
                        transcript_changed = true;
                        let mut outcome = apply_runtime_channel_event(
                            &mut app.state,
                            event,
                            &mut tool_receiver,
                            &mut tool_open,
                        );
                        if !outcome.0 {
                            let batch = drain_ready_events(
                                &mut app.state,
                                &mut runtime_receiver,
                                &mut runtime_open,
                                &mut tool_receiver,
                                &mut tool_open,
                            );
                            outcome.0 |= batch.0;
                            outcome.1 |= batch.1;
                        }
                        force_redraw |= outcome.0;
                        exit |= outcome.1;
                    }
                    None => runtime_open = false,
                }
            }
            event = tool_receiver.recv(), if tool_open => {
                match event {
                    Some(event) => {
                        state_changed = true;
                        transcript_changed = true;
                        let mut outcome = apply_tool_channel_event(&mut app.state, event);
                        if !outcome.0 {
                            let batch = drain_ready_events(
                                &mut app.state,
                                &mut runtime_receiver,
                                &mut runtime_open,
                                &mut tool_receiver,
                                &mut tool_open,
                            );
                            outcome.0 |= batch.0;
                            outcome.1 |= batch.1;
                        }
                        force_redraw |= outcome.0;
                        exit |= outcome.1;
                    }
                    None => tool_open = false,
                }
            }
            _ = &mut stream_redraw => force_redraw = true,
            _ = &mut animation => {
                animation_tick = true;
                force_redraw = true;
            }
        }
        if !app.state.sent_commands().is_empty() {
            app.flush_commands().await?;
        }
        if transcript_changed {
            transcript_cache.invalidate();
        }
        redraw_pending |= state_changed;
        let now = tokio::time::Instant::now();
        let channels_closed = !input_open && !runtime_open && !tool_open;
        if force_redraw || stream_redraw_due(last_draw, now, redraw_pending, channels_closed) {
            if commit_to_scrollback && !animation_tick {
                sync_alt_overlay(
                    uses_full_inline_viewport(&app.state),
                    &mut alt_overlay,
                    terminal,
                )?;
                prepare_inline_frame(&mut app.state, terminal, &mut transcript_cache)?;
            } else if !commit_to_scrollback {
                prepare_fullscreen_frame(&app.state, terminal, &mut transcript_cache)?;
            } else {
                transcript_cache.prepare(&app.state, terminal.get_frame().area());
            }
            terminal
                .draw(|frame| render_with_transcript(frame, &app.state, transcript_cache.lines()))
                .map_err(|error| error.to_string())?;
            last_draw = tokio::time::Instant::now();
            next_activity_frame = last_draw + ACTIVITY_FRAME_INTERVAL;
            redraw_pending = false;
        }
        if exit {
            break;
        }
    }
    if commit_to_scrollback {
        sync_alt_overlay(false, &mut alt_overlay, terminal)?;
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

#[cfg(test)]
fn copy_to_clipboard(_text: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(not(test))]
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let mut copied = false;
    for command in ["pbcopy", "wl-copy", "xclip"] {
        let mut child = match std::process::Command::new(command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => continue,
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        if child.wait().map(|status| status.success()).unwrap_or(false) {
            copied = true;
            break;
        }
    }
    if io::IsTerminal::is_terminal(&io::stdout()) {
        let mut encoded = String::new();
        base64_encode(text.as_bytes(), &mut encoded);
        let mut stdout = io::stdout();
        let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
        let _ = stdout.flush();
        copied = true;
    }
    if copied {
        Ok(())
    } else {
        Err("no clipboard command available".into())
    }
}

#[cfg(not(test))]
fn base64_encode(bytes: &[u8], out: &mut String) {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(a >> 2) as usize] as char);
        out.push(TABLE[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(c & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
}

fn save_pasted_image(project: &str, bytes: &[u8], ext: &str) -> Result<String, String> {
    let dir = Path::new(project).join(".kurama").join("paste");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let relative = format!(".kurama/paste/{stamp}.{ext}");
    std::fs::write(Path::new(project).join(&relative), bytes).map_err(|error| error.to_string())?;
    Ok(relative)
}

fn git_diff_stat(project: &str) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(["-C", project, "diff", "--stat"])
        .output()
        .map_err(|error| format!("git diff failed: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let mut diff = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if diff.chars().count() > 800 {
        let end = diff
            .char_indices()
            .nth(800)
            .map(|(index, _)| index)
            .unwrap_or(diff.len());
        diff.truncate(end);
        diff.push('…');
    }
    Ok(diff)
}

fn execution_mode_label(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Supervised => "supervised",
        ExecutionMode::Auto => "auto",
        ExecutionMode::Yolo => "yolo",
    }
}

struct ToolEventSink {
    sender: mpsc::Sender<RuntimeEvent>,
}

impl EventSink for ToolEventSink {
    fn emit(&self, event: RuntimeEvent) -> Result<(), KuramaError> {
        if matches!(event, RuntimeEvent::ToolOutputDelta { .. }) {
            match self.sender.try_send(event) {
                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(KuramaError::Cancelled);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use crossterm::event::{MouseEvent, MouseEventKind};
    use kurama_protocol::{
        id::{CallId, OperationId},
        policy::ApprovalRequest,
        tool::{CommandClass, Operation, ToolResult},
    };
    use ratatui::{
        TerminalOptions, Viewport,
        backend::{Backend, TestBackend, WindowSize},
        buffer::Cell as BufferCell,
        layout::{Position, Rect, Size},
        style::Color,
    };

    use super::*;
    use crate::tui::{ActivityState, TranscriptEntry, visible_activity_rect};

    #[derive(Clone)]
    struct DrawBudgetBackend {
        inner: TestBackend,
        remaining: Rc<Cell<usize>>,
    }

    impl DrawBudgetBackend {
        fn new(inner: TestBackend, remaining: Rc<Cell<usize>>) -> Self {
            Self { inner, remaining }
        }
    }

    impl Backend for DrawBudgetBackend {
        type Error = std::convert::Infallible;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a BufferCell)>,
        {
            let remaining = self.remaining.get();
            assert!(remaining > 0, "draw budget exceeded");
            self.remaining.set(remaining - 1);
            self.inner.draw(content)
        }

        fn append_lines(&mut self, line_count: u16) -> Result<(), Self::Error> {
            self.inner.append_lines(line_count)
        }

        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            self.inner.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Self::Error> {
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> Result<(), Self::Error> {
            self.inner.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
            self.inner.clear_region(clear_type)
        }

        fn size(&self) -> Result<Size, Self::Error> {
            self.inner.size()
        }

        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            self.inner.window_size()
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            self.inner.flush()
        }
    }

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
    fn startup_project_path_collapses_the_home_prefix() {
        assert_eq!(
            project_display_path(
                Path::new("/Users/tester/src/kurama"),
                Some(Path::new("/Users/tester")),
            ),
            "~/src/kurama"
        );
        assert_eq!(
            project_display_path(Path::new("/Users/tester"), Some(Path::new("/Users/tester"))),
            "~"
        );

        let home = tempfile::tempdir().expect("home");
        let project = home.path().join("project");
        std::fs::create_dir(&project).expect("project");
        assert_eq!(
            project_display_path(
                &project.canonicalize().expect("canonical project"),
                Some(home.path()),
            ),
            "~/project"
        );
    }

    #[test]
    fn exit_command_requests_shutdown_and_returns_resume_details() {
        let mut app = test_app();
        app.session_id = Some(SessionId::from("ses_deadbeef"));
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
Resume with kurama resume ses_deadbeef\n\
Session ID: ses_deadbeef"
        );
    }

    #[test]
    fn exit_summary_includes_cached_usage() {
        let summary = ExitSummary {
            session_id: SessionId::from("ses_cafebabe"),
            usage: Usage {
                input_tokens: 120,
                output_tokens: 30,
                cached_input_tokens: 80,
            },
        };

        assert_eq!(
            summary.to_string(),
            "Token usage: total=150 input=120 (+ 80 cached) output=30\n\
Resume with kurama resume ses_cafebabe\n\
Session ID: ses_cafebabe"
        );
    }

    #[test]
    fn approval_session_shortcut_queues_session_scoped_approval() {
        let mut app = test_app();
        app.state.begin_approval(ApprovalRequest {
            operation_id: OperationId::from("o_session"),
            operation: Operation::Bash {
                command: "cargo test".into(),
                cwd: ".".into(),
                class: CommandClass::ReadOnly,
                timeout_ms: 30_000,
            },
            summary: "Run tests".into(),
            arguments: serde_json::json!({"command":"cargo test"}),
        });

        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::NONE,
        )))
        .expect("approve for session");

        assert!(matches!(
            app.state.sent_commands().last(),
            Some(EngineCommand::ResolveApproval {
                operation_id,
                response: ApprovalResponse::ApproveSession,
            }) if operation_id.as_ref() == "o_session"
        ));
    }

    #[test]
    fn approval_editor_supports_cursor_movement_and_paste() {
        let mut app = test_app();
        app.state.begin_approval(ApprovalRequest {
            operation_id: OperationId::from("o_edit"),
            operation: Operation::Bash {
                command: "printf safe".into(),
                cwd: ".".into(),
                class: CommandClass::ReadOnly,
                timeout_ms: 30_000,
            },
            summary: "Edit the command".into(),
            arguments: serde_json::json!({"command":"safe"}),
        });
        app.state.begin_approval_edit();
        app.state.set_approval_editor("ab");

        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)))
            .expect("move approval cursor");
        app.handle_event(Event::Paste("XYZ".into()))
            .expect("paste approval text");

        let approval = app.state.approval.as_ref().expect("approval remains open");
        assert_eq!(approval.editor, "aXYZb");
        assert_eq!(approval.editor_cursor, 4);
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
    fn ctrl_c_interrupts_before_exiting() {
        let mut app = test_app();
        app.state.set_thinking();

        let exit = app
            .handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
            .expect("interrupt");
        assert!(!exit);
        assert_eq!(app.state.activity(), &ActivityState::Interrupted);
        assert!(matches!(
            app.state.sent_commands().last(),
            Some(EngineCommand::CancelTurn)
        ));

        app.state.composer = "draft".into();
        app.state.cursor = 5;
        let exit = app
            .handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
            .expect("clear composer");
        assert!(!exit);
        assert!(app.state.composer.is_empty());

        let exit = app
            .handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
            .expect("exit");
        assert!(exit);
        assert!(matches!(
            app.state.sent_commands().last(),
            Some(EngineCommand::Shutdown)
        ));
    }

    #[test]
    fn ctrl_a_and_ctrl_k_edit_the_composer() {
        let mut app = test_app();
        app.state.composer = "hello world".into();
        app.state.cursor = app.state.composer.len();

        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .expect("home");
        assert_eq!(app.state.cursor, 0);
        assert_eq!(app.state.composer, "hello world");

        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        )))
        .expect("kill");
        assert!(app.state.composer.is_empty());
    }

    #[test]
    fn up_recalls_previous_prompts() {
        let mut app = test_app();
        app.state.remember_prompt("first turn");
        app.state.remember_prompt("second turn");

        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)))
            .expect("newer history");
        assert_eq!(app.state.composer, "second turn");

        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)))
            .expect("older history");
        assert_eq!(app.state.composer, "first turn");

        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
            .expect("forward history");
        assert_eq!(app.state.composer, "second turn");
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
    fn paste_inserts_multiline_text_at_the_composer_cursor() {
        let mut app = test_app();
        app.state.composer = "before after".into();
        app.state.cursor = "before ".len();

        app.handle_event(Event::Paste("one\ntwo".into()))
            .expect("paste composer text");

        assert_eq!(app.state.composer, "before one\ntwoafter");
        assert_eq!(app.state.cursor, "before one\ntwo".len());
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
    fn stream_redraw_waits_for_cadence_but_terminal_events_do_not() {
        let last_draw = tokio::time::Instant::now();
        assert!(!stream_redraw_due(
            last_draw,
            last_draw + STREAM_REDRAW_INTERVAL - Duration::from_millis(1),
            true,
            false,
        ));
        assert!(stream_redraw_due(
            last_draw,
            last_draw + STREAM_REDRAW_INTERVAL,
            true,
            false,
        ));
        assert!(stream_redraw_due(last_draw, last_draw, true, true));
        assert!(!stream_redraw_due(
            last_draw,
            last_draw + STREAM_REDRAW_INTERVAL,
            false,
            true,
        ));

        for event in [
            RuntimeEvent::ApprovalRequired {
                request: ApprovalRequest {
                    operation_id: OperationId::from("operation_approval"),
                    operation: Operation::Read {
                        path: "README.md".into(),
                        external: false,
                    },
                    summary: "Read README".into(),
                    arguments: serde_json::json!({"path": "README.md"}),
                },
            },
            RuntimeEvent::ToolCompleted {
                operation_id: OperationId::from("operation_tool"),
                result: ToolResult::success(CallId::from("call_tool"), "done"),
            },
            RuntimeEvent::TurnCompleted,
            RuntimeEvent::Error {
                message: "failed".into(),
            },
            RuntimeEvent::Shutdown,
        ] {
            assert!(requires_immediate_redraw(&event));
        }
        assert!(!requires_immediate_redraw(&RuntimeEvent::AssistantDelta {
            text: "x".into()
        }));
    }

    #[tokio::test]
    async fn immediately_ready_runtime_deltas_share_one_redraw() {
        const DELTA_COUNT: usize = 32;

        let mut app = test_app();
        let remaining_draws = Rc::new(Cell::new(2));
        let backend = DrawBudgetBackend::new(TestBackend::new(80, 24), Rc::clone(&remaining_draws));
        let mut terminal = Terminal::new(backend).expect("terminal");
        let (_input_sender, mut input) = mpsc::channel(1);
        let (runtime_sender, runtime_events) = mpsc::channel(DELTA_COUNT + 1);
        for _ in 0..DELTA_COUNT {
            runtime_sender
                .try_send(RuntimeEvent::AssistantDelta { text: "x".into() })
                .expect("queue delta");
        }
        runtime_sender
            .try_send(RuntimeEvent::Shutdown)
            .expect("queue shutdown");
        drop(runtime_sender);

        run_loop(
            &mut app,
            &mut terminal,
            &mut input,
            Some(runtime_events),
            None,
            false,
            ResizeMode::PRESERVE,
        )
        .await
        .expect("run loop");

        assert_eq!(remaining_draws.get(), 0);
        assert!(matches!(
            &app.state.transcript[..],
            [TranscriptEntry::AssistantMessage { body }] if body.len() == DELTA_COUNT
        ));
        assert_eq!(app.state.activity(), &ActivityState::Idle);
    }

    #[tokio::test]
    async fn batched_tool_deltas_keep_the_canonical_completion_output() {
        let mut app = test_app();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        let (_input_sender, mut input) = mpsc::channel(1);
        let (runtime_sender, runtime_events) = mpsc::channel(3);
        let (tool_sender, tool_events) = mpsc::channel(4);
        tool_sender
            .try_send(RuntimeEvent::ToolOutputDelta {
                call_id: CallId::from("call_1"),
                stream: "stdout".into(),
                chunk: "partial ".into(),
            })
            .expect("queue first delta");
        tool_sender
            .try_send(RuntimeEvent::ToolOutputDelta {
                call_id: CallId::from("call_1"),
                stream: "stdout".into(),
                chunk: "output".into(),
            })
            .expect("queue second delta");
        drop(tool_sender);

        runtime_sender
            .try_send(RuntimeEvent::ToolStarted {
                operation_id: OperationId::from("operation_1"),
                name: "bash".into(),
                context: "printf output".into(),
            })
            .expect("queue tool start");
        let mut result = ToolResult::success(CallId::from("call_1"), "canonical output");
        result.metadata = serde_json::json!({"tool_name": "bash"});
        runtime_sender
            .try_send(RuntimeEvent::ToolCompleted {
                operation_id: OperationId::from("operation_1"),
                result,
            })
            .expect("queue completion");
        runtime_sender
            .try_send(RuntimeEvent::Shutdown)
            .expect("queue shutdown");
        drop(runtime_sender);

        run_loop(
            &mut app,
            &mut terminal,
            &mut input,
            Some(runtime_events),
            Some(tool_events),
            false,
            ResizeMode::PRESERVE,
        )
        .await
        .expect("run loop");

        assert!(matches!(
            &app.state.transcript[..],
            [TranscriptEntry::ToolCall(tool)]
                if tool.output == "canonical output"
                    && tool.lifecycle == crate::tui::ToolLifecycle::Completed
        ));
    }

    #[test]
    fn queued_tool_delta_is_applied_before_completion() {
        let mut state = TuiState::new("fixture", "frontier", ".", ExecutionMode::Supervised);
        let (tool_sender, mut tool_receiver) = mpsc::channel(1);
        tool_sender
            .try_send(RuntimeEvent::ToolOutputDelta {
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
    fn live_tool_output_drops_excess_events_instead_of_growing_unbounded() {
        let (sender, mut receiver) = mpsc::channel(1);
        let sink = ToolEventSink { sender };
        let delta = || RuntimeEvent::ToolOutputDelta {
            call_id: CallId::from("call_1"),
            stream: "stdout".into(),
            chunk: "output".into(),
        };

        sink.emit(delta()).expect("first live delta");
        sink.emit(delta()).expect("excess live delta is dropped");

        assert!(receiver.try_recv().is_ok());
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn queued_tool_delta_is_applied_before_runtime_error() {
        let mut state = TuiState::new("fixture", "frontier", ".", ExecutionMode::Supervised);
        let (tool_sender, mut tool_receiver) = mpsc::channel(1);
        tool_sender
            .try_send(RuntimeEvent::ToolOutputDelta {
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
    fn inline_terminal_initialization_clears_output_and_anchors_at_the_bottom() {
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

        assert_eq!(terminal.get_frame().area(), Rect::new(0, 28, 80, 12));
        assert!(!visible.contains("stale shell prompt"));
        assert!(!visible.contains("stale viewport content"));
        assert!(!visible.contains("stale lower content"));
    }

    #[test]
    fn active_and_expanded_views_use_the_available_terminal_height() {
        let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        state.push_assistant(
            (0..40)
                .map(|line| format!("- line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        state.set_thinking();

        assert_eq!(desired_inline_viewport_height(&state, 80, 24), 24);

        state.toggle_transcript_view();
        assert_eq!(desired_inline_viewport_height(&state, 80, 24), 24);

        state.toggle_transcript_view();
        state.overlay = Overlay::Agents;
        assert_eq!(desired_inline_viewport_height(&state, 80, 24), 24);

        state.overlay = Overlay::Shortcuts;
        assert_eq!(desired_inline_viewport_height(&state, 80, 24), 24);
        assert!(!uses_full_inline_viewport(&state));

        state.overlay = Overlay::Todos;
        assert_eq!(desired_inline_viewport_height(&state, 80, 24), 24);
        assert!(uses_full_inline_viewport(&state));
    }

    #[test]
    fn filtering_the_slash_palette_keeps_the_inline_viewport_stable() {
        let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        state.composer = "/".into();
        state.cursor = state.composer.len();
        let open_height = desired_inline_viewport_height(&state, 80, 24);

        state.composer = "/res".into();
        state.cursor = state.composer.len();
        let filtered_height = desired_inline_viewport_height(&state, 80, 24);

        assert!(open_height >= filtered_height);
        assert!(filtered_height >= 4);
    }

    #[test]
    fn committed_history_leaves_completion_and_composer_at_the_bottom() {
        let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        state.submit_turn("hi", false);
        state.push_assistant("hello from kurama");
        state.apply_runtime_event(RuntimeEvent::TurnCompleted);
        let mut terminal = initialize_inline_terminal(TestBackend::new(80, 24))
            .expect("initialize inline terminal");

        let mut transcript_cache = TranscriptRenderCache::default();
        prepare_inline_frame(&mut state, &mut terminal, &mut transcript_cache)
            .expect("prepare inline frame");
        terminal
            .draw(|frame| render_with_transcript(frame, &state, transcript_cache.lines()))
            .expect("draw idle frame");

        assert_eq!(terminal.get_frame().area().bottom(), 24);
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
        let worked_row = rows
            .iter()
            .position(|row| row.contains("Worked for"))
            .expect("duration divider row");

        assert!(
            worked_row > answer_row,
            "answer={answer_row} worked={worked_row} composer={composer_row} {rows:#?}"
        );
        assert!(
            composer_row > worked_row,
            "answer={answer_row} worked={worked_row} composer={composer_row} {rows:#?}"
        );
        assert!(composer_row < 23, "{rows:#?}");
    }

    #[test]
    fn growing_inline_viewport_preserves_committed_startup_history() {
        let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        state.prepend_startup("0.1.0", "~/project");
        let mut terminal = initialize_inline_terminal(TestBackend::new(80, 24))
            .expect("initialize inline terminal");
        let mut transcript_cache = TranscriptRenderCache::default();

        prepare_inline_frame(&mut state, &mut terminal, &mut transcript_cache)
            .expect("commit startup");
        terminal
            .draw(|frame| render_with_transcript(frame, &state, transcript_cache.lines()))
            .expect("draw startup frame");

        state.submit_turn("inspect the repository", false);
        prepare_inline_frame(&mut state, &mut terminal, &mut transcript_cache)
            .expect("commit user turn");
        terminal
            .draw(|frame| render_with_transcript(frame, &state, transcript_cache.lines()))
            .expect("draw active frame");

        let text = terminal
            .backend()
            .scrollback()
            .content()
            .iter()
            .chain(terminal.backend().buffer().content().iter())
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(text.matches("◢ kurama").count(), 1, "{text}");
        assert_eq!(text.matches("~/project").count(), 1, "{text}");
        assert_eq!(
            text.matches("› inspect the repository").count(),
            1,
            "{text}"
        );
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
        resize_inline_terminal(&mut terminal, 52, 12, false).expect("shrink inline terminal");
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 0, 52, 12));
        terminal
            .draw(|frame| render(frame, &state))
            .expect("draw narrow viewport");

        terminal.backend_mut().resize(100, 30);
        resize_inline_terminal(&mut terminal, 100, 30, false).expect("grow inline terminal");
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
        assert!(
            text.contains("work/model")
                || text.contains("enter send")
                || text.contains("shortcuts"),
            "{text}"
        );
    }

    #[test]
    fn expanding_inline_viewport_clears_rows_above_the_old_viewport() {
        let mut backend = TestBackend::with_lines(["stale terminal content"; 16]);
        backend
            .set_cursor_position(Position::new(0, 8))
            .expect("position inline viewport");
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(8),
            },
        )
        .expect("inline terminal");
        let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        state.toggle_transcript_view();

        set_inline_viewport_height(&mut terminal, 16).expect("expand inline viewport");
        terminal
            .draw(|frame| render(frame, &state))
            .expect("draw expanded transcript");

        let visible = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!visible.contains("stale terminal content"), "{visible}");
    }

    #[test]
    fn clearing_inline_terminal_removes_the_live_ui_before_exit_output() {
        let state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        let mut terminal = initialize_inline_terminal(TestBackend::new(80, 24))
            .expect("initialize inline terminal");
        terminal
            .draw(|frame| render(frame, &state))
            .expect("draw live viewport");
        let viewport_top = terminal.get_frame().area().as_position();

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
            viewport_top
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
            ResizeMode::PRESERVE,
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
            let bg = terminal
                .backend()
                .buffer()
                .cell((x, inserted_row))
                .expect("inserted background")
                .bg;
            bg == Color::Reset || bg == Color::Rgb(36, 40, 48)
        }));
        assert!(app.state.live_transcript().is_empty());
    }

    #[tokio::test]
    async fn inline_resize_replays_committed_history_without_duplicates() {
        let mut app = test_app();
        app.state.push_user("committed question");
        app.state.push_assistant("committed answer");
        let mut terminal = initialize_inline_terminal(TestBackend::new(80, 24))
            .expect("initialize inline terminal");
        let (input_sender, mut input) = mpsc::channel(2);
        input_sender
            .send(Event::Resize(60, 18))
            .await
            .expect("queue resize");
        drop(input_sender);

        run_loop(
            &mut app,
            &mut terminal,
            &mut input,
            None,
            None,
            true,
            ResizeMode::REPLAY,
        )
        .await
        .expect("run resized inline terminal");

        let text = terminal
            .backend()
            .scrollback()
            .content()
            .iter()
            .chain(terminal.backend().buffer().content().iter())
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(text.matches("committed question").count(), 1, "{text}");
        assert_eq!(text.matches("committed answer").count(), 1, "{text}");
    }

    #[test]
    fn inline_prepare_reuses_markdown_render_for_draw() {
        let mut state = TuiState::new("fixture", "frontier", ".", ExecutionMode::Supervised);
        state.apply_runtime_event(RuntimeEvent::AssistantDelta {
            text: "## Heading\n\n- one\n- two\n\n```rust\nfn main() {}\n```".into(),
        });
        let mut terminal =
            initialize_inline_terminal(TestBackend::new(80, 24)).expect("inline terminal");

        crate::tui::reset_transcript_render_calls();
        let mut transcript_cache = TranscriptRenderCache::default();
        prepare_inline_frame(&mut state, &mut terminal, &mut transcript_cache)
            .expect("prepare inline frame");
        terminal
            .draw(|frame| render_with_transcript(frame, &state, transcript_cache.lines()))
            .expect("render inline frame");

        assert_eq!(crate::tui::transcript_render_calls(), 1);
    }

    #[tokio::test]
    async fn animation_redraws_reuse_unchanged_markdown() {
        let mut app = test_app();
        app.state.apply_runtime_event(RuntimeEvent::AssistantDelta {
            text: "## Heading\n\n- one\n- two\n\n```rust\nfn main() {}\n```".into(),
        });
        let mut terminal =
            initialize_inline_terminal(TestBackend::new(80, 24)).expect("inline terminal");
        let (input_sender, mut input) = mpsc::channel(1);
        let closer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            drop(input_sender);
        });

        crate::tui::reset_transcript_render_calls();
        run_loop(
            &mut app,
            &mut terminal,
            &mut input,
            None,
            None,
            true,
            ResizeMode::PRESERVE,
        )
        .await
        .expect("run animated inline frame");
        closer.await.expect("close input channel");

        assert_eq!(crate::tui::transcript_render_calls(), 1);
    }

    #[tokio::test]
    async fn composer_input_reuses_unchanged_markdown() {
        let mut app = test_app();
        app.state.apply_runtime_event(RuntimeEvent::AssistantDelta {
            text: "## Heading\n\n- one\n- two\n\n```rust\nfn main() {}\n```".into(),
        });
        let mut terminal =
            initialize_inline_terminal(TestBackend::new(80, 24)).expect("inline terminal");
        let (input_sender, mut input) = mpsc::channel(1);
        input_sender
            .send(Event::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            )))
            .await
            .expect("send composer input");
        drop(input_sender);

        crate::tui::reset_transcript_render_calls();
        run_loop(
            &mut app,
            &mut terminal,
            &mut input,
            None,
            None,
            true,
            ResizeMode::PRESERVE,
        )
        .await
        .expect("run composer redraw");

        assert_eq!(app.state.composer, "x");
        assert_eq!(crate::tui::transcript_render_calls(), 1);
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

    #[tokio::test]
    async fn fullscreen_resize_wraps_markdown_at_the_current_backend_width() {
        let mut app = test_app();
        app.state
            .push_assistant("alpha beta gamma delta epsilon zeta eta theta iota kappa lambda");
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("fullscreen terminal");
        terminal.backend_mut().resize(28, 12);
        let (input_sender, input) = mpsc::channel(1);
        let (runtime_sender, runtime_events) = mpsc::channel(1);
        drop(input_sender);
        drop(runtime_sender);

        crate::tui::reset_transcript_render_calls();
        let app = run_with(app, &mut terminal, input, runtime_events)
            .await
            .expect("run resized fullscreen terminal");
        let visible = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert_eq!(terminal.get_frame().area().width, 28);
        assert!(visible.contains("lambda"), "{visible}");
        assert_eq!(crate::tui::transcript_render_calls(), 1);
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
