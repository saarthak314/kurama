use kurama_cli::tui::{AgentRow, Overlay, TuiState, render};
use kurama_protocol::{
    agent::AgentState, id::AgentId, policy::ExecutionMode, runtime::EngineCommand,
};
use ratatui::{Terminal, backend::TestBackend};

fn rendered(state: &TuiState) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn row(id: &str, state: AgentState) -> AgentRow {
    AgentRow {
        id: AgentId::from(id),
        role: "reviewer".into(),
        profile: "gpt-5.6".into(),
        task: "inspect auth".into(),
        scope: "read-only".into(),
        elapsed: "00:38".into(),
        state,
        activity: "reading policy".into(),
        transcript: vec!["Inspecting policy boundaries.".into()],
    }
}

#[test]
fn panel_shows_control_fields_but_not_tool_statistics() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.set_agents(vec![
        row("a_1", AgentState::Running),
        row("a_2", AgentState::Queued),
    ]);
    state.open_agents();
    let text = rendered(&state);
    for expected in [
        "a_1",
        "reviewer",
        "gpt-5.6",
        "inspect auth",
        "read-only",
        "RUNNING",
        "a_2",
        "QUEUED",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    assert!(!text.contains("tool calls"));
}

#[test]
fn enter_inspects_message_and_confirmed_cancel() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.set_agents(vec![row("a_1", AgentState::Running)]);
    state.open_agents();
    state.inspect_selected_agent();
    assert!(matches!(state.overlay(), Overlay::AgentInspect));
    state.begin_agent_message();
    state.set_agent_message("focus on the parser");
    state.submit_agent_message();
    assert!(matches!(
        state.sent_commands().last(),
        Some(EngineCommand::Agent(_))
    ));
    state.request_agent_cancel();
    assert!(matches!(state.overlay(), Overlay::ConfirmAgentCancel));
    state.confirm_agent_cancel();
    assert!(matches!(
        state.sent_commands().last(),
        Some(EngineCommand::Agent(_))
    ));
}
