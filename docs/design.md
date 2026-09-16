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
- Replay returns a consistent post-repair snapshot: a torn-tail `RecoveryRepair` is included immediately, along with the updated sequence and session timestamp. The next parent or child append does not require a second replay.
- Recovery distinguishes a log's owner from its descendants. A child's own `AgentStarted`/progress records are not evidence of an interrupted descendant; the parent still marks unfinished children interrupted on resume.
- The canonical in-memory event history remains authoritative for model context. Turn ranges, latest user/goal/todo positions, and evidence positions are maintained on append and rebuilt on replay. Live delegation detection does not reread the filesystem log. Recent completed turns fit as whole units or are omitted; current-turn content and the active goal keep their budget priority.
- Explicit compaction carries the active summary as model input data alongside newly eligible events, excluding superseded compaction records. Both count toward the input allowance; oversized requests fail before contacting the backend or changing durable coverage. Compaction remains explicit, not automatic.
- Assistant text flushes at a UTF-8-safe 4 KiB boundary or a 50 ms pending-text deadline, and at response termination. Chunking copies each emitted byte once rather than repeatedly moving the remaining suffix. No timer runs while the buffer is empty. Provider adapters may buffer their own control responses; runtime consumers must drain the bounded event channel. Received token usage is persisted before fallible display delivery.

## Terminal interface

The TUI owns one full-screen alternate buffer from startup through shutdown. The composer and footer stay anchored at the bottom; the transcript uses the remaining height. Startup clears the application canvas, and subsequent frame diffs remove stale cells without clearing every frame. Overlays share that buffer rather than entering nested alternate screens. Exiting restores the previous shell screen, cursor, line wrapping, and paste mode before printing resume details; shell scrollback is never purged. Startup and resize do not request cursor-position reports. Empty terminal geometry preserves transcript state until rows return.

The interface uses terminal-default foreground/background colors, restrained cyan accents, and an unboxed workspace-first masthead. The version is optional; model and safety mode keep priority on narrow terminals. Assistant prose is unboxed, tool rows show explicit lifecycle status, and the ASCII `> ` composer sits between thin horizontal rules. Transcript, tool output, and input reflow to the current width. Normal footers use a hints row above branch/mode/context; long input and approvals reserve footer space while very short terminals prioritize the operation and decision controls. `?` opens shortcuts, `/` opens commands, `Ctrl+O` expands the transcript, and `Ctrl+T` opens the todo list. Pickers and todos keep the selected item visible at narrow heights. Remaining context is derived from the latest reported input usage, not cumulative session usage.

Code blocks display highlighted code without backtick fences or language labels. Older assistant responses end with a full-width muted separator and one blank row on either side; the latest response does not. Markdown horizontal rules use the available content width too. Clickable HTTP(S) and mail links use a distinct blue and underline, including wrapped labels and web URLs in tool output, and open through the system URL handler on a plain left click. Dragging from a link selects its visible text instead of opening it. Unsafe schemes, control-bearing targets, and targets longer than 4 KiB stay plain text.

The mouse wheel scrolls transcript output in both normal and expanded views without changing the composer draft or prompt history. Left-drag highlights text and copies it on release. A high-contrast green `Copied N chars` badge confirms the count of Unicode characters copied, including newlines; `Esc` clears selection and feedback. The same badge appears in normal and expanded views and after `/copy`. Selected text preserves Unicode graphemes, indentation, and multiline order. The transcript stays visually stable while the pointer is held even as the engine continues processing events. `Ctrl+L` returns the normal view to the latest output. Mouse reporting includes motion only while a button is held, never all-pointer motion, and is restored on exit. Opening a link does not block terminal input while the desktop handler runs.

In the input bar, click to position the caret or drag to select and copy text. Typing or pasting replaces an input selection. Wrapped and multiline drafts keep stable hit positions and whole Unicode characters. Up/Down recall previous/next prompts, including the resumed session's history; returning past the newest prompt restores the unfinished draft and its caret. Alt+Up/Down move within multiline input, while file and command pickers retain their arrow-key navigation.

Empty composers draw one of twelve short prompts selected once from the runtime's randomized session ID. The choice stays stable through redraws, typing, completed turns, and resume; it is display text, never submitted input. No per-frame randomness, animation, extra dependency, or persisted preference is needed. The terminal gate recognizes the visible composer cursor/prompt rather than a particular placeholder sentence and checks that the displayed prompt remains stable.

The renderer caches wrapped transcript rows for the current width and detail mode. Streaming and todo updates invalidate the changed suffix while retaining unchanged entries; scrolling changes only the visible slice. Ignored input events do not force redraws. Compact tool previews retain bounded rows instead of formatting and allocating the entire output. Terminal control sequences are filtered before text layout, and composer cursor movement/deletion respects grapheme boundaries. The wrapped-row cache trades some memory for substantially less repeated parsing; full session history remains retained.

Filesystem-backed completed tool output retains a verified tail of at most 128 KiB in the normal view. Resume hydrates borrowed events without cloning the complete replay or materializing every display blob. `Ctrl+O` loads full verified output; collapse restores the bounded preview and discards the expanded render data. Blob verification still scans all bytes in bounded chunks, so this reduces allocation and retained memory, not integrity checks or verification I/O. Live output remains capped for every caller; embedded callers without a retrieval store retain full completion text when supplied.

Every input event gives ready runtime/tool events a bounded processing opportunity, including ignored input. Queued tool deltas precede canonical completion. Input already queued when an approval opens stays in its previous UI context, including keys buffered during resize; it cannot authorize the new prompt. Fresh approval responses and cancellation remain available.

### Reproducing terminal checks

```sh
cargo build --locked --release -p kurama-cli --bin kurama --example tui_bench --example control_plane_bench
target/release/examples/tui_bench --turns 1000 --frames 100 --repetitions 5
uv run --with pyte scripts/check-tui.py target/release/kurama .lavish/tui-check --no-images
uv run --with pyte scripts/check-tui.py target/release/kurama .lavish/tui-no-cpr --no-images --no-cpr --seed-todos
uv run --with pyte scripts/check-tui.py target/release/kurama .lavish/tui-recovery --no-images --torn-tail --seed-large-output-mib 8 --check-compaction
python3 scripts/bench-startup.py target/release/kurama
python3 scripts/bench-idle.py target/release/kurama
python3 scripts/bench-harness.py --bin-dir target/release/examples --output-dir .lavish/harness-measurements
```

The POSIX terminal gate drives the production binary through a PTY and a local SSE provider. It exercises multiline paste, read/write approval, Markdown and tool-output wrapping at 48×14, 72×18, and 120×40, narrower approval layouts, transcript browsing, draft preservation across overlay resize/dismissal, long output, and optional resumed todo navigation at 24×6. It requires bottom-anchored input/footer, exact write contents, successful tool results, one alternate-screen entry/exit, restored shell content and terminal modes, no cursor-position queries, and no scrollback purge. Add `--with pillow` to the `uv` invocation and omit `--no-images` for screenshots decoded from the PTY stream; these are not native terminal-window captures. These Python dependencies are development-only.

The same gate checks fence-free code, real SGR click/drag/release handling, exact copied text and URL dispatch, visible selection feedback, drag-versus-click behavior, wheel navigation without draft changes, narrow resize while browsing, older-response separators, no latest-response separator, and restored mouse reporting modes. Isolated clipboard and URL-handler executables prevent desktop side effects in CI. Unit regressions cover link hit targets, Unicode selection, stale coordinates, selection during streaming, separator spacing, and incremental cache invalidation.

The combined recovery scenario seeds a complete tool lifecycle and a large display blob, adds a torn log suffix, resumes, expands/collapses full output, compacts twice around another turn, then exercises real read/write approval and transcript controls. It checks contiguous repaired history, exactly one repair, and the first decision in both compaction model requests and the final durable summary. It also reports observed composer readiness and process RSS before expansion, while expanded, and after collapse; fixture construction is outside the readiness interval.

`tui_bench` measures the production `App` event loop with deterministic input and Ratatui's `TestBackend`, plus the compact-preview renderer. Compare identical benchmark source and release settings, run sequentially in alternating order, and retain raw samples. It isolates rendering work, not real-terminal paint time or model latency. The startup gate measures first terminal bytes, not interactive readiness; the PTY gate separately records composer readiness and verifies startup without cursor-position reports. The idle gate requires a live process before accepting RSS/CPU samples.

PR CI runs the real PTY checks with normal cursor reports and without reports plus resumed todos. Nightly runs actual PTYs on Linux and macOS, including the combined repair/large-output/compaction scenario. It also runs the release control-plane and rendering benchmarks sequentially through `bench-harness.py`. The standard-library runner retains exact stdout/stderr, per-child CPU and peak RSS, binary hashes, and host/toolchain provenance; malformed results, child failures, and watchdog timeouts fail the run without skipping the second benchmark. Raw artifacts upload even on failure. Shared runners have no brittle absolute rendering-latency threshold.

The terminal layout takes cues from [Pi's interactive interface](https://github.com/earendil-works/pi/tree/main/packages/coding-agent#interactive-mode): progressive tool disclosure, unboxed assistant text, and a restrained operational footer. Kurama retains its own workspace identity, explicit approvals, tool set, and keyboard conventions rather than copying another agent's branding or permission model. Transcript browsing stays in the application; the original shell screen and scrollback return on exit.

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
