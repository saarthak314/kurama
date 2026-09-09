use kurama_cli::{
    app::App,
    commands::{Command, parse_command},
    tui::{Overlay, TranscriptDetail, TranscriptEntry, TuiState, render, transcript_lines},
};
use kurama_protocol::{
    id::{CallId, OperationId, SessionId},
    policy::ExecutionMode,
    runtime::RuntimeEvent,
    session::{EventEnvelope, SessionEvent, TodoItem, TodoStatus},
    tool::ToolResult,
};
use ratatui::{Terminal, backend::TestBackend};

fn item(id: &str, content: &str, status: TodoStatus) -> TodoItem {
    TodoItem {
        id: id.into(),
        content: content.into(),
        status,
    }
}

fn replay_event(sequence: u64, event: SessionEvent) -> EventEnvelope {
    EventEnvelope::new(
        sequence,
        sequence,
        SessionId::from("session_todos"),
        None,
        event,
    )
}

fn rendered(state: &TuiState, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn plain_transcript(entries: &[TranscriptEntry]) -> String {
    transcript_lines(entries, 100, TranscriptDetail::Compact)
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn parses_todo_command_and_preserves_environment_tool_registry() {
    assert_eq!(parse_command("/todo").unwrap(), Command::Todo);
    assert!(parse_command("/todo extra").is_err());
    assert_eq!(App::tool_names(), ["bash", "read", "web-search", "write"]);
}

#[test]
fn replay_hydrates_only_the_latest_todo_list() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.hydrate_replay(&[
        replay_event(
            1,
            SessionEvent::TodoUpdated {
                items: vec![item("old", "old task", TodoStatus::Pending)],
            },
        ),
        replay_event(
            2,
            SessionEvent::TodoUpdated {
                items: vec![
                    item("done", "inspect parser", TodoStatus::Completed),
                    item("next", "implement fix", TodoStatus::InProgress),
                ],
            },
        ),
    ]);

    assert_eq!(state.todos.len(), 2);
    assert_eq!(state.todos[1].content, "implement fix");
    assert_eq!(
        state
            .transcript
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::Todos { .. }))
            .count(),
        1
    );
    assert_eq!(
        plain_transcript(&state.transcript),
        "• todo\n  [x] inspect parser\n  [>] implement fix"
    );
    assert!(!plain_transcript(&state.transcript).contains("Ran todo"));
}

#[test]
fn todo_overlay_is_compact_and_renders_item_content() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.todos = vec![
        item("next", "implement fix", TodoStatus::InProgress),
        item("later", "write tests", TodoStatus::Pending),
    ];
    state.open_todos();

    let text = rendered(&state, 80, 20);
    assert_eq!(state.overlay(), Overlay::Todos);
    assert!(text.contains("/TODO"));
    assert!(text.contains("implement fix"));
    assert!(text.contains("in progress"));
    assert!(text.contains("esc  close"));
}

#[test]
fn live_todo_tool_completion_updates_state_and_transcript() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    let items = vec![item("one", "ship the fix", TodoStatus::Pending)];
    state.apply_runtime_event(RuntimeEvent::ToolCompleted {
        operation_id: OperationId::from("operation_todo"),
        result: ToolResult {
            call_id: CallId::from("call_todo"),
            output: "pending  ship the fix".into(),
            is_error: false,
            metadata: serde_json::json!({"tool_name": "todo", "items": items}),
            truncated: false,
            blob_refs: Vec::new(),
        },
    });

    assert_eq!(state.todos, items);
    assert!(matches!(
        state.transcript.last(),
        Some(TranscriptEntry::Todos { items }) if items[0].content == "ship the fix"
    ));
    assert_eq!(
        plain_transcript(&state.transcript),
        "• todo\n  [ ] ship the fix"
    );
    assert!(
        state
            .transcript
            .iter()
            .all(|entry| !matches!(entry, TranscriptEntry::ToolCall(_)))
    );

    let updated = vec![
        item("one", "ship the fix", TodoStatus::Completed),
        item("two", "write tests", TodoStatus::InProgress),
    ];
    state.apply_runtime_event(RuntimeEvent::ToolCompleted {
        operation_id: OperationId::from("operation_todo_2"),
        result: ToolResult {
            call_id: CallId::from("call_todo_2"),
            output: "updated".into(),
            is_error: false,
            metadata: serde_json::json!({"tool_name": "todo", "items": updated}),
            truncated: false,
            blob_refs: Vec::new(),
        },
    });

    assert_eq!(
        state
            .transcript
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::Todos { .. }))
            .count(),
        1
    );
    assert_eq!(
        plain_transcript(&state.transcript),
        "• todo\n  [x] ship the fix\n  [>] write tests"
    );
}
