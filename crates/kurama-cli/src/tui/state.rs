use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use kurama_protocol::{
    agent::{AgentSnapshot, AgentState},
    id::{AgentId, CallId, OperationId},
    model::Usage,
    policy::{ApprovalRequest, ApprovalResponse, ExecutionMode},
    runtime::{AgentCommand, EngineCommand, RuntimeEvent},
    session::{EventEnvelope, SessionEvent},
    tool::ToolResult,
};

use crate::commands::{CommandSpec, command_suggestions};

use super::{AgentRow, ApprovalState, OnboardingState, sort_agents};

const MAX_LIVE_TOOL_OUTPUT_BYTES: usize = 128 * 1024;
const LIVE_OUTPUT_OMITTED: &str = "[earlier live output omitted]\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overlay {
    #[default]
    None,
    Onboarding,
    Approval,
    ApprovalEdit,
    Agents,
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
    pub cursor: usize,
    pub scroll: usize,
    pub running_agents: usize,
    pub queued_agents: usize,
    pub overlay: Overlay,
    pub onboarding: OnboardingState,
    pub approval: Option<ApprovalState>,
    pub agents: Vec<AgentRow>,
    pub selected_agent: usize,
    pub agent_message: String,
    pub agent_message_cursor: usize,
    command_selection: usize,
    command_palette_dismissed: bool,
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
    replayed_agent_ids: HashSet<AgentId>,
    committed_transcript_entries: usize,
    sent_commands: Vec<EngineCommand>,
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
            cursor: 0,
            scroll: 0,
            running_agents: 0,
            queued_agents: 0,
            overlay: Overlay::None,
            onboarding: OnboardingState::new(),
            approval: None,
            agents: Vec::new(),
            selected_agent: 0,
            agent_message: String::new(),
            agent_message_cursor: 0,
            command_selection: 0,
            command_palette_dismissed: false,
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
            replayed_agent_ids: HashSet::new(),
            committed_transcript_entries: 0,
            sent_commands: Vec::new(),
        }
    }

    pub fn command_suggestions(&self) -> Vec<CommandSpec> {
        if self.command_palette_dismissed
            || self.overlay != Overlay::None
            || self.transcript_view_expanded
        {
            return Vec::new();
        }
        command_suggestions(&self.composer)
    }

    pub const fn command_selection(&self) -> usize {
        self.command_selection
    }

    pub fn composer_edited(&mut self) {
        self.command_selection = 0;
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
        self.composer.truncate(self.cursor);
        self.composer_edited();
    }

    pub fn kill_to_start(&mut self) {
        self.composer.replace_range(..self.cursor, "");
        self.cursor = 0;
        self.composer_edited();
    }

    pub fn kill_previous_word(&mut self) {
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
        if self.command_suggestions().is_empty() {
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
                project: project.into(),
                mode: self.mode,
            },
        );
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
        if self.usage.input_tokens == 0 {
            return Some(compact_tokens(self.max_input_tokens));
        }
        let percent =
            (self.usage.input_tokens.saturating_mul(100) / self.max_input_tokens.max(1)).min(100);
        Some(format!("{percent}%"))
    }

    pub fn open_shortcuts(&mut self) {
        self.overlay = Overlay::Shortcuts;
    }

    pub fn toggle_transcript_view(&mut self) {
        self.transcript_view_expanded = !self.transcript_view_expanded;
        if self.transcript_view_expanded {
            self.scroll = 0;
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
    }

    pub fn stable_transcript_end(&self) -> usize {
        self.active_assistant_entry
            .into_iter()
            .chain(self.active_tool_entries.values().copied())
            .min()
            .unwrap_or(self.transcript.len())
            .max(self.committed_transcript_entries)
    }

    pub fn stable_transcript(&self) -> &[TranscriptEntry] {
        &self.transcript[self.committed_transcript_entries..self.stable_transcript_end()]
    }

    pub fn mark_transcript_committed(&mut self, end: usize) {
        self.committed_transcript_entries = self
            .committed_transcript_entries
            .max(end.min(self.stable_transcript_end()));
        self.scroll = 0;
    }

    pub fn reset_transcript_commit(&mut self) {
        self.committed_transcript_entries = 0;
        self.scroll = 0;
    }

    pub fn live_transcript(&self) -> &[TranscriptEntry] {
        &self.transcript[self.committed_transcript_entries..]
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
        self.replayed_agent_ids.clear();
        self.committed_transcript_entries = 0;
        self.transcript.clear();
        self.agents.clear();
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
                    self.push_transcript_entry(TranscriptEntry::ToolCall(tool_transcript(
                        result,
                        replayed_tool_contexts.remove(operation_id),
                    )));
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

    pub fn insert_agent_message(&mut self, text: &str) {
        self.agent_message
            .insert_str(self.agent_message_cursor, text);
        self.agent_message_cursor = self
            .agent_message_cursor
            .saturating_add(text.len())
            .min(self.agent_message.len());
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

    pub fn begin_approval(&mut self, request: ApprovalRequest) {
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
                self.complete_tool(operation_id, result);
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
            return;
        }

        self.push_assistant(text);
        self.active_assistant_entry = self.transcript.len().checked_sub(1);
    }

    fn append_tool_delta(&mut self, call_id: CallId, stream: String, chunk: String) {
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
        let display_output = tool_display_output(&result).to_owned();
        let lifecycle = tool_lifecycle(&result);
        let name = tool_name(&result).to_owned();
        let pending_context = self.pending_tool_context.take();
        let context = self
            .active_tool_contexts
            .remove(&operation_id)
            .or(pending_context);

        self.active_tool_streams.remove(&result.call_id);
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
            {
                append_error_boundary(&mut tool.output, &display_output);
            } else if !result.truncated
                || persisted_display_output.is_some()
                || tool.output.is_empty()
            {
                tool.output = display_output;
            }
            return;
        }

        self.push_transcript_entry(TranscriptEntry::ToolCall(ToolTranscript {
            call_id: Some(result.call_id),
            name,
            context,
            output: display_output,
            lifecycle,
        }));
    }
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
    output.push_str(chunk);
    if output.len() <= MAX_LIVE_TOOL_OUTPUT_BYTES {
        return;
    }

    let retained_bytes = MAX_LIVE_TOOL_OUTPUT_BYTES.saturating_sub(LIVE_OUTPUT_OMITTED.len());
    let mut retained_start = output.len().saturating_sub(retained_bytes);
    while !output.is_char_boundary(retained_start) {
        retained_start += 1;
    }
    let retained = output[retained_start..].to_owned();
    output.clear();
    output.push_str(LIVE_OUTPUT_OMITTED);
    output.push_str(&retained);
}

fn bounded_live_tool_output(mut output: String) -> String {
    if output.len() > MAX_LIVE_TOOL_OUTPUT_BYTES {
        let contents = std::mem::take(&mut output);
        append_live_tool_output(&mut output, &contents);
    }
    output
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

fn tool_transcript(result: &ToolResult, context: Option<String>) -> ToolTranscript {
    ToolTranscript {
        call_id: Some(result.call_id.clone()),
        name: tool_name(result).to_owned(),
        context,
        output: tool_display_output(result).to_owned(),
        lifecycle: tool_lifecycle(result),
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

fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1000 {
        format!("{}k", (tokens + 500) / 1000)
    } else {
        tokens.to_string()
    }
}
