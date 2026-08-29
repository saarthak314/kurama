use std::collections::HashMap;

use kurama_protocol::{
    agent::{AgentSnapshot, AgentState},
    id::CallId,
    policy::{ApprovalRequest, ApprovalResponse, ExecutionMode},
    runtime::{AgentCommand, EngineCommand, RuntimeEvent},
    session::{EventEnvelope, SessionEvent},
    tool::ToolResult,
};

use super::{AgentRow, ApprovalState, OnboardingState, sort_agents};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptKind {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub kind: TranscriptKind,
    pub label: String,
    pub body: String,
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
    pub status: String,
    pub running_agents: usize,
    pub queued_agents: usize,
    pub overlay: Overlay,
    pub onboarding: OnboardingState,
    pub approval: Option<ApprovalState>,
    pub agents: Vec<AgentRow>,
    pub selected_agent: usize,
    pub agent_message: String,
    active_assistant_entry: Option<usize>,
    active_tool_entries: HashMap<CallId, usize>,
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
            status: "ready".into(),
            running_agents: 0,
            queued_agents: 0,
            overlay: Overlay::None,
            onboarding: OnboardingState::new(),
            approval: None,
            agents: Vec::new(),
            selected_agent: 0,
            agent_message: String::new(),
            active_assistant_entry: None,
            active_tool_entries: HashMap::new(),
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
        state.status = "enter session credential".into();
        state
    }

    pub fn push_user(&mut self, body: impl Into<String>) {
        self.active_assistant_entry = None;
        self.transcript.push(TranscriptEntry {
            kind: TranscriptKind::User,
            label: "YOU".into(),
            body: body.into(),
        });
    }

    pub fn push_assistant(&mut self, body: impl Into<String>) {
        self.active_assistant_entry = None;
        self.transcript.push(TranscriptEntry {
            kind: TranscriptKind::Assistant,
            label: "KURAMA".into(),
            body: body.into(),
        });
    }

    pub fn push_tool(&mut self, label: impl Into<String>, body: impl Into<String>) {
        self.active_assistant_entry = None;
        self.transcript.push(TranscriptEntry {
            kind: TranscriptKind::Tool,
            label: label.into(),
            body: body.into(),
        });
    }

    pub fn push_system(&mut self, label: impl Into<String>, body: impl Into<String>) {
        self.active_assistant_entry = None;
        self.transcript.push(TranscriptEntry {
            kind: TranscriptKind::System,
            label: label.into(),
            body: body.into(),
        });
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
        self.committed_transcript_entries = end.min(self.transcript.len());
        self.scroll = 0;
    }

    pub fn live_transcript(&self) -> &[TranscriptEntry] {
        &self.transcript[self.committed_transcript_entries..]
    }

    pub fn hydrate_replay(&mut self, replay: &[EventEnvelope]) {
        self.active_assistant_entry = None;
        self.active_tool_entries.clear();
        self.committed_transcript_entries = 0;
        self.transcript.clear();
        self.agents.clear();
        for envelope in replay {
            match &envelope.event {
                SessionEvent::UserMessage { text } => self.push_user(text.clone()),
                SessionEvent::AssistantMessage { text } => self.push_assistant(text.clone()),
                SessionEvent::ToolCompleted { result, .. } => {
                    self.push_tool(tool_label(result), result.output.clone());
                }
                SessionEvent::ToolUnknown { reason, .. } => {
                    self.push_system("TOOL", reason.clone());
                }
                SessionEvent::ModeSelected { mode } => {
                    self.push_system("MODE", mode_label(*mode).to_owned());
                }
                SessionEvent::TurnFailed { error } => self.push_system("ERROR", error.clone()),
                SessionEvent::RecoveryRepair { removed_bytes } => self.push_system(
                    "RECOVERY",
                    format!("removed {removed_bytes} incomplete transcript bytes"),
                ),
                SessionEvent::AgentQueued { snapshot }
                | SessionEvent::AgentStarted { snapshot }
                | SessionEvent::AgentProgress { snapshot }
                | SessionEvent::AgentCompleted { snapshot, .. }
                | SessionEvent::AgentFailed { snapshot, .. }
                | SessionEvent::AgentCancelled { snapshot } => self.upsert_agent(snapshot.clone()),
                _ => {}
            }
        }
    }

    pub fn set_agent_counts(&mut self, running: usize, queued: usize) {
        self.running_agents = running;
        self.queued_agents = queued;
    }

    pub fn set_agents(&mut self, mut agents: Vec<AgentRow>) {
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
        if let Some(agent_id) = self.selected_agent().map(|agent| agent.id.clone()) {
            self.sent_commands
                .push(EngineCommand::Agent(AgentCommand::Inspect { agent_id }));
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
                self.status = "approval pending".into();
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
        self.status = "approval pending".into();
    }

    pub fn resolve_approval(&mut self, response: ApprovalResponse) {
        let Some(approval) = self.approval.take() else {
            self.status = "no approval is pending".into();
            return;
        };
        self.sent_commands.push(EngineCommand::ResolveApproval {
            operation_id: approval.request.operation_id,
            response,
        });
        self.overlay = Overlay::None;
        self.status = "approval submitted".into();
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
                self.status = message.clone();
                self.overlay = Overlay::ApprovalEdit;
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
        if !matches!(&event, RuntimeEvent::AssistantDelta { .. }) {
            self.active_assistant_entry = None;
        }
        match event {
            RuntimeEvent::Status { message } => self.status = message,
            RuntimeEvent::AssistantDelta { text } => self.append_assistant_delta(text),
            RuntimeEvent::ApprovalRequired { request } => {
                self.begin_approval(request);
            }
            RuntimeEvent::ToolStarted { name, .. } => self.status = format!("running {name}"),
            RuntimeEvent::ToolOutputDelta {
                call_id,
                chunk,
                stream,
            } => {
                self.append_tool_delta(call_id, stream, chunk);
            }
            RuntimeEvent::ToolCompleted { result, .. } => {
                self.complete_tool(result);
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
            RuntimeEvent::TurnCompleted => self.status = "ready".into(),
            RuntimeEvent::Error { message } => {
                self.push_system("ERROR", message);
                self.status = "ready".into();
            }
            RuntimeEvent::Shutdown => self.status = "shutdown".into(),
        }
    }

    fn append_assistant_delta(&mut self, text: String) {
        if let Some(entry) = self
            .active_assistant_entry
            .and_then(|index| self.transcript.get_mut(index))
        {
            entry.body.push_str(&text);
            return;
        }

        self.push_assistant(text);
        self.active_assistant_entry = self.transcript.len().checked_sub(1);
    }

    fn append_tool_delta(&mut self, call_id: CallId, stream: String, chunk: String) {
        if let Some(entry) = self
            .active_tool_entries
            .get(&call_id)
            .and_then(|index| self.transcript.get_mut(*index))
        {
            entry.body.push_str(&chunk);
            return;
        }

        self.push_tool(format!("BASH / {stream}"), chunk);
        if let Some(index) = self.transcript.len().checked_sub(1) {
            self.active_tool_entries.insert(call_id, index);
        }
    }

    fn complete_tool(&mut self, result: ToolResult) {
        let label = tool_label(&result);
        let entry = TranscriptEntry {
            kind: TranscriptKind::Tool,
            label,
            body: result.output,
        };

        if let Some(index) = self.active_tool_entries.remove(&result.call_id)
            && let Some(active_entry) = self.transcript.get_mut(index)
        {
            *active_entry = entry;
            return;
        }

        self.transcript.push(entry);
    }
}

fn tool_label(result: &ToolResult) -> String {
    result
        .metadata
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| "TOOL".into(), |name| format!("TOOL / {name}"))
}

fn mode_label(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Supervised => "supervised",
        ExecutionMode::Auto => "auto",
        ExecutionMode::Yolo => "yolo",
    }
}
