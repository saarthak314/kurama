# Codex-Inspired Responsive TUI Design

## Goal

Redesign Kurama's terminal interface around the restrained interaction patterns of Codex CLI while preserving Kurama's command surface, provider behavior, native terminal scrollback, small binary, and low idle cost. The interface should clearly communicate what the runtime is doing without turning the transcript into a dashboard.

This design supersedes the visual and interaction rules in `2026-08-29-inline-terminal-tui-design.md`. Its inline-terminal architecture remains valid.

## Product Boundaries

- Adopt Codex-like presentation and interaction patterns, not Codex feature or command parity.
- Keep stable transcript content in native terminal scrollback and selectable by the terminal.
- Preserve complete assistant and tool output. Collapsing changes presentation only; it never truncates stored output.
- Add no heavy UI dependency or background service.
- Keep the release binary below 10 MiB and avoid work while the interface is idle.

## Visual Language

Kurama will inherit the terminal's default foreground and background. It must not paint a full-screen surface, allowing Ghostty and other terminals to provide transparency naturally.

Content uses the terminal default foreground. Metadata is dimmed. A restrained accent identifies active work, yellow is reserved for action-required states, and red is reserved for failures. Chat bubbles, decorative cards, permanent borders, provider branding, and redundant labels are removed.

Whitespace and indentation provide hierarchy. User prompts begin with `›`; assistant replies remain unboxed rich Markdown; child activity uses `└` where a relationship needs to be explicit.

## Transcript Model

The current generic transcript kind will become an explicit presentation model:

- `UserTurn` contains the submitted prompt.
- `AssistantMessage` contains Markdown and streaming state.
- `ToolCall` contains call identity, concise description, lifecycle, and complete output.
- `Error` contains a durable user-facing failure.
- `Notice` contains non-error runtime information.

Each user submission begins a turn. A blank line separates turns, but related assistant, tool, and error entries remain visually grouped. Assistant content uses the existing Markdown renderer. Tool calls render beneath their owning turn as compact summaries. Completed tool output is collapsed by default; `Ctrl+O` toggles detailed tool output globally. Full output remains in memory and in persisted session events.

Errors are appended once to the transcript. Status text must not duplicate an error or instruct the user to inspect another status surface.

## Runtime Activity

Replace `TuiState.status: String` with a typed activity state. The state must distinguish at least:

- `Idle`
- `Thinking`
- `RunningTool`
- `AwaitingApproval`
- `Interrupted`

The runtime event reducer owns transitions. Render code must not infer state from arbitrary strings. Active states track their start time so the UI can display elapsed time without adding transcript noise.

While active, a single row above the composer shows a spinner, a concise activity label, elapsed time, and `Esc to interrupt` when space permits. Examples are `⠋ Thinking · 12s` and `⠋ Running cargo test · 12s`. The row disappears when Kurama becomes idle; there is no persistent `Ready` footer.

Animation ticks run only while an animated activity is visible. Idle input remains event-driven.

## Composer And Approvals

The composer stays at the bottom of the live viewport and uses the same horizontal alignment as transcript content. Secondary help text disappears before meaningful input is compressed.

Approvals replace the composer area inline. They do not open an overlay or another floating panel. The prompt shows the proposed action, the minimum context required to judge it, and keyboard choices. Narrow terminals stack choices vertically. Approving, denying, editing, and cancelling retain their existing semantics.

The `/agents` panel remains a deliberate command surface rather than part of the normal transcript. Its behavior is unchanged except for shared responsive layout and visual tokens.

## Responsive Layout

Every render derives its layout from the current terminal rectangle. Components declare prioritized content rather than assuming fixed widths or heights.

Width adaptation follows progressive disclosure:

1. Preserve the prompt, assistant content, approval decision, and active action.
2. Remove composer help, interrupt hints, and secondary activity context.
3. Shorten tool summaries and status labels without clipping Unicode text.
4. Stack approval choices and other controls that no longer fit horizontally.

Height adaptation gives the transcript priority. Composer help and blank padding shrink first, then approval context is bounded while the decision remains visible. No component may overwrite another, render outside its rectangle, or require horizontal scrolling.

Native mouse selection and terminal scrollback remain available because Kurama does not capture mouse events or enter the alternate screen. Existing keyboard scrolling remains supported.

## Module Boundaries

The current monolithic renderer will be divided by responsibility:

- `state.rs` owns typed activity and transcript state plus event reduction.
- `layout.rs` computes responsive regions and content visibility priorities.
- `transcript.rs` renders turns, Markdown, tools, errors, and notices.
- `activity.rs` renders the transient active-work row and elapsed time.
- `composer.rs` renders normal input and inline approval modes.
- `render.rs` coordinates these modules and commits stable transcript rows.

Existing onboarding and agent-management modules remain separate. Shared colors and spacing should be small constants or helper functions, not a theme framework.

## Data Flow And Failure Handling

Runtime events enter one reducer, which updates typed state and transcript ownership. Rendering is a pure projection of that state plus terminal size and current time. Stable transcript entries continue through Ratatui's inline viewport commit path; mutable streaming entries stay in the live viewport until complete.

Unknown or malformed runtime events produce a durable `Error` or `Notice` entry as appropriate and return the activity state to a usable condition. Interruptions stop animation, preserve received output, and leave the composer available. Rendering must degrade safely for zero-width, zero-height, and rapidly resized terminal rectangles.

## Verification

Testing stays focused:

- Reducer tests cover activity transitions, interruption, approvals, and single-emission errors.
- Transcript tests cover turn grouping, collapsed versus expanded tool output, and rich Markdown continuity.
- Buffer-level layout tests cover representative wide, medium, narrow, and short rectangles.
- Existing scrollback, selection, streaming, approval, and full-output regressions remain intact.
- Release verification runs the focused TUI tests, strict Clippy, the release build, and the existing binary-size gate.

No broad golden-screenshot suite is required. Assertions should target semantic rows, visibility priorities, clipping, and overlap.

## Acceptance Criteria

- Idle Kurama shows no status row.
- Active work shows one concise animated row with elapsed time.
- User turns, assistant Markdown, tools, and failures are visibly distinct without cards.
- Tool details can be hidden or shown without losing output.
- Approvals occupy the composer region and never create an overlay.
- Text remains natively selectable and transcript history remains scrollable.
- Wide, medium, narrow, and short terminals render without overlap or horizontal clipping.
- Idle rendering performs no animation tick.
- The release binary remains below 10 MiB.
