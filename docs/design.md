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

Delegation is an explicit structured capability enabled only when the user asks for agents or parallel work. Children cannot create children. They receive isolated briefs, write scopes, budgets, cancellation tokens, and logs. The parent remains the sole orchestrator and receives compact results rather than copied transcripts.

The standard binary exposes exactly `read`, `write`, `bash`, and `web-search`. Provider adapters normalize OpenAI Responses, Anthropic Messages, OpenAI-compatible chat completions, Codex CLI, and Claude CLI into the same protocol stream. Local inference is connect-only through an existing compatible HTTP endpoint.
