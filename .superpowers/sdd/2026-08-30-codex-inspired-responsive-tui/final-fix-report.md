# Final Fix Report

- Date: 2026-08-30
- Status: DONE
- Implementation commit: `440269ca3044d30a0f52ff005221a0a02f90e7a6`

## Root Causes

1. `read` and web-open bounded their model-visible strings without creating a staged complete-output source. The engine could persist Bash staging metadata, but emitted the durable bounded result unchanged and CLI replay only recognized Bash `stdout`/`stderr` blob keys.
2. `EngineActor::stream_round` flushed its sub-4 KiB assistant buffer only on clean EOF. Stream errors and command cancellation returned before flushing or appending the partial assistant message.
3. The TUI reducer treated terminal `Status` events as animated work and treated `ToolCompleted` as the end of a turn, despite the next model round still being active.
4. Transcript text and code used fixed RGB/amber foregrounds, while running-agent counters and state used failure red.
5. Bash converted each raw read independently with `String::from_utf8_lossy`, corrupting valid multibyte code points split across read boundaries.

## RED Evidence

The focused regressions were run before production edits and failed for the expected reasons:

- `cargo test --locked -p kurama-core --test engine_loop tool_completion_keeps_durable_output_bounded_and_hydrates_live_display -- --exact`
  - Failed because live `ToolCompleted` had no `display_output`.
- `cargo test --locked -p kurama-core --test engine_loop stream_failure_flushes_and_persists_sub_threshold_assistant_text -- --exact`
  - Failed because no partial `AssistantDelta` was emitted.
- `cargo test --locked -p kurama-core --test engine_loop cancellation_flushes_and_persists_sub_threshold_assistant_text -- --exact`
  - Failed because no partial `AssistantDelta` was emitted.
- `cargo test --locked -p kurama-adapters --features tools --test file_tools read_supports_binary_byte_ranges_and_bounds_visible_output -- --exact`
  - Failed because truncated read output had no `_display_staging.output` path.
- `cargo test --locked -p kurama-adapters --features tools,http --test web_search_tool yolo_open_keeps_limits_but_allows_private_http -- --exact`
  - Failed because truncated web-open output had no `_display_staging.output` path.
- `cargo test --locked -p kurama-adapters --features tools --test bash_tool bash_stream_events_decode_split_utf8_and_remain_bounded -- --exact`
  - Failed because the split euro sign was emitted as replacement characters.
- `cargo test --locked -p kurama-cli --test tui_flows status_updates_append_notices_without_starting_activity -- --exact`
  - Failed because `Status` produced animated `Working` activity.
- `cargo test --locked -p kurama-cli --test tui_flows tool_events_preserve_complete_output_and_lifecycle -- --exact`
  - Failed because `ToolCompleted` returned to `Idle`.
- `cargo test --locked -p kurama-cli --test bootstrap resume_hydrates_the_visible_transcript_once -- --exact`
  - Failed because replay ignored the non-Bash `output` display blob.
- `cargo test --locked -p kurama-cli --test tui_structure assistant_markdown_renders_inline_styles_and_links -- --exact`
  - Failed because normal and inline-code foregrounds were fixed RGB values instead of `Color::Reset`.
- `cargo test --locked -p kurama-cli --test tui_structure assistant_markdown_renders_blocks_lists_code_quotes_rules_and_tables -- --exact`
  - Failed because fenced code used amber instead of `Color::Reset`.
- `cargo test --locked -p kurama-cli --test tui_structure running_agent_metadata_uses_the_active_accent_not_failure_red -- --exact`
  - Failed because running-agent state used `Color::Red` instead of the active accent.

## GREEN Evidence

Every focused RED command above passed after the implementation. Adjacent regression suites also passed:

- `cargo test --locked -p kurama-core --test engine_loop` — 17 passed.
- `cargo test --locked -p kurama-adapters --features tools,http --test file_tools --test bash_tool --test web_search_tool` — 25 passed.
- `cargo test --locked -p kurama-cli --test tui_flows --test tui_structure --test bootstrap --test smoke` — 88 passed.
- `cargo test --locked -p kurama-cli --lib` — 19 passed.

Mandated workspace verification passed:

- `cargo fmt --all -- --check`
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`
- `cargo test --locked --workspace --all-features`

## Changed Files

- `crates/kurama-core/src/engine.rs`
- `crates/kurama-core/tests/engine_loop.rs`
- `crates/kurama-adapters/src/tools/bash.rs`
- `crates/kurama-adapters/src/tools/limits.rs`
- `crates/kurama-adapters/src/tools/read.rs`
- `crates/kurama-adapters/src/tools/web_search.rs`
- `crates/kurama-adapters/tests/bash_tool.rs`
- `crates/kurama-adapters/tests/file_tools.rs`
- `crates/kurama-adapters/tests/web_search_tool.rs`
- `crates/kurama-cli/src/app.rs`
- `crates/kurama-cli/src/tui/composer.rs`
- `crates/kurama-cli/src/tui/render.rs`
- `crates/kurama-cli/src/tui/state.rs`
- `crates/kurama-cli/src/tui/transcript.rs`
- `crates/kurama-cli/tests/bootstrap.rs`
- `crates/kurama-cli/tests/tui_flows.rs`
- `crates/kurama-cli/tests/tui_structure.rs`
- `.superpowers/sdd/2026-08-30-codex-inspired-responsive-tui/final-fix-report.md`

## Self-Review

- Durable `ToolResult.output` remains bounded and model-visible context still receives only the bounded value. Complete display content is stored in content-addressed session blobs and hydrated only into transient runtime/replay copies.
- Read, web-open, and Bash staging allocation now fails explicitly instead of silently returning truncated output without a complete display source. Non-truncated staging files are removed immediately; engine-owned truncated staging files are removed through RAII after blob persistence, including error paths.
- Partial assistant text is flushed and appended exactly once on stream failure and command cancellation. Successful rounds retain their existing single persistence path.
- Bash decoding retains incomplete UTF-8 tails between reads, emits valid split code points intact, keeps invalid input lossy, and preserves the 4 KiB event bound.
- `Status` is a durable unlabeled notice with idle activity. `ToolCompleted` transitions to `Thinking` until a terminal runtime event establishes the next state.
- Normal transcript/code foregrounds use `Color::Reset`; active metadata uses cyan; yellow remains on approval/cancellation decisions; red is confined to failures.
- No commands or dependencies were added. No release-size or visual-gallery command was run, per the brief.
- Final diff audit found no staging-file leftovers, command-surface changes, dependency-manifest changes, or unrelated edits.

## Residual Fix: Truncated Staging Failure

### Status

Complete. Residual implementation commit: `6fe67923b91bcb3bcffdd2f50c46b8cc645d1df5`.

### Root Cause

`take_truncated_staging` checked `staged_path` before `staging_error`. When `BoundedOutput::finish` discarded the path after a staging write, flush, or sync failure, truncated read and web-open output therefore returned `Ok(None)` instead of failing without a complete display source.

### RED Evidence

- `cargo test --locked -p kurama-adapters --features tools tools::limits::tests::take_truncated_staging_rejects_staging_failures -- --exact`
  - Failed because `take_truncated_staging` returned `Ok(None)` for truncated output with `staging_error: Some("staging write failed")`.

### GREEN Evidence

- The focused RED command passed after the correction — 1 passed.
- `cargo test --locked -p kurama-adapters --features tools,http` — 27 passed.
- `cargo fmt --all -- --check` — passed.
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` — passed.

### Changed Files

- `crates/kurama-adapters/src/tools/limits.rs`
- `.superpowers/sdd/2026-08-30-codex-inspired-responsive-tui/final-fix-report.md`

### Self-Review

- The new failure path is gated by `bounded.truncated`, so a staging failure remains non-fatal when the complete output is already present in `BoundedText.text`.
- Read and web-open callers now propagate the recorded staging I/O failure rather than returning truncated output without a display blob source.
- The correction changes no commands, dependencies, durable output limits, or successful staging behavior.
