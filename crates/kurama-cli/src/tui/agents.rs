use kurama_protocol::{
    agent::{AgentSnapshot, AgentState},
    id::AgentId,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub id: AgentId,
    pub role: String,
    pub profile: String,
    pub task: String,
    pub state: AgentState,
    pub activity: String,
    pub transcript: Vec<String>,
}

impl AgentRow {
    pub fn from_snapshot(snapshot: AgentSnapshot) -> Self {
        let AgentSnapshot {
            id,
            role,
            objective,
            profile,
            state,
            phase,
            active_operation,
            changed_files: _,
            last_error,
        } = snapshot;
        let transcript = last_error.clone().into_iter().collect();
        let activity = active_operation
            .or(phase)
            .or(last_error)
            .unwrap_or_else(|| state_label(&state).into());

        Self {
            id,
            role,
            profile,
            task: objective,
            state,
            activity,
            transcript,
        }
    }

    pub fn update_from_snapshot(&mut self, snapshot: AgentSnapshot) {
        let transcript = std::mem::take(&mut self.transcript);
        *self = Self::from_snapshot(snapshot);
        if !transcript.is_empty() {
            self.transcript = transcript;
        }
    }
}

pub fn sort_agents(agents: &mut [AgentRow]) {
    agents.sort_by_key(|agent| match agent.state {
        AgentState::Running => 0,
        AgentState::Queued => 1,
        AgentState::Completed => 2,
        AgentState::Failed => 3,
        AgentState::Cancelled => 4,
    });
}

pub(crate) fn state_label(state: &AgentState) -> &'static str {
    match state {
        AgentState::Queued => "queued",
        AgentState::Running => "running",
        AgentState::Completed => "completed",
        AgentState::Failed => "failed",
        AgentState::Cancelled => "cancelled",
    }
}
