use kurama_cli::{
    app::App,
    commands::{Command, parse_command},
    tui::{
        AgentRow, Overlay, ToolLifecycle, ToolTranscript, TranscriptDetail, TranscriptEntry,
        TuiState, render, transcript_lines,
    },
};
use kurama_protocol::{
    agent::AgentState,
    id::{AgentId, CallId, OperationId, SessionId},
    policy::{ApprovalRequest, ExecutionMode},
    runtime::RuntimeEvent,
    tool::Operation,
};
use ratatui::{
    Terminal,
    backend::{Backend, TestBackend},
    buffer::{Buffer, Cell},
    layout::Position,
    style::{Color, Modifier},
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

fn plain(lines: Vec<ratatui::text::Line<'static>>) -> String {
    lines
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

fn cell_at_text<'a>(buffer: &'a Buffer, needle: &str) -> &'a Cell {
    for y in 0..buffer.area.height {
        let row = (0..buffer.area.width)
            .map(|x| buffer.cell((x, y)).expect("cell").symbol())
            .collect::<String>();
        if let Some(x) = row.find(needle) {
            return buffer.cell((x as u16, y)).expect("styled cell");
        }
    }
    panic!("rendered text did not contain {needle:?}");
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

    let buffer = rendered(&state, 100, 30);
    let text = buffer_text(&buffer);

    assert!(text.contains("› Review the parser."));
    assert_eq!(
        cell_at_text(&buffer, "› Review the parser.").fg,
        Color::Rgb(116, 177, 255)
    );
    assert!(text.contains("I’ll inspect the parser and its focused tests."));
    assert!(text.contains("• I’ll inspect the parser and its focused tests."));
    assert!(text.contains("└ Ran bash"));
    assert!(!text.contains("cargo test -p kurama-cli"));
    assert!(text.contains("• MODE · supervised"));
    assert!(!text.contains("│ YOU"));
    assert!(!text.contains("│ KURAMA"));
    assert!(!text.contains("TOOL / bash  /"));
}

#[test]
fn transcript_groups_turns_and_expands_complete_tool_output() {
    let entries = vec![
        TranscriptEntry::UserTurn {
            body: "inspect".into(),
        },
        TranscriptEntry::ToolCall(ToolTranscript {
            call_id: Some(CallId::from("call_1")),
            name: "bash".into(),
            output: "line one\nline two".into(),
            lifecycle: ToolLifecycle::Completed,
        }),
    ];

    let compact = plain(transcript_lines(&entries, 80, TranscriptDetail::Compact));
    let expanded = plain(transcript_lines(&entries, 80, TranscriptDetail::Expanded));

    assert!(compact.contains("› inspect"));
    assert!(compact.contains("└ Ran bash"));
    assert!(!compact.contains("line one"));
    assert!(!compact.contains("line two"));
    assert!(expanded.contains("line one"));
    assert!(expanded.contains("line two"));
}

#[test]
fn transcript_uses_codex_gutters_and_separates_user_turns() {
    let entries = vec![
        TranscriptEntry::UserTurn {
            body: "first".into(),
        },
        TranscriptEntry::AssistantMessage {
            body: "answer".into(),
        },
        TranscriptEntry::ToolCall(ToolTranscript {
            call_id: None,
            name: "read".into(),
            output: "hidden".into(),
            lifecycle: ToolLifecycle::Running,
        }),
        TranscriptEntry::Error {
            body: "broken".into(),
        },
        TranscriptEntry::UserTurn {
            body: "second".into(),
        },
    ];

    let text = plain(transcript_lines(&entries, 80, TranscriptDetail::Compact));
    let lines = text.lines().collect::<Vec<_>>();

    assert_eq!(lines[0], "› first");
    assert_eq!(lines[1], "• answer");
    assert_eq!(lines[2], "└ Running read");
    assert_eq!(lines[3], "Error: broken");
    assert_eq!(lines[4], "");
    assert_eq!(lines[5], "› second");
}

#[test]
fn transcript_tool_summaries_follow_lifecycle_without_truncating_expanded_output() {
    let output = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ\nline two";
    let entries = vec![
        TranscriptEntry::ToolCall(ToolTranscript {
            call_id: None,
            name: "bash".into(),
            output: output.into(),
            lifecycle: ToolLifecycle::Completed,
        }),
        TranscriptEntry::ToolCall(ToolTranscript {
            call_id: None,
            name: "write".into(),
            output: "permission denied".into(),
            lifecycle: ToolLifecycle::Failed,
        }),
    ];

    let compact = plain(transcript_lines(&entries, 20, TranscriptDetail::Compact));
    let expanded = plain(transcript_lines(&entries, 20, TranscriptDetail::Expanded));

    assert!(compact.contains("└ Ran bash"));
    assert!(compact.contains("└ write failed"));
    assert!(!compact.contains("permission denied"));
    assert!(expanded.contains("abcdefghijklmnopqr"));
    assert!(expanded.contains("stuvwxyz0123456789"));
    assert!(expanded.contains("ABCDEFGHIJ"));
    assert!(expanded.contains("line two"));
    assert!(expanded.contains("permission denied"));
    assert!(!expanded.contains('…'));
}

#[test]
fn typed_tool_transcript_preserves_normalized_tool_name() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_tool("TOOL / Bash", "done");

    let text = buffer_text(&rendered(&state, 80, 20));

    assert!(text.contains("Ran bash"));
    assert!(!text.contains("Ran Bash"));
}

#[test]
fn unlabeled_notice_renders_body_only_in_dim_text() {
    let entries = vec![TranscriptEntry::Notice {
        label: None,
        body: "runtime resumed".into(),
    }];

    let lines = transcript_lines(&entries, 80, TranscriptDetail::Compact);

    assert_eq!(plain(lines.clone()), "runtime resumed");
    assert!(
        lines[0]
            .spans
            .iter()
            .all(|span| span.style.fg == Some(Color::Rgb(126, 132, 146)))
    );
}

#[test]
fn labeled_notice_keeps_its_label_in_dim_text() {
    let entries = vec![TranscriptEntry::Notice {
        label: Some("MODE".into()),
        body: "supervised".into(),
    }];

    let lines = transcript_lines(&entries, 80, TranscriptDetail::Compact);

    assert_eq!(plain(lines.clone()), "• MODE · supervised");
    assert!(
        lines[0]
            .spans
            .iter()
            .all(|span| span.style.fg == Some(Color::Rgb(126, 132, 146)))
    );
}

#[test]
fn committed_transcript_is_not_redrawn_in_the_live_viewport() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_user("committed question");
    state.mark_transcript_committed(1);
    state.push_assistant("live answer");

    let text = buffer_text(&rendered(&state, 80, 20));

    assert!(!text.contains("committed question"));
    assert!(text.contains("live answer"));
}

#[test]
fn expanded_transcript_view_renders_committed_canonical_history() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_user("committed question");
    state.mark_transcript_committed(1);
    state.push_tool("TOOL / bash", "complete output");

    let compact = buffer_text(&rendered(&state, 80, 20));
    assert!(!compact.contains("committed question"));
    assert!(!compact.contains("complete output"));

    state.toggle_transcript_view();
    let expanded = buffer_text(&rendered(&state, 80, 20));
    assert!(expanded.contains("committed question"));
    assert!(expanded.contains("complete output"));
}

#[test]
fn runtime_errors_render_in_the_transcript_instead_of_the_status_line() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.apply_runtime_event(RuntimeEvent::Error {
        message: "protocol error: malformed bridge output".into(),
    });

    let buffer = rendered(&state, 100, 20);
    let text = buffer_text(&buffer);

    assert!(text.contains("Error: protocol error: malformed bridge output"));
    assert!(
        text.lines()
            .any(|line| line.contains("work/model") && line.contains("ready"))
    );
    assert_eq!(cell_at_text(&buffer, "Error").fg, Color::Rgb(255, 92, 82));
}

#[test]
fn assistant_markdown_renders_inline_styles_and_links() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_assistant(concat!(
        "# Release\n\n",
        "Use **bold**, *italic*, ~~obsolete~~, `cargo test`, and ",
        "[docs](https://example.com)."
    ));

    let buffer = rendered(&state, 100, 24);
    let text = buffer_text(&buffer);

    assert!(text.contains("Release"));
    assert!(
        text.contains("Use bold, italic, obsolete, cargo test, and docs (https://example.com).")
    );
    assert!(!text.contains("# Release"));
    assert!(!text.contains("**bold**"));
    assert!(!text.contains("*italic*"));
    assert!(!text.contains("~~obsolete~~"));
    assert!(!text.contains("`cargo test`"));
    assert!(!text.contains("[docs](https://example.com)"));
    assert!(
        cell_at_text(&buffer, "Release")
            .modifier
            .contains(Modifier::BOLD)
    );
    assert!(
        cell_at_text(&buffer, "bold")
            .modifier
            .contains(Modifier::BOLD)
    );
    assert!(
        cell_at_text(&buffer, "italic")
            .modifier
            .contains(Modifier::ITALIC)
    );
    assert!(
        cell_at_text(&buffer, "obsolete")
            .modifier
            .contains(Modifier::CROSSED_OUT)
    );
    let inline_code = cell_at_text(&buffer, "cargo test");
    assert_eq!(inline_code.fg, Color::Rgb(224, 226, 232));
    assert!(inline_code.modifier.contains(Modifier::BOLD));
    let link = cell_at_text(&buffer, "docs");
    assert_eq!(link.fg, Color::Rgb(116, 177, 255));
    assert!(link.modifier.contains(Modifier::UNDERLINED));
}

#[test]
fn assistant_markdown_renders_blocks_lists_code_quotes_rules_and_tables() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_assistant(concat!(
        "## Plan\n\n",
        "> Quote with **weight**.\n\n",
        "- alpha\n",
        "- beta\n\n",
        "3. third\n",
        "4. fourth\n\n",
        "---\n\n",
        "```rust\n",
        "fn main() {\n",
        "    println!(\"hi\");\n",
        "}\n",
        "```\n\n",
        "| Name | State |\n",
        "| :--- | ---: |\n",
        "| parser | ready |"
    ));

    let buffer = rendered(&state, 120, 40);
    let text = buffer_text(&buffer);

    assert!(text.contains("Plan"));
    assert!(text.contains("│ Quote with weight."));
    assert!(text.contains("• alpha"));
    assert!(text.contains("• beta"));
    assert!(text.contains("3. third"));
    assert!(text.contains("4. fourth"));
    assert!(text.contains("────────────────"));
    assert!(text.contains("rust"));
    assert!(text.contains("│ fn main() {"));
    assert!(text.contains("│     println!(\"hi\");"));
    assert!(text.contains("Name   │ State"));
    assert!(text.contains("parser │ ready"));
    assert!(!text.contains("## Plan"));
    assert!(!text.contains("> Quote"));
    assert!(!text.contains("- alpha"));
    assert!(!text.contains("```"));
    assert!(!text.contains("| :--- | ---: |"));
    assert!(
        cell_at_text(&buffer, "Plan")
            .modifier
            .contains(Modifier::BOLD)
    );
    assert!(
        cell_at_text(&buffer, "weight")
            .modifier
            .contains(Modifier::BOLD)
    );
    assert_eq!(
        cell_at_text(&buffer, "fn main()").fg,
        Color::Rgb(220, 178, 73)
    );
    assert!(
        cell_at_text(&buffer, "Name")
            .modifier
            .contains(Modifier::BOLD)
    );
}

#[test]
fn assistant_markdown_preserves_viewport_wrapping_and_style() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_assistant("**abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ**");

    let buffer = rendered(&state, 40, 20);
    let text = buffer_text(&buffer);

    let rows = text.lines().map(str::trim_end).collect::<Vec<_>>();
    assert!(
        rows.contains(&"  • abcdefghijklmnopqrstuvwxyz01234567"),
        "{rows:#?}"
    );
    assert!(rows.contains(&"    89ABCDEFGHIJ"), "{rows:#?}");
    assert!(!rows.iter().any(|row| row.contains("• 89ABCDEFGHIJ")));
    assert!(!text.contains("**"));
    assert!(
        cell_at_text(&buffer, "abcdefghijklmnopqrstuvwxyz")
            .modifier
            .contains(Modifier::BOLD)
    );
    assert!(
        cell_at_text(&buffer, "ABCDEFGHIJ")
            .modifier
            .contains(Modifier::BOLD)
    );
}

#[test]
fn assistant_markdown_wraps_words_without_orphan_punctuation() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_assistant("1234567890 **hello**.");

    let buffer = rendered(&state, 20, 16);
    let text = buffer_text(&buffer);
    let lines = text.lines().map(str::trim).collect::<Vec<_>>();

    assert!(lines.contains(&"• 1234567890"));
    assert!(lines.contains(&"hello."));
    assert!(!lines.contains(&"."));
    assert!(
        cell_at_text(&buffer, "hello")
            .modifier
            .contains(Modifier::BOLD)
    );
}

#[test]
fn narrow_markdown_tables_render_as_stacked_records() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_assistant(concat!(
        "| Field | Value |\n",
        "| --- | --- |\n",
        "| command | cargo test --workspace --all-features |"
    ));

    let text = buffer_text(&rendered(&state, 40, 20));

    assert!(text.contains("Field: command"));
    assert!(text.contains("Value: cargo test --workspace"));
    assert!(text.contains("--all-features"));
    assert!(!text.contains('…'));
}

#[test]
fn stacked_tables_keep_values_visible_after_long_headers() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_assistant(concat!(
        "| ExtremelyLongHeader |\n",
        "| --- |\n",
        "| visible-value |"
    ));

    let text = buffer_text(&rendered(&state, 16, 20));
    let compact = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();

    assert!(compact.contains("ExtremelyLongHeader:visible-value"));
}

#[test]
fn tables_stack_when_separators_do_not_fit() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_assistant("| A | B |\n| --- | --- |\n| x | y |");

    let text = buffer_text(&rendered(&state, 8, 16));

    let compact = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    assert!(compact.contains("A:x"), "{text:?}\n{compact:?}");
    assert!(compact.contains("B:y"), "{text:?}\n{compact:?}");
}

#[test]
fn stacked_table_headers_preserve_inline_markdown_styles() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_assistant(concat!(
        "| *Field* | Value |\n",
        "| --- | --- |\n",
        "| command | cargo test --workspace --all-features |"
    ));

    let buffer = rendered(&state, 40, 20);

    assert!(
        cell_at_text(&buffer, "Field")
            .modifier
            .contains(Modifier::ITALIC)
    );
}

#[test]
fn transcript_scroll_reaches_visual_lines_beyond_u16_max() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    let output = (0..65_550)
        .map(|index| format!("line-{index:05}"))
        .collect::<Vec<_>>()
        .join("\n");
    state.push_tool("TOOL / bash", output);
    state.toggle_transcript_view();
    state.scroll = usize::from(u16::MAX) + 5;

    let text = buffer_text(&rendered(&state, 40, 12));

    assert!((0..20).any(|index| text.contains(&format!("line-{index:05}"))));
    assert!(!text.contains("line-65539"));
}

#[test]
fn transcript_follows_the_latest_answer_after_long_tool_output() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    let output = (0..100)
        .map(|index| format!("line-{index:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    state.push_tool("TOOL / bash", output);
    state.push_assistant("**complete answer**");

    let text = buffer_text(&rendered(&state, 40, 12));

    assert!(text.contains("complete answer"));
    assert!(!text.contains("line-000"));
}

#[test]
fn tool_output_wraps_on_unicode_grapheme_clusters() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_tool("TOOL / bash", "AB👨‍👩‍👧‍👦CD");
    state.toggle_transcript_view();

    let text = buffer_text(&rendered(&state, 10, 16));
    let rows = text.lines().map(str::trim_end).collect::<Vec<_>>();

    assert!(rows.contains(&"    AB👨‍👩‍👧‍👦"), "{rows:#?}");
    assert!(rows.contains(&"    CD"), "{rows:#?}");
}

#[test]
fn table_cells_preserve_inline_markdown_styles() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_assistant(concat!(
        "| Field | Value |\n",
        "| --- | --- |\n",
        "| **status** | `ready` and [docs](https://example.com) |"
    ));

    let buffer = rendered(&state, 100, 20);

    assert!(
        cell_at_text(&buffer, "status")
            .modifier
            .contains(Modifier::BOLD)
    );
    let inline_code = cell_at_text(&buffer, "ready");
    assert_eq!(inline_code.fg, Color::Rgb(224, 226, 232));
    assert!(inline_code.modifier.contains(Modifier::BOLD));
    let link = cell_at_text(&buffer, "docs");
    assert_eq!(link.fg, Color::Rgb(116, 177, 255));
    assert!(link.modifier.contains(Modifier::UNDERLINED));
}

#[test]
fn narrow_tables_stack_without_truncating_grapheme_clusters() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_assistant(concat!(
        "| A | B |\n",
        "| --- | --- |\n",
        "| x | 👨‍👩‍👧‍👦abcdefghijk |"
    ));

    let text = buffer_text(&rendered(&state, 14, 20));
    let compact = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();

    assert!(text.contains("A: x"));
    assert!(text.lines().any(|line| line.contains("B: 👨‍👩‍👧‍👦")));
    assert!(compact.contains("B:👨‍👩‍👧‍👦abcdefghijk"));
    assert!(!text.contains('…'));
}

#[test]
fn tool_output_preserves_every_wrapped_line() {
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
    state.toggle_transcript_view();

    let text = buffer_text(&rendered(&state, 40, 30));

    assert!(text.contains("  abcdefghijklmnopqrstuvwxyz01234567"));
    assert!(text.contains("  89ABCDEFGHIJ"));
    assert!(text.contains("head-two"));
    assert!(text.contains("middle-four"));
    assert!(text.contains("middle-five"));
    assert!(text.contains("middle-six"));
    assert!(text.contains("middle-seven"));
    assert!(text.contains("middle-eight"));
    assert!(text.contains("tail-nine"));
    assert!(text.contains("tail-ten"));
    assert!(!text.contains("lines omitted"));
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
fn every_tui_view_uses_a_readable_dark_surface() {
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
            buffer
                .content()
                .iter()
                .all(|cell| cell.bg == Color::Rgb(13, 16, 22)),
            "view left inconsistent background cells: {:?}",
            state.overlay
        );
    }
}

#[test]
fn onboarding_keeps_the_last_connection_option_visible() {
    let mut state = TuiState::onboarding(".");
    for _ in 0..4 {
        state.onboarding.select_next();
    }

    let text = buffer_text(&rendered(&state, 100, 24));

    assert!(text.contains("OpenAI-compatible or local endpoint"));
    assert!(text.contains("Connect to an existing HTTP endpoint"));
    assert!(text.contains("SELECTED"));
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
