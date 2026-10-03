# Configuration

Kurama keeps non-secret configuration in `~/.kurama/config.toml`. Configuration version `1` uses this shape:

```toml
version = 1
default_profile = "openai-main"
default_mode = "supervised"

[profiles.openai-main]
kind = "openai"
model = "gpt-5.6"
endpoint = "https://api.openai.com/v1"
auth = "keychain:openai/main"
max_input_tokens = 200000
max_output_tokens = 12000

[profiles.claude-sub]
kind = "claude_cli"
model = "sonnet"
command = "claude"
max_input_tokens = 128000
max_output_tokens = 16000

[roles.reviewer]
profile = "claude-sub"
escalation_profiles = ["openai-main"]

[orchestration]
max_concurrency = 4

[policy.auto]
write_roots = ["."]
allowed_commands = ["cargo", "git", "rg"]
allowed_hosts = ["api.openai.com", "api.anthropic.com"]

[search]
kind = "provider"
```

Only `default_mode = "supervised"` or `default_mode = "auto"` is valid. `yolo` is rejected because unrestricted consent must be explicit on every launch through `kurama --yolo`; resuming a YOLO session without the flag returns to a persisted supervised or auto mode.

## Profiles and Authentication

Profile kinds are `openai`, `anthropic`, `open_ai_compatible`, `codex_cli`, and `claude_cli`. HTTP profiles define a model, endpoint, input/output limits, and an auth reference. CLI profiles define the installed official command and model. OpenAI-compatible profiles are also the route for existing local inference servers.

Authentication accepts only:

- `env:NAME` — read a non-empty environment variable.
- `keychain:SERVICE/ACCOUNT` — use macOS Keychain or Linux Secret Service.
- `session` — prompt once and keep the value only in memory.

Plaintext keys are invalid. Kurama never reads Codex or Claude credential stores; BYOS bridges communicate only through the installed official CLI.

Native credential access requires the `native-credentials` feature. macOS uses Security.framework rather than passing secrets in process arguments; OS authorization is synchronous and has no cancellation deadline. Linux uses bounded nonblocking `secret-tool` pipes with a ten-second deadline and a 64 KiB read ceiling. Controlled exits clean their process groups; forcibly killing the parent or deliberately detached descendants is outside that guarantee.

## Routing and Search

`[roles.<name>]` routes an orchestrator-assigned child role to a profile. The model supplies objectives, scopes, budgets, and dependencies; it cannot select roles or profiles. Kurama derives roles from each objective, applies the matching role route, and otherwise inherits the parent profile. Researcher, planner, and reviewer write scopes are forced empty regardless of the model request. An implementer with an empty scope inherits the parent write scope; a non-empty implementer scope must stay a subset. Dependency entries name the exact objective text of prerequisite agents. `escalation_profiles` is an ordered allowlist; automatic escalation never crosses to an unlisted provider or downgrade.

Without a `[search]` section, the CLI automatically uses native search for `openai`, `codex_cli`, and `claude_cli` profiles. OpenAI uses the profile's API credentials. Codex and Claude use the selected model and the installed CLI's existing login; no separate search API key is required. Each CLI search runs in an isolated temporary directory with only native search enabled, without continuing the coding session or passing its workspace context. Search calls consume the provider's normal usage allowance.

`[search] kind = "provider"` explicitly requires native search and rejects unsupported profile kinds. Other profile kinds need `kind = "json"` with a configured search service. An explicit JSON service always takes precedence over native search and is not silently replaced if it fails. It must accept `{ "query", "limit" }` and return `{ "results": [{ "title", "url", "snippet" }] }`; optional `auth` uses the same environment/keychain/session references as profiles. Public-page `open` operations do not require a search backend.

## Auto Boundaries

Auto mode approves only operations contained by `write_roots`, commands whose executable is listed in `allowed_commands`, and network requests to `allowed_hosts`. A boundary violation is denied rather than converted into an approval prompt. Concurrency defaults to four and must remain between one and eight; children cannot create further children. The parent may run up to three delegation waves per user turn. A child is asked to wrap up at 80% of its turn, token, or time budget; the hard limit cancels unless the child already produced a summary, which is kept.

## Verification Recipes

Project-owned checks live in `.kurama/verification.toml`, separate from personal `~/.kurama/config.toml`:

```toml
version = 1

[recipes.quick]
command = "cargo test -p kurama-core"
timeout_ms = 120000

[recipes.full]
command = "cargo test --locked --workspace --all-features"
cwd = "."
timeout_ms = 600000
```

`/verify` lists configured recipes and last-run results; `/verify quick` explicitly executes one. Reading a recipe is not permission to run it. Execution uses the existing Bash tool, approval/auto boundaries, cancellation, output limits, and durable journal, without a model request. Another model or verification turn must finish before a new verification can start.

Reports distinguish `not_run`, `running`, `passed`, `failed`, `cancelled`, `denied`, and `interrupted`. They retain the executed command, cwd, timestamps, exit code, operation ID, and blob references where output was spilled. The operation ID identifies canonical output even without a separate blob. Approval edits cannot certify the original recipe. A changed recipe becomes `not_run`; an interrupted check is not rerun automatically on resume. A prior pass is a **last-run result**, not proof the current working files are unchanged.

The file is bounded to 64 KiB and 32 recipes. Names and commands are validated; unknown fields or versions are rejected. `cwd` defaults to `.` and must remain inside the workspace; `timeout_ms` defaults to 60,000 and must be positive and at most 3,600,000. Missing configuration produces an empty list rather than running inferred commands.

## Resolution and State

Profile resolution order is:

1. `--profile <name>`.
2. The canonical project mapping in `state.json`.
3. `default_profile`.
4. The first-launch connection wizard.

Personal configuration and resumable state are under `~/.kurama`:

```text
~/.kurama/
├── config.toml
├── state.json
├── sessions/<session-id>/events.jsonl
├── sessions/<session-id>/agents/<agent-id>.jsonl
├── blobs/<content-hash>
└── cache/
```

`state.json` contains mutable UI preferences, project/profile mappings, latest-session pointers, and supervised/auto mode history. Configuration stores authentication references, not plaintext keys. Application-owned credential buffers are zeroizing and known provider-error secrets are redacted; transcripts and arbitrary tool output are not a general secret boundary. Do not paste credentials into prompts or commands. The state directory is owner-only; `cache/` is disposable.
