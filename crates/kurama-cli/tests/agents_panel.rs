use kurama_cli::tui::{AgentRow, Overlay, TuiState, render};
use kurama_protocol::{
    agent::{AgentSnapshot, AgentState},
    id::{AgentId, SessionId},
    policy::ExecutionMode,
    runtime::{AgentCommand, EngineCommand, RuntimeEvent},
    session::{EventEnvelope, SessionEvent},
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
        state,
        activity: "reading policy".into(),
        transcript: vec!["Inspecting policy boundaries.".into()],
    }
}

fn snapshot(id: &str, state: AgentState) -> AgentSnapshot {
    AgentSnapshot {
        id: AgentId::from(id),
        role: "reviewer".into(),
        objective: "inspect auth".into(),
        profile: "gpt-5.6".into(),
        state,
        phase: Some("reviewing".into()),
        active_operation: None,
        changed_files: Vec::new(),
        last_error: None,
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
        "RUNNING",
        "a_2",
        "QUEUED",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    assert!(!text.contains("tool calls"));
    assert!(!text.contains("SCOPE"));
    assert!(!text.contains("TIME"));
    assert!(text.contains("ID        ROLE          PROFILE      TASK                  STATE"));
}

#[test]
fn replay_restores_terminal_agents_without_duplicating_startup_updates() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    let session_id = SessionId::from("s_resume");
    let events = [
        SessionEvent::AgentCompleted {
            snapshot: snapshot("a_completed", AgentState::Completed),
            summary: "review complete".into(),
        },
        SessionEvent::AgentFailed {
            snapshot: snapshot("a_failed", AgentState::Failed),
            error: "provider disconnected".into(),
        },
        SessionEvent::AgentCancelled {
            snapshot: snapshot("a_cancelled", AgentState::Cancelled),
        },
    ]
    .into_iter()
    .enumerate()
    .map(|(sequence, event)| {
        EventEnvelope::new(
            sequence as u64,
            sequence as u64 + 1,
            session_id.clone(),
            None,
            event,
        )
    })
    .collect::<Vec<_>>();

    state.hydrate_replay(&events);
    state.apply_runtime_event(RuntimeEvent::AgentUpdated {
        snapshot: snapshot("a_failed", AgentState::Failed),
    });

    assert_eq!(state.agents.len(), 3);
    assert!(
        state
            .agents
            .iter()
            .any(|agent| agent.id == AgentId::from("a_completed")
                && agent.state == AgentState::Completed)
    );
    assert!(
        state
            .agents
            .iter()
            .any(|agent| agent.id == AgentId::from("a_failed")
                && agent.state == AgentState::Failed)
    );
    assert!(
        state
            .agents
            .iter()
            .any(|agent| agent.id == AgentId::from("a_cancelled")
                && agent.state == AgentState::Cancelled)
    );
}

#[test]
fn enter_inspects_message_and_confirmed_cancel() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.set_agents(vec![row("a_1", AgentState::Running)]);
    state.open_agents();
    state.inspect_selected_agent();
    assert!(matches!(state.overlay(), Overlay::AgentInspect));
    assert!(matches!(
        state.sent_commands().last(),
        Some(EngineCommand::Agent(AgentCommand::Inspect { agent_id }))
            if agent_id == &AgentId::from("a_1")
    ));
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

#[test]
fn runtime_updates_populate_and_update_agents_panel() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);

    state.apply_runtime_event(RuntimeEvent::AgentUpdated {
        snapshot: snapshot("a_1", AgentState::Running),
    });

    assert_eq!(state.agents.len(), 1);
    assert_eq!(state.running_agents, 1);
    assert_eq!(state.agents[0].task, "inspect auth");
    assert_eq!(state.agents[0].activity, "reviewing");

    let mut updated = snapshot("a_1", AgentState::Completed);
    updated.phase = None;
    updated.active_operation = Some("summarizing findings".into());
    updated.changed_files = vec!["src/auth.rs".into()];
    state.apply_runtime_event(RuntimeEvent::AgentUpdated { snapshot: updated });

    assert_eq!(state.agents.len(), 1);
    assert_eq!(state.running_agents, 0);
    assert_eq!(state.agents[0].state, AgentState::Completed);
    assert_eq!(state.agents[0].activity, "summarizing findings");
    state.open_agents();
    let text = rendered(&state);
    assert!(!text.contains("1 file"));
    assert!(!text.contains("elapsed"));
}

#[test]
fn inspection_updates_the_matching_agent_not_the_selected_row() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.set_agents(vec![
        row("a_1", AgentState::Running),
        row("a_2", AgentState::Queued),
    ]);

    state.apply_runtime_event(RuntimeEvent::AgentInspection {
        snapshot: snapshot("a_2", AgentState::Queued),
        transcript: vec!["Queued behind a_1.".into()],
    });

    assert_eq!(state.agents[0].id, AgentId::from("a_1"));
    assert_eq!(
        state.agents[0].transcript,
        vec!["Inspecting policy boundaries."]
    );
    let inspected = state
        .agents
        .iter()
        .find(|agent| agent.id == AgentId::from("a_2"))
        .expect("matching agent remains present");
    assert_eq!(inspected.transcript, vec!["Queued behind a_1."]);
}
