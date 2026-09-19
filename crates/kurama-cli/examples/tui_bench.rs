//! Offline rendering benchmark through the production App event loop.
//! cargo run --release -p kurama-cli --example tui_bench -- --turns 1000 --frames 100
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Instant,
};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use kurama_cli::{
    app::{App, run_with},
    tui::{
        ToolLifecycle, ToolTranscript, TranscriptDetail, TranscriptEntry, TuiState,
        transcript_lines,
    },
};
use kurama_core::testing::ScriptedBackend;
use kurama_protocol::{
    policy::ExecutionMode,
    runtime::RuntimeEvent,
    session::{EventEnvelope, SessionMetadata},
    tool::ToolResult,
};
use kurama_sdk::Agent;
use ratatui::{
    Terminal,
    backend::{Backend, ClearType, TestBackend, WindowSize},
    buffer::Cell as TerminalCell,
    layout::{Position, Size},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

type Result<T, E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

#[derive(Clone)]
struct CountingBackend {
    inner: Rc<RefCell<TestBackend>>,
    draws: Rc<Cell<usize>>,
    cells: Rc<Cell<usize>>,
}

impl Backend for CountingBackend {
    type Error = <TestBackend as Backend>::Error;
    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a TerminalCell)>,
    {
        let mut cells = 0;
        self.inner
            .borrow_mut()
            .draw(content.inspect(|_| cells += 1))?;
        self.draws.set(self.draws.get() + 1);
        self.cells.set(self.cells.get() + cells);
        Ok(())
    }
    fn append_lines(&mut self, count: u16) -> Result<(), Self::Error> {
        self.inner.borrow_mut().append_lines(count)
    }
    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.borrow_mut().hide_cursor()
    }
    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.borrow_mut().show_cursor()
    }
    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.borrow_mut().get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.borrow_mut().set_cursor_position(position)
    }
    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.borrow_mut().clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.borrow_mut().clear_region(clear_type)
    }
    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.borrow().size()
    }
    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.borrow_mut().window_size()
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.borrow_mut().flush()
    }
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

async fn app_sample(
    turns: usize,
    frames: usize,
    scroll: bool,
    width: u16,
    height: u16,
) -> Result<Value> {
    let agent = Agent::new()
        .backend(ScriptedBackend::new(Vec::new()))
        .build()?;
    let metadata = SessionMetadata {
        id: "tui-bench".into(),
        created_at_ms: 0,
        project_root: ".".into(),
        profile: agent.active_profile().into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    };
    let (handle, events) = agent.launch(metadata.clone(), Vec::new())?;
    let mut state = TuiState::new("fixture", "fixture", ".", ExecutionMode::Supervised);
    for turn in 0..turns {
        state.push_user(format!("Question {turn}"));
        state.push_assistant(format!("## Answer {turn}\n\nA **formatted** response with `inline code` and a [link](https://example.com/{turn}).\n\n- first item {turn}\n- second item\n\n```rust\nlet value = {turn};\n```"));
    }
    if scroll {
        state.toggle_transcript_view();
    }
    let app = App::from_runtime(state, handle, agent.orchestrator(), metadata.id);
    let (sender, input) = mpsc::channel(frames + 2);
    for index in 0..frames {
        let event = if scroll {
            key(
                if index < frames / 2 {
                    KeyCode::Up
                } else {
                    KeyCode::Down
                },
                KeyModifiers::NONE,
            )
        } else {
            Event::FocusLost
        };
        sender.try_send(event)?;
    }
    if scroll {
        sender.try_send(key(KeyCode::Esc, KeyModifiers::NONE))?;
    }
    sender.try_send(key(KeyCode::Char('d'), KeyModifiers::CONTROL))?;
    drop(sender);
    let backend = CountingBackend {
        inner: Rc::new(RefCell::new(TestBackend::new(width, height))),
        draws: Rc::new(Cell::new(0)),
        cells: Rc::new(Cell::new(0)),
    };
    let stats = backend.clone();
    let mut terminal = Terminal::new(backend)?;
    let started = Instant::now();
    let app = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        run_with(app, &mut terminal, input, events),
    )
    .await??;
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(app.state.transcript.len(), turns * 2);
    assert!(app.state.composer.is_empty());
    Ok(
        json!({"elapsed_ms":elapsed_ms,"draw_calls":stats.draws.get(),"changed_cells":stats.cells.get(),"transcript_entries":app.state.transcript.len()}),
    )
}

fn tool_sample(frames: usize, width: u16) -> Value {
    let mut output = String::new();
    for line in 0..2048 {
        output.push_str(&format!(
            "line {line:04} {}\n",
            "readable output ".repeat(5)
        ));
    }
    output.push_str("TAIL_MARKER");
    let entries = vec![TranscriptEntry::ToolCall(ToolTranscript {
        call_id: None,
        name: "bash".into(),
        context: Some("inspect output".into()),
        output,
        lifecycle: ToolLifecycle::Completed,
    })];
    let started = Instant::now();
    let mut visible_rows = 0;
    for _ in 0..frames {
        let lines = transcript_lines(
            &entries,
            usize::from(width.saturating_sub(4)),
            TranscriptDetail::Compact,
        );
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.content.contains("TAIL_MARKER"))
        }));
        assert!(lines.len() <= 5);
        visible_rows = std::hint::black_box(lines.len());
    }
    json!({"elapsed_ms":started.elapsed().as_secs_f64()*1000.0,"preview_rows":visible_rows,"source_lines":2049})
}

async fn interactive_sample(
    mode: &str,
    turns: usize,
    frames: usize,
    width: u16,
    height: u16,
) -> Result<Value> {
    let workspace = tempfile::tempdir()?;
    if mode == "files" {
        for index in 0..400 {
            std::fs::write(
                workspace.path().join(format!("file_{index:03}.rs")),
                "fixture\n",
            )?;
        }
    }
    let agent = Agent::new()
        .backend(ScriptedBackend::new(Vec::new()))
        .workspace(workspace.path())
        .build()?;
    let metadata = SessionMetadata {
        id: "interactive-bench".into(),
        created_at_ms: 0,
        project_root: workspace.path().to_string_lossy().into_owned(),
        profile: agent.active_profile().into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    };
    let (handle, engine_events) = agent.launch(metadata.clone(), Vec::new())?;
    let mut state = TuiState::new(
        "fixture",
        "fixture",
        &metadata.project_root,
        ExecutionMode::Supervised,
    );
    if mode == "history" {
        let replay = (0..turns).map(|index| {
            Ok(EventEnvelope::new(index as u64, 0, metadata.id.clone(), None,
                serde_json::from_value(json!({"type":"user_message", "text":format!("Question {index}"), "explicit_delegation":false}))?))
        }).collect::<Result<Vec<_>>>()?;
        state.hydrate_replay(&replay);
    }
    let app = App::from_runtime(state, handle, agent.orchestrator(), metadata.id);
    let (input_sender, input) = mpsc::channel(frames + 4);
    let (runtime_sender, synthetic_events) = mpsc::channel(frames + 4);
    let events = if mode == "streaming" {
        synthetic_events
    } else {
        engine_events
    };
    let backend = CountingBackend {
        inner: Rc::new(RefCell::new(TestBackend::new(width, height))),
        draws: Rc::new(Cell::new(0)),
        cells: Rc::new(Cell::new(0)),
    };
    let stats = backend.clone();
    let mut terminal = Terminal::new(backend)?;
    let contains = |needle: &str| {
        stats
            .inner
            .borrow()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .contains(needle)
    };
    let cold = Instant::now();
    let running = async {
        Ok::<_, Box<dyn std::error::Error>>(run_with(app, &mut terminal, input, events).await?)
    };
    let producing = async {
        if mode == "history" || mode == "files" {
            let before = stats.draws.get();
            if mode == "history" {
                input_sender
                    .send(key(KeyCode::Char('r'), KeyModifiers::CONTROL))
                    .await?;
                input_sender
                    .send(Event::Paste(format!("Question {}", turns / 2)))
                    .await?;
            } else {
                input_sender.send(Event::Paste("@file_".into())).await?;
            }
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while stats.draws.get() < before + if mode == "history" { 2 } else { 1 }
                    || !contains(if mode == "history" {
                        "Question"
                    } else {
                        "file_000"
                    })
                {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
        }
        let ready_ms = cold.elapsed().as_secs_f64() * 1000.0;
        let started = Instant::now();
        let draws = stats.draws.get();
        let cells = stats.cells.get();
        if mode == "streaming" {
            runtime_sender
                .send(RuntimeEvent::ToolStarted {
                    operation_id: "bench-operation".into(),
                    name: "bash".into(),
                    context: "stream fixture".into(),
                })
                .await?;
            for index in 0..frames {
                runtime_sender
                    .send(RuntimeEvent::ToolOutputDelta {
                        call_id: "bench-call".into(),
                        stream: "stdout".into(),
                        chunk: format!("chunk {index}: {}\n", "fixture output ".repeat(128)),
                    })
                    .await?;
                let before = stats.draws.get();
                input_sender
                    .send(key(KeyCode::Char('x'), KeyModifiers::NONE))
                    .await?;
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while stats.draws.get() <= before {
                        tokio::task::yield_now().await;
                    }
                })
                .await?;
            }
            runtime_sender
                .send(RuntimeEvent::ToolOutputDelta {
                    call_id: "bench-call".into(),
                    stream: "stdout".into(),
                    chunk: "STREAMING_TAIL".into(),
                })
                .await?;
            let mut result = ToolResult::success("bench-call".into(), "STREAMING_TAIL");
            result.truncated = true;
            result.metadata = json!({"tool_name":"bash"});
            runtime_sender
                .send(RuntimeEvent::ToolCompleted {
                    operation_id: "bench-operation".into(),
                    result,
                })
                .await?;
            runtime_sender.send(RuntimeEvent::TurnCompleted).await?;
            runtime_sender
                .send(RuntimeEvent::Status {
                    message: "STREAM_BENCH_DONE".into(),
                })
                .await?;
            input_sender.send(Event::Resize(width, height)).await?;
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !contains("STREAM_BENCH_DONE") {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
        } else {
            for index in 0..frames {
                input_sender
                    .send(key(
                        if index % 2 == 0 {
                            KeyCode::Down
                        } else {
                            KeyCode::Up
                        },
                        KeyModifiers::NONE,
                    ))
                    .await?;
            }
        }
        if mode == "history" {
            input_sender
                .send(key(KeyCode::Esc, KeyModifiers::NONE))
                .await?;
        } else {
            input_sender
                .send(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .await?;
        }
        input_sender
            .send(key(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .await?;
        drop(input_sender);
        drop(runtime_sender);
        Ok::<_, Box<dyn std::error::Error>>((started, ready_ms, draws, cells))
    };
    let (app, (started, ready_ms, draws, cells)) = tokio::try_join!(running, producing)?;
    assert!(app.state.composer.is_empty());
    if mode == "streaming" {
        assert!(app.state.transcript.iter().any(|entry| matches!(entry,
            TranscriptEntry::ToolCall(tool) if tool.lifecycle == ToolLifecycle::Completed && tool.output.contains("STREAMING_TAIL"))));
    }
    Ok(
        json!({"elapsed_ms":started.elapsed().as_secs_f64()*1000.0,"ready_ms":ready_ms,
        "draw_calls":stats.draws.get()-draws,"changed_cells":stats.cells.get()-cells}),
    )
}

fn distribution(samples: &[Value]) -> Value {
    let mut times = samples
        .iter()
        .map(|sample| sample["elapsed_ms"].as_f64().expect("time"))
        .collect::<Vec<_>>();
    times.sort_by(f64::total_cmp);
    let middle = times.len() / 2;
    let median = if times.len().is_multiple_of(2) {
        (times[middle - 1] + times[middle]) / 2.0
    } else {
        times[middle]
    };
    json!({"median_ms":median,"p95_ms":times[(times.len()*95).div_ceil(100)-1],"samples":samples})
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut turns = 1000;
    let mut frames = 100;
    let mut repetitions = 5;
    let mut width = 100;
    let mut height = 36;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value: usize = args.next().ok_or("missing value")?.parse()?;
        match flag.as_str() {
            "--turns" => turns = value,
            "--frames" => frames = value,
            "--repetitions" => repetitions = value,
            "--width" => width = u16::try_from(value)?,
            "--height" => height = u16::try_from(value)?,
            _ => return Err(format!("unknown option {flag}").into()),
        }
    }
    if turns == 0 || frames == 0 || repetitions == 0 {
        return Err("counts must be positive".into());
    }
    if width < 24 || height < 8 {
        return Err("viewport must be at least 24x8".into());
    }
    let mut scroll = Vec::new();
    let mut ignored = Vec::new();
    let mut tools = Vec::new();
    let mut history = Vec::new();
    let mut files = Vec::new();
    let mut streaming = Vec::new();
    for repetition in 0..=repetitions {
        let a = app_sample(turns, frames, true, width, height).await?;
        let b = app_sample(turns, frames, false, width, height).await?;
        let c = tool_sample(frames, width);
        let d = interactive_sample("history", turns, frames, width, height).await?;
        let e = interactive_sample("files", turns, frames, width, height).await?;
        let f = interactive_sample("streaming", turns, frames, width, height).await?;
        if repetition > 0 {
            scroll.push(a);
            ignored.push(b);
            tools.push(c);
            history.push(d);
            files.push(e);
            streaming.push(f);
        }
    }
    println!(
        "{}",
        json!({"benchmark":"kurama-tui","release_build":!cfg!(debug_assertions),"turns":turns,"frames":frames,"width":width,"height":height,"warmups":1,"repetitions":repetitions,"expanded_scroll":distribution(&scroll),"ignored_events":distribution(&ignored),"compact_tool_preview":distribution(&tools),"compact_tool_preview_kind":"transcript_rebuild","history_palette":distribution(&history),"file_palette":distribution(&files),"streaming_tool_input":distribution(&streaming)})
    );
    Ok(())
}
