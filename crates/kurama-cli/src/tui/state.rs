use kurama_protocol::{
    agent::{AgentSnapshot, AgentState},
    policy::{ApprovalRequest, ApprovalResponse, ExecutionMode},
    runtime::{AgentCommand, EngineCommand, RuntimeEvent},
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
    pub scroll: u16,
    pub status: String,
    pub running_agents: usize,
    pub queued_agents: usize,
    pub overlay: Overlay,
    pub onboarding: OnboardingState,
    pub approval: Option<ApprovalState>,
    pub agents: Vec<AgentRow>,
    pub selected_agent: usize,
    pub agent_message: String,
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
        self.transcript.push(TranscriptEntry {
            kind: TranscriptKind::User,
            label: "YOU".into(),
            body: body.into(),
        });
    }

    pub fn push_assistant(&mut self, body: impl Into<String>) {
        self.transcript.push(TranscriptEntry {
            kind: TranscriptKind::Assistant,
            label: "KURAMA".into(),
            body: body.into(),
        });
    }

    pub fn push_tool(&mut self, label: impl Into<String>, body: impl Into<String>) {
        self.transcript.push(TranscriptEntry {
            kind: TranscriptKind::Tool,
            label: label.into(),
            body: body.into(),
        });
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

    pub fn begin_approval(&mut self, request: ApprovalRequest, arguments: serde_json::Value) {
        self.approval = Some(ApprovalState::new(request, arguments));
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
        match event {
            RuntimeEvent::Status { message } => self.status = message,
            RuntimeEvent::AssistantDelta { text } => self.push_assistant(text),
            RuntimeEvent::ApprovalRequired { request } => {
                self.begin_approval(request, serde_json::Value::Object(Default::default()));
            }
            RuntimeEvent::ToolStarted { name, .. } => self.status = format!("running {name}"),
            RuntimeEvent::ToolOutputDelta { chunk, stream, .. } => {
                self.push_tool(format!("BASH / {stream}"), chunk);
            }
            RuntimeEvent::ToolCompleted { result, .. } => self.push_tool("TOOL", result.output),
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
            RuntimeEvent::Error { message } => self.status = message,
            RuntimeEvent::Shutdown => self.status = "shutdown".into(),
        }
    }
}
