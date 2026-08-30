# Kurama

A lean coding agent built in Rust. Terminal-first, fast to boot, easy to inspect, and not trying to become an IDE.

It gives models four tools: bounded file reads, atomic writes, timed shell commands, and public web search. Sub-agents are explicit. Sessions are resumable. The transcript stays the main character.

## Run it

Requires Rust 1.98.0.

```bash
cargo build --locked --release -p kurama-cli
install target/release/kurama ~/.local/bin/kurama
kurama
```

Connect a Codex or Claude subscription, an OpenAI or Anthropic API key, or any OpenAI-compatible endpoint on first launch.

Useful commands:

```bash
kurama --profile work
kurama resume ses_deadbeef
kurama --continue
kurama --yolo
```

Supervised mode asks before risky moves. Auto mode stays inside configured boundaries. YOLO mode removes the guardrails for one launch. You know the vibe.

## Develop

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Docs: [architecture](docs/design.md), [configuration](docs/configuration.md), and [SDK](docs/sdk.md).
