use std::{
    collections::BTreeMap,
    fmt, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(not(test))]
use std::io::Write as _;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use kurama_adapters::{
    AppPaths, BashTool, ClaudeNativeSearch, CodexNativeSearch, ConfigRepository,
    CredentialResolver, FsSessionStore, HttpClient, JsonSearchBackend, OpenAiNativeSearch,
    ProviderFactory, ReadTool, SearchBackend, SecretValue, SessionSecrets, WebSearchTool,
    WriteTool,
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
    session::{EventEnvelope, SessionEvent, SessionMetadata},
    traits::{EventSink, Orchestrator, SessionStore, Tool},
};
use kurama_sdk::{Agent, Events, Handle};
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend},
    layout::Rect,
};
use tokio::sync::mpsc;

use crate::{
    args::{Args, ResumeChoice},
    commands::{Command, GoalAction, command_missing_required_arguments, parse_command},
    tui::{
        OnboardingState, OnboardingSubmission, Overlay, TerminalGuard, TranscriptDetail,
        TranscriptLine, TranscriptPoint, TranscriptSelection, TuiState, command_palette_height,
        composer_cursor_vertical, main_area, main_layout, next_grapheme_boundary,
        previous_grapheme_boundary, render_with_transcript, spawn_input_thread,
        transcript_lines_with_entry_starts, visible_activity_rect,
    },
};

const TOOL_EVENT_CAPACITY: usize = 64;
const MAX_PASTE_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const READY_EVENT_BATCH_LIMIT: usize = 128;
const STREAM_REDRAW_INTERVAL: Duration = Duration::from_millis(33);
const ACTIVITY_FRAME_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Default)]
struct TranscriptRenderCache {
    key: Option<TranscriptCacheKey>,
    lines: Vec<TranscriptLine>,
    entry_starts: Vec<usize>,
    viewport_height: u16,
    last_assistant_entry: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct TranscriptCacheKey {
    width: usize,
    revision: u64,
    expanded: bool,
    entries: usize,
}

impl TranscriptRenderCache {
    fn prepare(&mut self, state: &mut TuiState, frame_area: Rect) {
        if frame_area.is_empty() {
            state.transcript_selection = None;
            return;
        }
        let expanded = state.transcript_view_expanded();
        let width = main_area(frame_area).width as usize;
        let key = TranscriptCacheKey {
            width,
            revision: state.transcript_revision(),
            expanded,
            entries: state.transcript.len(),
        };
        let same_layout = self
            .key
            .is_some_and(|old| old.width == width && old.expanded == expanded);
        if !same_layout || state.overlay() != Overlay::None {
            state.transcript_selection = None;
        }
        let frozen = state
            .transcript_selection
            .as_ref()
            .is_some_and(|selection| selection.dragging);
        let viewport = if expanded {
            frame_area
                .height
                .saturating_sub(u16::from(frame_area.height > 1))
        } else {
            main_layout(frame_area, state).transcript.height
        };
        state.transcript_width.set(width as u16);
        state.viewport_height.set(viewport);
        let previous_start = self
            .lines
            .len()
            .saturating_sub(self.viewport_height as usize)
            .saturating_sub(state.scroll);
        if frozen {
            // Keep the text under a held pointer stable while the engine continues running.
            if viewport != self.viewport_height {
                state.scroll = self
                    .lines
                    .len()
                    .saturating_sub(viewport as usize)
                    .saturating_sub(previous_start);
            }
            state.set_transcript_geometry(width as u16, self.lines.len(), &self.entry_starts);
            self.viewport_height = viewport;
            return;
        }
        if self.key == Some(key) {
            let max_scroll = self.lines.len().saturating_sub(viewport as usize);
            if state.scroll > 0 && viewport != self.viewport_height {
                state.scroll = max_scroll.saturating_sub(previous_start);
            }
            state.scroll = state.scroll.min(max_scroll);
            self.viewport_height = viewport;
            return;
        }
        state.transcript_selection = None;
        let anchor = if self.key.is_some_and(|key| key.expanded == expanded) && state.scroll > 0 {
            self.entry_starts
                .partition_point(|&row| row <= previous_start)
                .checked_sub(1)
                .map(|entry| {
                    (
                        entry,
                        previous_start.saturating_sub(self.entry_starts[entry]),
                    )
                })
        } else {
            None
        };
        let reuse = self
            .key
            .is_some_and(|old| old.width == width && old.expanded == expanded);
        let mut retained = if reuse {
            state
                .transcript_dirty_from()
                .min(state.transcript.len())
                .min(self.entry_starts.len())
        } else {
            0
        };
        // A new answer makes the former latest answer gain its ending rule.
        if reuse
            && state
                .transcript
                .iter()
                .skip(self.entry_starts.len())
                .any(|entry| matches!(entry, crate::tui::TranscriptEntry::AssistantMessage { .. }))
            && let Some(previous) = self.last_assistant_entry
        {
            retained = retained.min(previous);
        }
        let prefix_rows = self
            .entry_starts
            .get(retained)
            .copied()
            .unwrap_or(self.lines.len());
        let (mut lines, starts) = transcript_lines_with_entry_starts(
            &state.transcript[retained..],
            width,
            if expanded {
                TranscriptDetail::Expanded
            } else {
                TranscriptDetail::Compact
            },
        );
        if retained == 0 {
            self.lines = lines;
            self.entry_starts = starts;
        } else {
            self.lines.truncate(prefix_rows);
            self.lines.append(&mut lines);
            self.entry_starts.truncate(retained);
            self.entry_starts
                .extend(starts.into_iter().map(|row| prefix_rows + row));
        }
        if let Some((entry, offset)) = anchor {
            let start = self
                .entry_starts
                .get(entry)
                .map(|&start| {
                    let end = self
                        .entry_starts
                        .get(entry + 1)
                        .copied()
                        .unwrap_or(self.lines.len());
                    start + offset.min(end.saturating_sub(start).saturating_sub(1))
                })
                .unwrap_or(previous_start);
            state.scroll = self
                .lines
                .len()
                .saturating_sub(viewport as usize)
                .saturating_sub(start);
        }
        state.set_transcript_geometry(width as u16, self.lines.len(), &self.entry_starts);
        state.scroll = state
            .scroll
            .min(self.lines.len().saturating_sub(viewport as usize));
        state.mark_transcript_rendered();
        self.last_assistant_entry = state.transcript.iter().rposition(|entry| {
            matches!(entry, crate::tui::TranscriptEntry::AssistantMessage { .. })
        });
        self.viewport_height = viewport;
        self.key = Some(key);
    }

    fn point_at(
        &self,
        state: &TuiState,
        area: Rect,
        mouse: MouseEvent,
        clamp: bool,
    ) -> Option<TranscriptPoint> {
        let mut view = main_area(area);
        view.height = self.viewport_height;
        if view.is_empty() || self.lines.is_empty() {
            return None;
        }
        if !state.transcript_view_expanded() {
            let layout = main_layout(area, state);
            let palette_height =
                command_palette_height(state, layout.input.y.saturating_sub(view.y));
            if palette_height > 0
                && mouse.row >= layout.input.y.saturating_sub(palette_height)
                && mouse.row < layout.input.y
            {
                return None;
            }
        }
        let inside = mouse.column >= view.x
            && mouse.column < view.right()
            && mouse.row >= view.y
            && mouse.row < view.bottom();
        if !clamp && !inside {
            return None;
        }
        let column = mouse.column.clamp(view.x, view.right() - 1) - view.x;
        let visible_row = mouse.row.clamp(view.y, view.bottom() - 1) - view.y;
        let first = self
            .lines
            .len()
            .saturating_sub(view.height as usize)
            .saturating_sub(state.scroll);
        let row = first.saturating_add(visible_row as usize);
        if !clamp && row >= self.lines.len() {
            return None;
        }
        Some(TranscriptPoint {
            row: row.min(self.lines.len() - 1),
            column: column as usize,
        })
    }

    fn lines(&self) -> Option<&[TranscriptLine]> {
        self.key.map(|_| self.lines.as_slice())
    }
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
    link_tasks: tokio::task::JoinSet<Result<(), String>>,
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
        state.set_composer_session(&session_id);
        state.max_input_tokens = active.max_input_tokens;
        state.set_display_store(Arc::clone(&store));
        state.hydrate_replay(&replay);
        let (engine, runtime_events) = agent
            .launch(metadata, replay)
            .map_err(|error| error.to_string())?;
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
            link_tasks: tokio::task::JoinSet::new(),
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
        mut state: TuiState,
        engine: Handle,
        orchestrator: Arc<dyn Orchestrator>,
        session_id: SessionId,
    ) -> Self {
        state.set_composer_session(&session_id);
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
            link_tasks: tokio::task::JoinSet::new(),
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
            link_tasks: tokio::task::JoinSet::new(),
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

    fn accepts_event(&self, event: &Event) -> bool {
        let key = match event {
            Event::Mouse(mouse) => {
                return self.state.overlay() == Overlay::None
                    && matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp
                            | MouseEventKind::ScrollDown
                            | MouseEventKind::Down(MouseButton::Left)
                            | MouseEventKind::Drag(MouseButton::Left)
                            | MouseEventKind::Up(MouseButton::Left)
                    );
            }
            Event::Resize(..) => return true,
            Event::Paste(text) => {
                return !text.is_empty()
                    && (matches!(
                        self.state.overlay(),
                        Overlay::ApprovalEdit | Overlay::Onboarding | Overlay::AgentMessage
                    ) || (self.state.overlay() == Overlay::None
                        && !self.state.transcript_view_expanded()));
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => key,
            _ => return false,
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => return true,
                KeyCode::Char('o') => return self.state.overlay() == Overlay::None,
                _ if self.state.overlay() == Overlay::None
                    && !self.state.transcript_view_expanded() =>
                {
                    return if self.state.history_search_active() {
                        key.code == KeyCode::Char('r')
                    } else {
                        matches!(
                            key.code,
                            KeyCode::Char(
                                'a' | 'e' | 'k' | 'u' | 'w' | 'j' | 'l' | 't' | 'r' | 'd'
                            )
                        )
                    };
                }
                _ => {}
            }
        }
        if self.state.transcript_view_expanded() {
            return matches!(
                key.code,
                KeyCode::Esc
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::Char('{' | '}')
            );
        }
        match self.state.overlay() {
            Overlay::None => matches!(
                key.code,
                KeyCode::Char(_)
                    | KeyCode::Backspace
                    | KeyCode::Delete
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::Enter
                    | KeyCode::Esc
                    | KeyCode::Tab
                    | KeyCode::BackTab
            ),
            Overlay::Todos => matches!(
                key.code,
                KeyCode::Esc
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::PageUp
                    | KeyCode::PageDown
            ),
            Overlay::Shortcuts => {
                matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?'))
            }
            Overlay::Agents => matches!(
                key.code,
                KeyCode::Esc
                    | KeyCode::Enter
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Char('m' | 'x')
            ),
            Overlay::AgentInspect => matches!(key.code, KeyCode::Esc | KeyCode::Char('m' | 'x')),
            Overlay::ConfirmAgentCancel => {
                matches!(key.code, KeyCode::Esc | KeyCode::Char('y' | 'n'))
            }
            Overlay::AgentMessage => {
                matches!(
                    key.code,
                    KeyCode::Esc
                        | KeyCode::Enter
                        | KeyCode::Backspace
                        | KeyCode::Delete
                        | KeyCode::Left
                        | KeyCode::Right
                        | KeyCode::Home
                        | KeyCode::End
                ) || matches!(key.code, KeyCode::Char(_))
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
            }
            Overlay::Approval => {
                matches!(key.code, KeyCode::Esc | KeyCode::Up | KeyCode::Down)
                    || key.modifiers.is_empty()
                        && matches!(
                            key.code,
                            KeyCode::Enter | KeyCode::Char('a' | 's' | 'd' | 'e')
                        )
            }
            Overlay::ApprovalEdit => {
                matches!(
                    key.code,
                    KeyCode::Esc
                        | KeyCode::Enter
                        | KeyCode::Backspace
                        | KeyCode::Delete
                        | KeyCode::Left
                        | KeyCode::Right
                        | KeyCode::Home
                        | KeyCode::End
                        | KeyCode::Up
                        | KeyCode::Down
                ) || matches!(key.code, KeyCode::Char(_))
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            }
            Overlay::Onboarding => matches!(
                key.code,
                KeyCode::Char(_)
                    | KeyCode::Backspace
                    | KeyCode::Enter
                    | KeyCode::Esc
                    | KeyCode::Up
                    | KeyCode::Down
            ),
        }
    }

    pub fn handle_event(&mut self, event: Event) -> Result<bool, String> {
        let key = match event {
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.state.scroll_transcript(3);
                    }
                    MouseEventKind::ScrollDown => {
                        self.state.scroll_transcript(-3);
                    }
                    _ => {}
                }
                return Ok(false);
            }
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
            Event::Key(key) if key.kind != KeyEventKind::Release => key,
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
                KeyCode::PageUp => {
                    self.state.scroll = self.state.scroll.saturating_add(
                        self.state.viewport_height.get().saturating_sub(1).max(1) as usize,
                    )
                }
                KeyCode::PageDown => {
                    self.state.scroll = self.state.scroll.saturating_sub(
                        self.state.viewport_height.get().saturating_sub(1).max(1) as usize,
                    )
                }
                KeyCode::Home => self.state.scroll = self.state.max_transcript_scroll(),
                KeyCode::End => self.state.scroll = 0,
                KeyCode::Char('{') => self
                    .state
                    .jump_user_turn(-1, self.state.transcript_width.get() as usize),
                KeyCode::Char('}') => self
                    .state
                    .jump_user_turn(1, self.state.transcript_width.get() as usize),
                _ => {}
            }
            self.state.scroll = self.state.scroll.min(self.state.max_transcript_scroll());
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
                let page = self.state.viewport_height.get().saturating_sub(1).max(1) as usize;
                self.state.selected_todo = match key.code {
                    KeyCode::Esc => {
                        self.state.close_overlay();
                        self.state.selected_todo
                    }
                    KeyCode::Up => self.state.selected_todo.saturating_sub(1),
                    KeyCode::Down => self.state.selected_todo.saturating_add(1),
                    KeyCode::PageUp => self.state.selected_todo.saturating_sub(page),
                    KeyCode::PageDown => self.state.selected_todo.saturating_add(page),
                    KeyCode::Home => 0,
                    KeyCode::End => self.state.todos.len().saturating_sub(1),
                    _ => self.state.selected_todo,
                }
                .min(self.state.todos.len().saturating_sub(1));
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
        self.state.normalize_composer_cursor();
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
                        let next = next_grapheme_boundary(&self.state.composer, self.state.cursor);
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
                let previous = previous_grapheme_boundary(&self.state.composer, self.state.cursor);
                self.state.composer.drain(previous..self.state.cursor);
                self.state.cursor = previous;
                self.state.composer_edited();
            }
            KeyCode::Delete if self.state.cursor < self.state.composer.len() => {
                let next = next_grapheme_boundary(&self.state.composer, self.state.cursor);
                self.state.composer.drain(self.state.cursor..next);
                self.state.composer_edited();
            }
            KeyCode::Left => {
                self.state.cursor =
                    previous_grapheme_boundary(&self.state.composer, self.state.cursor);
            }
            KeyCode::Right if self.state.cursor < self.state.composer.len() => {
                self.state.cursor = next_grapheme_boundary(&self.state.composer, self.state.cursor);
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
                    self.state.composer_inner_width.get() as usize,
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
                    self.state.composer_inner_width.get() as usize,
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
                Command::Goal(action) => self.handle_goal_command(action)?,
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
        self.state.normalize_agent_message_cursor();
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
                    let previous = previous_grapheme_boundary(
                        &self.state.agent_message,
                        self.state.agent_message_cursor,
                    );
                    self.state
                        .agent_message
                        .drain(previous..self.state.agent_message_cursor);
                    self.state.agent_message_cursor = previous;
                }
            }
            (Overlay::AgentMessage, KeyCode::Left) => {
                self.state.agent_message_cursor = previous_grapheme_boundary(
                    &self.state.agent_message,
                    self.state.agent_message_cursor,
                );
            }
            (Overlay::AgentMessage, KeyCode::Right)
                if self.state.agent_message_cursor < self.state.agent_message.len() =>
            {
                self.state.agent_message_cursor = next_grapheme_boundary(
                    &self.state.agent_message,
                    self.state.agent_message_cursor,
                );
            }
            (Overlay::AgentMessage, KeyCode::Delete) => {
                let next = next_grapheme_boundary(
                    &self.state.agent_message,
                    self.state.agent_message_cursor,
                );
                self.state
                    .agent_message
                    .drain(self.state.agent_message_cursor..next);
            }
            (Overlay::AgentMessage, KeyCode::Home) => self.state.agent_message_cursor = 0,
            (Overlay::AgentMessage, KeyCode::End) => {
                self.state.agent_message_cursor = self.state.agent_message.len()
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
        self.state.normalize_composer_cursor();
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

    fn handle_goal_command(&mut self, action: GoalAction) -> Result<(), String> {
        match action {
            GoalAction::View => self.state.push_goal_status(),
            GoalAction::Set(objective) => {
                if !self.state.submit_goal(objective) {
                    self.state
                        .push_error("finish or interrupt the current turn first");
                }
            }
            GoalAction::Edit(objective) => {
                if self.state.goal.is_none() {
                    self.state.push_error("no active goal");
                } else {
                    self.state
                        .queue_command(EngineCommand::EditGoal { objective });
                }
            }
            GoalAction::Pause => {
                if !self
                    .state
                    .goal
                    .as_ref()
                    .is_some_and(|goal| goal.status.is_active())
                {
                    self.state.push_error("no pursuing goal");
                } else {
                    self.state.queue_command(EngineCommand::PauseGoal);
                }
            }
            GoalAction::Resume => {
                if !self.state.resume_goal() {
                    self.state.push_error("no paused goal to resume");
                }
            }
            GoalAction::Clear => {
                if self.state.goal.is_none() {
                    self.state.push_error("no active goal");
                } else {
                    self.state.queue_command(EngineCommand::ClearGoal);
                }
            }
        }
        Ok(())
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
                EngineCommand::SetGoal { objective } => engine.set_goal(objective).await,
                EngineCommand::EditGoal { objective } => engine.edit_goal(objective).await,
                EngineCommand::PauseGoal => engine.pause_goal().await,
                EngineCommand::ResumeGoal => engine.resume_goal().await,
                EngineCommand::ClearGoal => engine.clear_goal().await,
                EngineCommand::Agent(command) => engine.agent_command(command).await,
                EngineCommand::Shutdown => engine.shutdown().await,
            }
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

fn prepare_fullscreen_frame<B>(
    state: &mut TuiState,
    terminal: &mut Terminal<B>,
    transcript_cache: &mut TranscriptRenderCache,
) -> Result<(), String>
where
    B: Backend,
{
    terminal.autoresize().map_err(|error| error.to_string())?;
    if matches!(
        state.overlay(),
        Overlay::Onboarding
            | Overlay::Agents
            | Overlay::Todos
            | Overlay::AgentInspect
            | Overlay::AgentMessage
            | Overlay::ConfirmAgentCancel
    ) {
        return Ok(());
    }
    transcript_cache.prepare(state, terminal.get_frame().area());
    Ok(())
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
    B: Backend,
{
    run_loop(&mut app, terminal, &mut input, Some(runtime_events), None).await?;
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
        // The completing operation has already emitted its deltas. Drain the
        // queued snapshot, not an endlessly refilled stream from another tool.
        for _ in 0..tool_receiver.len() {
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
            | RuntimeEvent::GoalUpdated { .. }
            | RuntimeEvent::GoalCleared
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
) -> (bool, bool, bool) {
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
    (immediate_redraw, exit, processed > 0)
}

fn handle_input_with_current_geometry(
    app: &mut App,
    cache: &mut TranscriptRenderCache,
    area: Rect,
    event: Event,
) -> Result<bool, String> {
    if let Event::Mouse(mouse) = event {
        cache.prepare(&mut app.state, area);
        if let Some(action) = update_transcript_pointer(&mut app.state, cache, area, mouse) {
            apply_pointer_action(app, action);
        }
        return Ok(false);
    }
    if let Event::Key(key) = &event {
        if key.kind != KeyEventKind::Release && app.state.overlay() == Overlay::None {
            if key.code == KeyCode::Esc
                && (app.state.transcript_selection.is_some() || app.state.selection_copied)
            {
                app.state.transcript_selection = None;
                app.state.selection_copied = false;
                return Ok(false);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && key.code == KeyCode::Char('c')
                && let Some(selection) = app
                    .state
                    .transcript_selection
                    .as_ref()
                    .filter(|selection| selection.dragged)
            {
                let text = selection.text(&cache.lines);
                if let Some(selection) = app.state.transcript_selection.as_mut() {
                    selection.dragging = false;
                }
                apply_pointer_action(app, PointerAction::Copy(text));
                return Ok(false);
            }
        }
        if key.kind != KeyEventKind::Release {
            app.state.transcript_selection = None;
            app.state.selection_copied = false;
        }
    } else if matches!(&event, Event::Paste(_)) {
        app.state.transcript_selection = None;
        app.state.selection_copied = false;
    }
    if app.state.transcript_view_expanded()
        && matches!(
            &event,
            Event::Key(KeyEvent {
                code: KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::Char('{')
                    | KeyCode::Char('}'),
                ..
            })
        )
    {
        cache.prepare(&mut app.state, area);
    }
    app.handle_event(event)
}

#[derive(Debug, PartialEq, Eq)]
enum PointerAction {
    Open(Arc<str>),
    Copy(String),
}

fn update_transcript_pointer(
    state: &mut TuiState,
    cache: &TranscriptRenderCache,
    area: Rect,
    mouse: MouseEvent,
) -> Option<PointerAction> {
    if state.overlay() != Overlay::None {
        return None;
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            state.selection_copied = false;
            state.transcript_selection = cache.point_at(state, area, mouse, false).map(|point| {
                TranscriptSelection::new(point, cache.lines[point.row].link_at(point.column))
            });
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let point = cache.point_at(state, area, mouse, true);
            if let Some(selection) = state
                .transcript_selection
                .as_mut()
                .filter(|selection| selection.dragging)
                && let Some(point) = point
            {
                selection.update(point);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let mut selection = state.transcript_selection.take()?;
            if !selection.dragging {
                state.transcript_selection = Some(selection);
                return None;
            }
            let released = cache.point_at(state, area, mouse, false);
            let point = cache.point_at(state, area, mouse, true)?;
            if point != selection.anchor {
                selection.update(point);
            }
            selection.finish(point);
            if selection.dragged {
                let text = selection.text(&cache.lines);
                if !text.is_empty() {
                    state.transcript_selection = Some(selection);
                    return Some(PointerAction::Copy(text));
                }
            } else if released == Some(selection.anchor)
                && let Some(target) = selection.pressed_link
                && cache.lines[point.row].link_at(point.column).as_deref() == Some(target.as_ref())
            {
                return Some(PointerAction::Open(target));
            }
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            state.scroll_transcript(if mouse.kind == MouseEventKind::ScrollUp {
                3
            } else {
                -3
            });
            let point = cache.point_at(state, area, mouse, true);
            if let Some(selection) = state
                .transcript_selection
                .as_mut()
                .filter(|selection| selection.dragging)
                && let Some(point) = point
            {
                selection.update(point);
            }
        }
        _ => {}
    }
    None
}

fn apply_pointer_action(app: &mut App, action: PointerAction) {
    let result = match action {
        PointerAction::Open(target) => {
            app.link_tasks
                .spawn(async move { open_link(&target).await });
            Ok(())
        }
        PointerAction::Copy(text) => copy_to_clipboard(&text).inspect(|()| {
            app.state.selection_copied = true;
            if let Some(selection) = app.state.transcript_selection.as_mut() {
                selection.copied = true;
            }
        }),
    };
    if let Err(error) = result {
        app.state.push_error(error);
    }
}

fn handle_preapproval_input(
    app: &mut App,
    cache: &mut TranscriptRenderCache,
    area: Rect,
    origin: &mut Overlay,
    event: Event,
) -> Result<(bool, bool), String> {
    if matches!(&event, Event::Mouse(_)) {
        return Ok((false, false));
    }
    let interrupt = matches!(&event, Event::Key(key)
        if key.code == KeyCode::Esc
            || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')));
    if interrupt {
        return Ok((
            true,
            handle_input_with_current_geometry(app, cache, area, event)?,
        ));
    }
    if matches!(*origin, Overlay::Approval | Overlay::ApprovalEdit) {
        // Responses queued for a replaced prompt cannot authorize its successor.
        return Ok((false, false));
    }
    let visible_overlay = std::mem::replace(&mut app.state.overlay, *origin);
    let accepted = app.accepts_event(&event);
    let result = if accepted {
        handle_input_with_current_geometry(app, cache, area, event)
    } else {
        Ok(false)
    };
    *origin = app.state.overlay;
    if app.state.approval.is_some() {
        app.state.overlay = visible_overlay;
    }
    Ok((accepted, result?))
}

async fn run_loop<B>(
    app: &mut App,
    terminal: &mut Terminal<B>,
    input: &mut mpsc::Receiver<Event>,
    runtime_events: Option<mpsc::Receiver<RuntimeEvent>>,
    tool_events: Option<mpsc::Receiver<RuntimeEvent>>,
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
    let (tool_tx, mut tool_receiver) = mpsc::channel(1);
    let mut tool_open = if let Some(events) = tool_events {
        tool_receiver = events;
        true
    } else {
        drop(tool_tx);
        false
    };
    let mut input_open = true;
    let mut pending_input = None;
    let mut approval_generation = app.state.approval_generation();
    let mut preapproval_input = if app.state.approval.is_some() {
        input.len()
    } else {
        0
    };
    let mut preapproval_overlay = Overlay::None;
    let mut transcript_cache = TranscriptRenderCache::default();
    // Establish a clean canvas once per run/restart. Ratatui's frame diff clears
    // cells removed by subsequent redraws without erasing the screen each tick.
    // resize resets the back buffer without clear's blocking cursor-position query.
    let area = terminal.size().map_err(|error| error.to_string())?.into();
    terminal.resize(area).map_err(|error| error.to_string())?;
    prepare_fullscreen_frame(&mut app.state, terminal, &mut transcript_cache)?;
    terminal
        .draw(|frame| render_with_transcript(frame, &app.state, transcript_cache.lines()))
        .map_err(|error| error.to_string())?;
    let mut last_draw = tokio::time::Instant::now();
    let mut next_activity_frame = last_draw + ACTIVITY_FRAME_INTERVAL;
    let mut redraw_pending = false;

    while input_open || runtime_open || tool_open {
        let mut exit = false;
        let mut force_redraw = false;
        let mut state_changed = false;
        let mut input_origin = app.state.overlay;
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
            event = async { if pending_input.is_some() { pending_input.take() } else { input.recv().await } }, if input_open => {
                let predates_approval = preapproval_input > 0;
                if event.is_some() {
                    preapproval_input = preapproval_input.saturating_sub(1);
                }
                match event {
                    Some(Event::Resize(..)) => {
                        for _ in 0..READY_EVENT_BATCH_LIMIT {
                            match input.try_recv() {
                                Ok(Event::Resize(..)) => {
                                    preapproval_input = preapproval_input.saturating_sub(1);
                                }
                                Ok(event) => { pending_input = Some(event); break; }
                                Err(_) => break,
                            }
                        }
                        state_changed = true;
                        force_redraw = true;
                    }
                    Some(event) if predates_approval && app.state.approval.is_some() => {
                        let outcome = handle_preapproval_input(
                            app, &mut transcript_cache, terminal_area, &mut preapproval_overlay, event,
                        )?;
                        state_changed |= outcome.0;
                        force_redraw |= outcome.0;
                        exit |= outcome.1;
                    }
                    Some(event) if app.accepts_event(&event) => {
                        exit = handle_input_with_current_geometry(app, &mut transcript_cache, terminal_area, event)?;
                        state_changed = true;
                        force_redraw = true;
                    }
                    Some(_) => {}
                    None => input_open = false,
                }
                input_origin = if predates_approval { preapproval_overlay } else { app.state.overlay };
                // Input keeps first refusal for interrupts, but every input event
                // also gives ready runtime/tool events a bounded turn. Even ignored
                // input must not postpone approval, completion, or shutdown.
                if !exit {
                    let batch = drain_ready_events(
                        &mut app.state,
                        &mut runtime_receiver,
                        &mut runtime_open,
                        &mut tool_receiver,
                        &mut tool_open,
                    );
                    force_redraw |= batch.0;
                    exit |= batch.1;
                    state_changed |= batch.2;
                }
            }
            event = runtime_receiver.recv(), if runtime_open => {
                match event {
                    Some(event) => {
                        state_changed = true;
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
            result = app.link_tasks.join_next(), if !app.link_tasks.is_empty() => {
                let error = match result {
                    Some(Ok(Err(error))) => Some(error),
                    Some(Err(error)) => Some(format!("link opener failed: {error}")),
                    _ => None,
                };
                if let Some(error) = error {
                    app.state.push_error(error);
                    state_changed = true;
                    force_redraw = true;
                }
            }
            _ = &mut stream_redraw => force_redraw = true,
            _ = &mut animation => {
                force_redraw = true;
            }
        }
        if approval_generation != app.state.approval_generation() {
            approval_generation = app.state.approval_generation();
            // Keep input already queued before this prompt in its original UI
            // context, including a key held by resize coalescing. Runtime work
            // still progresses; a fresh response is required for the new prompt.
            preapproval_input = input
                .len()
                .saturating_add(usize::from(pending_input.is_some()));
            preapproval_overlay = input_origin;
        }
        if !app.state.sent_commands().is_empty() {
            app.flush_commands().await?;
        }
        redraw_pending |= state_changed;
        let now = tokio::time::Instant::now();
        let channels_closed = !input_open && !runtime_open && !tool_open;
        if force_redraw || stream_redraw_due(last_draw, now, redraw_pending, channels_closed) {
            prepare_fullscreen_frame(&mut app.state, terminal, &mut transcript_cache)?;
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
        None | Some(SearchConfig::Provider) if active.kind == ProfileKind::OpenAi => {
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
        None | Some(SearchConfig::Provider) if active.kind == ProfileKind::CodexCli => {
            Ok(Some(Arc::new(CodexNativeSearch::new(
                active.command.as_deref().unwrap_or("codex"),
                active.model.clone(),
            ))))
        }
        None | Some(SearchConfig::Provider) if active.kind == ProfileKind::ClaudeCli => {
            Ok(Some(Arc::new(ClaudeNativeSearch::new(
                active.command.as_deref().unwrap_or("claude"),
                active.model.clone(),
            ))))
        }
        Some(SearchConfig::Provider) => Err(format!(
            "profile {active_profile} does not support native web search; configure [search] kind = \"json\" with a search endpoint"
        )),
        None => Ok(None),
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

async fn open_link(target: &str) -> Result<(), String> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "rundll32.exe"
    } else {
        "xdg-open"
    };
    let mut command = tokio::process::Command::new(program);
    #[cfg(target_os = "windows")]
    command.arg("url.dll,FileProtocolHandler");
    let status = command
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map_err(|error| format!("could not open link: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("system link opener failed: {status}"))
    }
}

#[cfg(test)]
fn copy_to_clipboard(_text: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(not(test))]
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    for program in ["pbcopy", "wl-copy", "xclip"] {
        let mut command = std::process::Command::new(program);
        if program == "xclip" {
            command.args(["-selection", "clipboard"]);
        }
        let mut child = match command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => continue,
        };
        let written = child
            .stdin
            .take()
            .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
        if !written {
            let _ = child.kill();
        }
        if child.wait().is_ok_and(|status| status.success()) && written {
            return Ok(());
        }
    }
    if io::IsTerminal::is_terminal(&io::stdout()) {
        let mut encoded = String::new();
        base64_encode(text.as_bytes(), &mut encoded);
        let mut stdout = io::stdout();
        write!(stdout, "\x1b]52;c;{encoded}\x07")
            .and_then(|()| stdout.flush())
            .map_err(|error| format!("could not send text to the clipboard: {error}"))
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("git diff failed");
        if first.contains("not a git repository") || first.contains("Not a git repository") {
            return Err("not a git repository".into());
        }
        let mut message = first.to_owned();
        if message.chars().count() > 200 {
            let end = message
                .char_indices()
                .nth(200)
                .map(|(index, _)| index)
                .unwrap_or(message.len());
            message.truncate(end);
            message.push('…');
        }
        return Err(message);
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
        backend::{Backend, ClearType, TestBackend, WindowSize},
        buffer::Cell as BufferCell,
        layout::{Position, Rect, Size},
    };

    use super::*;
    use crate::tui::{ActivityState, TranscriptEntry, transcript_lines, visible_activity_rect};

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
            link_tasks: tokio::task::JoinSet::new(),
        }
    }

    #[tokio::test]
    async fn pending_desktop_link_does_not_block_exit() {
        let mut app = test_app();
        app.link_tasks.spawn(std::future::pending());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let (sender, mut input) = mpsc::channel(1);
        sender
            .send(Event::Key(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )))
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(1),
            run_loop(&mut app, &mut terminal, &mut input, None, None),
        )
        .await
        .expect("desktop handler must not block input")
        .unwrap();
        assert!(app.exit_requested);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_profile_searches_without_a_separate_search_configuration() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("codex");
        std::fs::write(&program, r#"#!/bin/sh
input=$(cat)
case "$*" in
  *'web_search="live"'*)
    printf '%s\n' '{"type":"item.completed","item":{"type":"web_search","query":"Rust async book","action":{"type":"search"}}}'
    printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"{\"results\":[{\"title\":\"Async Book\",\"url\":\"https://rust-lang.github.io/async-book/\",\"snippet\":\"Official Rust async guide.\"}]}"}}'
    ;;
  *)
    printf '%s\n' '{"type":"thread.started","thread_id":"search-session"}'
    case "$input" in
    *'https://rust-lang.github.io/async-book/'*)
      printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"{\"kind\":\"final\",\"text\":\"Found the guide.\"}"}}'
      ;;
    *)
      printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"{\"kind\":\"tool_calls\",\"text\":\"\",\"calls\":[{\"call_id\":\"search\",\"name\":\"web-search\",\"arguments\":\"{\\\"operation\\\":\\\"search\\\",\\\"query\\\":\\\"Rust async book\\\",\\\"limit\\\":2}\"}],\"agents\":[]}"}}'
      ;;
    esac
    ;;
esac
printf '%s\n' '{"type":"turn.completed"}'
"#).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let paths = AppPaths::from_root(root.path().join("state"));
        let mut config = empty_config();
        config.default_profile = Some("codex".into());
        config.profiles.insert(
            "codex".into(),
            ProfileConfig {
                kind: ProfileKind::CodexCli,
                model: "fixture".into(),
                command: Some(program.display().to_string()),
                endpoint: None,
                auth: None,
                max_input_tokens: 32_000,
                max_output_tokens: 4_000,
                escalation_profiles: Vec::new(),
            },
        );
        ConfigRepository::open(paths.clone())
            .unwrap()
            .write_config(&config)
            .unwrap();
        let mut app = App::bootstrap_with_paths(
            &Args::default(),
            root.path().to_path_buf(),
            paths,
            SessionSecrets::default(),
        )
        .unwrap();
        app.engine
            .as_ref()
            .unwrap()
            .submit("Find the official Rust async book.", false)
            .await
            .unwrap();
        let results = tokio::time::timeout(Duration::from_secs(5), async {
            let mut results = Vec::new();
            while let Some(event) = app.runtime_events.as_mut().unwrap().recv().await {
                match event {
                    RuntimeEvent::ToolCompleted { result, .. } => results.push(result),
                    RuntimeEvent::TurnCompleted => break,
                    RuntimeEvent::Error { message } => panic!("{message}"),
                    _ => {}
                }
            }
            results
        })
        .await
        .expect("search turn completes");
        app.engine.as_ref().unwrap().shutdown().await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(!results[0].is_error, "{}", results[0].output);
        assert_eq!(
            results[0].metadata["results"][0]["url"],
            "https://rust-lang.github.io/async-book/"
        );
    }

    fn display_app() -> (tempfile::TempDir, Arc<FsSessionStore>, App) {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(FsSessionStore::open(temp.path().to_path_buf()).unwrap());
        let mut app = test_app();
        app.state.set_display_store(Arc::clone(&store));
        (temp, store, app)
    }

    fn displayed_tool_output(app: &App, index: usize) -> &str {
        match &app.state.transcript[index] {
            TranscriptEntry::ToolCall(tool) => &tool.output,
            _ => panic!("expected tool output"),
        }
    }

    fn toggle_transcript(app: &mut App) {
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
        )))
        .unwrap();
    }

    #[test]
    fn completed_display_previews_keep_mixed_streams_and_reused_call_ids() {
        let (_temp, store, mut app) = display_app();
        let stdout = format!("stdout head\n{}stdout tail\n", "界🙂a\n".repeat(32_768));
        let stderr = format!("stderr head\n{}stderr tail\n", "warning\n".repeat(32_768));
        let mut result = ToolResult::success(CallId::from("reused"), "model summary");
        result.truncated = true;
        result.metadata = serde_json::json!({
            "tool_name": "bash",
            "display_blobs": {
                "stdout": store.put_blob(stdout.as_bytes()).unwrap(),
                "stderr": store.put_blob(stderr.as_bytes()).unwrap(),
            },
        });
        app.state.apply_runtime_event(RuntimeEvent::ToolCompleted {
            operation_id: OperationId::from("first"),
            result,
        });
        let preview = displayed_tool_output(&app, 0).to_owned();
        assert!(preview.len() <= 128 * 1_024);
        assert!(preview.contains("stdout tail\n\n[stderr]\n"));
        assert!(preview.ends_with("stderr tail\n"));
        assert!(!preview.contains('\u{fffd}'));
        toggle_transcript(&mut app);
        let full = format!("{stdout}\n[stderr]\n{stderr}");
        assert_eq!(displayed_tool_output(&app, 0), full);
        let mut cache = TranscriptRenderCache::default();
        cache.prepare(&mut app.state, Rect::new(0, 0, 80, 24));

        // A later operation can reuse the provider's call id. Completing it while
        // expanded must hydrate the new entry without replacing the first source.
        let second = format!("second head\n{}second tail\n", "line\n".repeat(32_768));
        let mut result = ToolResult::success(CallId::from("reused"), "second summary");
        result.truncated = true;
        result.metadata = serde_json::json!({
            "tool_name": "read",
            "display_output": second,
            "display_blobs": { "output": store.put_blob(second.as_bytes()).unwrap() },
        });
        app.state
            .apply_runtime_event(RuntimeEvent::ToolOutputDelta {
                call_id: CallId::from("reused"),
                stream: "stdout".into(),
                chunk: "partial".into(),
            });
        app.state.apply_runtime_event(RuntimeEvent::ToolCompleted {
            operation_id: OperationId::from("second"),
            result,
        });
        cache.prepare(&mut app.state, Rect::new(0, 0, 80, 24));
        assert_eq!(displayed_tool_output(&app, 0), full);
        assert_eq!(displayed_tool_output(&app, 1), second);
        assert!(
            cache
                .lines()
                .unwrap()
                .iter()
                .any(|line| line.text.to_string().contains("second head"))
        );
        toggle_transcript(&mut app);
        cache.prepare(&mut app.state, Rect::new(0, 0, 80, 24));
        assert_eq!(displayed_tool_output(&app, 0), preview);
        assert!(displayed_tool_output(&app, 1).len() <= 128 * 1_024);
        assert!(
            !cache
                .lines()
                .unwrap()
                .iter()
                .any(|line| line.text.to_string().contains("second head"))
        );
        toggle_transcript(&mut app);
        assert_eq!(displayed_tool_output(&app, 0), full);
        assert_eq!(displayed_tool_output(&app, 1), second);
        app.state.hydrate_replay(&[]);
        toggle_transcript(&mut app);
        assert!(app.state.transcript.is_empty());
    }

    #[test]
    fn display_blob_corruption_is_visible_on_resume_and_expansion() {
        let (_temp, store, mut app) = display_app();
        let full = format!("head\n{}tail\n", "x".repeat(256 * 1_024));
        let reference = store.put_blob(full.as_bytes()).unwrap();
        let mut result = ToolResult::success(CallId::from("call"), "fallback summary");
        result.truncated = true;
        result.metadata = serde_json::json!({"display_blobs": {"output": reference}});
        let replay = [EventEnvelope::new(
            0,
            1,
            "session".into(),
            None,
            SessionEvent::ToolCompleted {
                operation_id: OperationId::from("operation"),
                result,
            },
        )];
        app.state.hydrate_replay(&replay);
        assert!(displayed_tool_output(&app, 0).ends_with("tail\n"));
        let path = store.root().join("blobs").join(&reference.sha256);
        let mut corrupt = full.as_bytes().to_vec();
        corrupt[0] = b'!';
        std::fs::write(&path, &corrupt).unwrap();
        toggle_transcript(&mut app);
        assert!(displayed_tool_output(&app, 0).contains("unavailable"));
        assert!(displayed_tool_output(&app, 0).contains("mismatch"));
        toggle_transcript(&mut app);
        app.state.hydrate_replay(&replay);
        assert!(displayed_tool_output(&app, 0).contains("fallback summary"));
        assert!(displayed_tool_output(&app, 0).contains("unavailable"));
        std::fs::write(&path, full.as_bytes()).unwrap();
        toggle_transcript(&mut app);
        assert_eq!(displayed_tool_output(&app, 0), full);
    }

    #[test]
    fn completed_output_without_store_remains_available_after_collapse() {
        let mut app = test_app();
        let full = format!("head\n{}tail\n", "x".repeat(256 * 1_024));
        let mut result = ToolResult::success(CallId::from("call"), "model summary");
        result.truncated = true;
        result.metadata = serde_json::json!({
            "display_output": full,
            // No filesystem control exists in an embedded App. The supplied
            // output is the only retrievable copy, regardless of this reference.
            "display_blobs": {"output": {"sha256": "0".repeat(64), "bytes": full.len()}},
        });
        app.state.apply_runtime_event(RuntimeEvent::ToolCompleted {
            operation_id: OperationId::from("operation"),
            result,
        });
        assert_eq!(displayed_tool_output(&app, 0), full);
        toggle_transcript(&mut app);
        assert_eq!(displayed_tool_output(&app, 0), full);
        toggle_transcript(&mut app);
        assert_eq!(displayed_tool_output(&app, 0), full);
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
    fn expanded_transcript_scrolling_clamps_and_closing_returns_to_live_tail() {
        let mut app = test_app();
        app.state.push_assistant("line\n\n".repeat(100));
        app.state.viewport_height.set(10);
        app.state.toggle_transcript_view();
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)))
            .unwrap();
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert_eq!(app.state.scroll, 10);
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)))
            .unwrap();
        let first = app.state.scroll;
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert_eq!(app.state.scroll, first);
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        assert!(!app.state.transcript_view_expanded());
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
    async fn sustained_input_cannot_starve_tool_completion_approval_or_shutdown() {
        let mut app = test_app();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let (input_sender, mut input) = mpsc::channel(513);
        for _ in 0..512 {
            input_sender.try_send(Event::FocusGained).unwrap();
        }
        input_sender
            .try_send(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
            .unwrap();
        let (runtime_sender, runtime_events) = mpsc::channel(4);
        let (tool_sender, tool_events) = mpsc::channel(1);
        tool_sender
            .try_send(RuntimeEvent::ToolOutputDelta {
                call_id: CallId::from("call"),
                stream: "stdout".into(),
                chunk: "partial".into(),
            })
            .unwrap();
        for event in [
            RuntimeEvent::ToolCompleted {
                operation_id: OperationId::from("tool"),
                result: ToolResult::success(CallId::from("call"), "canonical completion"),
            },
            RuntimeEvent::ApprovalRequired {
                request: ApprovalRequest {
                    operation_id: OperationId::from("approval"),
                    operation: Operation::Read {
                        path: "file".into(),
                        external: false,
                    },
                    summary: "Read file".into(),
                    arguments: serde_json::json!({"path": "file"}),
                },
            },
            RuntimeEvent::Shutdown,
        ] {
            runtime_sender.try_send(event).unwrap();
        }
        run_loop(
            &mut app,
            &mut terminal,
            &mut input,
            Some(runtime_events),
            Some(tool_events),
        )
        .await
        .unwrap();

        assert!(
            matches!(&app.state.transcript[..], [TranscriptEntry::ToolCall(tool)]
            if tool.output == "canonical completion" && tool.lifecycle == crate::tui::ToolLifecycle::Completed)
        );
        assert!(app.state.approval.is_some());
        assert_eq!(app.state.activity(), &ActivityState::Idle);
        // Check progress while input is continuously ready, not elapsed time.
        assert!(input.len() >= 510);
        assert!(!app.exit_requested);
    }

    #[tokio::test]
    async fn queued_composer_input_cannot_answer_a_new_approval() {
        for first in [
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Event::Resize(80, 24),
        ] {
            let resized = matches!(first, Event::Resize(..));
            let mut app = test_app();
            app.state.set_thinking();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let (input_sender, mut input) = mpsc::channel(3);
            for event in [
                first,
                Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
                Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)),
            ] {
                input_sender.try_send(event).unwrap();
            }
            let (runtime_sender, runtime_events) = mpsc::channel(2);
            runtime_sender
                .try_send(RuntimeEvent::ApprovalRequired {
                    request: ApprovalRequest {
                        operation_id: OperationId::from("new-approval"),
                        operation: Operation::Read {
                            path: "file".into(),
                            external: false,
                        },
                        summary: "Read file".into(),
                        arguments: serde_json::json!({"path":"file"}),
                    },
                })
                .unwrap();
            runtime_sender.try_send(RuntimeEvent::Shutdown).unwrap();
            run_loop(
                &mut app,
                &mut terminal,
                &mut input,
                Some(runtime_events),
                None,
            )
            .await
            .unwrap();
            assert!(
                app.state.approval.is_some(),
                "typeahead answered the prompt"
            );
            assert_eq!(app.state.composer, if resized { "a" } else { "ca" });
            assert!(app.state.sent_commands().is_empty());

            app.handle_event(Event::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
            )))
            .unwrap();
            assert!(matches!(
                app.state.sent_commands(),
                [EngineCommand::ResolveApproval {
                    response: ApprovalResponse::ApproveOnce,
                    ..
                }]
            ));
        }
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
    }

    #[test]
    fn wheel_scroll_moves_output_without_mutating_the_draft() {
        let mut app = test_app();
        app.state.push_assistant(
            (0..100)
                .map(|row| format!("output {row}\n\n"))
                .collect::<String>(),
        );
        app.state.remember_prompt("previous prompt");
        app.state.composer = "current draft".into();
        app.state.cursor = 3;
        let area = Rect::new(0, 0, 48, 12);
        let mut cache = TranscriptRenderCache::default();
        cache.prepare(&mut app.state, area);
        let wheel = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 4,
                row: 9,
                modifiers: KeyModifiers::NONE,
            })
        };
        let up = wheel(MouseEventKind::ScrollUp);
        assert!(app.accepts_event(&up));
        handle_input_with_current_geometry(&mut app, &mut cache, area, up).unwrap();
        assert!(app.state.scroll > 0);
        assert_eq!(app.state.composer, "current draft");
        assert_eq!(app.state.cursor, 3);
        handle_input_with_current_geometry(
            &mut app,
            &mut cache,
            area,
            wheel(MouseEventKind::ScrollDown),
        )
        .unwrap();
        assert_eq!(app.state.scroll, 0);
        assert_eq!(app.state.composer, "current draft");
        assert_eq!(app.state.cursor, 3);
        assert!(!app.accepts_event(&wheel(MouseEventKind::Moved)));
    }

    fn pointer(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn plain_link_click_opens_but_link_drag_selects_without_changing_the_draft() {
        let mut app = test_app();
        app.state
            .push_assistant("Select [reference](https://example.test/one).");
        app.state.composer = "keep draft".into();
        app.state.cursor = 4;
        let area = Rect::new(0, 0, 80, 24);
        let mut cache = TranscriptRenderCache::default();
        cache.prepare(&mut app.state, area);
        assert_eq!(
            update_transcript_pointer(
                &mut app.state,
                &cache,
                area,
                pointer(MouseEventKind::Down(MouseButton::Left), 9, 0)
            ),
            None
        );
        assert_eq!(
            update_transcript_pointer(
                &mut app.state,
                &cache,
                area,
                pointer(MouseEventKind::Up(MouseButton::Left), 9, 0)
            ),
            Some(PointerAction::Open(Arc::from("https://example.test/one")))
        );
        update_transcript_pointer(
            &mut app.state,
            &cache,
            area,
            pointer(MouseEventKind::Down(MouseButton::Left), 9, 0),
        );
        update_transcript_pointer(
            &mut app.state,
            &cache,
            area,
            pointer(MouseEventKind::Drag(MouseButton::Left), 13, 0),
        );
        assert_eq!(
            update_transcript_pointer(
                &mut app.state,
                &cache,
                area,
                pointer(MouseEventKind::Up(MouseButton::Left), 13, 0)
            ),
            Some(PointerAction::Copy("refer".into()))
        );
        assert_eq!(app.state.composer, "keep draft");
        assert_eq!(app.state.cursor, 4);
    }

    #[test]
    fn dragging_copies_the_visible_snapshot_while_streaming_markdown_reflows() {
        let mut app = test_app();
        app.state.apply_runtime_event(RuntimeEvent::AssistantDelta {
            text: "[reference](https://example.test".into(),
        });
        let area = Rect::new(0, 0, 80, 24);
        let mut cache = TranscriptRenderCache::default();
        cache.prepare(&mut app.state, area);
        update_transcript_pointer(
            &mut app.state,
            &cache,
            area,
            pointer(MouseEventKind::Down(MouseButton::Left), 3, 0),
        );
        app.state
            .apply_runtime_event(RuntimeEvent::AssistantDelta { text: ")".into() });
        cache.prepare(&mut app.state, area);
        update_transcript_pointer(
            &mut app.state,
            &cache,
            area,
            pointer(MouseEventKind::Drag(MouseButton::Left), 11, 0),
        );
        assert_eq!(
            update_transcript_pointer(
                &mut app.state,
                &cache,
                area,
                pointer(MouseEventKind::Up(MouseButton::Left), 11, 0)
            ),
            Some(PointerAction::Copy("reference".into()))
        );
        cache.prepare(&mut app.state, area);
        assert!(app.state.transcript_selection.is_none());
        assert!(cache.lines[0].text.to_string().starts_with("reference"));
    }

    #[test]
    fn resize_cancels_pointer_coordinates_before_a_link_can_open() {
        let mut app = test_app();
        app.state
            .push_assistant("[reference](https://example.test)");
        let mut cache = TranscriptRenderCache::default();
        let wide = Rect::new(0, 0, 80, 24);
        let narrow = Rect::new(0, 0, 40, 12);
        cache.prepare(&mut app.state, wide);
        update_transcript_pointer(
            &mut app.state,
            &cache,
            wide,
            pointer(MouseEventKind::Down(MouseButton::Left), 2, 0),
        );
        cache.prepare(&mut app.state, narrow);
        assert_eq!(
            update_transcript_pointer(
                &mut app.state,
                &cache,
                narrow,
                pointer(MouseEventKind::Up(MouseButton::Left), 2, 0)
            ),
            None
        );
    }

    #[test]
    fn escape_dismisses_copied_feedback_without_cancelling_the_active_turn() {
        let mut app = test_app();
        app.state.apply_runtime_event(RuntimeEvent::AssistantDelta {
            text: "still working".into(),
        });
        app.state.selection_copied = true;
        let mut cache = TranscriptRenderCache::default();
        handle_input_with_current_geometry(
            &mut app,
            &mut cache,
            Rect::new(0, 0, 80, 24),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        )
        .unwrap();
        assert!(!app.state.selection_copied);
        assert!(app.state.activity().is_animated());
        assert!(app.state.sent_commands().is_empty());
    }

    #[test]
    fn fullscreen_prepare_reuses_markdown_render_for_draw() {
        let mut state = TuiState::new("fixture", "frontier", ".", ExecutionMode::Supervised);
        state.apply_runtime_event(RuntimeEvent::AssistantDelta {
            text: "## Heading\n\n- one\n- two\n\n```rust\nfn main() {}\n```".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("fullscreen terminal");

        crate::tui::reset_transcript_render_calls();
        let mut transcript_cache = TranscriptRenderCache::default();
        prepare_fullscreen_frame(&mut state, &mut terminal, &mut transcript_cache)
            .expect("prepare fullscreen frame");
        terminal
            .draw(|frame| render_with_transcript(frame, &state, transcript_cache.lines()))
            .expect("render fullscreen frame");

        assert_eq!(crate::tui::transcript_render_calls(), 1);
    }

    #[tokio::test]
    async fn animation_redraws_reuse_unchanged_markdown() {
        let mut app = test_app();
        app.state.apply_runtime_event(RuntimeEvent::AssistantDelta {
            text: "## Heading\n\n- one\n- two\n\n```rust\nfn main() {}\n```".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("fullscreen terminal");
        let (input_sender, mut input) = mpsc::channel(1);
        let closer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            drop(input_sender);
        });

        crate::tui::reset_transcript_render_calls();
        run_loop(&mut app, &mut terminal, &mut input, None, None)
            .await
            .expect("run animated fullscreen frame");
        closer.await.expect("close input channel");

        assert_eq!(crate::tui::transcript_render_calls(), 1);
    }

    #[tokio::test]
    async fn composer_input_reuses_unchanged_markdown() {
        let mut app = test_app();
        app.state.apply_runtime_event(RuntimeEvent::AssistantDelta {
            text: "## Heading\n\n- one\n- two\n\n```rust\nfn main() {}\n```".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("fullscreen terminal");
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
        run_loop(&mut app, &mut terminal, &mut input, None, None)
            .await
            .expect("run composer redraw");

        assert_eq!(app.state.composer, "x");
        assert_eq!(crate::tui::transcript_render_calls(), 1);
    }

    #[tokio::test]
    async fn fullscreen_run_clears_old_cells_and_anchors_the_composer() {
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
            link_tasks: tokio::task::JoinSet::new(),
        };
        app.state.push_user("visible fullscreen question");
        let mut rows = vec![" ".repeat(80); 16];
        rows[0] = "stale shell header".into();
        rows[8] = "stale screen content".into();
        let mut backend = TestBackend::with_lines(rows);
        backend.set_cursor_position(Position::new(22, 8)).unwrap();
        let mut terminal = Terminal::new(backend).expect("fullscreen terminal");
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
        assert_eq!(app.state.transcript.len(), 1);
        assert!(!visible.contains("stale shell header"));
        assert!(!visible.contains("stale screen content"));
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 0, 80, 16));
        assert_eq!(terminal.backend_mut().get_cursor_position().unwrap().y, 12);
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
        assert_eq!(app.state.transcript.len(), 1);
    }

    #[tokio::test]
    async fn ignored_input_does_not_redraw_or_execute_global_shortcuts() {
        let mut app = test_app();
        let draws = Rc::new(Cell::new(1));
        let backend = DrawBudgetBackend::new(TestBackend::new(80, 24), draws.clone());
        let mut terminal = Terminal::new(backend).unwrap();
        let (sender, mut input) = mpsc::channel(8);
        for event in [
            Event::FocusGained,
            Event::FocusLost,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            }),
            Event::Key(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                KeyEventKind::Release,
            )),
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('o'),
                KeyModifiers::CONTROL,
                KeyEventKind::Release,
            )),
        ] {
            sender.send(event).await.unwrap();
        }
        drop(sender);
        run_loop(&mut app, &mut terminal, &mut input, None, None)
            .await
            .unwrap();
        assert_eq!(draws.get(), 0);
        assert!(!app.exit_requested);
        assert!(!app.state.transcript_view_expanded());
    }

    #[test]
    fn expanded_cache_uses_actual_width_and_true_prompt_anchors() {
        let mut app = test_app();
        for prompt in ["first prompt", "second prompt", "third prompt"] {
            app.state.push_user(prompt);
            app.state
                .push_tool("bash", "> not a user prompt\n".repeat(30));
        }
        app.state.toggle_transcript_view();
        app.state.composer_inner_width.set(90);
        let mut terminal = Terminal::new(TestBackend::new(32, 8)).unwrap();
        let mut cache = TranscriptRenderCache::default();
        prepare_fullscreen_frame(&mut app.state, &mut terminal, &mut cache).unwrap();
        terminal
            .draw(|frame| render_with_transcript(frame, &app.state, cache.lines()))
            .unwrap();
        crate::tui::reset_transcript_render_calls();
        for (key, prompt) in [
            (KeyCode::Home, "first prompt"),
            (KeyCode::Char('}'), "second prompt"),
            (KeyCode::Char('}'), "third prompt"),
            (KeyCode::Char('{'), "second prompt"),
        ] {
            app.handle_event(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)))
                .unwrap();
            prepare_fullscreen_frame(&mut app.state, &mut terminal, &mut cache).unwrap();
            terminal
                .draw(|frame| render_with_transcript(frame, &app.state, cache.lines()))
                .unwrap();
            let first_row: String = terminal.backend().buffer().content()[..32]
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(first_row.contains(prompt), "{first_row}");
        }
        app.state.apply_runtime_event(RuntimeEvent::Usage {
            usage: Usage {
                input_tokens: 100,
                ..Usage::default()
            },
        });
        prepare_fullscreen_frame(&mut app.state, &mut terminal, &mut cache).unwrap();
        assert_eq!(crate::tui::transcript_render_calls(), 0);
    }

    #[test]
    fn read_position_survives_new_output_and_height_changes() {
        for expanded in [false, true] {
            let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
            state.apply_runtime_event(RuntimeEvent::AssistantDelta {
                text: (0..80).map(|i| format!("row {i}\n\n")).collect(),
            });
            if expanded {
                state.toggle_transcript_view();
            }
            let mut cache = TranscriptRenderCache::default();
            cache.prepare(&mut state, Rect::new(0, 0, 40, 10));
            state.scroll = 40;
            let top = cache.lines.len() - cache.viewport_height as usize - state.scroll;
            let before = cache.lines[top].text.clone();
            state.apply_runtime_event(RuntimeEvent::AssistantDelta {
                text: "new output\n\n".into(),
            });
            cache.prepare(&mut state, Rect::new(0, 0, 40, 7));
            let top = cache.lines.len() - cache.viewport_height as usize - state.scroll;
            assert_eq!(cache.lines[top].text, before);
            state.scroll = usize::MAX;
            cache.prepare(&mut state, Rect::new(0, 0, 40, 7));
            assert_eq!(
                state.scroll,
                cache.lines.len() - cache.viewport_height as usize
            );
        }
    }

    #[test]
    fn composer_deletes_whole_graphemes_and_normalizes_invalid_byte_cursors() {
        let mut app = test_app();
        app.state.composer = "e\u{301}👨‍👩‍👧‍👦界".into();
        app.state.cursor = app.state.composer.len();
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert_eq!(app.state.composer, "e\u{301}👨‍👩‍👧‍👦");
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)))
            .unwrap();
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Delete,
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert_eq!(app.state.composer, "e\u{301}");
        app.state.cursor = 2;
        app.handle_event(Event::Paste("x".into())).unwrap();
        assert_eq!(app.state.composer, "xe\u{301}");
        assert!(app.state.composer.is_char_boundary(app.state.cursor));
    }

    #[test]
    fn context_remaining_tracks_the_latest_model_window() {
        let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        state.max_input_tokens = 1000;
        for (tokens, remaining) in [(0, 100), (900, 10), (200, 80), (1500, 0)] {
            state.apply_runtime_event(RuntimeEvent::Usage {
                usage: Usage {
                    input_tokens: tokens,
                    ..Usage::default()
                },
            });
            assert_eq!(
                state.context_label().unwrap(),
                format!("{remaining}% context left")
            );
        }
    }

    #[test]
    fn expanded_width_reflow_keeps_the_same_entry_at_the_reading_position() {
        let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        state.push_assistant("earlier wrapping text ".repeat(80));
        state.push_user("ANCHOR");
        state.push_assistant(
            (0..30)
                .map(|i| format!("short {i}\n\n"))
                .collect::<String>(),
        );
        state.toggle_transcript_view();
        let mut cache = TranscriptRenderCache::default();
        cache.prepare(&mut state, Rect::new(0, 0, 80, 10));
        let anchor = cache
            .lines
            .iter()
            .position(|line| {
                line.text
                    .spans
                    .iter()
                    .any(|span| span.content.contains("ANCHOR"))
            })
            .unwrap();
        state.scroll = cache.lines.len() - 9 - anchor;
        for width in [32, 100, 18] {
            cache.prepare(&mut state, Rect::new(0, 0, width, 10));
            let first = &cache.lines[cache.lines.len() - 9 - state.scroll];
            assert!(
                first
                    .text
                    .spans
                    .iter()
                    .any(|span| span.content.contains("ANCHOR")),
                "lost reading position at width {width}: {first:?}"
            );
        }
    }

    #[test]
    fn session_placeholder_is_stable_and_never_submitted_as_input() {
        let mut app = test_app();
        let session = SessionId::from("placeholder-session");
        app.state.set_composer_session(&session);
        let placeholder = app.state.composer_placeholder();
        for (width, height) in [(80, 24), (32, 8), (100, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| crate::tui::render(frame, &app.state))
                .unwrap();
            let visible = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            if width >= 80 {
                assert!(visible.contains(placeholder));
            }
            assert_eq!(app.state.composer_placeholder(), placeholder);
            assert!(app.state.composer.is_empty());
        }
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert!(app.state.sent_commands().is_empty());
        assert!(app.state.transcript.is_empty());
        app.state.composer = "actual draft".into();
        app.state.cursor = app.state.composer.len();
        app.state.clear_composer();
        app.state.set_thinking();
        app.state.apply_runtime_event(RuntimeEvent::TurnCompleted);
        assert_eq!(app.state.composer_placeholder(), placeholder);
        let mut resumed = TuiState::new(
            "other-profile",
            "other-model",
            ".",
            ExecutionMode::Supervised,
        );
        resumed.set_composer_session(&session);
        assert_eq!(resumed.composer_placeholder(), placeholder);
    }

    #[test]
    fn distinct_sessions_can_display_different_composer_prompts() {
        let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        let prompts = (0..32)
            .map(|index| {
                state.set_composer_session(&SessionId::from(format!("session-{index}")));
                state.composer_placeholder()
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            prompts.len() > 1,
            "session selection always returned the same prompt"
        );
    }

    #[test]
    fn prompt_navigation_uses_unpainted_stream_geometry_once() {
        let mut app = test_app();
        for prompt in ["first prompt", "second prompt", "third prompt"] {
            app.state.push_user(prompt);
            app.state.push_tool("bash", "tool output row\n".repeat(30));
        }
        app.state.toggle_transcript_view();
        let area = Rect::new(0, 0, 32, 8);
        let mut cache = TranscriptRenderCache::default();
        cache.prepare(&mut app.state, area);
        handle_input_with_current_geometry(
            &mut app,
            &mut cache,
            area,
            Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
        )
        .unwrap();
        app.state.apply_runtime_event(RuntimeEvent::AssistantDelta {
            text: "unpainted content ".repeat(20),
        });
        handle_input_with_current_geometry(
            &mut app,
            &mut cache,
            area,
            Event::Key(KeyEvent::new(KeyCode::Char('}'), KeyModifiers::NONE)),
        )
        .unwrap();
        cache.prepare(&mut app.state, area);
        let row = &cache.lines[cache.lines.len() - 7 - app.state.scroll];
        assert!(
            row.text
                .spans
                .iter()
                .any(|span| span.content.contains("second prompt")),
            "{row:?}"
        );
    }

    #[test]
    fn pasted_joiner_backspace_preserves_neighboring_composer_text() {
        let mut app = test_app();
        app.state.composer = "A\u{1f469}\u{1f467}Z".into();
        app.state.cursor = "A\u{1f469}".len();
        app.handle_event(Event::Paste("\u{200d}".into())).unwrap();
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )))
        .unwrap();
        assert_eq!(app.state.composer, "AZ");
    }

    #[test]
    fn incremental_stream_and_todo_updates_match_fresh_rendering() {
        let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        for index in 0..20 {
            state.push_user(format!("earlier prompt {index}"));
            state.push_assistant(format!("**earlier answer {index}**"));
        }
        state.toggle_transcript_view();
        let mut cache = TranscriptRenderCache::default();
        let area = Rect::new(0, 0, 48, 10);
        cache.prepare(&mut state, area);
        let todo = |items: serde_json::Value| {
            let mut result =
                kurama_protocol::tool::ToolResult::success("todo-call".into(), "updated");
            result.metadata = serde_json::json!({"tool_name":"todo", "items":items});
            RuntimeEvent::ToolCompleted {
                operation_id: "todo-operation".into(),
                result,
            }
        };
        let mut completed =
            kurama_protocol::tool::ToolResult::success("bash-call".into(), "complete output");
        completed.metadata = serde_json::json!({"tool_name":"bash"});
        for event in [
            RuntimeEvent::AssistantDelta {
                text: "streaming **answer".into(),
            },
            RuntimeEvent::AssistantDelta {
                text: "**\n\nsecond paragraph".into(),
            },
            RuntimeEvent::ToolOutputDelta {
                call_id: "bash-call".into(),
                stream: "stdout".into(),
                chunk: "first output".into(),
            },
            todo(serde_json::json!([{"id":"one","content":"in-flight item","status":"pending"}])),
            RuntimeEvent::ToolOutputDelta {
                call_id: "bash-call".into(),
                stream: "stdout".into(),
                chunk: "\nupdated output".into(),
            },
            todo(serde_json::json!([])),
            RuntimeEvent::ToolCompleted {
                operation_id: "bash-operation".into(),
                result: completed,
            },
            RuntimeEvent::AssistantDelta {
                text: "final answer".into(),
            },
        ] {
            state.apply_runtime_event(event);
            cache.prepare(&mut state, area);
            assert_eq!(
                cache
                    .lines
                    .iter()
                    .map(|line| &line.text)
                    .collect::<Vec<_>>(),
                transcript_lines(&state.transcript, 44, TranscriptDetail::Expanded)
                    .iter()
                    .collect::<Vec<_>>(),
            );
        }
    }

    #[test]
    fn zero_height_terminal_preserves_content_until_rows_return() {
        let mut terminal = Terminal::new(TestBackend::new(80, 0)).unwrap();
        let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        state.push_assistant("deferred output");
        let mut cache = TranscriptRenderCache::default();
        prepare_fullscreen_frame(&mut state, &mut terminal, &mut cache).unwrap();
        terminal.backend_mut().resize(80, 12);
        prepare_fullscreen_frame(&mut state, &mut terminal, &mut cache).unwrap();
        terminal
            .draw(|frame| render_with_transcript(frame, &state, cache.lines()))
            .unwrap();
        let visible = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert_eq!(visible.matches("deferred output").count(), 1);
        assert_eq!(state.transcript.len(), 1);
        assert_eq!(terminal.backend_mut().get_cursor_position().unwrap().y, 8);
    }
}
