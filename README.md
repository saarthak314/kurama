# kurama

a terminal coding agent written in rust. one process, no daemon, no database.

it gives models four tools: bounded file reads, atomic writes, timed shell commands, and public web search. sub-agents exist only when you ask for parallel work, and they cannot spawn children. sessions are resumable jsonl logs under `~/.kurama`.

## install

requires rust 1.98.0.

```bash
cargo build --locked --release -p kurama-cli
install target/release/kurama ~/.local/bin/kurama
kurama
```

binaries: [releases](https://github.com/saarthak314/kurama/releases).

first launch connects a codex or claude subscription, an openai or anthropic api key, or any openai-compatible endpoint.

## usage

```bash
kurama
kurama --profile work
kurama resume ses_deadbeef
kurama --continue
kurama --yolo
```

- supervised asks before writes and non-readonly commands
- auto allows only the `write_roots`, `allowed_commands`, and `allowed_hosts` in config; anything else is denied
- yolo skips approvals for this launch only (`--yolo` is required every time; it cannot be saved in config)

non-secret config is `~/.kurama/config.toml`. auth is `env:NAME`, `keychain:SERVICE/ACCOUNT`, or in-memory `session`. plaintext keys are rejected.

see [configuration](docs/configuration.md) and [architecture](docs/design.md).

## embed

```rust
use kurama::prelude::*;

let reply = Kurama::openai(std::env::var("OPENAI_API_KEY")?)
    .workspace(".")
    .yolo()
    .prompt("fix the failing tests")
    .await?;
```

`.yolo()` is launch-only, same as the cli. without it, `prompt` returns an error on the first write that needs approval. `reply.session_id` is what you pass to `resume`. more: [sdk](docs/sdk.md).

## develop

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```
