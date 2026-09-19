# kurama

a terminal coding agent written in rust. one process, no daemon, no database.

it gives models four environment tools: bounded file reads, atomic writes, timed shell commands, and public web search. a session todo list is parent-only orchestration, not a fifth environment tool. sub-agents exist only when you ask for parallel work, cannot spawn children, and stay read-only unless they are implementers. sessions are resumable jsonl logs under `~/.kurama`.

## install

requires rust 1.98.0. released targets are macOS and Linux.

```bash
cargo build --locked --release -p kurama-cli
install -d "$HOME/.local/bin"
install -m 755 target/release/kurama "$HOME/.local/bin/kurama"
kurama
```

ensure `$HOME/.local/bin` is on your `PATH`.

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

`/diff` reviews changes and prepares hunk feedback. `/context` explains the current request budget. While working, Enter steers and Alt+Enter queues a follow-up; `/queue` edits pending work.

`/verify` lists named project checks; `/verify quick` runs one with normal approvals and records its result. Recipes live in `.kurama/verification.toml`.

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

TypeScript and Python clients use the same Rust engine through `kurama --stdio`; no daemon or native bindings. See [SDK setup, streaming, approvals, and resume](docs/sdk.md#typescript-and-python).

## develop

```bash
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
```

credential-free [terminal checks and benchmarks](docs/design.md#reproducing-terminal-checks) exercise the real binary in isolated PTYs and close their processes afterward.

## release

use rust 1.98.0 and python 3.11 or newer. the release tag must be exactly
`v` plus `[workspace.package].version`; for example, `0.2.0` requires
`v0.2.0`. use SemVer (`major.minor.patch`, optionally prerelease/build
identifiers), never a fourth numeric component such as `v0.1.9.5`.

1. update `[workspace.package].version` in `Cargo.toml` and the six workspace
   package versions in `Cargo.lock` together. leave dependency versions and
   checksums unchanged; each member must keep `version.workspace = true`.
2. run the checks below and all checks in `.github/workflows/ci.yml`:

   ```bash
   python3 -m unittest discover -s scripts -p 'test_release_version.py' -v
   python3 scripts/check-release-version.py v0.2.0
   cargo test --locked --workspace --all-features
   cargo build --locked --release -p kurama-cli
   scripts/check-size.sh target/release/kurama
   python3 scripts/check-release-version.py v0.2.0 --binary target/release/kurama
   ```

3. merge the reviewed version change with green CI, then create and push the
   matching tag on that commit. replace `v0.2.0` above with the intended tag
   for future releases. never move or replace an existing tag/release.
4. the release workflow checks tag/manifest/lockfile consistency and tests the
   source before packaging. every native build must report `kurama 0.2.0`
   (the tag without `v`) from `--version` after stripping and before archiving.
   only after all packages pass does it publish the archives and checksums.
