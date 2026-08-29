use kurama_cli::{
    app::App,
    commands::{Command, parse_command},
    tui::{AgentRow, Overlay, TuiState, render},
};
use kurama_protocol::{
    agent::AgentState,
    id::{AgentId, OperationId, SessionId},
    policy::{ApprovalRequest, ExecutionMode},
    tool::Operation,
};
use ratatui::{
    Terminal,
    backend::{Backend, TestBackend},
    buffer::Buffer,
    layout::Position,
    style::Color,
};

fn buffer_text(buffer: &Buffer) -> String {
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer.cell((x, y)).expect("cell").symbol());
        }
        text.push('\n');
    }
    text
}

fn rendered(state: &TuiState, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    terminal.backend().buffer().clone()
}

fn approval_request() -> ApprovalRequest {
    ApprovalRequest {
        operation_id: OperationId::from("o_1"),
        operation: Operation::Bash {
            command: "cargo test -p kurama-cli".into(),
            cwd: ".".into(),
            class: kurama_protocol::tool::CommandClass::ReadOnly,
            timeout_ms: 30_000,
        },
        summary: "Run the focused CLI tests".into(),
        arguments: serde_json::json!({"command":"cargo test -p kurama-cli"}),
    }
}

fn narrow_approval_request() -> ApprovalRequest {
    let mut request = approval_request();
    request.summary =
        "Run the focused CLI tests before accepting this narrow terminal operation".into();
    request
}

#[test]
fn parses_all_product_commands_without_restart() {
    assert_eq!(parse_command("/agents").unwrap(), Command::Agents);
    assert_eq!(
        parse_command("/model openai-main").unwrap(),
        Command::Model(Some("openai-main".into()))
    );
    assert_eq!(
        parse_command("/resume s_123").unwrap(),
        Command::Resume(SessionId::from("s_123"))
    );
    assert_eq!(
        parse_command("/mode auto").unwrap(),
        Command::Mode(ExecutionMode::Auto)
    );
    assert!(parse_command("/restart").is_err());
    assert!(parse_command("/mode yolo").is_err());
}

#[test]
fn main_screen_is_transcript_first_without_tool_statistics() {
    let mut state = TuiState::new(
        "openai-main",
        "gpt-5.6",
        "~/src/kurama",
        ExecutionMode::Supervised,
    );
    state.push_user("Use sub-agents to review the parser.");
    state.push_assistant("I’ll split this between an implementer and reviewer.");
    state.set_agent_counts(1, 1);

    for (width, height) in [(80, 24), (100, 30), (160, 50)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("KURAMA"));
        assert!(text.contains("SUPERVISED"));
        assert!(text.contains("agents 1 running · 1 queued"));
        assert!(!text.contains("tool calls"));
        assert!(!text.contains("tokens/sec"));
    }
}

#[test]
fn transcript_uses_compact_codex_style_hierarchy() {
    let mut state = TuiState::new(
        "openai-main",
        "gpt-5.6",
        "~/src/kurama",
        ExecutionMode::Supervised,
    );
    state.push_user("Review the parser.");
    state.push_assistant("I’ll inspect the parser and its focused tests.");
    state.push_tool("TOOL / bash", "cargo test -p kurama-cli");
    state.push_system("MODE", "supervised");

    let text = buffer_text(&rendered(&state, 100, 30));

    assert!(text.contains("› Review the parser."));
    assert!(text.contains("I’ll inspect the parser and its focused tests."));
    assert!(text.contains("• Ran bash"));
    assert!(text.contains("  └ cargo test -p kurama-cli"));
    assert!(text.contains("• MODE · supervised"));
    assert!(!text.contains("│ YOU"));
    assert!(!text.contains("│ KURAMA"));
    assert!(!text.contains("TOOL / bash  /"));
}

#[test]
fn tool_output_is_hard_wrapped_and_bounded_with_head_and_tail() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_tool(
        "TOOL / bash",
        concat!(
            "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ\n",
            "head-two\n",
            "head-three\n",
            "middle-four\n",
            "middle-five\n",
            "middle-six\n",
            "middle-seven\n",
            "middle-eight\n",
            "tail-nine\n",
            "tail-ten"
        ),
    );

    let text = buffer_text(&rendered(&state, 40, 30));

    assert!(text.contains("  └ abcdefghijklmnopqrstuvwxyz012345"));
    assert!(text.contains("    6789ABCDEFGHIJ"));
    assert!(text.contains("head-two"));
    assert!(text.contains("… 6 lines omitted …"));
    assert!(!text.contains("middle-four"));
    assert!(!text.contains("middle-eight"));
    assert!(text.contains("tail-nine"));
    assert!(text.contains("tail-ten"));
}

#[test]
fn approvals_render_inline_without_hiding_the_main_screen() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_user("Run the CLI tests.");
    state.begin_approval(approval_request());

    let pending = buffer_text(&rendered(&state, 100, 30));
    assert!(pending.contains("KURAMA"));
    assert!(pending.contains("› Run the CLI tests."));
    assert!(pending.contains("• Approval required"));
    assert!(pending.contains("Run the focused CLI tests"));
    assert!(pending.contains("a approve once · d deny · e edit"));
    assert!(!pending.contains("Message Kurama or type / for commands"));
    assert!(pending.contains("approval pending"));
    let pending_lines = pending.lines().collect::<Vec<_>>();
    let approval_line = pending_lines
        .iter()
        .position(|line| line.contains("• Approval required"))
        .expect("inline approval line");
    assert!(!pending_lines[approval_line.saturating_sub(1)].contains('┌'));

    state.begin_approval_edit();
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
    terminal.draw(|frame| render(frame, &state)).unwrap();
    let editing = buffer_text(terminal.backend().buffer());
    assert!(editing.contains("KURAMA"));
    assert!(editing.contains("› Run the CLI tests."));
    assert!(editing.contains("• Edit arguments"));
    assert!(editing.contains(r#""command": "cargo test -p kurama-cli""#));
    assert!(editing.contains("Enter submit · Esc return"));
    assert!(!editing.contains("Message Kurama or type / for commands"));
    assert!(format!("{:?}", terminal.backend()).contains("cursor: true"));
}

#[test]
fn narrow_pending_approval_keeps_all_controls_visible() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.begin_approval(narrow_approval_request());

    let text = buffer_text(&rendered(&state, 40, 24));
    let lines = text.lines().map(str::trim).collect::<Vec<_>>();

    assert!(text.contains("• Approval required"));
    assert!(text.contains("a approve once"));
    assert!(text.contains("d deny"));
    assert!(text.contains("e edit"));
    assert!(lines.contains(&"Run the focused CLI tests before"));
    assert!(lines.contains(&"accepting this narrow terminal"));
}

#[test]
fn narrow_approval_edit_cursor_follows_wrapped_context() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.begin_approval(narrow_approval_request());
    state.begin_approval_edit();

    let mut terminal = Terminal::new(TestBackend::new(40, 26)).unwrap();
    terminal.draw(|frame| render(frame, &state)).unwrap();
    let text = buffer_text(terminal.backend().buffer());
    let lines = text.lines().collect::<Vec<_>>();
    let editor_end = lines
        .iter()
        .position(|line| line.trim() == "}")
        .expect("last editor line remains visible");
    let controls = lines
        .iter()
        .position(|line| line.contains("Enter submit · Esc return"))
        .expect("edit controls remain visible");
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    let editor_end_column = lines[editor_end].find('}').unwrap() as u16 + 1;

    assert!(editor_end < controls);
    assert_eq!(cursor, Position::new(editor_end_column, editor_end as u16));
}

#[test]
fn every_tui_view_preserves_the_terminal_background() {
    let main = TuiState::new("work", "model", ".", ExecutionMode::Yolo);

    let mut approval = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    approval.begin_approval(approval_request());

    let mut agents = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    agents.set_agents(vec![AgentRow {
        id: AgentId::from("a_1"),
        role: "reviewer".into(),
        profile: "work".into(),
        task: "inspect rendering".into(),
        state: AgentState::Running,
        activity: "reading render.rs".into(),
        transcript: vec!["No opaque panels.".into()],
    }]);
    agents.open_agents();

    let mut inspect = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    inspect.set_agents(agents.agents.clone());
    inspect.open_agents();
    inspect.inspect_selected_agent();

    let onboarding = TuiState::onboarding(".");

    for state in [&main, &approval, &agents, &inspect, &onboarding] {
        let buffer = rendered(state, 100, 30);
        assert!(
            buffer.content().iter().all(|cell| cell.bg == Color::Reset),
            "view painted an opaque terminal background: {:?}",
            state.overlay
        );
    }
}

#[test]
fn composer_cursor_tracks_the_visual_insertion_point() {
    let mut state = TuiState::new(
        "openai-main",
        "gpt-5.6",
        "~/src/kurama",
        ExecutionMode::Supervised,
    );
    state.composer = "kurama".into();
    state.cursor = 2;

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &state)).unwrap();

    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    assert_eq!(cursor, Position::new(9, 21));
    assert_eq!(
        terminal.backend().buffer().cell(cursor).unwrap().symbol(),
        "r"
    );
    assert!(format!("{:?}", terminal.backend()).contains("cursor: true"));
}

#[test]
fn overlays_hide_the_composer_cursor() {
    let mut state = TuiState::new(
        "openai-main",
        "gpt-5.6",
        "~/src/kurama",
        ExecutionMode::Supervised,
    );
    state.composer = "kurama".into();
    state.cursor = state.composer.len();

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &state)).unwrap();
    assert!(format!("{:?}", terminal.backend()).contains("cursor: true"));

    state.overlay = Overlay::Agents;
    terminal.draw(|frame| render(frame, &state)).unwrap();
    assert!(format!("{:?}", terminal.backend()).contains("cursor: false"));
}

#[test]
fn standard_cli_registry_contains_exactly_four_tools() {
    assert_eq!(App::tool_names(), ["bash", "read", "web-search", "write"]);
}
