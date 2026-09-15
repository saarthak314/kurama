# Architecture

Kurama is one Rust process organized as a small dependency-directed workspace:

```text
kurama          ──────▶ kurama-sdk ─────▶ kurama-core ─────▶ kurama-protocol
kurama-cli      ──────▶ kurama-sdk
kurama-adapters ──────────────────────────────────────────▶ kurama-protocol
```

`kurama-protocol` defines dependency-light public events, commands, model requests, tools, policies, sessions, and extension traits. `kurama-core` owns the event-driven model/tool loop, bounded context, approvals, recovery, and depth-one child scheduling. `kurama-adapters` contains providers, official CLI bridges, filesystem storage, credentials, and the four environment tools. `kurama-sdk` composes those interfaces for embedders and supplies `Agent` with production defaults. `kurama` is the batteries-included crate (`Kurama::openai`, standard tools, filesystem sessions). `kurama-cli` renders the TUI and translates user input into engine commands; it never executes tools directly.

The runtime uses a Tokio current-thread scheduler. Terminal input blocks in one named OS thread and enters the async coordinator through a bounded channel. Model streams, tool output, approvals, persistence, and child progress are typed events. Nothing polls in the background, and there is no daemon or database.

Sessions are append-only JSONL. Parent and child logs are separate, while large outputs are content-addressed blobs. Resume replays durable events and evaluates each incomplete operation before any side effect: safe reads may retry, atomic writes compare pre/post hashes, and unknown Bash operations require a decision. Full history remains recoverable, but model requests contain only recent turns, compact summaries, referenced evidence, and bounded tool results.

Delegation is an explicit structured capability enabled only when the user asks for agents or parallel work. The parent remains the sole orchestrator and may run up to three delegation waves in one user turn; children cannot create children. They receive isolated briefs, write scopes, budgets, cancellation tokens, and logs. Researcher, planner, and reviewer children are read-only; implementers inherit or subset the parent write scope. At 80% of a child budget the parent asks it to wrap up; the hard limit cancels unless the child already produced a summary, which is kept. The parent receives compact results rather than copied transcripts.

The standard binary exposes exactly four environment tools: `read`, `write`, `bash`, and `web-search`. `todo` is a parent-only session list, not an environment tool. `/goal` is a parent-only persisted objective: Kurama keeps working across turns until the model marks it complete or blocked, or the user pauses or clears it. The transcript shows the live checklist and updates it in place; `/todo` opens the same list as a compact overlay. Provider adapters normalize OpenAI Responses, Anthropic Messages, OpenAI-compatible chat completions, Codex CLI, and Claude CLI into the same protocol stream. Local inference is connect-only through an existing compatible HTTP endpoint.

## Control-plane invariants

- The parent engine owns its session sequence. Child engines and their manager share a child log, so both use `SessionStore::append_next`: sequence allocation and durable append happen under one storage lock. `FsSessionStore` reads only the trailing record for allocation, serializes each new event into one buffer, and retains `sync_data` for every append. Explicit-sequence `append` still validates the supplied sequence.
- Recovery distinguishes a log's owner from its descendants. A child's own `AgentStarted`/progress records are not evidence of an interrupted descendant; the parent still marks unfinished children interrupted on resume.
- The canonical in-memory event history remains authoritative for model context. Turn ranges, latest user/goal/todo positions, and evidence positions are maintained on append and rebuilt on replay. Live delegation detection does not reread the filesystem log. Recent completed turns fit as whole units or are omitted; current-turn content and the active goal keep their budget priority.
- Assistant text flushes at a UTF-8-safe 4 KiB boundary or a 50 ms pending-text deadline, and at response termination. Chunking copies each emitted byte once rather than repeatedly moving the remaining suffix. No timer runs while the buffer is empty. Provider adapters may buffer their own control responses; runtime consumers must drain the bounded event channel. Received token usage is persisted before fallible display delivery.

## Terminal interface

The primary screen stays inline with native shell scrollback; expanded transcript browsing and full-height pickers use the alternate screen. Dismissing or resizing an overlay restores the inline composer without purging scrollback. Terminals without cursor-position reports fall back to a fresh bottom row after Crossterm's query timeout rather than failing startup. Empty terminal geometry never commits unseen transcript rows.

The interface uses terminal-default foreground/background colors, restrained cyan accents, a compact model/directory/approval header, and a borderless composer. `?` opens shortcuts, `/` opens commands, `Ctrl+O` expands the transcript, and `Ctrl+T` opens the todo list. Pickers and todos keep the selected item visible at narrow heights. Remaining context is derived from the latest reported input usage, not cumulative session usage.

The renderer caches wrapped transcript rows for the current width and detail mode. Streaming and todo updates invalidate the changed suffix while retaining unchanged entries; scrolling changes only the visible slice. Ignored input events do not force redraws. Compact tool previews retain bounded rows instead of formatting and allocating the entire output. Terminal control sequences are filtered before text layout, and composer cursor movement/deletion respects grapheme boundaries. The wrapped-row cache trades some memory for substantially less repeated parsing; full session history remains retained.

### Reproducing terminal checks

```sh
cargo build --locked --release -p kurama-cli --bin kurama --example tui_bench
target/release/examples/tui_bench --turns 1000 --frames 100 --repetitions 5
uv run --with pyte scripts/check-tui.py target/release/kurama .lavish/tui-check --no-images
uv run --with pyte scripts/check-tui.py target/release/kurama .lavish/tui-fallback --no-images --no-cpr --seed-todos
python3 scripts/bench-startup.py target/release/kurama
python3 scripts/bench-idle.py target/release/kurama
```

The POSIX terminal gate drives the production binary through a PTY and a local SSE provider. It exercises multiline paste, read/write approval, Markdown, resize, transcript browsing/restoration, long output, and optional resumed todo navigation. It requires exact write contents, successful tool results, clean exit, preserved shell history, and no scrollback purge. Add `--with pillow` to the `uv` invocation and omit `--no-images` for screenshots decoded from the PTY stream; these are not native terminal-window captures. `--baseline` permits old scrollback behavior when capturing a before comparison. These Python dependencies are development-only.

`tui_bench` measures the production `App` event loop with deterministic input and Ratatui's `TestBackend`, plus the compact-preview renderer. Compare identical benchmark source and release settings, run sequentially in alternating order, and retain raw samples. It isolates rendering work, not real-terminal paint time or model latency. The startup gate measures first terminal bytes, not interactive readiness; a terminal that does not answer cursor-position queries can still incur the fallback timeout. The idle gate requires a live process before accepting RSS/CPU samples.

The visual direction takes cues from [Codex's terminal ownership](https://github.com/openai/codex/blob/main/codex-rs/tui/src/tui.rs) and [composer implementation](https://github.com/openai/codex/blob/main/codex-rs/tui/src/bottom_pane/chat_composer.rs): separate inline history from overlays and keep editing in a dedicated bottom pane. Kurama applies restrained visual chrome while retaining its own tools, approval semantics, and keyboard conventions.

## Research basis and tradeoffs

These are established design patterns, not a universal agent-orchestration standard:

- [Anthropic's multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system) uses a lead orchestrator, isolated specialists, bounded briefs, and compact results. It also documents the token cost and coordination limits of additional agents. Kurama retains its explicit delegation gate, depth-one limit, scope checks, and bounded waves rather than increasing fan-out indiscriminately.
- [OpenAI Agents SDK orchestration](https://openai.github.io/openai-agents-python/multi_agent/) distinguishes manager-owned specialist work from handoffs and recommends code-driven control when predictability matters. Kurama keeps policy, dependencies, approvals, and resource limits in deterministic code rather than handing those decisions to the model.
- [Temporal workflow execution](https://docs.temporal.io/workflow-execution) separates event history from cached execution state and relies on deterministic replay. Kurama applies the local equivalent: serialized durable logs plus replayable in-memory projections, without adding a workflow service or database.
- [Tokio channels](https://tokio.rs/tokio/tutorial/channels) provide bounded backpressure; [graceful shutdown](https://tokio.rs/tokio/topics/shutdown) requires signalling and then waiting. [OpenAI's `gather_with_cancel`](https://github.com/openai/openai-agents-python/blob/main/src/agents/util/_asyncio_tasks.py) likewise cancels and drains sibling tasks on failure. Kurama retains bounded event delivery and the existing child cleanup grace instead of dropping authoritative events to claim throughput.

Blanket parallel tool execution, asynchronous parent/child synthesis, background persistence, and a distributed scheduler are intentionally not part of this optimization. Each changes ordering, approval, recovery, or resource semantics; benchmark gains alone would not establish their safety. Slow event consumers still exert backpressure, full history is still retained in memory, and explicit resume still validates/replays the durable log.

## Reproducing control-plane measurements

```sh
cargo build --locked --release -p kurama-cli --example control_plane_bench
target/release/examples/control_plane_bench --repetitions 10 --warmups 2
cargo test -p kurama-cli --test control_plane --test smoke
cargo test -p kurama-adapters --all-features --test fs_store
```

The offline benchmark uses production SDK orchestration, filesystem sessions, and atomic file writes; delayed streams use a loopback SSE provider. It measures warmed live turns after 100/1,000/5,000 completed turns, short-response visibility, two concurrent child tool loops, and a 16-round durable write loop. Setup, history seeding, initial replay, and end-state validation are outside the measured interval. JSON retains raw samples and correctness failures; successful-run latency statistics exclude failures explicitly. Compare the identical benchmark source and release settings on both revisions, run binaries sequentially in alternating order, and keep the machine/filesystem constant. These measurements isolate harness overhead and local I/O, not real-model reasoning speed or answer quality.
