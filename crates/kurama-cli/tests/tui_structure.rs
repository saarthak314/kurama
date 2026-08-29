use kurama_cli::{
    commands::{Command, parse_command},
    tui::{TuiState, render},
};
use kurama_protocol::{id::SessionId, policy::ExecutionMode};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

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

    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| render(frame, &state)).unwrap();
    let text = buffer_text(terminal.backend().buffer());
    assert!(text.contains("KURAMA"));
    assert!(text.contains("SUPERVISED"));
    assert!(text.contains("agents 1 running · 1 queued"));
    assert!(!text.contains("tool calls"));
    assert!(!text.contains("tokens/sec"));
}
