use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use kurama_cli::{
    app::App,
    tui::{OnboardingState, Overlay, TuiState, render},
};
use kurama_core::testing::ScriptedBackend;
use kurama_protocol::{
    agent::{AgentSnapshot, AgentState},
    policy::ExecutionMode,
    runtime::RuntimeEvent,
    session::{SessionMetadata, TodoItem, TodoStatus},
};
use kurama_sdk::Agent;
use ratatui::{
    Terminal,
    backend::{Backend, TestBackend},
    buffer::Buffer,
};

fn text(buffer: &Buffer) -> String {
    buffer
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
}

#[test]
fn credential_cursor_remains_inside_tiny_terminals_and_masks_the_secret() {
    let mut state = TuiState::new("fixture", "model", ".", ExecutionMode::Supervised);
    state.overlay = Overlay::Onboarding;
    state.onboarding = OnboardingState::credential("fixture");
    state
        .onboarding
        .insert_str("secret-value-that-must-never-be-shown");
    for width in 1..=12 {
        for height in 1..=8 {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| render(frame, &state))
                .expect("render credential");
            let cursor = terminal
                .backend_mut()
                .get_cursor_position()
                .expect("cursor");
            assert!(
                cursor.x < width && cursor.y < height,
                "cursor {cursor:?} outside {width}x{height}"
            );
            assert!(!text(terminal.backend().buffer()).contains("secret-value"));
        }
    }
}

#[test]
fn agent_message_window_keeps_the_insertion_tail_visible() {
    let mut state = TuiState::new("fixture", "model", ".", ExecutionMode::Supervised);
    state.apply_runtime_event(RuntimeEvent::AgentUpdated {
        snapshot: AgentSnapshot {
            id: "child".into(),
            role: "reviewer".into(),
            objective: "inspect".into(),
            profile: "fixture".into(),
            state: AgentState::Running,
            phase: Some("working".into()),
            active_operation: None,
            changed_files: Vec::new(),
            last_error: None,
        },
    });
    state.overlay = Overlay::AgentMessage;
    state.agent_message = format!("{}TAIL_VISIBLE", "earlier text ".repeat(30));
    state.agent_message_cursor = state.agent_message.len();
    let mut terminal = Terminal::new(TestBackend::new(36, 10)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &state))
        .expect("render message");
    assert!(text(terminal.backend().buffer()).contains("TAIL_VISIBLE"));
    let cursor = terminal
        .backend_mut()
        .get_cursor_position()
        .expect("cursor");
    assert!(cursor.x < 36 && cursor.y < 10);
}

#[tokio::test]
async fn todo_navigation_reveals_items_beyond_a_short_viewport() {
    let agent = Agent::new()
        .backend(ScriptedBackend::new(Vec::new()))
        .build()
        .expect("agent");
    let metadata = SessionMetadata {
        id: "todo-view".into(),
        created_at_ms: 0,
        project_root: ".".into(),
        profile: agent.active_profile().into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    };
    let (handle, _events) = agent.launch(metadata.clone(), Vec::new()).expect("launch");
    let mut state = TuiState::new("fixture", "model", ".", ExecutionMode::Supervised);
    state.todos = (0..20)
        .map(|index| TodoItem {
            id: index.to_string(),
            content: format!("visible task {index}"),
            status: TodoStatus::Pending,
        })
        .collect();
    state.overlay = Overlay::Todos;
    let mut app = App::from_runtime(state, handle, agent.orchestrator(), metadata.id);
    for _ in 0..19 {
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
            .expect("navigate");
    }
    let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("terminal");
    terminal
        .draw(|frame| render(frame, &app.state))
        .expect("render todo list");
    assert!(text(terminal.backend().buffer()).contains("visible task 19"));
}

#[test]
fn inserting_a_joiner_does_not_redirect_backspace_into_json_syntax() {
    let request = kurama_protocol::policy::ApprovalRequest {
        operation_id: "edit".into(),
        operation: kurama_protocol::tool::Operation::Read {
            paths: vec![".".into()],
            external: false,
        },
        summary: "read".into(),
        arguments: serde_json::json!({}),
    };
    let mut approval = kurama_cli::tui::ApprovalState::new(request);
    approval.set_editor("{\"x\":\"\u{1f469}\u{1f467}\"}");
    approval.editor_cursor = "{\"x\":\"\u{1f469}".len();
    approval.insert_str("\u{200d}");
    approval.backspace();
    assert_eq!(approval.editor, "{\"x\":\"\"}");
}
