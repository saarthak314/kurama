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

Profile kinds are `openai`, `anthropic`, `openai_compatible`, `codex_cli`, and `claude_cli`. HTTP profiles define a model, endpoint, input/output limits, and an auth reference. CLI profiles define the installed official command and model. OpenAI-compatible profiles are also the route for existing local inference servers.

Authentication accepts only:

- `env:NAME` — read a non-empty environment variable.
- `keychain:SERVICE/ACCOUNT` — use macOS Keychain or Linux Secret Service.
- `session` — prompt once and keep the value only in memory.

Plaintext keys are invalid. Kurama never reads Codex or Claude credential stores; BYOS bridges communicate only through the installed official CLI.

## Routing and Search

`[roles.<name>]` routes an orchestrator-assigned child role to a profile. The model supplies objectives, scopes, budgets, and dependencies; it cannot select roles or profiles. Kurama derives roles from each objective, applies the matching role route, and otherwise inherits the parent profile. Researcher, planner, and reviewer write scopes are forced empty regardless of the model request. An implementer with an empty scope inherits the parent write scope; a non-empty implementer scope must stay a subset. Dependency entries name the exact objective text of prerequisite agents. `escalation_profiles` is an ordered allowlist; automatic escalation never crosses to an unlisted provider or downgrade.

`[search] kind = "provider"` uses provider-native search when supported. Use `kind = "json"` with an HTTP endpoint for a configured search service; it must accept `{ "query", "limit" }` and return ranked `{ "title", "url", "snippet" }` results.

## Auto Boundaries

Auto mode approves only operations contained by `write_roots`, commands whose executable is listed in `allowed_commands`, and network requests to `allowed_hosts`. A boundary violation is denied rather than converted into an approval prompt. Concurrency defaults to four and must remain between one and eight; children cannot create further children.

## Resolution and State

Profile resolution order is:

1. `--profile <name>`.
2. The canonical project mapping in `state.json`.
3. `default_profile`.
4. The first-launch connection wizard.

All user-owned state is under `~/.kurama`:

```text
~/.kurama/
├── config.toml
├── state.json
├── sessions/<session-id>/events.jsonl
├── sessions/<session-id>/agents/<agent-id>.jsonl
├── blobs/<content-hash>
└── cache/
```

`state.json` contains mutable UI preferences, project/profile mappings, latest-session pointers, and supervised/auto mode history. API keys never enter configuration, state, transcripts, logs, or shell history. The directory is owner-only; `cache/` is disposable.
