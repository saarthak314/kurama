mod agents;
mod approval;
mod input;
mod onboarding;
mod state;

mod render_compat {
    use kurama_protocol::policy::ExecutionMode;

    pub(super) use super::{ApprovalState, Overlay};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum TranscriptKind {
        User,
        Assistant,
        Tool,
        System,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct TranscriptEntry {
        pub kind: TranscriptKind,
        pub label: String,
        pub body: String,
    }

    impl From<&super::TranscriptEntry> for TranscriptEntry {
        fn from(entry: &super::TranscriptEntry) -> Self {
            match entry {
                super::TranscriptEntry::UserTurn { body } => Self {
                    kind: TranscriptKind::User,
                    label: "YOU".into(),
                    body: body.clone(),
                },
                super::TranscriptEntry::AssistantMessage { body } => Self {
                    kind: TranscriptKind::Assistant,
                    label: "KURAMA".into(),
                    body: body.clone(),
                },
                super::TranscriptEntry::ToolCall(tool) => Self {
                    kind: TranscriptKind::Tool,
                    label: format!("TOOL / {}", tool.name),
                    body: tool.output.clone(),
                },
                super::TranscriptEntry::Error { body } => Self {
                    kind: TranscriptKind::System,
                    label: "ERROR".into(),
                    body: body.clone(),
                },
                super::TranscriptEntry::Notice { label, body } => Self {
                    kind: TranscriptKind::System,
                    label: label.clone().unwrap_or_else(|| "NOTICE".into()),
                    body: body.clone(),
                },
            }
        }
    }

    pub(super) struct TuiState<'a> {
        pub profile: &'a str,
        pub model: &'a str,
        pub project: &'a str,
        pub mode: ExecutionMode,
        pub composer: &'a str,
        pub cursor: usize,
        pub scroll: usize,
        pub status: &'a String,
        pub running_agents: usize,
        pub queued_agents: usize,
        pub overlay: Overlay,
        pub onboarding: &'a super::OnboardingState,
        pub approval: &'a Option<ApprovalState>,
        pub agents: &'a Vec<super::AgentRow>,
        pub selected_agent: usize,
        pub agent_message: &'a str,
        live_transcript: Vec<TranscriptEntry>,
    }

    impl<'a> From<&'a super::TuiState> for TuiState<'a> {
        fn from(state: &'a super::TuiState) -> Self {
            Self {
                profile: &state.profile,
                model: &state.model,
                project: &state.project,
                mode: state.mode,
                composer: &state.composer,
                cursor: state.cursor,
                scroll: state.scroll,
                status: &state.status,
                running_agents: state.running_agents,
                queued_agents: state.queued_agents,
                overlay: state.overlay,
                onboarding: &state.onboarding,
                approval: &state.approval,
                agents: &state.agents,
                selected_agent: state.selected_agent,
                agent_message: &state.agent_message,
                live_transcript: state
                    .live_transcript()
                    .iter()
                    .map(TranscriptEntry::from)
                    .collect(),
            }
        }
    }

    impl TuiState<'_> {
        pub(super) fn live_transcript(&self) -> &[TranscriptEntry] {
            &self.live_transcript
        }

        pub(super) fn selected_agent(&self) -> Option<&super::AgentRow> {
            self.agents.get(self.selected_agent)
        }
    }

    mod implementation {
        include!("render.rs");
    }

    pub(crate) use implementation::SURFACE;

    pub fn render(frame: &mut ratatui::Frame<'_>, state: &super::TuiState) {
        implementation::render(frame, &TuiState::from(state));
    }

    pub(crate) fn transcript_lines(
        entries: &[super::TranscriptEntry],
        width: usize,
    ) -> Vec<ratatui::text::Line<'static>> {
        let entries = entries
            .iter()
            .map(TranscriptEntry::from)
            .collect::<Vec<_>>();
        implementation::transcript_lines(&entries, width)
    }
}

pub use agents::{AgentRow, sort_agents};
pub use approval::ApprovalState;
pub use input::{TerminalGuard, spawn_input_thread};
pub use onboarding::{OnboardingState, OnboardingSubmission};
pub use render_compat::render;
pub(crate) use render_compat::{SURFACE, transcript_lines};
pub use state::{ActivityState, Overlay, ToolLifecycle, ToolTranscript, TranscriptEntry, TuiState};
