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
use kurama_protocol::{policy::ExecutionMode, session::SessionMetadata};
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

async fn app_sample(turns: usize, frames: usize, scroll: bool) -> Result<Value> {
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
        inner: Rc::new(RefCell::new(TestBackend::new(100, 36))),
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

fn tool_sample(frames: usize) -> Value {
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
        let lines = transcript_lines(&entries, 96, TranscriptDetail::Compact);
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
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value: usize = args.next().ok_or("missing value")?.parse()?;
        match flag.as_str() {
            "--turns" => turns = value,
            "--frames" => frames = value,
            "--repetitions" => repetitions = value,
            _ => return Err(format!("unknown option {flag}").into()),
        }
    }
    if turns == 0 || frames == 0 || repetitions == 0 {
        return Err("counts must be positive".into());
    }
    let mut scroll = Vec::new();
    let mut ignored = Vec::new();
    let mut tools = Vec::new();
    for repetition in 0..=repetitions {
        let a = app_sample(turns, frames, true).await?;
        let b = app_sample(turns, frames, false).await?;
        let c = tool_sample(frames);
        if repetition > 0 {
            scroll.push(a);
            ignored.push(b);
            tools.push(c);
        }
    }
    println!(
        "{}",
        json!({"benchmark":"kurama-tui","release_build":!cfg!(debug_assertions),"turns":turns,"frames":frames,"warmups":1,"repetitions":repetitions,"expanded_scroll":distribution(&scroll),"ignored_events":distribution(&ignored),"compact_tool_preview":distribution(&tools)})
    );
    Ok(())
}
