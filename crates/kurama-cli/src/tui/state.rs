use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kurama_adapters::FsSessionStore;
use kurama_protocol::{
    agent::{AgentSnapshot, AgentState},
    id::{AgentId, CallId, OperationId, SessionId},
    model::Usage,
    policy::{ApprovalRequest, ApprovalResponse, ExecutionMode},
    runtime::{AgentCommand, EngineCommand, RuntimeEvent},
    session::{BlobRef, EventEnvelope, GoalStatus, SessionEvent, SessionGoal, TodoItem},
    tool::ToolResult,
    traits::SessionStore,
};

use crate::commands::{CommandSpec, command_suggestions};

use super::input::{
    grapheme_boundary_at_or_after, next_grapheme_boundary, previous_grapheme_boundary,
};
use super::{AgentRow, ApprovalState, OnboardingState, sort_agents};

const MAX_LIVE_TOOL_OUTPUT_BYTES: usize = 128 * 1024;
const LIVE_OUTPUT_OMITTED: &str = "[earlier live output omitted]\n";
const OUTPUT_OMITTED: &str = "[earlier output omitted; Ctrl+O for full output]\n";

const COMPOSER_PLACEHOLDERS: [&str; 12] = [
    "What should we work on?",
    "What needs fixing?",
    "What would you like to build?",
    "Which part should we inspect?",
    "Describe the change you need.",
    "Point me at a file or a problem.",
    "What would you like to understand?",
    "Where should we start?",
    "Describe the behavior you expect.",
    "What should be simpler?",
    "Which task comes next?",
    "Tell me the goal.",
];

enum DisplaySource {
    Output(BlobRef),
    Streams {
        stdout: Option<BlobRef>,
        stderr: Option<BlobRef>,
    },
}

struct DeferredToolOutput {
    source: DisplaySource,
    // Only retained while expanded; the full string replaces the visible preview.
    preview: Option<String>,
}

impl DisplaySource {
    fn from_result(result: &ToolResult) -> Result<Option<Self>, String> {
        let Some(value) = result.metadata.get("display_blobs") else {
            return Ok(None);
        };
        let blobs = value
            .as_object()
            .ok_or_else(|| "invalid display blob references".to_owned())?;
        let reference = |name: &str| -> Result<Option<BlobRef>, String> {
            blobs
                .get(name)
                .map(|value| {
                    serde_json::from_value(value.clone())
                        .map_err(|error| format!("invalid {name} display blob reference: {error}"))
                })
                .transpose()
        };
        if let Some(output) = reference("output")? {
            return Ok(Some(Self::Output(output)));
        }
        let stdout = reference("stdout")?;
        let stderr = reference("stderr")?;
        if stdout.is_none() && stderr.is_none() {
            return Ok(None);
        }
        Ok(Some(Self::Streams { stdout, stderr }))
    }

    fn load(&self, store: &FsSessionStore, max_bytes: Option<usize>) -> Result<String, String> {
        match self {
            Self::Output(reference) => display_blob_text(store, reference, max_bytes),
            Self::Streams { stdout, stderr } => {
                const SEPARATOR: &str = "\n[stderr]\n";
                let budget = max_bytes.map(|limit| {
                    if stdout.is_some() && stderr.is_some() {
                        limit.saturating_sub(SEPARATOR.len()) / 2
                    } else {
                        limit
                    }
                });
                let load = |reference: &Option<BlobRef>| -> Result<String, String> {
                    reference
                        .as_ref()
                        .map(|reference| display_blob_text(store, reference, budget))
                        .transpose()
                        .map(Option::unwrap_or_default)
                };
                let mut output = load(stdout)?;
                let stderr = load(stderr)?;
                if !output.is_empty() && !stderr.is_empty() {
                    output.push_str(SEPARATOR);
                }
                output.push_str(&stderr);
                Ok(output)
            }
        }
    }
}

fn display_blob_text(
    store: &FsSessionStore,
    reference: &BlobRef,
    max_bytes: Option<usize>,
) -> Result<String, String> {
    let Some(limit) = max_bytes else {
        let bytes = store
            .get_blob(reference)
            .map_err(|error| error.to_string())?;
        return Ok(String::from_utf8(bytes)
            .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()));
    };
    let bytes = store
        .get_blob_tail(reference, limit)
        .map_err(|error| error.to_string())?;
    let omitted = reference.bytes > bytes.len() as u64;
    // A tail can begin inside a UTF-8 character. Drop just its continuation
    // bytes rather than manufacturing replacement characters at the boundary.
    let start = if omitted {
        bytes
            .iter()
            .take(3)
            .take_while(|byte| **byte & 0xc0 == 0x80)
            .count()
    } else {
        0
    };
    let text = String::from_utf8_lossy(&bytes[start..]);
    Ok(bounded_output(&text, limit, omitted))
}

fn bounded_output(text: &str, limit: usize, omitted: bool) -> String {
    if !omitted && text.len() <= limit {
        return text.to_owned();
    }
    let mut start = text
        .len()
        .saturating_sub(limit.saturating_sub(OUTPUT_OMITTED.len()));
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let mut output = String::with_capacity(OUTPUT_OMITTED.len() + text.len() - start);
    output.push_str(OUTPUT_OMITTED);
    output.push_str(&text[start..]);
    output
}

fn display_load_error(fallback: &str, error: &str) -> String {
    let error = format!("\n[display output unavailable: {error}]\n");
    let mut output = bounded_output(
        fallback,
        MAX_LIVE_TOOL_OUTPUT_BYTES.saturating_sub(error.len()),
        false,
    );
    output.push_str(&error);
    output
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HistorySearch {
    query: String,
    selection: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overlay {
    #[default]
    None,
    Onboarding,
    Approval,
    ApprovalEdit,
    Agents,
    Todos,
    AgentInspect,
    AgentMessage,
    ConfirmAgentCancel,
    Shortcuts,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ActivityState {
    #[default]
    Idle,
    Thinking {
        started_at: Instant,
    },
    Working {
        label: String,
        started_at: Instant,
    },
    RunningTool {
        name: String,
        started_at: Instant,
    },
    AwaitingApproval,
    Interrupted,
}

impl ActivityState {
    pub const fn is_animated(&self) -> bool {
        matches!(
            self,
            Self::Thinking { .. } | Self::Working { .. } | Self::RunningTool { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolLifecycle {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolTranscript {
    pub call_id: Option<CallId>,
    pub name: String,
    pub context: Option<String>,
    pub output: String,
    pub lifecycle: ToolLifecycle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingTurn {
    text: String,
    explicit_delegation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEntry {
    Startup {
        version: String,
        model: String,
        project: String,
        mode: ExecutionMode,
    },
    UserTurn {
        body: String,
    },
    AssistantMessage {
        body: String,
    },
    ToolCall(ToolTranscript),
    Todos {
        items: Vec<TodoItem>,
    },
    Error {
        body: String,
    },
    Notice {
        label: Option<String>,
        body: String,
    },
}

pub struct TuiState {
    pub profile: String,
    pub model: String,
    pub project: String,
    pub mode: ExecutionMode,
    pub max_input_tokens: u64,
    pub usage: Usage,
    pub transcript: Vec<TranscriptEntry>,
    pub composer: String,
    pub composer_inner_width: Cell<u16>,
    pub transcript_width: Cell<u16>,
    pub cursor: usize,
    pub scroll: usize,
    pub running_agents: usize,
    pub queued_agents: usize,
    pub overlay: Overlay,
    pub onboarding: OnboardingState,
    pub approval: Option<ApprovalState>,
    pub agents: Vec<AgentRow>,
    pub todos: Vec<TodoItem>,
    pub selected_todo: usize,
    pub goal: Option<SessionGoal>,
    pub git_branch: Option<String>,
    pub viewport_height: Cell<u16>,
    pub selected_agent: usize,
    pub agent_message: String,
    pub agent_message_cursor: usize,
    command_selection: usize,
    command_palette_dismissed: bool,
    file_selection: usize,
    file_index: RefCell<Option<Vec<String>>>,
    history_search: Option<HistorySearch>,
    pending_turns: VecDeque<PendingTurn>,
    composer_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    activity: ActivityState,
    turn_started_at: Option<Instant>,
    last_turn_elapsed: Option<Duration>,
    transcript_view_expanded: bool,
    active_assistant_entry: Option<usize>,
    active_tool_entries: HashMap<CallId, usize>,
    active_tool_streams: HashMap<CallId, String>,
    active_tool_contexts: HashMap<OperationId, String>,
    pending_tool_context: Option<String>,
    display_store: Option<Arc<FsSessionStore>>,
    deferred_tool_outputs: HashMap<usize, DeferredToolOutput>,
    replayed_agent_ids: HashSet<AgentId>,
    approval_generation: u64,
    transcript_revision: u64,
    transcript_dirty_from: usize,
    transcript_geometry: RefCell<Option<TranscriptGeometry>>,
    sent_commands: Vec<EngineCommand>,
    composer_placeholder_index: usize,
}

struct TranscriptGeometry {
    revision: u64,
    width: u16,
    lines: usize,
    prompt_starts: Vec<usize>,
}

impl TuiState {
    pub fn new(
        profile: impl Into<String>,
        model: impl Into<String>,
        project: impl Into<String>,
        mode: ExecutionMode,
    ) -> Self {
        Self {
            profile: profile.into(),
            model: model.into(),
            project: project.into(),
            mode,
            max_input_tokens: 0,
            usage: Usage::default(),
            transcript: Vec::new(),
            composer: String::new(),
            composer_inner_width: Cell::new(72),
            transcript_width: Cell::new(72),
            cursor: 0,
            scroll: 0,
            running_agents: 0,
            queued_agents: 0,
            overlay: Overlay::None,
            onboarding: OnboardingState::new(),
            approval: None,
            agents: Vec::new(),
            todos: Vec::new(),
            selected_todo: 0,
            goal: None,
            git_branch: None,
            viewport_height: Cell::new(12),
            selected_agent: 0,
            agent_message: String::new(),
            agent_message_cursor: 0,
            command_selection: 0,
            command_palette_dismissed: false,
            file_selection: 0,
            file_index: RefCell::new(None),
            history_search: None,
            pending_turns: VecDeque::new(),
            composer_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            activity: ActivityState::Idle,
            turn_started_at: None,
            last_turn_elapsed: None,
            transcript_view_expanded: false,
            active_assistant_entry: None,
            active_tool_entries: HashMap::new(),
            active_tool_streams: HashMap::new(),
            active_tool_contexts: HashMap::new(),
            pending_tool_context: None,
            display_store: None,
            deferred_tool_outputs: HashMap::new(),
            replayed_agent_ids: HashSet::new(),
            approval_generation: 0,
            transcript_revision: 0,
            transcript_dirty_from: 0,
            transcript_geometry: RefCell::new(None),
            sent_commands: Vec::new(),
            composer_placeholder_index: 0,
        }
    }

    pub fn composer_placeholder(&self) -> &'static str {
        COMPOSER_PLACEHOLDERS[self.composer_placeholder_index]
    }

    pub(crate) fn set_composer_session(&mut self, session_id: &SessionId) {
        // Session IDs are randomized by the runtime. Derive the hint once so
        // redraws, later turns, and resume do not change the empty-input copy.
        let mut hash = DefaultHasher::new();
        session_id.hash(&mut hash);
        self.composer_placeholder_index =
            (hash.finish() % COMPOSER_PLACEHOLDERS.len() as u64) as usize;
    }

    pub fn command_suggestions(&self) -> Vec<CommandSpec> {
        if self.history_search.is_some()
            || self.command_palette_dismissed
            || self.overlay != Overlay::None
            || self.transcript_view_expanded
            || self.file_mention().is_some()
        {
            return Vec::new();
        }
        command_suggestions(&self.composer)
    }

    pub const fn command_selection(&self) -> usize {
        self.command_selection
    }

    pub fn composer_edited(&mut self) {
        self.cursor = grapheme_boundary_at_or_after(&self.composer, self.cursor);
        self.command_selection = 0;
        self.file_selection = 0;
        self.command_palette_dismissed = false;
        self.history_index = None;
    }

    pub fn clear_composer(&mut self) {
        self.composer.clear();
        self.cursor = 0;
        self.composer_edited();
    }

    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_end(&mut self) {
        self.cursor = self.composer.len();
    }

    pub fn kill_to_end(&mut self) {
        self.normalize_composer_cursor();
        self.composer.truncate(self.cursor);
        self.composer_edited();
    }

    pub fn kill_to_start(&mut self) {
        self.normalize_composer_cursor();
        self.composer.replace_range(..self.cursor, "");
        self.cursor = 0;
        self.composer_edited();
    }

    pub fn kill_previous_word(&mut self) {
        self.normalize_composer_cursor();
        if self.cursor == 0 {
            return;
        }
        let before = &self.composer[..self.cursor];
        let trimmed = before.trim_end();
        let word_start = trimmed
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace())
            .map(|(index, character)| index + character.len_utf8())
            .unwrap_or(0);
        self.composer.replace_range(word_start..self.cursor, "");
        self.cursor = word_start;
        self.composer_edited();
    }

    pub(crate) fn normalize_composer_cursor(&mut self) {
        self.cursor = grapheme_cursor(&self.composer, self.cursor);
    }

    pub(crate) const fn transcript_revision(&self) -> u64 {
        self.transcript_revision
    }

    pub(crate) const fn transcript_dirty_from(&self) -> usize {
        self.transcript_dirty_from
    }

    pub(crate) fn mark_transcript_rendered(&mut self) {
        self.transcript_dirty_from = self.transcript.len();
    }

    fn transcript_changed(&mut self, from: usize) {
        self.transcript_revision = self.transcript_revision.wrapping_add(1);
        self.transcript_dirty_from = self.transcript_dirty_from.min(from);
    }

    pub(crate) fn set_transcript_geometry(&self, width: u16, lines: usize, entry_starts: &[usize]) {
        *self.transcript_geometry.borrow_mut() = Some(TranscriptGeometry {
            revision: self.transcript_revision,
            width,
            lines,
            prompt_starts: self
                .transcript
                .iter()
                .zip(entry_starts)
                .filter_map(|(entry, &row)| {
                    matches!(entry, TranscriptEntry::UserTurn { .. }).then_some(row)
                })
                .collect(),
        });
    }

    fn ensure_transcript_geometry(&self, width: u16) {
        if self
            .transcript_geometry
            .borrow()
            .as_ref()
            .is_some_and(|geometry| {
                geometry.revision == self.transcript_revision && geometry.width == width
            })
        {
            return;
        }
        let (lines, starts) = super::transcript::transcript_lines_with_entry_starts(
            &self.transcript,
            width as usize,
            super::TranscriptDetail::Expanded,
        );
        self.set_transcript_geometry(width, lines.len(), &starts);
    }

    pub(crate) fn max_transcript_scroll(&self) -> usize {
        self.ensure_transcript_geometry(self.transcript_width.get());
        self.transcript_geometry
            .borrow()
            .as_ref()
            .map_or(0, |geometry| {
                geometry
                    .lines
                    .saturating_sub(self.viewport_height.get() as usize)
            })
    }

    pub fn remember_prompt(&mut self, text: &str) {
        if text.is_empty() || text.starts_with('/') {
            return;
        }
        if self.composer_history.last().map(String::as_str) != Some(text) {
            self.composer_history.push(text.to_owned());
        }
        self.history_index = None;
        self.history_draft.clear();
    }

    pub fn history_previous(&mut self) -> bool {
        if self.composer_history.is_empty() {
            return false;
        }
        match self.history_index {
            None => {
                self.history_draft.clone_from(&self.composer);
                self.history_index = Some(self.composer_history.len() - 1);
            }
            Some(0) => return false,
            Some(index) => self.history_index = Some(index - 1),
        }
        if let Some(index) = self.history_index {
            self.composer.clone_from(&self.composer_history[index]);
            self.cursor = self.composer.len();
        }
        true
    }

    pub fn history_next(&mut self) -> bool {
        let Some(index) = self.history_index else {
            return false;
        };
        if index + 1 < self.composer_history.len() {
            self.history_index = Some(index + 1);
            self.composer.clone_from(&self.composer_history[index + 1]);
        } else {
            self.history_index = None;
            self.composer.clone_from(&self.history_draft);
        }
        self.cursor = self.composer.len();
        true
    }

    pub fn dismiss_command_palette(&mut self) -> bool {
        if self.command_palette_dismissed || self.history_search.is_some() {
            return false;
        }
        let showing_files = super::mention_at_cursor(&self.composer, self.cursor).is_some()
            && !self.file_suggestions().is_empty();
        let showing_commands = !command_suggestions(&self.composer).is_empty()
            && self.overlay == Overlay::None
            && !self.transcript_view_expanded;
        if !showing_files && !showing_commands {
            return false;
        }
        self.command_palette_dismissed = true;
        true
    }

    pub fn select_previous_command(&mut self) -> bool {
        let len = self.command_suggestions().len();
        if len == 0 {
            return false;
        }
        self.command_selection = if self.command_selection == 0 {
            len - 1
        } else {
            self.command_selection - 1
        };
        true
    }

    pub fn select_next_command(&mut self) -> bool {
        let len = self.command_suggestions().len();
        if len == 0 {
            return false;
        }
        self.command_selection = (self.command_selection + 1) % len;
        true
    }

    pub fn selected_command(&self) -> Option<CommandSpec> {
        let suggestions = self.command_suggestions();
        suggestions
            .get(
                self.command_selection
                    .min(suggestions.len().saturating_sub(1)),
            )
            .copied()
    }

    pub fn complete_selected_command(&mut self) -> Option<CommandSpec> {
        let selected = self.selected_command()?;
        let first_line_end = self.composer.find('\n').unwrap_or(self.composer.len());
        let token_end = self.composer[..first_line_end]
            .find(char::is_whitespace)
            .unwrap_or(first_line_end);
        let tail = self.composer[token_end..].to_owned();
        self.composer = format!("/{}", selected.name);
        if tail.is_empty() && selected.accepts_arguments {
            self.composer.push(' ');
        } else {
            self.composer.push_str(&tail);
        }
        self.cursor = self.composer.len();
        self.command_selection = 0;
        self.command_palette_dismissed = true;
        Some(selected)
    }

    pub fn file_mention(&self) -> Option<(usize, String)> {
        if self.history_search.is_some()
            || self.overlay != Overlay::None
            || self.command_palette_dismissed
        {
            return None;
        }
        super::mention_at_cursor(&self.composer, self.cursor)
    }

    pub fn file_suggestions(&self) -> Vec<String> {
        let Some((_, query)) = self.file_mention() else {
            return Vec::new();
        };
        if self.file_index.borrow().is_none() {
            *self.file_index.borrow_mut() = Some(super::collect_files(Path::new(&self.project)));
        }
        let files = self.file_index.borrow();
        super::filter_files(files.as_deref().unwrap_or(&[]), &query)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    pub fn selected_file(&self) -> Option<String> {
        let suggestions = self.file_suggestions();
        suggestions
            .get(self.file_selection.min(suggestions.len().saturating_sub(1)))
            .cloned()
    }

    pub fn select_previous_file(&mut self) -> bool {
        let len = self.file_suggestions().len();
        if len == 0 {
            return false;
        }
        self.file_selection = if self.file_selection == 0 {
            len - 1
        } else {
            self.file_selection - 1
        };
        true
    }

    pub fn select_next_file(&mut self) -> bool {
        let len = self.file_suggestions().len();
        if len == 0 {
            return false;
        }
        self.file_selection = (self.file_selection + 1) % len;
        true
    }

    pub fn complete_selected_file(&mut self) -> bool {
        let Some((start, _)) = self.file_mention() else {
            return false;
        };
        let Some(path) = self.selected_file() else {
            return false;
        };
        let suffix = self.composer[self.cursor..].to_owned();
        self.composer.truncate(start);
        self.composer.push('@');
        self.composer.push_str(&path);
        self.composer.push(' ');
        let cursor = self.composer.len();
        self.composer.push_str(&suffix);
        self.cursor = cursor;
        self.composer_edited();
        true
    }

    pub fn history_search_active(&self) -> bool {
        self.history_search.is_some()
    }

    pub fn history_matches(&self) -> Vec<&str> {
        let Some(search) = &self.history_search else {
            return Vec::new();
        };
        let query = search.query.to_ascii_lowercase();
        self.composer_history
            .iter()
            .rev()
            .filter(|prompt| query.is_empty() || prompt.to_ascii_lowercase().contains(&query))
            .map(String::as_str)
            .take(8)
            .collect()
    }

    pub fn start_history_search(&mut self) {
        if self.history_search.is_some() {
            let matches = self.history_matches().len();
            if let Some(search) = &mut self.history_search
                && matches > 0
            {
                search.selection = (search.selection + 1) % matches;
            }
            return;
        }
        self.history_search = Some(HistorySearch {
            query: String::new(),
            selection: 0,
        });
    }

    pub fn push_history_search_char(&mut self, character: char) {
        if let Some(search) = &mut self.history_search {
            search.query.push(character);
            search.selection = 0;
        }
    }

    pub fn pop_history_search_char(&mut self) {
        if let Some(search) = &mut self.history_search {
            let boundary = previous_grapheme_boundary(&search.query, search.query.len());
            search.query.truncate(boundary);
            search.selection = 0;
        }
    }

    pub fn select_previous_history_match(&mut self) -> bool {
        let len = self.history_matches().len();
        let Some(search) = &mut self.history_search else {
            return false;
        };
        if len == 0 {
            return false;
        }
        search.selection = if search.selection == 0 {
            len - 1
        } else {
            search.selection - 1
        };
        true
    }

    pub fn select_next_history_match(&mut self) -> bool {
        let len = self.history_matches().len();
        let Some(search) = &mut self.history_search else {
            return false;
        };
        if len == 0 {
            return false;
        }
        search.selection = (search.selection + 1) % len;
        true
    }

    pub fn accept_history_search(&mut self) -> bool {
        let Some(search) = &self.history_search else {
            return false;
        };
        let selection = search.selection;
        let Some(prompt) = self
            .history_matches()
            .get(selection)
            .map(|prompt| (*prompt).to_owned())
        else {
            self.history_search = None;
            return false;
        };
        self.composer = prompt;
        self.cursor = self.composer.len();
        self.history_search = None;
        self.history_index = None;
        true
    }

    pub fn cancel_history_search(&mut self) -> bool {
        self.history_search.take().is_some()
    }

    pub fn history_search_query(&self) -> Option<&str> {
        self.history_search
            .as_ref()
            .map(|search| search.query.as_str())
    }

    pub fn history_search_selection(&self) -> usize {
        self.history_search
            .as_ref()
            .map(|search| search.selection)
            .unwrap_or(0)
    }

    pub fn file_selection(&self) -> usize {
        self.file_selection
    }

    pub fn insert_mention(&mut self, path: &str) {
        self.normalize_composer_cursor();
        self.composer.insert_str(self.cursor, &format!("@{path} "));
        self.cursor += path.len() + 2;
        self.composer_edited();
    }

    pub fn onboarding(project: impl Into<String>) -> Self {
        let mut state = Self::new(
            "not connected",
            "select a model",
            project,
            ExecutionMode::Supervised,
        );
        state.overlay = Overlay::Onboarding;
        state
    }

    pub fn prepend_startup(&mut self, version: impl Into<String>, project: impl Into<String>) {
        self.transcript.insert(
            0,
            TranscriptEntry::Startup {
                version: version.into(),
                model: self.model.clone(),
                project: project.into(),
                mode: self.mode,
            },
        );
        self.deferred_tool_outputs = std::mem::take(&mut self.deferred_tool_outputs)
            .into_iter()
            .map(|(index, output)| (index + 1, output))
            .collect();
        for index in self.active_tool_entries.values_mut() {
            *index += 1;
        }
        self.active_assistant_entry = self.active_assistant_entry.map(|index| index + 1);
        self.transcript_changed(0);
    }

    pub fn credential(project: impl Into<String>, profile: impl Into<String>) -> Self {
        let profile = profile.into();
        let mut state = Self::new(
            profile.clone(),
            "credential required",
            project,
            ExecutionMode::Supervised,
        );
        state.onboarding = OnboardingState::credential(profile);
        state.overlay = Overlay::Onboarding;
        state
    }

    pub const fn activity(&self) -> &ActivityState {
        &self.activity
    }

    pub const fn last_turn_elapsed(&self) -> Option<Duration> {
        self.last_turn_elapsed
    }

    pub fn set_thinking(&mut self) {
        let now = Instant::now();
        if self.turn_started_at.is_none() {
            self.turn_started_at = Some(now);
            self.last_turn_elapsed = None;
        }
        self.activity = ActivityState::Thinking { started_at: now };
    }

    pub fn submit_turn(&mut self, text: impl Into<String>, explicit_delegation: bool) {
        let text = text.into();
        if !matches!(self.activity, ActivityState::Idle) {
            self.pending_turns.push_back(PendingTurn {
                text,
                explicit_delegation,
            });
            return;
        }
        self.start_turn(text, explicit_delegation);
    }

    pub fn pending_turn_count(&self) -> usize {
        self.pending_turns.len()
    }

    pub fn pending_prompts(&self) -> impl Iterator<Item = &str> {
        self.pending_turns.iter().map(|turn| turn.text.as_str())
    }

    pub fn context_label(&self) -> Option<String> {
        if self.max_input_tokens == 0 {
            return None;
        }
        let used = self.usage.input_tokens.saturating_mul(100) / self.max_input_tokens;
        Some(format!("{}% context left", 100_u64.saturating_sub(used)))
    }

    pub fn open_shortcuts(&mut self) {
        self.overlay = Overlay::Shortcuts;
    }

    pub fn toggle_transcript_view(&mut self) {
        self.transcript_view_expanded = !self.transcript_view_expanded;
        self.scroll = 0;
        if let Some(store) = &self.display_store {
            let mut dirty_from = self.transcript.len();
            for (&index, deferred) in &mut self.deferred_tool_outputs {
                if let Some(TranscriptEntry::ToolCall(tool)) = self.transcript.get_mut(index) {
                    if self.transcript_view_expanded {
                        let full = deferred
                            .source
                            .load(store, None)
                            .unwrap_or_else(|error| display_load_error(&tool.output, &error));
                        deferred.preview = Some(std::mem::replace(&mut tool.output, full));
                    } else if let Some(preview) = deferred.preview.take() {
                        tool.output = preview;
                    }
                    dirty_from = dirty_from.min(index);
                }
            }
            if dirty_from < self.transcript.len() {
                self.transcript_changed(dirty_from);
            }
        }
    }

    pub const fn transcript_view_expanded(&self) -> bool {
        self.transcript_view_expanded
    }

    pub fn push_user(&mut self, body: impl Into<String>) {
        self.active_assistant_entry = None;
        self.push_transcript_entry(TranscriptEntry::UserTurn { body: body.into() });
    }

    pub fn push_assistant(&mut self, body: impl Into<String>) {
        self.active_assistant_entry = None;
        self.push_transcript_entry(TranscriptEntry::AssistantMessage { body: body.into() });
    }

    pub fn push_tool(&mut self, label: impl Into<String>, body: impl Into<String>) {
        self.active_assistant_entry = None;
        let label = label.into();
        self.push_transcript_entry(TranscriptEntry::ToolCall(ToolTranscript {
            call_id: None,
            name: transcript_tool_name(&label).to_owned(),
            context: None,
            output: body.into(),
            lifecycle: ToolLifecycle::Completed,
        }));
    }

    pub fn push_system(&mut self, label: impl Into<String>, body: impl Into<String>) {
        let label = label.into();
        if label == "ERROR" {
            self.push_error(body);
        } else {
            self.push_notice(Some(label), body);
        }
    }

    pub fn push_notice(&mut self, label: Option<String>, body: impl Into<String>) {
        self.active_assistant_entry = None;
        self.push_transcript_entry(TranscriptEntry::Notice {
            label,
            body: body.into(),
        });
    }

    pub fn push_error(&mut self, body: impl Into<String>) {
        self.active_assistant_entry = None;
        self.push_transcript_entry(TranscriptEntry::Error { body: body.into() });
    }

    pub fn interrupt_active(&mut self) -> bool {
        if matches!(self.overlay, Overlay::Approval | Overlay::ApprovalEdit) {
            self.approval = None;
            self.overlay = Overlay::None;
            self.sent_commands.push(EngineCommand::CancelTurn);
            self.activity = ActivityState::Interrupted;
            self.pending_turns.clear();
            return true;
        }
        if !self.activity.is_animated() {
            return false;
        }
        self.sent_commands.push(EngineCommand::CancelTurn);
        self.activity = ActivityState::Interrupted;
        self.pending_turns.clear();
        true
    }

    pub fn pop_queued_follow_up(&mut self) -> bool {
        self.pending_turns.pop_back().is_some()
    }

    fn push_transcript_entry(&mut self, entry: TranscriptEntry) {
        self.transcript.push(entry);
        self.transcript_changed(self.transcript.len() - 1);
    }

    pub(crate) fn set_display_store(&mut self, store: Arc<FsSessionStore>) {
        self.display_store = Some(store);
    }

    pub fn hydrate_replay(&mut self, replay: &[EventEnvelope]) {
        self.activity = ActivityState::Idle;
        self.turn_started_at = None;
        self.last_turn_elapsed = None;
        self.active_assistant_entry = None;
        self.active_tool_entries.clear();
        self.active_tool_streams.clear();
        self.active_tool_contexts.clear();
        self.pending_tool_context = None;
        self.deferred_tool_outputs.clear();
        self.replayed_agent_ids.clear();
        self.transcript.clear();
        self.transcript_changed(0);
        self.agents.clear();
        self.todos.clear();
        self.usage = Usage::default();
        let mut replayed_tool_contexts = HashMap::new();
        for envelope in replay {
            match &envelope.event {
                SessionEvent::UserMessage { text } => self.push_user(text.clone()),
                SessionEvent::AssistantMessage { text } => self.push_assistant(text.clone()),
                SessionEvent::ToolProposed {
                    operation_id,
                    operation,
                    ..
                } => {
                    replayed_tool_contexts
                        .insert(operation_id.clone(), operation_context(operation));
                }
                SessionEvent::ToolCompleted {
                    operation_id,
                    result,
                } => {
                    if todo_items(result).is_none() {
                        let (output, deferred) = self.completed_display_output(result);
                        self.push_transcript_entry(TranscriptEntry::ToolCall(ToolTranscript {
                            call_id: Some(result.call_id.clone()),
                            name: tool_name(result).to_owned(),
                            context: replayed_tool_contexts.remove(operation_id),
                            output,
                            lifecycle: tool_lifecycle(result),
                        }));
                        self.remember_display_source(self.transcript.len() - 1, deferred);
                    }
                }
                SessionEvent::ToolUnknown { reason, .. } => {
                    self.push_notice(Some("TOOL".into()), reason.clone());
                }
                SessionEvent::ModeSelected { mode } => {
                    self.push_notice(Some("MODE".into()), mode_label(*mode).to_owned());
                }
                SessionEvent::ModelUsage { usage } => {
                    self.usage = *usage;
                }
                SessionEvent::TurnFailed { error } => self.push_error(error.clone()),
                SessionEvent::RecoveryRepair { removed_bytes } => self.push_notice(
                    Some("RECOVERY".into()),
                    format!("removed {removed_bytes} incomplete transcript bytes"),
                ),
                SessionEvent::AgentQueued { snapshot }
                | SessionEvent::AgentStarted { snapshot }
                | SessionEvent::AgentProgress { snapshot } => {
                    self.replayed_agent_ids.insert(snapshot.id.clone());
                    self.upsert_agent(snapshot.clone());
                }
                SessionEvent::AgentCompleted { snapshot, summary } => {
                    self.replayed_agent_ids.insert(snapshot.id.clone());
                    self.upsert_agent_with_transcript(snapshot.clone(), summary.clone());
                }
                SessionEvent::AgentFailed { snapshot, error } => {
                    self.replayed_agent_ids.insert(snapshot.id.clone());
                    self.upsert_agent_with_transcript(snapshot.clone(), error.clone());
                }
                SessionEvent::AgentCancelled { snapshot } => {
                    self.replayed_agent_ids.insert(snapshot.id.clone());
                    self.upsert_agent_with_transcript(
                        snapshot.clone(),
                        snapshot
                            .last_error
                            .clone()
                            .unwrap_or_else(|| "cancelled".into()),
                    );
                }
                SessionEvent::TodoUpdated { items } => self.replace_todos(items.clone()),
                SessionEvent::GoalUpdated { goal } => self.apply_goal(Some(goal.clone()), false),
                SessionEvent::GoalCleared => self.apply_goal(None, false),
                _ => {}
            }
        }
    }

    pub fn set_agent_counts(&mut self, running: usize, queued: usize) {
        self.running_agents = running;
        self.queued_agents = queued;
    }

    pub fn set_agents(&mut self, mut agents: Vec<AgentRow>) {
        self.replayed_agent_ids.clear();
        let selected_id = self.selected_agent().map(|agent| agent.id.clone());
        sort_agents(&mut agents);
        self.agents = agents;
        self.restore_agent_selection(selected_id.as_ref());
        self.refresh_agent_counts();
    }

    fn upsert_agent(&mut self, snapshot: AgentSnapshot) {
        let selected_id = self.selected_agent().map(|agent| agent.id.clone());
        if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == snapshot.id) {
            agent.update_from_snapshot(snapshot);
        } else {
            self.agents.push(AgentRow::from_snapshot(snapshot));
        }
        sort_agents(&mut self.agents);
        self.restore_agent_selection(selected_id.as_ref());
        self.refresh_agent_counts();
    }

    fn upsert_agent_with_transcript(&mut self, snapshot: AgentSnapshot, line: String) {
        let agent_id = snapshot.id.clone();
        self.upsert_agent(snapshot);
        if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == agent_id) {
            agent.transcript = vec![line];
        }
    }

    fn restore_agent_selection(&mut self, selected_id: Option<&kurama_protocol::id::AgentId>) {
        if self.agents.is_empty() {
            self.selected_agent = 0;
        } else if let Some(index) =
            selected_id.and_then(|id| self.agents.iter().position(|agent| &agent.id == id))
        {
            self.selected_agent = index;
        } else {
            self.selected_agent = self.selected_agent.min(self.agents.len() - 1);
        }
    }

    fn refresh_agent_counts(&mut self) {
        self.running_agents = self
            .agents
            .iter()
            .filter(|agent| agent.state == AgentState::Running)
            .count();
        self.queued_agents = self
            .agents
            .iter()
            .filter(|agent| agent.state == AgentState::Queued)
            .count();
    }

    pub fn open_agents(&mut self) {
        self.overlay = Overlay::Agents;
    }

    pub fn open_todos(&mut self) {
        self.selected_todo = 0;
        self.overlay = Overlay::Todos;
    }

    pub fn toggle_todos(&mut self) {
        if self.overlay == Overlay::Todos {
            self.close_overlay();
        } else if self.overlay == Overlay::None {
            self.open_todos();
        }
    }

    pub fn last_assistant_text(&self) -> Option<&str> {
        self.transcript.iter().rev().find_map(|entry| match entry {
            TranscriptEntry::AssistantMessage { body } => Some(body.as_str()),
            _ => None,
        })
    }

    pub fn refresh_git_branch(&mut self) {
        self.git_branch = std::process::Command::new("git")
            .args(["-C", &self.project, "rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|branch| branch.trim().to_owned())
            .filter(|branch| !branch.is_empty() && branch != "HEAD");
    }

    pub fn jump_user_turn(&mut self, direction: i32, width: usize) {
        let width = width.min(u16::MAX as usize) as u16;
        self.ensure_transcript_geometry(width);
        let geometry = self.transcript_geometry.borrow();
        let Some(geometry) = geometry.as_ref() else {
            return;
        };
        let max_scroll = geometry
            .lines
            .saturating_sub(self.viewport_height.get() as usize);
        let current_start = max_scroll.saturating_sub(self.scroll);
        let target = if direction < 0 {
            geometry
                .prompt_starts
                .iter()
                .rev()
                .find(|&&start| start < current_start)
                .or_else(|| geometry.prompt_starts.first())
        } else {
            geometry
                .prompt_starts
                .iter()
                .find(|&&start| start > current_start)
                .or_else(|| geometry.prompt_starts.last())
        };
        if let Some(&target) = target {
            self.scroll = max_scroll.saturating_sub(target);
        }
    }

    pub const fn overlay(&self) -> Overlay {
        self.overlay
    }

    pub fn selected_agent(&self) -> Option<&AgentRow> {
        self.agents.get(self.selected_agent)
    }

    pub fn select_next_agent(&mut self) {
        if !self.agents.is_empty() {
            self.selected_agent = (self.selected_agent + 1).min(self.agents.len() - 1);
        }
    }

    pub fn select_previous_agent(&mut self) {
        self.selected_agent = self.selected_agent.saturating_sub(1);
    }

    pub fn inspect_selected_agent(&mut self) {
        if let Some((agent_id, needs_refresh)) = self.selected_agent().map(|agent| {
            (
                agent.id.clone(),
                !self.replayed_agent_ids.contains(&agent.id),
            )
        }) {
            if needs_refresh {
                self.sent_commands
                    .push(EngineCommand::Agent(AgentCommand::Inspect { agent_id }));
            }
            self.overlay = Overlay::AgentInspect;
        }
    }

    pub fn begin_agent_message(&mut self) {
        if self.selected_agent().is_some() {
            self.agent_message.clear();
            self.agent_message_cursor = 0;
            self.overlay = Overlay::AgentMessage;
        }
    }

    pub fn set_agent_message(&mut self, message: impl Into<String>) {
        self.agent_message = message.into();
        self.agent_message_cursor = self.agent_message.len();
    }

    pub(crate) fn normalize_agent_message_cursor(&mut self) {
        self.agent_message_cursor = grapheme_cursor(&self.agent_message, self.agent_message_cursor);
    }

    pub fn insert_agent_message(&mut self, text: &str) {
        self.normalize_agent_message_cursor();
        self.agent_message
            .insert_str(self.agent_message_cursor, text);
        self.agent_message_cursor = self
            .agent_message_cursor
            .saturating_add(text.len())
            .min(self.agent_message.len());
        self.agent_message_cursor =
            grapheme_boundary_at_or_after(&self.agent_message, self.agent_message_cursor);
    }

    pub fn submit_agent_message(&mut self) {
        if let Some(agent_id) = self.selected_agent().map(|agent| agent.id.clone()) {
            let text = std::mem::take(&mut self.agent_message);
            self.agent_message_cursor = 0;
            if !text.trim().is_empty() {
                self.sent_commands
                    .push(EngineCommand::Agent(AgentCommand::Message {
                        agent_id,
                        text,
                    }));
            }
            self.overlay = Overlay::AgentInspect;
        }
    }

    pub fn request_agent_cancel(&mut self) {
        if self.selected_agent().is_some() {
            self.overlay = Overlay::ConfirmAgentCancel;
        }
    }

    pub fn confirm_agent_cancel(&mut self) {
        if let Some(agent_id) = self.selected_agent().map(|agent| agent.id.clone()) {
            self.sent_commands
                .push(EngineCommand::Agent(AgentCommand::Cancel { agent_id }));
        }
        self.overlay = Overlay::Agents;
    }

    pub fn close_overlay(&mut self) {
        self.overlay = match self.overlay {
            Overlay::Approval => Overlay::Approval,
            Overlay::ApprovalEdit => {
                if let Some(approval) = &mut self.approval {
                    approval.editing = false;
                }
                Overlay::Approval
            }
            Overlay::AgentMessage | Overlay::ConfirmAgentCancel => Overlay::AgentInspect,
            Overlay::AgentInspect => Overlay::Agents,
            _ => Overlay::None,
        };
    }

    pub(crate) const fn approval_generation(&self) -> u64 {
        self.approval_generation
    }

    pub fn begin_approval(&mut self, request: ApprovalRequest) {
        self.approval_generation = self.approval_generation.wrapping_add(1);
        self.approval = Some(ApprovalState::new(request));
        self.overlay = Overlay::Approval;
        self.activity = ActivityState::AwaitingApproval;
    }

    pub fn resolve_approval(&mut self, response: ApprovalResponse) {
        let Some(approval) = self.approval.take() else {
            self.push_error("no approval is pending");
            return;
        };
        self.sent_commands.push(EngineCommand::ResolveApproval {
            operation_id: approval.request.operation_id,
            response,
        });
        self.overlay = Overlay::None;
        self.set_thinking();
    }

    pub fn begin_approval_edit(&mut self) {
        if let Some(approval) = &mut self.approval {
            approval.editing = true;
            approval.editor_cursor = approval.editor.len();
            approval.validation_error = None;
            self.overlay = Overlay::ApprovalEdit;
        }
    }

    pub fn replace_approval_arguments(&mut self, arguments: serde_json::Value) {
        if let Some(approval) = &mut self.approval {
            approval.set_editor(
                serde_json::to_string_pretty(&arguments).unwrap_or_else(|_| "{}".into()),
            );
            approval.arguments = arguments;
        }
    }

    pub fn set_approval_editor(&mut self, editor: impl Into<String>) {
        if let Some(approval) = &mut self.approval {
            approval.set_editor(editor);
        }
    }

    pub fn insert_approval_text(&mut self, value: &str) {
        if let Some(approval) = &mut self.approval {
            approval.insert_str(value);
        }
    }

    pub fn submit_approval_edit(&mut self) -> Result<(), String> {
        let editor = self
            .approval
            .as_ref()
            .ok_or_else(|| "no approval is pending".to_owned())?
            .editor
            .clone();
        let arguments: serde_json::Value = match serde_json::from_str(&editor) {
            Ok(arguments) => arguments,
            Err(error) => {
                let message = format!("invalid approval arguments: {error}");
                self.overlay = Overlay::ApprovalEdit;
                self.activity = ActivityState::AwaitingApproval;
                if let Some(approval) = &mut self.approval {
                    approval.editing = true;
                    approval.validation_error = Some(error.to_string());
                }
                return Err(message);
            }
        };
        if let Some(approval) = &mut self.approval {
            approval.arguments = arguments.clone();
            approval.validation_error = None;
        }
        self.resolve_approval(ApprovalResponse::Edit { arguments });
        Ok(())
    }

    pub fn sent_commands(&self) -> &[EngineCommand] {
        &self.sent_commands
    }

    pub fn queue_command(&mut self, command: EngineCommand) {
        self.sent_commands.push(command);
    }

    pub fn take_commands(&mut self) -> Vec<EngineCommand> {
        std::mem::take(&mut self.sent_commands)
    }

    pub fn apply_runtime_event(&mut self, event: RuntimeEvent) {
        let terminal_turn_event = match &event {
            RuntimeEvent::TurnCompleted => true,
            RuntimeEvent::Error { message } => !is_non_terminal_runtime_error(message),
            _ => false,
        };
        let completes_active_streams =
            matches!(&event, RuntimeEvent::TurnCompleted | RuntimeEvent::Shutdown);
        let preserves_active_streams = matches!(
            &event,
            RuntimeEvent::Error { message } if is_non_terminal_runtime_error(message)
        );
        if !matches!(
            &event,
            RuntimeEvent::AssistantDelta { .. } | RuntimeEvent::Usage { .. }
        ) && !preserves_active_streams
        {
            self.active_assistant_entry = None;
        }
        match event {
            RuntimeEvent::Status { message } => {
                self.push_notice(None, message);
                self.activity = ActivityState::Idle;
            }
            RuntimeEvent::AssistantDelta { text } => {
                if matches!(
                    self.activity,
                    ActivityState::Idle | ActivityState::Interrupted
                ) {
                    self.set_thinking();
                }
                self.append_assistant_delta(text);
            }
            RuntimeEvent::ApprovalRequired { request } => {
                self.begin_approval(request);
            }
            RuntimeEvent::ToolStarted {
                operation_id,
                name,
                context,
            } => {
                self.activity = ActivityState::RunningTool {
                    name,
                    started_at: Instant::now(),
                };
                self.active_tool_contexts
                    .insert(operation_id, context.clone());
                self.pending_tool_context = Some(context);
            }
            RuntimeEvent::ToolOutputDelta {
                call_id,
                chunk,
                stream,
            } => {
                self.append_tool_delta(call_id, stream, chunk);
            }
            RuntimeEvent::ToolCompleted {
                operation_id,
                result,
            } => {
                let todo_items = todo_items(&result);
                self.complete_tool(operation_id, result);
                if let Some(items) = todo_items {
                    self.replace_todos(items);
                }
                self.set_thinking();
            }
            RuntimeEvent::AgentUpdated { snapshot } => self.upsert_agent(snapshot),
            RuntimeEvent::AgentInspection {
                snapshot,
                transcript,
            } => {
                let agent_id = snapshot.id.clone();
                self.upsert_agent(snapshot);
                if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == agent_id) {
                    agent.transcript = transcript;
                }
            }
            RuntimeEvent::Usage { usage } => self.usage = usage,
            RuntimeEvent::GoalUpdated { goal } => {
                let pursuing = goal.status.is_active();
                self.apply_goal(Some(goal), true);
                if pursuing
                    && matches!(
                        self.activity,
                        ActivityState::Idle | ActivityState::Interrupted
                    )
                {
                    self.set_thinking();
                }
            }
            RuntimeEvent::GoalCleared => self.apply_goal(None, true),
            RuntimeEvent::TurnCompleted => self.finish_turn(),
            RuntimeEvent::Error { message } => {
                if terminal_turn_event {
                    self.push_error(message);
                    self.finish_turn();
                } else {
                    self.push_transcript_entry(TranscriptEntry::Error { body: message });
                }
            }
            RuntimeEvent::Shutdown => {
                self.turn_started_at = None;
                self.activity = ActivityState::Idle;
            }
        }
        if completes_active_streams || terminal_turn_event {
            self.active_tool_entries.clear();
            self.active_tool_streams.clear();
            self.active_tool_contexts.clear();
            self.pending_tool_context = None;
        }
        if terminal_turn_event {
            self.start_next_pending_turn();
        }
    }

    fn start_turn(&mut self, text: String, explicit_delegation: bool) {
        self.push_user(text.clone());
        self.sent_commands.push(EngineCommand::SubmitTurn {
            text,
            explicit_delegation,
        });
        let now = Instant::now();
        self.turn_started_at = Some(now);
        self.last_turn_elapsed = None;
        self.activity = ActivityState::Thinking { started_at: now };
    }

    fn start_next_pending_turn(&mut self) {
        let Some(turn) = self.pending_turns.pop_front() else {
            return;
        };
        self.start_turn(turn.text, turn.explicit_delegation);
    }

    fn finish_turn(&mut self) {
        if let Some(started_at) = self.turn_started_at.take() {
            self.last_turn_elapsed = Some(Instant::now().saturating_duration_since(started_at));
        }
        self.activity = ActivityState::Idle;
    }

    fn append_assistant_delta(&mut self, text: String) {
        if let Some(index) = self.active_assistant_entry
            && let Some(TranscriptEntry::AssistantMessage { body }) = self.transcript.get_mut(index)
        {
            body.push_str(&text);
            self.transcript_changed(index);
            return;
        }

        self.push_assistant(text);
        self.active_assistant_entry = self.transcript.len().checked_sub(1);
    }

    fn append_tool_delta(&mut self, call_id: CallId, stream: String, chunk: String) {
        if matches!(
            &self.activity,
            ActivityState::RunningTool { name, .. } if name == "todo"
        ) {
            return;
        }
        if let Some(index) = self.active_tool_entries.get(&call_id).copied() {
            let stream_changed = self
                .active_tool_streams
                .get(&call_id)
                .is_some_and(|active| active != &stream);
            if let Some(TranscriptEntry::ToolCall(tool)) = self.transcript.get_mut(index) {
                if stream_changed {
                    append_stream_boundary(&mut tool.output, &stream);
                }
                append_live_tool_output(&mut tool.output, &chunk);
                self.transcript_changed(index);
            }
            self.active_tool_streams.insert(call_id, stream);
            return;
        }

        let name = match &self.activity {
            ActivityState::RunningTool { name, .. } => name.clone(),
            _ => stream.clone(),
        };
        if !matches!(self.activity, ActivityState::RunningTool { .. }) {
            self.activity = ActivityState::RunningTool {
                name: name.clone(),
                started_at: Instant::now(),
            };
        }
        self.push_transcript_entry(TranscriptEntry::ToolCall(ToolTranscript {
            call_id: Some(call_id.clone()),
            name,
            context: self.pending_tool_context.clone(),
            output: bounded_live_tool_output(chunk),
            lifecycle: ToolLifecycle::Running,
        }));
        if let Some(index) = self.transcript.len().checked_sub(1) {
            self.active_tool_entries.insert(call_id.clone(), index);
            self.active_tool_streams.insert(call_id, stream);
        }
    }

    fn complete_tool(&mut self, operation_id: OperationId, result: ToolResult) {
        let persisted_display_output = result
            .metadata
            .get("display_output")
            .and_then(serde_json::Value::as_str);
        let (display_output, deferred) = self.completed_display_output(&result);
        let has_display_blobs =
            self.display_store.is_some() && result.metadata.get("display_blobs").is_some();
        let lifecycle = tool_lifecycle(&result);
        let name = tool_name(&result).to_owned();
        let pending_context = self.pending_tool_context.take();
        let context = self
            .active_tool_contexts
            .remove(&operation_id)
            .or(pending_context);

        self.active_tool_streams.remove(&result.call_id);
        if name == "todo" && lifecycle != ToolLifecycle::Failed {
            if let Some(index) = self.active_tool_entries.remove(&result.call_id) {
                self.transcript.remove(index);
                self.transcript_changed(index);
                self.reindex_active_entries(index);
            }
            return;
        }
        if let Some(index) = self.active_tool_entries.remove(&result.call_id)
            && let Some(TranscriptEntry::ToolCall(tool)) = self.transcript.get_mut(index)
        {
            tool.name = name;
            if tool.context.is_none() {
                tool.context = context;
            }
            tool.lifecycle = lifecycle;
            if result
                .metadata
                .get("execution_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                && !tool.output.is_empty()
                && !has_display_blobs
            {
                append_error_boundary(&mut tool.output, &display_output);
            } else if !result.truncated
                || persisted_display_output.is_some()
                || has_display_blobs
                || tool.output.is_empty()
            {
                tool.output = display_output;
            }
            self.transcript_changed(index);
            self.remember_display_source(index, deferred);
            return;
        }

        self.push_transcript_entry(TranscriptEntry::ToolCall(ToolTranscript {
            call_id: Some(result.call_id),
            name,
            context,
            output: display_output,
            lifecycle,
        }));
        self.remember_display_source(self.transcript.len() - 1, deferred);
    }

    fn completed_display_output(
        &self,
        result: &ToolResult,
    ) -> (String, Option<DeferredToolOutput>) {
        let fallback = tool_display_output(result);
        let Some(store) = &self.display_store else {
            return (fallback.to_owned(), None);
        };
        match DisplaySource::from_result(result) {
            Ok(Some(source)) => {
                let output = source
                    .load(store, Some(MAX_LIVE_TOOL_OUTPUT_BYTES))
                    .unwrap_or_else(|error| display_load_error(fallback, &error));
                (
                    output,
                    Some(DeferredToolOutput {
                        source,
                        preview: None,
                    }),
                )
            }
            Ok(None) => (fallback.to_owned(), None),
            Err(error) => (display_load_error(fallback, &error), None),
        }
    }

    fn remember_display_source(&mut self, index: usize, deferred: Option<DeferredToolOutput>) {
        let Some(mut deferred) = deferred else { return };
        if self.transcript_view_expanded
            && let Some(store) = &self.display_store
            && let Some(TranscriptEntry::ToolCall(tool)) = self.transcript.get_mut(index)
        {
            let full = deferred
                .source
                .load(store, None)
                .unwrap_or_else(|error| display_load_error(&tool.output, &error));
            deferred.preview = Some(std::mem::replace(&mut tool.output, full));
        }
        self.deferred_tool_outputs.insert(index, deferred);
    }

    fn reindex_active_entries(&mut self, removed: usize) {
        self.active_tool_entries.retain(|_, index| {
            if *index == removed {
                return false;
            }
            if *index > removed {
                *index -= 1;
            }
            true
        });
        self.active_assistant_entry = self
            .active_assistant_entry
            .and_then(|index| (index != removed).then(|| index - usize::from(index > removed)));
        self.deferred_tool_outputs = std::mem::take(&mut self.deferred_tool_outputs)
            .into_iter()
            .filter_map(|(index, output)| {
                (index != removed).then(|| (index - usize::from(index > removed), output))
            })
            .collect();
    }

    pub fn submit_goal(&mut self, objective: String) -> bool {
        if !matches!(self.activity, ActivityState::Idle) {
            return false;
        }
        self.apply_goal(
            Some(SessionGoal {
                objective: objective.clone(),
                status: GoalStatus::Pursuing,
                turns: 1,
                blocked_streak: 0,
            }),
            true,
        );
        self.push_user(objective.clone());
        self.sent_commands
            .push(EngineCommand::SetGoal { objective });
        let now = Instant::now();
        self.turn_started_at = Some(now);
        self.last_turn_elapsed = None;
        self.activity = ActivityState::Thinking { started_at: now };
        true
    }

    pub fn resume_goal(&mut self) -> bool {
        let Some(goal) = &self.goal else {
            return false;
        };
        if !matches!(
            goal.status,
            GoalStatus::Paused | GoalStatus::Blocked | GoalStatus::BudgetLimited
        ) {
            return false;
        }
        if !matches!(self.activity, ActivityState::Idle) {
            return false;
        }
        self.sent_commands.push(EngineCommand::ResumeGoal);
        let now = Instant::now();
        self.turn_started_at = Some(now);
        self.last_turn_elapsed = None;
        self.activity = ActivityState::Thinking { started_at: now };
        true
    }

    pub fn push_goal_status(&mut self) {
        match &self.goal {
            None => self.push_notice(
                Some("GOAL".into()),
                "no active goal; /goal <objective> to start",
            ),
            Some(goal) => self.push_notice(
                Some("GOAL".into()),
                format!("{}  {}", goal.status.as_str(), goal.objective),
            ),
        }
    }

    fn apply_goal(&mut self, goal: Option<SessionGoal>, notify: bool) {
        let changed = match (&self.goal, &goal) {
            (None, None) => false,
            (None, Some(_)) | (Some(_), None) => true,
            (Some(previous), Some(next)) => {
                previous.status != next.status || previous.objective != next.objective
            }
        };
        if notify && changed {
            match &goal {
                Some(goal) => self.push_notice(
                    Some("GOAL".into()),
                    format!("{}  {}", goal.status.as_str(), goal.objective),
                ),
                None => self.push_notice(Some("GOAL".into()), "cleared"),
            }
        }
        self.goal = goal;
    }

    fn replace_todos(&mut self, items: Vec<TodoItem>) {
        self.todos.clone_from(&items);
        self.selected_todo = self.selected_todo.min(items.len().saturating_sub(1));
        let existing = self
            .transcript
            .iter()
            .position(|entry| matches!(entry, TranscriptEntry::Todos { .. }));
        if items.is_empty() {
            if let Some(index) = existing {
                self.transcript.remove(index);
                self.transcript_changed(index);
                self.reindex_active_entries(index);
            }
            return;
        }
        if let Some(index) = existing {
            self.transcript[index] = TranscriptEntry::Todos { items };
            self.transcript_changed(index);
        } else {
            self.push_transcript_entry(TranscriptEntry::Todos { items });
        }
    }
}

fn grapheme_cursor(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    if cursor == 0 {
        return 0;
    }
    let previous = previous_grapheme_boundary(text, cursor);
    let next = next_grapheme_boundary(text, previous);
    if next == cursor { cursor } else { previous }
}

fn is_non_terminal_runtime_error(message: &str) -> bool {
    matches!(
        message,
        "another command cannot start during an active turn"
            | "command is unavailable while approval is pending"
            | "command is unavailable while agents are running"
            | "there is no pending approval"
            | "approval request is no longer pending"
    )
}

fn append_live_tool_output(output: &mut String, chunk: &str) {
    let retained_bytes = MAX_LIVE_TOOL_OUTPUT_BYTES.saturating_sub(LIVE_OUTPUT_OMITTED.len());
    if chunk.len() > retained_bytes {
        let mut start = chunk.len() - retained_bytes;
        while !chunk.is_char_boundary(start) {
            start += 1;
        }
        output.clear();
        output.push_str(LIVE_OUTPUT_OMITTED);
        output.push_str(&chunk[start..]);
        return;
    }
    output.push_str(chunk);
    if output.len() <= MAX_LIVE_TOOL_OUTPUT_BYTES {
        return;
    }
    let mut retained_start = output.len() - retained_bytes;
    while !output.is_char_boundary(retained_start) {
        retained_start += 1;
    }
    output.replace_range(..retained_start, LIVE_OUTPUT_OMITTED);
}

fn bounded_live_tool_output(output: String) -> String {
    if output.len() <= MAX_LIVE_TOOL_OUTPUT_BYTES {
        return output;
    }
    let mut bounded = String::with_capacity(MAX_LIVE_TOOL_OUTPUT_BYTES);
    append_live_tool_output(&mut bounded, &output);
    bounded
}

fn append_stream_boundary(body: &mut String, stream: &str) {
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push('[');
    body.push_str(stream);
    body.push_str("]\n");
}

fn append_error_boundary(body: &mut String, error: &str) {
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str("\n[error]\n");
    body.push_str(error);
}

fn tool_display_output(result: &ToolResult) -> &str {
    result
        .metadata
        .get("display_output")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&result.output)
}

fn tool_name(result: &ToolResult) -> &str {
    result
        .metadata
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("tool")
}

fn todo_items(result: &ToolResult) -> Option<Vec<TodoItem>> {
    if tool_name(result) != "todo" || result.is_error {
        return None;
    }
    let items: Vec<TodoItem> = result
        .metadata
        .get("items")
        .cloned()
        .and_then(|items| serde_json::from_value(items).ok())
        .or_else(|| serde_json::from_str(&result.output).ok())?;
    TodoItem::validate_list(&items).ok()?;
    Some(items)
}

fn tool_lifecycle(result: &ToolResult) -> ToolLifecycle {
    if result.is_error
        || result
            .metadata
            .get("execution_error")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    {
        ToolLifecycle::Failed
    } else {
        ToolLifecycle::Completed
    }
}

fn operation_context(operation: &kurama_protocol::tool::Operation) -> String {
    match operation {
        kurama_protocol::tool::Operation::Read { path, .. } => path.display().to_string(),
        kurama_protocol::tool::Operation::Write { paths, .. } => paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        kurama_protocol::tool::Operation::Bash { command, .. } => command.clone(),
        kurama_protocol::tool::Operation::WebSearch { query, .. } => query.clone(),
        kurama_protocol::tool::Operation::WebOpen { url, .. } => url.clone(),
    }
}

fn transcript_tool_name(label: &str) -> &str {
    label.split_once('/').map_or(label, |(_, name)| name).trim()
}

fn mode_label(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Supervised => "supervised",
        ExecutionMode::Auto => "auto",
        ExecutionMode::Yolo => "yolo",
    }
}
