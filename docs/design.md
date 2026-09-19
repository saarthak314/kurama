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
- Child launch order follows plan registration order, not random ID sorting. SDK artifact collection reuses the completed child's replay rather than loading it again. In-memory and filesystem stores both reject duplicate session creation and report the latest parent/child event timestamp.
- Configuration updates hold stable transaction locks; bootstrap remembers profile/session/mode in one durable transaction. Session creation publishes a complete synced directory and syncs its parents. Clean log scans use shared locks, reacquiring and rechecking under an exclusive lock before repair.
- Unix filesystem operations retain directory descriptors and reject symlink redirection. Cooperating writers serialize on the parent directory, without creating project lock files. This is not an atomic snapshot/CAS guarantee against unrelated writers modifying opened files or ignoring locks. Unsupported platforms fail closed.
- Reads stream hash/UTF-8 validation while retaining only the requested range. Deduplicated blobs are verified in chunks. Read/write transactions run off the async scheduler with cancellation checkpoints; an already-committed rename never becomes a cancelled result. Individual filesystem syscalls and diff computation are not forcibly interruptible.
- Display staging is lazy and disposable, with no staging fsync. Canonical sessions, configuration, and blobs retain their durability syncs. Tool output ceilings include notes and labels, while raw recovery streams retain their exact bytes.

## Terminal interface

The TUI owns one full-screen alternate buffer from startup through shutdown. The composer and footer stay anchored at the bottom; the transcript uses the remaining height. Startup clears the application canvas, and subsequent frame diffs remove stale cells without clearing every frame. Overlays share that buffer rather than entering nested alternate screens. Exiting restores the previous shell screen, cursor, line wrapping, and paste mode before printing resume details; shell scrollback is never purged. Startup and resize do not request cursor-position reports. Empty terminal geometry preserves transcript state until rows return.

The interface uses terminal-default foreground/background colors, restrained cyan accents, and an unboxed workspace-first masthead. The version is optional; model and safety mode keep priority on narrow terminals. Assistant prose is unboxed, tool rows show explicit lifecycle status, and the ASCII `> ` composer sits between thin horizontal rules. Transcript, tool output, and input reflow to the current width. Normal footers use a hints row above branch/mode/context; long input and approvals reserve footer space while very short terminals prioritize the operation and decision controls. `?` opens shortcuts, `/` opens commands, `Ctrl+O` expands the transcript, and `Ctrl+T` opens the todo list. Pickers and todos keep the selected item visible at narrow heights. Remaining context is derived from the latest reported input usage, not cumulative session usage.

Code blocks display highlighted code without backtick fences or language labels. Older assistant responses end with a full-width muted separator and one blank row on either side; the latest response does not. Markdown horizontal rules use the available content width too. Clickable HTTP(S) and mail links use a distinct blue and underline, including wrapped labels and web URLs in tool output, and open through the system URL handler on a plain left click. Dragging from a link selects its visible text instead of opening it. Unsafe schemes, control-bearing targets, and targets longer than 4 KiB stay plain text.

The mouse wheel scrolls transcript output in both normal and expanded views without changing the composer draft or prompt history. Left-drag highlights text and copies it on release. A high-contrast green `Copied N chars` badge confirms the count of Unicode characters copied, including newlines; `Esc` clears selection and feedback. The same badge appears in normal and expanded views and after `/copy`. Selected text preserves Unicode graphemes, indentation, and multiline order. The transcript stays visually stable while the pointer is held even as the engine continues processing events. `Ctrl+L` returns the normal view to the latest output. Mouse reporting includes motion only while a button is held, never all-pointer motion, and is restored on exit. Opening a link does not block terminal input while the desktop handler runs.

In the input bar, click to position the caret or drag to select and copy text. Typing or pasting replaces an input selection. Wrapped and multiline drafts keep stable hit positions and whole Unicode characters. Up/Down recall previous/next prompts, including the resumed session's history; returning past the newest prompt restores the unfinished draft and its caret. Alt+Up/Down move within multiline input, while file and command pickers retain their arrow-key navigation.

Home/End operate on the current logical input line; Ctrl+A/E retain whole-buffer movement. Onboarding fields support the same grapheme-safe middle editing and keep secrets masked. Shortcuts and agent transcripts scroll at short heights; agent lists support page/Home/End navigation. Expanded transcript Tab/Shift+Tab selects visible links, Enter opens them, and `y` copies their targets.

File completion is indexed off the render thread, refreshed on reopening a mention or workspace-mutating tools, and cooperatively cancelled when its app loop ends. Admission is deterministic and bounded to 400 files/directories and depth six; useful dot directories such as `.github` are included. Image paths and base64 are checked against the 8 MiB limit before payload allocation; failed raw-base64 guesses still allow valid image filenames.

Enter submits a new prompt when idle and steers the active turn while working. Steering is applied between complete model/tool/delegation batches, not halfway through a tool call or an internal provider retry. Alt+Enter queues a separate follow-up. `/queue` selects, edits, and deletes follow-ups; Enter saves an edit without turning it into steering, and Esc restores the original draft. Interrupted queues stay paused until `s` resumes them. Both pending-input queues are bounded to 32 entries and 256 KiB; rejected or unapplied cancelled steering returns to the draft instead of running automatically. Applied steering is recorded in the current durable turn.

`/context` shows an on-demand snapshot from the same assembler used for model requests: estimated tokens by category, effective input/output reserves, included and omitted completed-turn counts, summary coverage, and the next compaction request's coverage and budget fit. `r` refreshes; arrows and page keys scroll. Inspection never contacts the provider or compacts automatically. The ordinary footer labels provider usage as the last request, not the current assembled context.

`/diff` asynchronously reviews separate staged, unstaged, and ignored-aware untracked changes. `n`/`p` or Tab/Shift+Tab select changes; arrows and page keys scroll. Enter puts the exact selected text hunk and its path/range in an editable feedback draft; sending it uses normal prompt/steering behavior, and Esc restores the previous draft. Binary and metadata-only changes remain visible without fabricated text hunks. Non-UTF-8 hunks cannot generate lossy feedback. Review limits fail visibly rather than silently dropping changes: 4 MiB total patches, 2 MiB per untracked file, 256 KiB per hunk, and 512 file changes.

Empty composers draw one of twelve short prompts selected once from the runtime's randomized session ID. The choice stays stable through redraws, typing, completed turns, and resume; it is display text, never submitted input. No per-frame randomness, animation, extra dependency, or persisted preference is needed. The terminal gate recognizes the visible composer cursor/prompt rather than a particular placeholder sentence and checks that the displayed prompt remains stable.

The renderer caches wrapped transcript rows for the current width and detail mode. Streaming and todo updates invalidate the changed suffix while retaining unchanged entries; scrolling changes only the visible slice. Ignored input events do not force redraws. Compact tool previews retain bounded rows instead of formatting and allocating the entire output. Terminal control sequences are filtered before text layout, and composer cursor movement/deletion respects grapheme boundaries. The wrapped-row cache trades some memory for substantially less repeated parsing; full session history remains retained.

Filesystem-backed completed tool output retains a verified tail of at most 128 KiB in the normal view. Resume hydrates borrowed events without cloning the complete replay or materializing every display blob. `Ctrl+O` loads full verified output; collapse restores the bounded preview and discards the expanded render data. Blob verification still scans all bytes in bounded chunks, so this reduces allocation and retained memory, not integrity checks or verification I/O. Live output remains capped for every caller; embedded callers without a retrieval store retain full completion text when supplied.

Every input event gives ready runtime/tool events a bounded processing opportunity, including ignored input. Queued tool deltas precede canonical completion. Input already queued when an approval opens stays in its previous UI context, including keys buffered during resize; it cannot authorize the new prompt. Fresh approval responses and cancellation remain available.

### Reproducing terminal checks

```sh
cargo build --locked --release -p kurama-cli --bin kurama --examples
target/release/examples/tui_bench --turns 1000 --frames 100 --repetitions 5 --width 100 --height 36
uv run --with pyte scripts/check-tui.py target/release/kurama target/verification/tui-normal --no-images --check-controls
uv run --with pyte scripts/check-tui.py target/release/kurama target/verification/tui-no-cpr --no-images --no-cpr --seed-todos --check-controls
uv run --with pyte scripts/check-tui.py target/release/kurama target/verification/tui-recovery --no-images --torn-tail --seed-large-output-mib 8 --check-compaction --check-controls
uv run --with pyte scripts/check-tui.py target/release/kurama target/verification/tui-vt100 --no-images --term vt100 --check-controls
python3 scripts/bench-startup.py target/release/kurama
python3 scripts/bench-idle.py target/release/kurama
python3 scripts/bench-harness.py --bin-dir target/release/examples --output-dir target/verification/harness-run --include-adapters
python3 scripts/test-verification.py
```

The POSIX terminal gate drives the production binary through a PTY and a local SSE provider. It exercises multiline paste, read/write approval, Markdown and tool-output wrapping at 48×14, 72×18, and 120×40, narrower approval layouts, transcript browsing, draft preservation across overlay resize/dismissal, long output, and optional resumed todo navigation at 24×6. It requires bottom-anchored input/footer, exact write contents, successful tool results, one alternate-screen entry/exit, restored shell content and terminal modes, no cursor-position queries, and no scrollback purge. Add `--with pillow` to the `uv` invocation and omit `--no-images` for screenshots decoded from the PTY stream; these are not native terminal-window captures. These Python dependencies are development-only.

The same gate checks fence-free code, real SGR click/drag/release handling, exact copied text and URL dispatch, visible selection feedback, drag-versus-click behavior, wheel navigation without draft changes, narrow resize while browsing, older-response separators, no latest-response separator, and restored mouse reporting modes. Isolated clipboard and URL-handler executables prevent desktop side effects in CI. Unit regressions cover link hit targets, Unicode selection, stale coordinates, selection during streaming, separator spacing, and incremental cache invalidation.

`--check-controls` checks actual provider requests for hunk feedback, safe-boundary steering, editable/deletable follow-ups, cancelled input restoration, and model-free context inspection. It also exercises short-height shortcuts, keyboard links, refreshed file mentions, bounded image attachment, onboarding/input editing, and live agent list/transcript navigation. The harness creates isolated PTYs, not desktop terminal windows. It closes process groups, PTY descriptors, and the fixture server on success, failure, and setup errors. Output directories must be empty so screenshots cannot silently survive from an older run.

The combined recovery scenario seeds a complete tool lifecycle and a large display blob, adds a torn log suffix, resumes, expands/collapses full output, compacts twice around another turn, then exercises real read/write approval and transcript controls. It checks contiguous repaired history, exactly one repair, and the first decision in both compaction model requests and the final durable summary. It also reports observed composer readiness and process RSS before expansion, while expanded, and after collapse; fixture construction is outside the readiness interval.

`tui_bench` measures the production `App` event loop for scrolling, ignored events, history/file palettes, and interleaved tool streaming/input. Its separately labelled `compact_tool_preview` case is a full transcript rebuild, not cached steady-state rendering. `--width/--height` allow identical narrow/wide comparisons. `adapter_bench` covers fragmented/burst SSE decoding, tag-dense HTML, large request construction, small/large output bounds, and a first-line read that still hashes a 16 MiB file. Compare identical benchmark source/release settings, alternate retained binaries sequentially, and retain raw samples. These isolate local overhead, not model or network latency.

PR CI runs one locked all-features workspace suite and actual PTY controls. Nightly tests real `xterm-256color`, `screen-256color`, and `vt100` PTYs on Linux/macOS, plus repair/large-output/compaction. Startup samples restore identical configured state (not cold OS caches); idle measurements drain the PTY and use process-CPU deltas over three windows. The benchmark runner retains raw output, exact-child CPU/peak RSS, hashes, provenance, and failure/timeout records, and cleans residual process groups before the next benchmark. It does not claim accounting for unawaited descendants or impose fragile absolute rendering thresholds on shared runners.

The terminal layout takes cues from [Pi's interactive interface](https://github.com/earendil-works/pi/tree/main/packages/coding-agent#interactive-mode): progressive tool disclosure, unboxed assistant text, and a restrained operational footer. Kurama retains its own workspace identity, explicit approvals, tool set, and keyboard conventions rather than copying another agent's branding or permission model. Transcript browsing stays in the application; the original shell screen and scrollback return on exit.

## Research basis and tradeoffs

These are established design patterns, not a universal agent-orchestration standard:

- [Anthropic's multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system) uses a lead orchestrator, isolated specialists, bounded briefs, and compact results. It also documents the token cost and coordination limits of additional agents. Kurama retains its explicit delegation gate, depth-one limit, scope checks, and bounded waves rather than increasing fan-out indiscriminately.
- [OpenAI Agents SDK orchestration](https://openai.github.io/openai-agents-python/multi_agent/) distinguishes manager-owned specialist work from handoffs and recommends code-driven control when predictability matters. Kurama keeps policy, dependencies, approvals, and resource limits in deterministic code rather than handing those decisions to the model.
- [Temporal workflow execution](https://docs.temporal.io/workflow-execution) separates event history from cached execution state and relies on deterministic replay. Kurama applies the local equivalent: serialized durable logs plus replayable in-memory projections, without adding a workflow service or database.
- [Tokio channels](https://tokio.rs/tokio/tutorial/channels) provide bounded backpressure; [graceful shutdown](https://tokio.rs/tokio/topics/shutdown) requires signalling and then waiting. [OpenAI's `gather_with_cancel`](https://github.com/openai/openai-agents-python/blob/main/src/agents/util/_asyncio_tasks.py) likewise cancels and drains sibling tasks on failure. Kurama retains bounded event delivery and the existing child cleanup grace instead of dropping authoritative events to claim throughput.
- [WHATWG SSE parsing](https://html.spec.whatwg.org/multipage/server-sent-events.html#parsing-an-event-stream) specifies UTF-8, a leading BOM, and CR/LF/CRLF line endings. The bounded incremental decoder handles those wire forms without repeatedly scanning or copying the buffered prefix. Provider completion validation remains separate from wire decoding.
- [Tokio cancellation safety](https://docs.rs/tokio/latest/tokio/macro.select.html#cancellation-safety) distinguishes safe receive operations from operations that lose progress when dropped. Provider streams retain one owned cancellation future; subprocess cancellation signals and drains the process group rather than merely dropping a reader.
- [Directory-relative filesystem APIs](https://man7.org/linux/man-pages/man2/open.2.html) retain a stable directory reference across renames. Kurama combines no-follow component traversal, descriptor-relative publication, and cooperating-writer locks. This narrows pathname races without claiming isolation from arbitrary external writers.

Blanket parallel tool execution, asynchronous parent/child synthesis, background persistence, and a distributed scheduler are intentionally not part of this optimization. Each changes ordering, approval, recovery, or resource semantics; benchmark gains alone would not establish their safety. Slow event consumers still exert backpressure, full history is still retained in memory, and explicit resume still validates/replays the durable log.

## Reproducing control-plane measurements

```sh
cargo build --locked --release -p kurama-cli --example control_plane_bench
target/release/examples/control_plane_bench --repetitions 10 --warmups 2
cargo test -p kurama-cli --test control_plane --test smoke
cargo test -p kurama-adapters --all-features --test fs_store
```

The offline benchmark uses production SDK orchestration, filesystem sessions, and atomic file writes; delayed streams use a loopback SSE provider. It measures warmed live turns after 100/1,000/5,000 completed turns, short-response visibility, two concurrent child tool loops, and a 16-round durable write loop. Setup, history seeding, initial replay, and end-state validation are outside the measured interval. JSON retains raw samples and correctness failures; successful-run latency statistics exclude failures explicitly. Compare the identical benchmark source and release settings on both revisions, run binaries sequentially in alternating order, and keep the machine/filesystem constant. These measurements isolate harness overhead and local I/O, not real-model reasoning speed or answer quality.

### Repository audit measurements

The September 2026 audit compares retained `a4b1956` binaries with the audit implementation, not with the original `origin/main` baseline. All 16 sequential ABBA runs passed their correctness checks. Each latency below is the median of ten measured samples per variant, with identical benchmark source and release settings on one macOS arm64 host. Local records are under `target/verification/paired-performance/`; CI retains fresh raw measurements as workflow artifacts.

| Workload | Before | After |
| --- | ---: | ---: |
| SSE, 32 KiB delivered one byte at a time | 2,937.80 ms | 0.45 ms |
| SSE, burst of 10,000 records | 26.37 ms | 1.03 ms |
| Tag-dense 2 MiB HTML extraction | 27.06 ms | 19.07 ms |
| Large compatible-provider request construction | 0.317 ms | 0.184 ms |
| Small output with staging requested | 3.875 ms | 0.0013 ms |
| Bounded 1 MiB output | 3.346 ms | 0.525 ms |
| First-line read, hashing the full 16 MiB file | 27.95 ms | 16.18 ms |
| History palette, 100×36 | 16.08 ms | 10.11 ms |
| History palette, 48×14 | 10.69 ms | 4.63 ms |
| 100 scroll steps, 100×36 | 80.67 ms | 83.09 ms |
| 100 scroll steps, 48×14 | 79.88 ms | 73.23 ms |
| Warm live turn after 100 completed turns | 9.79 ms | 10.69 ms |
| Warm live turn after 5,000 completed turns | 10.49 ms | 9.73 ms |
| Two-child orchestration | 511.06 ms | 506.48 ms |
| 16-round durable write loop | 628.05 ms | 626.41 ms |

The adapter stress process's median peak RSS across its two runs fell from 746.6 MiB to 18.7 MiB. This is a mixed synthetic workload, not ordinary CLI memory usage. The byte-fragmented SSE and small-staging cases deliberately expose pathological rescanning and unnecessary filesystem work; their speedups do not represent end-to-end model latency. Wide scrolling and the short-history live turn were slower in this run; other control-plane timings were essentially unchanged. No uniform speedup or statistical significance is claimed.

The audit removes redundant CI suite invocations, fixture-only production APIs, operation counters, unused schema I/O, duplicate approval state, and tests of incidental wording. The replacement verification path uses production imports, isolated local providers, actual PTYs, retained failure/resource records, and a process-cleanup regression suite. It adds no daemon, watcher, distributed scheduler, or general-purpose testing framework.

Separate ABBA startup/idle runs used the same restored, valid configured fixture for both binaries. Spawn-to-first-terminal-byte latency rose from 53.65 ms to 77.80 ms (30 samples each), remaining below the existing 100 ms gate; this is a startup regression, not a gain. Median idle RSS was 6,232 versus 6,240 KiB, with 0.0% measured process CPU in both variants across six five-second windows each (macOS CPU-time resolution: 0.01 s). All eight runner invocations passed and completed cleanup. These are neither cold-OS-cache nor full-composer-readiness measurements.
