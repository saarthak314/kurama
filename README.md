# Kurama

Kurama is a minimal Rust coding agent built around a focused terminal interface, bounded context, explicit sub-agent orchestration, and remote frontier models. It ships as one binary with low startup, memory, CPU, prompt, and distribution overhead.

## Principles

- Keep the transcript primary; avoid permanent dashboards and tool statistics.
- Make execution understandable in supervised mode and bounded in auto mode.
- Delegate only when the user explicitly asks for sub-agents or parallel work.
- Store durable, resumable JSONL sessions without replaying the full transcript into every request.
- Expose a public Rust SDK without turning the CLI into a dynamic plugin host.

## Environment Tools

The standard CLI registers exactly four model-visible tools:

- `read` reads bounded file ranges and returns metadata plus a content hash.
- `write` atomically replaces files or applies unified patches with conflict checks.
- `bash` runs bounded Bash commands with timeouts and process-tree cancellation.
- `web-search` performs ranked search or opens one selected public page as bounded text.

Delegation is an internal control action, not a fifth tool.

## Install and Run

Build from source with Rust 1.98.0:

```bash
cargo build --locked --release -p kurama-cli
install target/release/kurama ~/.local/bin/kurama
```

Common launches:

```bash
kurama
kurama --profile work
kurama resume s_1234
kurama --continue
kurama --yolo
```

The first launch opens a connection wizard for an official Codex or Claude subscription bridge, an OpenAI or Anthropic API-key profile, or an OpenAI-compatible remote/local endpoint. Normal use does not require a provider flag.

Kurama supports `aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`, and `x86_64-unknown-linux-gnu`. Windows is deferred.

## Permission Modes

- **Supervised** is the default. Safe reads run directly; writes and risky operations request approval.
- **Auto** runs only inside configured file, command, and host boundaries. Violations are denied.
- **YOLO** is enabled only by `--yolo` for the current launch. It removes approval and boundary enforcement for the parent and its children.

YOLO is unrestricted. It remains logged, but redaction is best-effort and the mode is never persisted or restored automatically.

## TUI Commands

- `/agents` opens the child-agent controller; Enter inspects, `m` messages, and `x` cancels with confirmation.
- `/model` selects a model; `/connect` manages provider connections.
- `/sessions`, `/resume <id>`, and `/new` manage sessions.
- `/context` explains active context; `/compact` forces compaction.
- `/exit` closes the TUI and prints token usage, the session ID, and the resume command.

Startup also supports `--resume <id>` and `--continue`. There is no ambiguous `/restart` command.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --locked --release -p kurama-cli
scripts/check-size.sh target/release/kurama
scripts/check-prompt-budget.sh target/release/kurama
```

The hard release limit is a 10 MiB stripped binary, with an 8 MiB stretch target. The primary performance targets are a usable TUI within 100 ms, idle RSS at or below 25 MiB, idle CPU below 0.5%, and at most 2,000 tokens for the fixed system prompt plus four tool schemas. UPX and other executable packers are not used.

See the [architecture overview](docs/design.md), [configuration reference](docs/configuration.md), and [SDK guide](docs/sdk.md).
