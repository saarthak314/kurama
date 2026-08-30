use std::collections::{HashMap, HashSet};
use std::time::Instant;

use kurama_protocol::{
    agent::{AgentSnapshot, AgentState},
    id::{AgentId, CallId},
    policy::{ApprovalRequest, ApprovalResponse, ExecutionMode},
    runtime::{AgentCommand, EngineCommand, RuntimeEvent},
    session::{EventEnvelope, SessionEvent},
    tool::ToolResult,
};

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
    pub output: String,
    pub lifecycle: ToolLifecycle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEntry {
    UserTurn { body: String },
    AssistantMessage { body: String },
    ToolCall(ToolTranscript),
    Error { body: String },
    Notice { label: Option<String>, body: String },
}

pub struct TuiState {
    pub profile: String,
    pub model: String,
    pub project: String,
    pub mode: ExecutionMode,
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
    activity: ActivityState,
    transcript_view_expanded: bool,
    active_assistant_entry: Option<usize>,
    active_tool_entries: HashMap<CallId, usize>,
    active_tool_streams: HashMap<CallId, String>,
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
            activity: ActivityState::Idle,
            transcript_view_expanded: false,
            active_assistant_entry: None,
            active_tool_entries: HashMap::new(),
            active_tool_streams: HashMap::new(),
            replayed_agent_ids: HashSet::new(),
            committed_transcript_entries: 0,
            sent_commands: Vec::new(),
        }
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

    pub fn set_thinking(&mut self) {
        self.activity = ActivityState::Thinking {
            started_at: Instant::now(),
        };
    }

    pub fn toggle_transcript_view(&mut self) {
        self.transcript_view_expanded = !self.transcript_view_expanded;
        self.scroll = 0;
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
        if !self.activity.is_animated() {
            return false;
        }
        self.sent_commands.push(EngineCommand::CancelTurn);
        self.activity = ActivityState::Interrupted;
        true
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
        self.active_assistant_entry = None;
        self.active_tool_entries.clear();
        self.active_tool_streams.clear();
        self.replayed_agent_ids.clear();
        self.committed_transcript_entries = 0;
        self.transcript.clear();
        self.agents.clear();
        for envelope in replay {
            match &envelope.event {
                SessionEvent::UserMessage { text } => self.push_user(text.clone()),
                SessionEvent::AssistantMessage { text } => self.push_assistant(text.clone()),
                SessionEvent::ToolCompleted { result, .. } => {
                    self.push_transcript_entry(TranscriptEntry::ToolCall(tool_transcript(result)));
                }
                SessionEvent::ToolUnknown { reason, .. } => {
                    self.push_notice(Some("TOOL".into()), reason.clone());
                }
                SessionEvent::ModeSelected { mode } => {
                    self.push_notice(Some("MODE".into()), mode_label(*mode).to_owned());
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
            self.overlay = Overlay::AgentMessage;
        }
    }

    pub fn set_agent_message(&mut self, message: impl Into<String>) {
        self.agent_message = message.into();
    }

    pub fn submit_agent_message(&mut self) {
        if let Some(agent_id) = self.selected_agent().map(|agent| agent.id.clone()) {
            let text = std::mem::take(&mut self.agent_message);
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
            Overlay::AgentInspect | Overlay::AgentMessage | Overlay::ConfirmAgentCancel => {
                Overlay::Agents
            }
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
            self.overlay = Overlay::ApprovalEdit;
        }
    }

    pub fn replace_approval_arguments(&mut self, arguments: serde_json::Value) {
        if let Some(approval) = &mut self.approval {
            approval.editor =
                serde_json::to_string_pretty(&arguments).unwrap_or_else(|_| "{}".into());
            approval.arguments = arguments;
        }
    }

    pub fn set_approval_editor(&mut self, editor: impl Into<String>) {
        if let Some(approval) = &mut self.approval {
            approval.editor = editor.into();
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
                self.push_error(message.clone());
                self.overlay = Overlay::ApprovalEdit;
                self.activity = ActivityState::AwaitingApproval;
                if let Some(approval) = &mut self.approval {
                    approval.editing = true;
                }
                return Err(message);
            }
        };
        if let Some(approval) = &mut self.approval {
            approval.arguments = arguments.clone();
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
        let completes_active_streams = matches!(
            &event,
            RuntimeEvent::TurnCompleted | RuntimeEvent::Error { .. } | RuntimeEvent::Shutdown
        );
        if !matches!(&event, RuntimeEvent::AssistantDelta { .. }) {
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
            RuntimeEvent::ToolStarted { name, .. } => {
                self.activity = ActivityState::RunningTool {
                    name,
                    started_at: Instant::now(),
                };
            }
            RuntimeEvent::ToolOutputDelta {
                call_id,
                chunk,
                stream,
            } => {
                self.append_tool_delta(call_id, stream, chunk);
            }
            RuntimeEvent::ToolCompleted { result, .. } => {
                self.complete_tool(result);
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
            RuntimeEvent::TurnCompleted => self.activity = ActivityState::Idle,
            RuntimeEvent::Error { message } => {
                self.push_error(message);
                self.activity = ActivityState::Idle;
            }
            RuntimeEvent::Shutdown => self.activity = ActivityState::Idle,
        }
        if completes_active_streams {
            self.active_tool_entries.clear();
            self.active_tool_streams.clear();
        }
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
            output: bounded_live_tool_output(chunk),
            lifecycle: ToolLifecycle::Running,
        }));
        if let Some(index) = self.transcript.len().checked_sub(1) {
            self.active_tool_entries.insert(call_id.clone(), index);
            self.active_tool_streams.insert(call_id, stream);
        }
    }

    fn complete_tool(&mut self, result: ToolResult) {
        let persisted_display_output = result
            .metadata
            .get("display_output")
            .and_then(serde_json::Value::as_str);
        let display_output = tool_display_output(&result).to_owned();
        let lifecycle = tool_lifecycle(&result);
        let name = tool_name(&result).to_owned();

        self.active_tool_streams.remove(&result.call_id);
        if let Some(index) = self.active_tool_entries.remove(&result.call_id)
            && let Some(TranscriptEntry::ToolCall(tool)) = self.transcript.get_mut(index)
        {
            tool.name = name;
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
            output: display_output,
            lifecycle,
        }));
    }
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

fn tool_transcript(result: &ToolResult) -> ToolTranscript {
    ToolTranscript {
        call_id: Some(result.call_id.clone()),
        name: tool_name(result).to_owned(),
        output: tool_display_output(result).to_owned(),
        lifecycle: tool_lifecycle(result),
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
