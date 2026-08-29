use kurama_protocol::{agent::AgentState, id::AgentId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub id: AgentId,
    pub role: String,
    pub profile: String,
    pub task: String,
    pub scope: String,
    pub elapsed: String,
    pub state: AgentState,
    pub activity: String,
    pub transcript: Vec<String>,
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
