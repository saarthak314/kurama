# Codex-Inspired Responsive TUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Kurama's status-string-driven terminal UI with a Codex-inspired, responsive, typed interface that preserves native scrollback, full tool output, transparency, and the sub-10 MiB release target.

**Architecture:** Runtime events reduce into typed activity and transcript state. Focused render modules project that state into a responsive inline viewport, while a `Ctrl+O` transcript view exposes complete historical tool output that cannot be redrawn in committed native scrollback. Animation remains event-driven and wakes only while active work is visible.

**Tech Stack:** Rust 2024, Ratatui 0.30, Crossterm 0.29, Pulldown-cmark 0.13, Tokio 1.53.

**Spec:** `docs/superpowers/specs/2026-08-30-codex-inspired-responsive-tui-design.md`

## Global Constraints

- Preserve Kurama's existing commands, provider behavior, approval semantics, session persistence, and four model-visible tools.
- Keep stable transcript rows in native terminal scrollback and keep mouse capture disabled.
- Never discard or truncate captured assistant or tool output.
- Do not paint a full-screen background; inherit the terminal's foreground and background.
- Add no new runtime dependency.
- Keep focused tests only; do not add a broad golden-screenshot suite.
- Keep the stripped release binary below 10 MiB.
- Commit each completed task without AI attribution and do not push.

---

### Task 1: Typed Activity And Transcript State

**Files:**
- Modify: `crates/kurama-cli/src/tui/state.rs`
- Modify: `crates/kurama-cli/src/tui/mod.rs`
- Modify: `crates/kurama-cli/tests/tui_flows.rs`

**Interfaces:**
- Produces: `ActivityState`, `TranscriptEntry`, `ToolTranscript`, `ToolLifecycle`.
- Produces: `TuiState::activity()`, `TuiState::set_thinking()`, `TuiState::push_notice()`, `TuiState::push_error()`, and `TuiState::toggle_transcript_view()`.
- Preserves: `stable_transcript()`, `live_transcript()`, streaming entry indices, approval commands, agent state, and replay hydration.

- [ ] **Step 1: Add failing reducer and transcript tests**

Add focused cases proving that submit/start/delta/complete/error events transition through typed states, one error produces one transcript entry, replay produces the explicit variants, and tool output remains complete:

```rust
#[test]
fn runtime_events_drive_typed_activity_without_duplicate_errors() {
    let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
    state.set_thinking();
    assert!(matches!(state.activity(), ActivityState::Thinking { .. }));

    state.apply_runtime_event(RuntimeEvent::ToolStarted {
        call_id: CallId::from("call_1"),
        name: "bash".into(),
    });
    assert!(matches!(state.activity(), ActivityState::RunningTool { name, .. } if name == "bash"));

    state.apply_runtime_event(RuntimeEvent::Error { message: "broken".into() });
    assert_eq!(state.activity(), &ActivityState::Idle);
    assert_eq!(state.transcript.iter().filter(|entry| matches!(entry, TranscriptEntry::Error { body } if body == "broken")).count(), 1);
}
```

- [ ] **Step 2: Run the focused test and confirm failure**

Run: `cargo test --locked -p kurama-cli --test tui_flows runtime_events_drive_typed_activity_without_duplicate_errors`

Expected: compilation fails because the typed state and enum variants do not exist.

- [ ] **Step 3: Implement explicit state types**

Use these shapes, keeping `Instant` inside active variants so elapsed time is derived rather than stored as strings:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityState {
    Idle,
    Thinking { started_at: Instant },
    Working { label: String, started_at: Instant },
    RunningTool { name: String, started_at: Instant },
    AwaitingApproval,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolLifecycle {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolTranscript {
    pub call_id: Option<CallId>,
    pub name: String,
    pub output: String,
    pub lifecycle: ToolLifecycle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEntry {
    UserTurn { body: String },
    AssistantMessage { body: String },
    ToolCall(ToolTranscript),
    Error { body: String },
    Notice { label: Option<String>, body: String },
}
```

Keep transcript helper methods private where possible. Reset activity to `Idle` on completion and errors, set `AwaitingApproval` on approval requests, return to `Thinking` after approval submission, and preserve complete tool streams when final results arrive.

- [ ] **Step 4: Run state and flow tests**

Run: `cargo test --locked -p kurama-cli --test tui_flows`

Expected: all reducer, approval, replay, and full-output tests pass.

- [ ] **Step 5: Commit typed state**

```bash
git add crates/kurama-cli/src/tui/state.rs crates/kurama-cli/src/tui/mod.rs crates/kurama-cli/tests/tui_flows.rs
git commit -m "refactor: model tui activity and transcript state"
```

### Task 2: Transcript Renderer And Full History View

**Files:**
- Create: `crates/kurama-cli/src/tui/transcript.rs`
- Modify: `crates/kurama-cli/src/tui/render.rs`
- Modify: `crates/kurama-cli/src/tui/mod.rs`
- Modify: `crates/kurama-cli/src/app.rs`
- Modify: `crates/kurama-cli/tests/tui_structure.rs`

**Interfaces:**
- Consumes: `TranscriptEntry` and `ToolTranscript` from Task 1.
- Produces: `transcript_lines(entries: &[TranscriptEntry], width: usize, detail: TranscriptDetail) -> Vec<Line<'static>>`.
- Produces: `TranscriptDetail::{Compact, Expanded}` and `render_transcript_view(frame, state)`.
- Preserves: existing Markdown parsing, Unicode wrapping, narrow-table stacking, and stable transcript commit behavior.

- [ ] **Step 1: Add failing transcript-structure tests**

Add cases for Codex-style prompt markers, blank turn separation, compact tool summaries, expanded full output, and error placement:

```rust
#[test]
fn transcript_groups_turns_and_expands_complete_tool_output() {
    let entries = vec![
        TranscriptEntry::UserTurn { body: "inspect".into() },
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
    assert!(!compact.contains("line two"));
    assert!(expanded.contains("line one"));
    assert!(expanded.contains("line two"));
}
```

- [ ] **Step 2: Run the focused test and confirm failure**

Run: `cargo test --locked -p kurama-cli --test tui_structure transcript_groups_turns_and_expands_complete_tool_output`

Expected: compilation fails because `TranscriptDetail` and the new renderer signature do not exist.

- [ ] **Step 3: Extract transcript rendering**

Move the Markdown parser, styled wrapping, table rendering, transcript formatting, and related style helpers from `render.rs` into `transcript.rs` without changing Markdown semantics. Render:

- user turns as `› prompt` using a restrained accent;
- assistant Markdown without labels or boxes;
- compact tools as `└ Running <name>`, `└ Ran <name>`, or `└ <name> failed`;
- expanded output as indented lines below the tool summary;
- errors as `Error: <message>` in red;
- notices as dim text with an optional label.

- [ ] **Step 4: Add the `Ctrl+O` transcript view**

Committed inline scrollback cannot be redrawn. Make `Ctrl+O` open a live full-history view backed by the canonical `state.transcript`, rendered with `TranscriptDetail::Expanded`. `Ctrl+O` or `Esc` closes it. Page Up/Down and arrow scrolling continue to use `state.scroll`. Normal scrollback commits compact summaries.

- [ ] **Step 5: Run transcript and app tests**

Run: `cargo test --locked -p kurama-cli --test tui_structure && cargo test --locked -p kurama-cli --lib`

Expected: transcript structure, Markdown, scroll limits, and key routing pass.

- [ ] **Step 6: Commit transcript rendering**

```bash
git add crates/kurama-cli/src/tui/transcript.rs crates/kurama-cli/src/tui/render.rs crates/kurama-cli/src/tui/mod.rs crates/kurama-cli/src/app.rs crates/kurama-cli/tests/tui_structure.rs
git commit -m "feat: add structured transcript rendering"
```

### Task 3: Responsive Activity, Composer, And Approval Surfaces

**Files:**
- Create: `crates/kurama-cli/src/tui/layout.rs`
- Create: `crates/kurama-cli/src/tui/activity.rs`
- Create: `crates/kurama-cli/src/tui/composer.rs`
- Modify: `crates/kurama-cli/src/tui/render.rs`
- Modify: `crates/kurama-cli/src/tui/mod.rs`
- Modify: `crates/kurama-cli/tests/tui_structure.rs`
- Modify: `crates/kurama-cli/tests/tui_flows.rs`

**Interfaces:**
- Consumes: typed activity and explicit transcript state from Task 1.
- Produces: `ResponsiveLayout::for_area(area, input_height, activity_visible)`.
- Produces: `activity_line(activity, width, now)`, `render_composer`, and `render_approval`.
- Preserves: onboarding and `/agents` behavior while sharing terminal-default visual tokens.

- [ ] **Step 1: Add failing responsive buffer tests**

Render representative `120x32`, `80x24`, `48x16`, and `32x10` buffers. Assert that user content, the approval decision, and the active action remain visible; wide-only metadata disappears in priority order; and no non-space cell lies outside its assigned rectangle.

```rust
#[test]
fn narrow_layout_preserves_action_and_stacks_approval_choices() {
    let mut state = approval_state();
    state.apply_runtime_event(RuntimeEvent::ApprovalRequired { request: approval_request() });
    let rendered = render_text(&state, 32, 10);
    assert!(rendered.contains("Action required"));
    assert!(rendered.contains("y approve"));
    assert!(rendered.contains("n deny"));
    assert!(!rendered.contains("Esc to interrupt"));
}
```

- [ ] **Step 2: Run responsive tests and confirm failure**

Run: `cargo test --locked -p kurama-cli --test tui_structure narrow_layout_preserves_action_and_stacks_approval_choices`

Expected: assertions fail against the fixed header/footer/panel layout.

- [ ] **Step 3: Implement responsive layout policy**

Remove the permanent branded header and full-screen surface fill. Compute transcript, activity, input, and contextual-footer rectangles from the current frame on every draw. Use progressive disclosure rather than fixed breakpoints:

1. keep prompt/input and approval decisions;
2. keep the active action;
3. drop help and interrupt hints;
4. drop project and model context;
5. shorten labels by Unicode display width;
6. stack approval choices.

Keep the execution mode visible whenever possible, especially `YOLO`; omit ambient agent counts before omitting the mode.

- [ ] **Step 4: Implement Codex-style activity and composer surfaces**

Render active work directly above the composer using spinner frames selected from elapsed milliseconds. Show elapsed time in compact form, include `Esc to interrupt` only when it fits, and suppress the row for `Idle` and inline approval states. Render a borderless composer with `›` alignment, wrapping cursor math, and a dim contextual footer. Use terminal-default colors plus dim, cyan, yellow, and red accents.

- [ ] **Step 5: Move approvals into the composer region**

Retain the existing approval data and shortcuts, but render them without `Clear`, modal borders, or a floating panel. Bound context by available height, keep the action and decision controls visible, and stack controls on narrow terminals.

- [ ] **Step 6: Run TUI structure and flow tests**

Run: `cargo test --locked -p kurama-cli --test tui_structure && cargo test --locked -p kurama-cli --test tui_flows`

Expected: all wide, narrow, short, Markdown, approval, and selection regressions pass.

- [ ] **Step 7: Commit responsive surfaces**

```bash
git add crates/kurama-cli/src/tui/layout.rs crates/kurama-cli/src/tui/activity.rs crates/kurama-cli/src/tui/composer.rs crates/kurama-cli/src/tui/render.rs crates/kurama-cli/src/tui/mod.rs crates/kurama-cli/tests/tui_structure.rs crates/kurama-cli/tests/tui_flows.rs
git commit -m "feat: add responsive codex-style tui surfaces"
```

### Task 4: Event-Driven Animation And Notice Routing

**Files:**
- Modify: `crates/kurama-cli/src/app.rs`
- Modify: `crates/kurama-cli/src/tui/state.rs`
- Modify: `crates/kurama-cli/tests/bootstrap.rs`
- Modify: `crates/kurama-cli/tests/tui_flows.rs`

**Interfaces:**
- Consumes: `ActivityState::is_animated()` and state notice/error helpers.
- Produces: active-only animation wakeups in `run_loop`.
- Produces: command feedback as transcript `Notice` or `Error` entries instead of footer status strings.

- [ ] **Step 1: Add failing command-feedback and activity tests**

Replace assertions against `state.status` with assertions against typed activity or transcript entries. Cover unknown commands, disconnected execution, profile changes, `/status`, restart notices, invalid approval edits, cancellation, and turn completion.

```rust
#[test]
fn invalid_command_is_a_transcript_error_not_an_activity_label() {
    let mut app = disconnected_app();
    app.submit_command("/definitely-missing");
    assert_eq!(app.state.activity(), &ActivityState::Idle);
    assert!(app.state.transcript.iter().any(|entry| matches!(entry, TranscriptEntry::Error { body } if body.contains("unknown or invalid command"))));
}
```

- [ ] **Step 2: Run focused bootstrap tests and confirm failure**

Run: `cargo test --locked -p kurama-cli --test bootstrap invalid_slash_commands_report_status_without_exiting`

Expected: the old status-string assertion or renamed behavior fails.

- [ ] **Step 3: Route feedback through transcript entries**

Replace direct `state.status = ...` assignments in `app.rs` and approval handling with `push_notice`, `push_error`, or typed activity transitions. `/status` and successful configuration commands emit notices; invalid commands and rejected local actions emit errors. Runtime errors remain single-emission transcript errors.

- [ ] **Step 4: Add active-only animation wakeups**

Inside `run_loop`, add a pinned future that sleeps for one frame only when `state.activity().is_animated()`; otherwise await `std::future::pending()`. Include it as one `tokio::select!` branch. A frame tick redraws but does not mutate transcript or enqueue commands. Idle state creates no timer wakeup.

```rust
let animation = async {
    if app.state.activity().is_animated() {
        tokio::time::sleep(ACTIVITY_FRAME_INTERVAL).await;
    } else {
        std::future::pending::<()>().await;
    }
};
tokio::pin!(animation);
```

- [ ] **Step 5: Run CLI library and integration tests**

Run: `cargo test --locked -p kurama-cli --lib && cargo test --locked -p kurama-cli --tests`

Expected: command routing, animation state, inline viewport, scrolling, approval, and bootstrap tests pass.

- [ ] **Step 6: Commit event integration**

```bash
git add crates/kurama-cli/src/app.rs crates/kurama-cli/src/tui/state.rs crates/kurama-cli/tests/bootstrap.rs crates/kurama-cli/tests/tui_flows.rs
git commit -m "refactor: drive tui feedback from runtime state"
```

### Task 5: Visual Audit And Release Verification

**Files:**
- Modify only if audit finds a scoped defect in files from Tasks 1-4.
- Create temporarily, then remove: `crates/kurama-cli/tests/visual_audit_tmp.rs`.

**Interfaces:**
- Verifies: normal idle, thinking, running tool, expanded transcript, approval, error, wide, medium, narrow, and short terminal states.
- Verifies: no full-screen background color, no clipping or overlap, and native selection assumptions remain intact.

- [ ] **Step 1: Format and run focused checks**

Run:

```bash
cargo fmt --all -- --check
cargo test --locked -p kurama-cli --tests
cargo clippy --locked -p kurama-cli --all-targets --all-features -- -D warnings
```

Expected: all commands pass.

- [ ] **Step 2: Perform one visual verification audit**

Create a temporary ignored-by-final-diff test that renders a scenario gallery through `TestBackend` and prints semantic terminal frames at `120x32`, `80x24`, `48x16`, and `32x10`. Include idle, thinking, running-tool output, inline approval, error, and expanded transcript states. Run it with:

```bash
cargo test --locked -p kurama-cli --test visual_audit_tmp -- --nocapture
```

Inspect every frame for hierarchy, wrapping, clipping, overlap, excessive chrome, visible background fill, shortcut placement, and action visibility. Make only scoped corrections, rerun the gallery, then delete `visual_audit_tmp.rs`.

- [ ] **Step 3: Run full repository verification**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo build --locked --release -p kurama-cli
scripts/check-size.sh target/release/kurama
scripts/check-prompt-budget.sh target/release/kurama
```

Expected: all checks pass, the stripped binary is below 10 MiB, and the prompt bundle remains below 2,000 tokens.

- [ ] **Step 4: Inspect final scope and commit corrections**

Run `git status --short`, `git diff --check`, and `git diff --stat`. Confirm the temporary audit test is absent and no unrelated files changed. If the audit required corrections, commit them:

```bash
git add crates/kurama-cli/src crates/kurama-cli/tests
git commit -m "fix: polish responsive tui rendering"
```

- [ ] **Step 5: Record completion**

Report the commits, changed modules, visual audit sizes/states, test commands, release binary size, prompt token count, and whether anything remains uncommitted. Do not push.
