use kurama_cli::{
    app::App,
    commands::{Command, GoalAction, parse_command},
    tui::{
        ActivityState, AgentRow, Overlay, ResponsiveLayout, ToolLifecycle, ToolTranscript,
        TranscriptDetail, TranscriptEntry, TuiState, activity_line, render, transcript_lines,
    },
};
use kurama_protocol::{
    agent::AgentState,
    id::{AgentId, CallId, OperationId, SessionId},
    model::Usage,
    policy::{ApprovalRequest, ExecutionMode},
    runtime::RuntimeEvent,
    session::{EventEnvelope, SessionEvent},
    tool::Operation,
};
use ratatui::{
    Terminal,
    backend::{Backend, TestBackend},
    buffer::{Buffer, Cell},
    layout::Rect,
    style::{Color, Modifier},
    text::{Line, Span},
};
use std::time::{Duration, Instant};

fn visible_row(buffer: &Buffer, y: u16) -> Vec<(u16, &Cell)> {
    let mut cells = Vec::new();
    let mut x = 0;
    while x < buffer.area.width {
        let cell = buffer.cell((x, y)).expect("cell");
        cells.push((x, cell));
        x += Span::raw(cell.symbol()).width().max(1) as u16;
    }
    cells
}

fn buffer_text(buffer: &Buffer) -> String {
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for (_, cell) in visible_row(buffer, y) {
            text.push_str(cell.symbol());
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

#[test]
fn startup_banner_left_truncates_long_paths_on_narrow_terminals() {
    let lines = transcript_lines(
        &[TranscriptEntry::Startup {
            version: "0.1.0".into(),
            model: "fixture-model".into(),
            project: "~/src/harness-eng/coding-agent-with-subagents".into(),
            mode: ExecutionMode::Supervised,
        }],
        28,
        TranscriptDetail::Compact,
    );
    let text = plain(lines.clone());

    assert!(lines.iter().all(|line| line.width() <= 28));
    assert!(text.contains("subagents"), "{text}");
    assert!(!text.contains("~/src"), "{text}");
}

#[test]
fn startup_banner_sits_above_the_onboarding_prompt() {
    let mut state = TuiState::onboarding("~/src/kurama");
    state.prepend_startup("0.1.0", "~/src/kurama");

    let buffer = rendered(&state, 80, 24);
    let rows = buffer_text(&buffer)
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let banner_row = rows
        .iter()
        .position(|row| row.contains("0.1.0"))
        .expect("startup banner");
    let prompt_row = rows
        .iter()
        .position(|row| row.contains(state.onboarding.options()[0]))
        .expect("onboarding prompt");

    assert!(banner_row < prompt_row, "{rows:#?}");
}

fn cell_at_text<'a>(buffer: &'a Buffer, needle: &str) -> &'a Cell {
    for y in 0..buffer.area.height {
        let cells = visible_row(buffer, y);
        let row = cells
            .iter()
            .map(|(_, cell)| cell.symbol())
            .collect::<String>();
        if let Some(byte_offset) = row.find(needle) {
            let mut consumed = 0;
            for (_, cell) in cells {
                let end = consumed + cell.symbol().len();
                if (consumed..end).contains(&byte_offset) {
                    return cell;
                }
                consumed = end;
            }
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
        parse_command("/resume ses_deadbeef").unwrap(),
        Command::Resume(SessionId::from("ses_deadbeef"))
    );
    assert_eq!(
        parse_command("/mode auto").unwrap(),
        Command::Mode(ExecutionMode::Auto)
    );
    assert_eq!(parse_command("/help").unwrap(), Command::Help);
    assert_eq!(parse_command("/copy").unwrap(), Command::Copy);
    assert_eq!(parse_command("/diff").unwrap(), Command::Diff);
    assert_eq!(parse_command("/status").unwrap(), Command::Status);
    assert_eq!(
        parse_command("/goal").unwrap(),
        Command::Goal(GoalAction::View)
    );
    assert_eq!(
        parse_command("/goal keep tests green").unwrap(),
        Command::Goal(GoalAction::Set("keep tests green".into()))
    );
    assert_eq!(
        parse_command("/goal set ship the e2e pass").unwrap(),
        Command::Goal(GoalAction::Set("ship the e2e pass".into()))
    );
    assert_eq!(
        parse_command("/goal view").unwrap(),
        Command::Goal(GoalAction::View)
    );
    assert_eq!(
        parse_command("/goal pause").unwrap(),
        Command::Goal(GoalAction::Pause)
    );
    assert!(parse_command("/goal set").is_err());
    assert_eq!(parse_command("/exit").unwrap(), Command::Exit);
    assert!(parse_command("/restart").is_err());
    assert!(parse_command("/mode yolo").is_err());
}

#[test]
fn activity_row_visibility_follows_layout_geometry() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.set_thinking();

    for height in [0, 1] {
        assert!(!buffer_text(&rendered(&state, 80, height)).contains("Thinking"));
    }

    assert!(buffer_text(&rendered(&state, 80, 6)).contains("Thinking"));
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
    state.composer = "draft".into();

    for (width, height) in [(80, 24), (100, 30), (160, 50)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Use sub-agents to review the parser."));
        assert!(text.contains("I’ll split this between an implementer and reviewer."));
        assert!(!text.contains("KURAMA"));
        assert!(
            text.contains("supervised")
                || text.contains("enter send")
                || text.contains("shortcuts")
        );
        assert!(!text.contains("tool calls"));
        assert!(!text.contains("tokens/sec"));
    }
}

#[test]
fn slash_palette_filters_commands_and_keeps_descriptions_readable() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.composer = "/res".into();
    state.cursor = state.composer.len();

    let wide = buffer_text(&rendered(&state, 80, 24));
    assert!(wide.contains("/resume"));
    assert!(wide.contains("resume a saved session"));
    assert!(!wide.contains("select or list profiles"));

    let narrow = rendered(&state, 24, 10);
    let narrow_text = buffer_text(&narrow);
    assert!(narrow_text.contains("/resume"));
    assert!(narrow.content().iter().all(|cell| cell.bg == Color::Reset));
}

#[test]
fn responsive_layout_regions_stay_inside_the_requested_area() {
    for (area, input_height, activity_visible) in [
        (Rect::new(3, 5, 120, 32), 2, false),
        (Rect::new(3, 5, 80, 24), 3, true),
        (Rect::new(3, 5, 48, 16), 5, true),
        (Rect::new(3, 5, 32, 10), 6, false),
        (Rect::new(3, 5, 0, 0), 6, true),
        (Rect::new(3, 5, 1, 1), 6, true),
    ] {
        let layout = ResponsiveLayout::for_area(area, input_height, activity_visible, 0);
        let regions = [
            layout.transcript,
            layout.activity,
            layout.queue,
            layout.input,
            layout.footer,
        ];

        for region in regions {
            assert!(region.x >= area.x);
            assert!(region.y >= area.y);
            assert!(region.right() <= area.right());
            assert!(region.bottom() <= area.bottom());
        }
        assert!(layout.transcript.bottom() <= layout.activity.y || layout.activity.is_empty());
        assert!(layout.activity.bottom() <= layout.queue.y || layout.activity.is_empty());
        assert!(layout.queue.bottom() <= layout.input.y || layout.queue.is_empty());
        assert!(layout.transcript.bottom() <= layout.input.y);
        assert!(layout.input.bottom() <= layout.footer.y || layout.footer.is_empty());
    }
}

#[test]
fn tiny_width_growth_never_reduces_visible_composer_content() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.composer = "abcd".into();
    state.cursor = state.composer.len();

    let visible_letters = |width| {
        let text = buffer_text(&rendered(&state, width, 4));
        ['a', 'b', 'c', 'd']
            .into_iter()
            .filter(|character| text.contains(*character))
            .count()
    };

    assert!(visible_letters(5) >= visible_letters(4));
}

#[test]
fn activity_line_formats_elapsed_time_and_measures_the_interrupt_hint() {
    let now = Instant::now();
    let seconds = ActivityState::Thinking {
        started_at: now - Duration::from_secs(59),
    };
    let minutes = ActivityState::Working {
        label: "tests".into(),
        started_at: now - Duration::from_secs(60),
    };
    let hours = ActivityState::RunningTool {
        name: "cargo test".into(),
        started_at: now - Duration::from_secs(3_600),
    };

    let seconds = plain(vec![
        activity_line(&seconds, 80, now).expect("thinking line"),
    ]);
    let minutes = plain(vec![
        activity_line(&minutes, 80, now).expect("working line"),
    ]);
    let hours = plain(vec![activity_line(&hours, 80, now).expect("tool line")]);
    let narrow = plain(vec![
        activity_line(&hours_state(now), 24, now).expect("narrow activity line"),
    ]);

    assert_eq!(seconds, "⠋ Thinking (59s • esc to interrupt)");
    assert_eq!(minutes, "⠋ Working tests (1m 00s • esc to interrupt)");
    assert_eq!(
        hours,
        "⠋ Running cargo test (1h 00m 00s • esc to interrupt)"
    );
    assert!(!narrow.contains("esc to interrupt"));
    assert!(activity_line(&ActivityState::Idle, 80, now).is_none());
    assert!(activity_line(&ActivityState::AwaitingApproval, 80, now).is_none());
}

#[test]
fn completed_answer_precedes_composer_and_safety_footer() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.submit_turn("show the result", false);
    state.push_assistant("Finished cleanly.");
    state.apply_runtime_event(RuntimeEvent::TurnCompleted);

    let text = buffer_text(&rendered(&state, 80, 16));
    let rows = text.lines().collect::<Vec<_>>();
    let answer_row = rows
        .iter()
        .position(|row| row.contains("Finished cleanly."))
        .expect("assistant answer");
    let composer_row = rows
        .iter()
        .position(|row| row.contains(state.composer_placeholder()))
        .expect("composer");
    let footer_row = rows
        .iter()
        .position(|row| row.contains("supervised"))
        .expect("footer");

    assert!(composer_row > answer_row, "{rows:#?}");
    assert!(footer_row > composer_row, "{rows:#?}");
    assert_eq!(composer_row, 12, "{rows:#?}");
    assert_eq!(footer_row, 15, "{rows:#?}");
}

#[test]
fn composer_and_footer_stay_bottom_anchored_after_resize() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.composer = "draft".into();
    state.cursor = state.composer.len();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

    for (width, height) in [(80, 24), (40, 10), (100, 30)] {
        terminal.backend_mut().resize(width, height);
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        let rows = text.lines().collect::<Vec<_>>();

        assert!(rows[usize::from(height) - 4].contains("draft"), "{text}");
        assert!(
            rows[usize::from(height) - 1].contains("supervised"),
            "{text}"
        );
    }
}

#[test]
fn dismissing_overlays_restores_the_entire_main_screen_without_stale_cells() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_user("keep this conversation visible");
    state.composer = "draft".into();
    state.cursor = state.composer.len();
    state.set_agents(vec![AgentRow {
        id: AgentId::from("a_1"),
        role: "reviewer".into(),
        profile: "work".into(),
        task: "overlay-only task text".into(),
        state: AgentState::Running,
        activity: "reading".into(),
        transcript: Vec::new(),
    }]);
    let expected = rendered(&state, 80, 24);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &state)).unwrap();

    for overlay in [
        Overlay::Shortcuts,
        Overlay::Agents,
        Overlay::Todos,
        Overlay::Onboarding,
    ] {
        state.overlay = overlay;
        terminal.draw(|frame| render(frame, &state)).unwrap();
        assert_ne!(terminal.backend().buffer(), &expected, "{overlay:?}");

        state.close_overlay();
        terminal.draw(|frame| render(frame, &state)).unwrap();

        assert_eq!(
            terminal.backend().buffer(),
            &expected,
            "stale cells after {overlay:?}"
        );
    }
}

#[test]
fn activity_spinner_advances_between_frame_ticks() {
    let started_at = Instant::now();
    let activity = ActivityState::Thinking { started_at };
    let first = plain(vec![
        activity_line(&activity, 80, started_at).expect("first spinner frame"),
    ]);
    let second = plain(vec![
        activity_line(&activity, 80, started_at + Duration::from_millis(100))
            .expect("second spinner frame"),
    ]);

    assert_ne!(first, second);
}

#[test]
fn narrow_activity_drops_elapsed_before_the_active_tool_name() {
    let now = Instant::now();
    let activity = ActivityState::RunningTool {
        name: "cargo test".into(),
        started_at: now - Duration::from_secs(3_600),
    };

    let line = activity_line(&activity, 24, now).expect("activity line");
    let text = plain(vec![line.clone()]);

    assert!(text.contains("Running cargo test"));
    assert!(!text.contains("1h 00m 00s"));
    assert!(line.width() <= 24);
}

fn hours_state(now: Instant) -> ActivityState {
    ActivityState::RunningTool {
        name: "cargo test with a deliberately long Unicode label 界".into(),
        started_at: now - Duration::from_secs(3_600),
    }
}

#[test]
fn footer_keeps_the_safety_mode_when_space_is_limited() {
    let mut state = TuiState::new("work", "model", "/a/long/project/path", ExecutionMode::Yolo);
    state.composer = "draft".into();
    for (width, height) in [(120, 32), (48, 16), (32, 10), (12, 6)] {
        let text = buffer_text(&rendered(&state, width, height));
        assert!(
            text.contains("yolo"),
            "safety mode hidden at {width}x{height}: {text}"
        );
    }
}

#[test]
fn queued_follow_ups_are_visible_without_becoming_fake_user_turns() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.set_thinking();
    state.submit_turn("check the failing test", false);

    let text = buffer_text(&rendered(&state, 80, 12));

    assert!(text.contains("queued  check the failing test"));
    assert!(!text.contains("> check the failing test"));
}

#[test]
fn question_mark_opens_a_shortcuts_overlay() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.open_shortcuts();

    let text = buffer_text(&rendered(&state, 80, 24));

    assert!(text.contains("shortcuts"));
    assert!(text.contains("ctrl+c"));
    assert!(text.contains("ctrl+j"));
    assert!(text.contains("ctrl+t"));
    assert!(text.contains("ctrl+r"));
    assert!(text.contains("close overlay") || text.contains("interrupt"));
}

#[test]
fn live_usage_replaces_the_footer_fraction_instead_of_summing() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.max_input_tokens = 100_000;
    state.composer = "draft".into();

    state.apply_runtime_event(RuntimeEvent::Usage {
        usage: Usage {
            input_tokens: 10_000,
            output_tokens: 1,
            cached_input_tokens: 0,
        },
    });
    assert!(buffer_text(&rendered(&state, 80, 12)).contains("90%"));

    state.apply_runtime_event(RuntimeEvent::Usage {
        usage: Usage {
            input_tokens: 25_000,
            output_tokens: 2,
            cached_input_tokens: 0,
        },
    });
    let text = buffer_text(&rendered(&state, 80, 12));
    assert!(text.contains("75%"));
    assert!(!text.contains("65%"));
}

#[test]
fn replay_usage_keeps_the_latest_window_fill() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.max_input_tokens = 100_000;
    state.composer = "draft".into();
    state.hydrate_replay(&[
        EventEnvelope::new(
            1,
            1,
            SessionId::from("session_1"),
            None,
            SessionEvent::ModelUsage {
                usage: Usage {
                    input_tokens: 10_000,
                    output_tokens: 1,
                    cached_input_tokens: 0,
                },
            },
        ),
        EventEnvelope::new(
            2,
            2,
            SessionId::from("session_1"),
            None,
            SessionEvent::ModelUsage {
                usage: Usage {
                    input_tokens: 40_000,
                    output_tokens: 2,
                    cached_input_tokens: 0,
                },
            },
        ),
    ]);

    let text = buffer_text(&rendered(&state, 80, 12));
    assert!(text.contains("60%"));
    assert!(!text.contains("50%"));
}

#[test]
fn compact_tool_rows_preview_output_while_expanded_preserves_it() {
    let entries = vec![
        TranscriptEntry::UserTurn {
            body: "inspect".into(),
        },
        TranscriptEntry::ToolCall(ToolTranscript {
            call_id: Some(CallId::from("call_1")),
            name: "bash".into(),
            context: None,
            output: "line one\nline two".into(),
            lifecycle: ToolLifecycle::Completed,
        }),
    ];

    let compact = plain(transcript_lines(&entries, 80, TranscriptDetail::Compact));
    let expanded = plain(transcript_lines(&entries, 80, TranscriptDetail::Expanded));

    assert!(compact.contains("line one"));
    assert!(compact.contains("line two"));
    assert!(expanded.contains("line one"));
    assert!(expanded.contains("line two"));
}

#[test]
fn transcript_squeezes_blank_padding_between_tool_rows() {
    let entries = vec![
        TranscriptEntry::UserTurn {
            body: "inspect".into(),
        },
        TranscriptEntry::AssistantMessage {
            body: "\n\n \n\nlooking".into(),
        },
        TranscriptEntry::ToolCall(ToolTranscript {
            call_id: Some(CallId::from("call_1")),
            name: "bash".into(),
            context: Some("ls".into()),
            output: (0..40)
                .map(|index| format!("file-{index}"))
                .collect::<Vec<_>>()
                .join("\n\n\n"),
            lifecycle: ToolLifecycle::Completed,
        }),
        TranscriptEntry::ToolCall(ToolTranscript {
            call_id: Some(CallId::from("call_2")),
            name: "read".into(),
            context: Some("README.md".into()),
            output: "title\n\n\n\n\nbody".into(),
            lifecycle: ToolLifecycle::Running,
        }),
    ];
    let text = plain(transcript_lines(&entries, 80, TranscriptDetail::Compact));
    let blank_run = text
        .lines()
        .map(|line| line.trim().is_empty())
        .collect::<Vec<_>>()
        .windows(3)
        .any(|window| window.iter().all(|blank| *blank));
    assert!(!blank_run, "{text}");
}

#[test]
fn narrow_user_prompt_keeps_punctuation_attached_and_continuation_aligned() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_user(
        "Fix prompt wrapping; preserve its behavior, and keep the continuation gutter aligned.",
    );

    let text = buffer_text(&rendered(&state, 48, 16));
    let rows = text.lines().map(str::trim_end).collect::<Vec<_>>();

    assert!(
        rows.contains(&"  > Fix prompt wrapping; preserve its"),
        "{rows:#?}"
    );
    assert!(
        rows.contains(&"    behavior, and keep the continuation gutter"),
        "{rows:#?}"
    );
    assert!(rows.contains(&"    aligned."), "{rows:#?}");
    assert!(!rows.iter().any(|row| row.trim_start().starts_with(',')));
}

#[test]
fn transcript_tool_summaries_follow_lifecycle_without_truncating_expanded_output() {
    let output = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ\nline two";
    let entries = vec![
        TranscriptEntry::ToolCall(ToolTranscript {
            call_id: None,
            name: "bash".into(),
            context: Some("cargo test -p kurama-cli".into()),
            output: output.into(),
            lifecycle: ToolLifecycle::Completed,
        }),
        TranscriptEntry::ToolCall(ToolTranscript {
            call_id: None,
            name: "write".into(),
            context: None,
            output: "permission denied".into(),
            lifecycle: ToolLifecycle::Failed,
        }),
    ];

    let compact = plain(transcript_lines(&entries, 40, TranscriptDetail::Compact));
    let expanded = plain(transcript_lines(&entries, 40, TranscriptDetail::Expanded));

    assert!(
        compact
            .lines()
            .any(|line| line.starts_with("bash") && line.ends_with("done"))
    );
    assert!(compact.contains("cargo test -p kurama-cli"));
    assert!(
        compact
            .lines()
            .any(|line| line.starts_with("write") && line.ends_with("failed"))
    );
    assert!(compact.contains("line two"));
    assert!(compact.contains("permission denied"));
    assert!(compact.contains("… 1 earlier line"));
    assert!(!compact.contains("abcdefghijklmnopqrstuvwxyz"));
    let unwrapped = expanded.split_whitespace().collect::<String>();
    assert!(unwrapped.contains("abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ"));
    assert!(expanded.contains("line two"));
    assert!(expanded.contains("permission denied"));
    assert!(!expanded.contains('…'));
}

#[test]
fn running_tool_output_shows_only_a_bounded_tail() {
    let entries = vec![TranscriptEntry::ToolCall(ToolTranscript {
        call_id: None,
        name: "bash".into(),
        context: None,
        output: "first\nsecond\nthird\nfourth".into(),
        lifecycle: ToolLifecycle::Running,
    })];

    let text = plain(transcript_lines(&entries, 80, TranscriptDetail::Compact));

    assert!(
        text.lines()
            .any(|line| line.starts_with("bash") && line.ends_with("running"))
    );
    assert!(text.contains("… 2 earlier lines"));
    assert!(text.contains("third"));
    assert!(text.contains("fourth"));
    assert!(!text.contains("first"));
    assert!(!text.contains("second"));
}

#[test]
fn expanded_tool_output_preserves_whitespace_and_code_layout() {
    let entries = vec![TranscriptEntry::ToolCall(ToolTranscript {
        call_id: None,
        name: "read".into(),
        context: None,
        output: "fn main() {\n    let value  = 1;\n}".into(),
        lifecycle: ToolLifecycle::Completed,
    })];

    let text = plain(transcript_lines(&entries, 80, TranscriptDetail::Expanded));

    let opening = text
        .lines()
        .find(|line| line.contains("fn main() {"))
        .expect("opening line");
    let statement = text
        .lines()
        .find(|line| line.contains("let value  = 1;"))
        .expect("statement");
    assert_eq!(
        statement.find("let value  = 1;"),
        opening.find("fn main() {").map(|column| column + 4)
    );
    assert!(text.lines().any(|line| line.trim_end().ends_with('}')));
}

#[test]
fn typed_tool_transcript_preserves_normalized_tool_name() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_tool("TOOL / Bash", "done");

    let text = buffer_text(&rendered(&state, 80, 20));

    assert!(
        text.lines()
            .any(|line| line.trim_start().starts_with("bash "))
    );
    assert!(!text.contains("TOOL /"));
}

#[test]
fn compact_and_expanded_transcript_views_render_completed_history() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_user("completed question");
    state.push_tool("TOOL / bash", "complete output");
    state.apply_runtime_event(RuntimeEvent::TurnCompleted);

    let compact = buffer_text(&rendered(&state, 80, 20));
    assert!(compact.contains("completed question"));
    assert!(compact.contains("complete output"));

    state.toggle_transcript_view();
    let expanded = buffer_text(&rendered(&state, 80, 20));
    assert!(expanded.contains("completed question"));
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

    assert!(text.contains("× Error · protocol error: malformed bridge output"));
    assert!(text.lines().any(|line| {
        line.contains("work/model") || line.contains("enter send") || line.contains("shortcuts")
    }));
    assert!(!text.contains("ready"));
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
    assert_eq!(cell_at_text(&buffer, "Use bold").fg, Color::Reset);
    assert_eq!(inline_code.bg, Color::Reset);
    assert!(!inline_code.modifier.contains(Modifier::BOLD));
    let link = cell_at_text(&buffer, "docs");
    assert!(link.modifier.contains(Modifier::UNDERLINED));
    assert!(buffer.content.iter().all(|cell| {
        cell.symbol()
            .chars()
            .all(|character| !character.is_control())
    }));
}

#[test]
fn wide_markdown_lists_keep_inline_code_and_punctuation_together() {
    let lines = transcript_lines(
        &[TranscriptEntry::AssistantMessage {
            body: concat!(
                "- Path `/Users/sarthak/src/harness-eng/coding-agent-with-subagents`, ",
                "git repo, branch `main`, clean tree.\n",
                "1. Re-enable `read`/`bash` for this session and inspect the repository."
            )
            .into(),
        }],
        120,
        TranscriptDetail::Compact,
    );
    let text = plain(lines);

    assert!(
        text.lines().any(|line| {
            line.contains("Path /Users/sarthak/src/harness-eng/coding-agent-with-subagents, git repo, branch main, clean tree.")
        }),
        "{text}"
    );
    assert!(
        text.lines().any(|line| {
            line.contains("1. Re-enable read/bash for this session and inspect the repository.")
        }),
        "{text}"
    );
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
    assert!(!text.contains("```"));
    assert!(!text.lines().any(|row| row.trim() == "rust"));
    assert!(text.contains("fn main() {"));
    assert!(text.contains("    println!(\"hi\");"));
    assert!(text.contains("Name   │ State"));
    assert!(text.contains("parser │ ready"));
    assert!(!text.contains("> Quote"));
    assert!(!text.contains("- alpha"));
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
    assert!(text.contains("abcdefghijklmnopqrstuvwxyz"), "{rows:#?}");
    let unwrapped = text.split_whitespace().collect::<String>();
    assert!(
        unwrapped.contains("abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ"),
        "{rows:#?}"
    );
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

    assert!(lines.iter().any(|line| line.contains("1234567890")));
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

    assert!(text.contains("Field: command") || text.contains("command"));
    assert!(text.contains("cargo test") || text.contains("Value:"));
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
fn compact_transcript_follows_the_latest_answer_and_can_scroll_to_earlier_history() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_user("earliest question");
    let output = (0..100)
        .map(|index| format!("line-{index:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    state.push_tool("TOOL / bash", output);
    state.push_assistant("**complete answer**");

    let text = buffer_text(&rendered(&state, 40, 12));

    assert!(text.contains("complete answer"));
    assert!(!text.contains("line-000"));

    state.scroll = usize::MAX;
    let history = buffer_text(&rendered(&state, 40, 12));
    assert!(history.contains("earliest question"));
    assert!(!history.contains("complete answer"));
}

#[test]
fn tool_output_wraps_on_unicode_grapheme_clusters() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_tool("TOOL / bash", "AB👨‍👩‍👧‍👦CD");
    state.toggle_transcript_view();

    let text = buffer_text(&rendered(&state, 10, 16));
    let rows = text.lines().map(str::trim_end).collect::<Vec<_>>();

    let unwrapped = text.split_whitespace().collect::<String>();
    assert!(
        unwrapped.contains("AB\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}CD"),
        "{rows:#?}"
    );
    assert!(rows.iter().any(|row| row.contains("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}")), "{rows:#?}");
    assert!(
        rows.iter().any(|row| row.trim_end().ends_with("CD")),
        "{rows:#?}"
    );
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
    assert_eq!(inline_code.bg, Color::Reset);
    assert!(!inline_code.modifier.contains(Modifier::BOLD));
    let link = cell_at_text(&buffer, "docs");
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
    assert!(
        text.lines()
            .any(|line| line.contains("B: 👨‍👩‍👧‍👦") || line.contains("👨‍👩‍👧‍👦"))
    );
    assert!(compact.contains("👨‍👩‍👧‍👦"));
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

    let unwrapped = text.split_whitespace().collect::<String>();
    assert!(unwrapped.contains("abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ"));
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
    assert!(!pending.contains("KURAMA"));
    assert!(pending.contains("> Run the CLI tests."));
    assert!(pending.contains("Action required"));
    assert!(pending.contains("Run the focused CLI tests"));
    assert!(pending.contains("Approve once") || pending.contains("a approve once"));
    assert!(!pending.contains("Message Kurama or type / for commands"));
    assert!(!pending.contains("approval pending"));
    let pending_lines = pending.lines().collect::<Vec<_>>();
    let approval_line = pending_lines
        .iter()
        .position(|line| line.contains("Action required"))
        .expect("inline approval line");
    assert!(!pending_lines[approval_line.saturating_sub(1)].contains('┌'));

    state.begin_approval_edit();
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
    terminal.draw(|frame| render(frame, &state)).unwrap();
    let editing = buffer_text(terminal.backend().buffer());
    assert!(!editing.contains("KURAMA"));
    assert!(editing.contains("> Run the CLI tests."));
    assert!(editing.contains(r#""command": "cargo test -p kurama-cli""#));
    assert!(!editing.contains("Message Kurama or type / for commands"));
    assert!(format!("{:?}", terminal.backend()).contains("cursor: true"));
}

#[test]
fn narrow_pending_approval_keeps_all_controls_visible() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.begin_approval(narrow_approval_request());

    let text = buffer_text(&rendered(&state, 40, 24));

    assert!(text.contains("Action required"));
    assert!(text.contains("Approve once") || text.contains("a approve once"));
    assert!(
        text.contains("Approve session")
            || text.contains("s approve session")
            || text.contains("s session")
            || text.contains("session")
    );
    assert!(text.contains("Deny") || text.contains("d deny"));
    assert!(text.contains("Edit") || text.contains("e edit"));
    assert!(text.contains("Run the focused CLI tests"));
    assert!(text.contains("narrow terminal"));
}

#[test]
fn wrapped_approval_preserves_decisions_and_the_safety_footer() {
    let mut request = approval_request();
    request.summary = "Review this operation before authorizing its execution. ".repeat(5);
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.begin_approval(request);

    let text = buffer_text(&rendered(&state, 36, 14));
    assert!(text.contains("kurama-cli"), "{text}");
    for decision in ["a approve once", "s approve session", "d deny", "e edit"] {
        assert!(text.contains(decision), "{text}");
    }
    assert!(
        text.lines().last().unwrap().contains("supervised"),
        "{text}"
    );
}

#[test]
fn narrow_layout_preserves_action_and_stacks_approval_choices() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.push_user("Run the CLI tests.");
    state.apply_runtime_event(RuntimeEvent::ApprovalRequired {
        request: approval_request(),
    });

    let text = buffer_text(&rendered(&state, 32, 10));

    assert!(text.contains("Action required"));
    assert!(text.contains("cargo test") || text.contains("kurama-cli"));
    assert!(
        text.contains("Approve once")
            || text.contains("a approve once")
            || text.contains("a approve")
    );
    assert!(
        text.contains("Approve session")
            || text.contains("s approve session")
            || text.contains("s session")
            || text.contains("session")
    );
    assert!(text.contains("Deny") || text.contains("d deny"));
    assert!(text.contains("Edit") || text.contains("e edit"));
    assert!(!text.contains("Esc to interrupt"));
}

#[test]
fn short_narrow_approval_compacts_controls_before_losing_action() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.begin_approval(approval_request());

    let text = buffer_text(&rendered(&state, 32, 2));

    assert!(text.contains("$ cargo test -p kurama-cli"));
    assert!(text.contains("a/s/d/e"));
}

#[test]
fn very_narrow_short_approval_keeps_every_decision_shortcut() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.begin_approval(approval_request());

    let text = buffer_text(&rendered(&state, 24, 2));
    let lines = text.lines().map(str::trim).collect::<Vec<_>>();

    assert!(text.contains('…'));
    assert!(text.contains("-p kurama-cli"));
    assert!(lines.contains(&"a/s/d/e"));
}

#[test]
fn tiny_approval_shows_the_dangerous_command_tail_with_an_omission_marker() {
    let mut request = approval_request();
    request.operation = Operation::Bash {
        command: "printf safe-output && rm -rf /important".into(),
        cwd: ".".into(),
        class: kurama_protocol::tool::CommandClass::Mutating,
        timeout_ms: 30_000,
    };
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.begin_approval(request);

    let text = buffer_text(&rendered(&state, 32, 2));

    assert!(text.contains('…'));
    assert!(text.contains("rm -rf /important"));
    assert!(text.contains("a/s/d/e"));
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
        .position(|line| line.contains('}'))
        .expect("last editor line remains visible");
    let controls = lines
        .iter()
        .position(|line| line.contains("Enter submit"))
        .expect("edit controls remain visible");
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    let brace_x = (0..terminal.backend().buffer().area.width)
        .find(|x| {
            terminal
                .backend()
                .buffer()
                .cell((*x, editor_end as u16))
                .is_some_and(|cell| cell.symbol() == "}")
        })
        .expect("editor closing brace");

    assert!(editor_end < controls);
    assert_eq!(cursor.y, editor_end as u16);
    assert_eq!(cursor.x, brace_x + 1);
}

#[test]
fn approval_editor_keeps_trailing_json_punctuation_attached() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.begin_approval(ApprovalRequest {
        operation_id: OperationId::from("o_json"),
        operation: Operation::Write {
            paths: vec!["crates/kurama-cli/src/tui/composer.rs".into()],
            destructive: false,
            external: false,
        },
        summary: "Write the responsive composer changes after reviewing the diff".into(),
        arguments: serde_json::json!({
            "path": "crates/kurama-cli/src/tui/composer.rs",
            "content": "responsive approval content"
        }),
    });
    state.begin_approval_edit();

    let text = buffer_text(&rendered(&state, 48, 16));
    let rows = text.lines().map(str::trim).collect::<Vec<_>>();

    assert!(!rows.contains(&","), "{rows:#?}");
}

#[test]
fn approval_validation_error_renders_inline_with_the_editor() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.begin_approval(approval_request());
    state.begin_approval_edit();
    state.set_approval_editor(r#"{"command":"cargo test""#);
    state
        .submit_approval_edit()
        .expect_err("invalid JSON must remain in the editor");

    let text = buffer_text(&rendered(&state, 60, 16));

    assert!(text.contains("Invalid JSON"), "{text}");
    assert!(
        !text.contains("× Error · invalid approval arguments"),
        "{text}"
    );
}

#[test]
fn approval_editor_cursor_marks_the_insertion_point() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.begin_approval(approval_request());
    state.begin_approval_edit();
    state.set_approval_editor("abcd");
    state.approval.as_mut().expect("approval").move_left();

    let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
    terminal.draw(|frame| render(frame, &state)).unwrap();
    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    let symbol = terminal
        .backend()
        .buffer()
        .cell((cursor.x, cursor.y))
        .expect("cursor cell")
        .symbol();

    assert_eq!(symbol, "d");
}

#[test]
fn every_tui_view_preserves_the_terminal_default_background() {
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
            "view painted a background cell: {:?}",
            state.overlay
        );
    }
}

#[test]
fn running_agent_metadata_uses_the_active_accent_not_failure_red() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.set_agents(vec![AgentRow {
        id: AgentId::from("a_1"),
        role: "reviewer".into(),
        profile: "frontier".into(),
        task: "inspect parser".into(),
        state: AgentState::Running,
        activity: "reading".into(),
        transcript: Vec::new(),
    }]);
    state.open_agents();

    let buffer = rendered(&state, 120, 30);

    assert_eq!(cell_at_text(&buffer, "RUNNING").fg, Color::Cyan);
}

#[test]
fn onboarding_keeps_the_last_connection_option_visible() {
    let mut state = TuiState::onboarding(".");
    for _ in 0..4 {
        state.onboarding.select_next();
    }

    let text = buffer_text(&rendered(&state, 100, 24));

    assert!(text.contains("OpenAI-compatible or local endpoint"));
}

#[test]
fn composer_cursor_tracks_the_visual_insertion_point() {
    let mut state = TuiState::new(
        "openai-main",
        "gpt-5.6",
        "~/src/kurama",
        ExecutionMode::Supervised,
    );
    state.composer = "first line\n界界second line".into();
    state.cursor = state.composer.len();

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &state)).unwrap();

    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    let text = buffer_text(terminal.backend().buffer());
    assert_eq!(text.matches('>').count(), 1);
    assert!(text.contains("> first line"));
    let second_line = text
        .lines()
        .find(|line| line.contains("second line"))
        .expect("second composer line");
    assert_eq!(second_line.chars().take(2).collect::<String>(), "  ");
    let prompt_x = (0..terminal.backend().buffer().area.width)
        .find(|x| {
            terminal
                .backend()
                .buffer()
                .cell((*x, cursor.y))
                .is_some_and(|cell| cell.symbol() == "界")
        })
        .expect("wide character column");
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((prompt_x, cursor.y))
            .unwrap()
            .symbol(),
        "界"
    );
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((prompt_x + 2, cursor.y))
            .unwrap()
            .symbol(),
        "界"
    );
    assert_eq!(
        cursor.x,
        prompt_x + Line::from("界界second line").width() as u16
    );
    assert!(format!("{:?}", terminal.backend()).contains("cursor: true"));
}

#[test]
fn composer_cursor_handles_char_boundary_inside_combining_grapheme() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.composer = "e\u{301}x".into();
    state.cursor = "e".len();

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| render(frame, &state)).unwrap();

    let cursor = terminal.backend_mut().get_cursor_position().unwrap();
    let text = buffer_text(terminal.backend().buffer());
    let composer_y = text
        .lines()
        .position(|line| line.contains('>'))
        .expect("composer prompt") as u16;
    assert_eq!(cursor.y, composer_y);
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

    state.overlay = Overlay::Shortcuts;
    terminal.draw(|frame| render(frame, &state)).unwrap();
    assert!(format!("{:?}", terminal.backend()).contains("cursor: false"));

    state.overlay = Overlay::AgentMessage;
    state.agent_message = "hi".into();
    state.agent_message_cursor = 2;
    state.agents = vec![AgentRow {
        id: AgentId::from("a_1"),
        role: "reviewer".into(),
        profile: "gpt-5.6".into(),
        task: "inspect".into(),
        state: AgentState::Running,
        activity: "reading".into(),
        transcript: vec!["line one".into(), "line two".into()],
    }];
    state.selected_agent = 0;
    terminal.draw(|frame| render(frame, &state)).unwrap();
    assert!(format!("{:?}", terminal.backend()).contains("cursor: true"));
}

#[test]
fn agent_inspect_does_not_double_space_transcript_lines() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.agents = vec![AgentRow {
        id: AgentId::from("a_1"),
        role: "reviewer".into(),
        profile: "gpt-5.6".into(),
        task: "inspect".into(),
        state: AgentState::Running,
        activity: "reading".into(),
        transcript: vec!["line one".into(), "line two".into()],
    }];
    state.open_agents();
    state.inspect_selected_agent();

    let text = buffer_text(&rendered(&state, 80, 24));
    let rows = text.lines().map(str::trim_end).collect::<Vec<_>>();
    let one = rows.iter().position(|row| row.contains("line one"));
    let two = rows.iter().position(|row| row.contains("line two"));
    assert!(one.is_some() && two.is_some(), "{rows:#?}");
    assert_eq!(two.unwrap(), one.unwrap() + 1, "{rows:#?}");
    assert!(text.contains("RUNNING"));
    assert!(!text.contains("Running"));
}

#[test]
fn standard_cli_registry_contains_exactly_four_tools() {
    assert_eq!(App::tool_names(), ["bash", "read", "web-search", "write"]);
}

#[test]
fn at_sign_mentions_complete_a_project_file() {
    let dir = tempfile::tempdir().expect("temp");
    std::fs::create_dir_all(dir.path().join("src")).expect("src");
    std::fs::write(dir.path().join("src/lib.rs"), "fn x() {}").expect("write");
    let mut state = TuiState::new(
        "work",
        "model",
        dir.path().to_string_lossy().into_owned(),
        ExecutionMode::Supervised,
    );
    state.composer = "@lib".into();
    state.cursor = 4;
    state.composer_edited();

    let text = buffer_text(&rendered(&state, 80, 12));
    assert!(text.contains("src/lib.rs"), "{text}");
    assert!(state.complete_selected_file());
    assert_eq!(state.composer, "@src/lib.rs ");
}

#[test]
fn history_search_filters_and_restores_a_prompt() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.remember_prompt("open the palette");
    state.remember_prompt("fix the failing tests");
    state.start_history_search();
    state.push_history_search_char('f');
    state.push_history_search_char('a');

    let text = buffer_text(&rendered(&state, 80, 12));
    assert!(text.contains("history  fa"), "{text}");
    assert!(text.contains("fix the failing tests"), "{text}");
    assert!(!text.contains("open the palette"), "{text}");
    assert!(state.accept_history_search());
    assert_eq!(state.composer, "fix the failing tests");
    assert!(!state.history_search_active());
}
