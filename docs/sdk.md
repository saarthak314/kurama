# Rust SDK

Kurama's public `0.x` SDK is a compile-time composition boundary. `Agent` is the product API. `AgentBuilder` remains the explicit wiring harness used by tests and by embedders who want every seam. It is not a dynamic plugin system or stable binary ABI.

```text
kurama          ──────▶ kurama-sdk ─────▶ kurama-core ─────▶ kurama-protocol
kurama-cli      ──────▶ kurama-sdk
kurama-adapters ──────────────────────────────────────────▶ kurama-protocol
```

## Simple

`kurama` ships first-party backends and the four standard tools. Coding work that writes files or runs mutating commands needs `.yolo()` (launch-only, same rule as the CLI) or a `Turn` loop that resolves approvals. Supervised `prompt` returns an error on the first `Ask` instead of blocking on stdin.

```rust
use kurama::prelude::*;

let reply = Kurama::openai(std::env::var("OPENAI_API_KEY")?)
    .model("gpt-5.6")
    .workspace(".")
    .yolo()
    .prompt("fix the failing tests")
    .await?;
println!("{reply}");
println!("{}", reply.session_id);
```

`prompt` returns `TurnOutcome`. It displays as the assistant text. Record `reply.session_id` for later `resume`; `Agent::session_id()` is the live session, or `None` before the first turn. `allocate_session_id()` mints an id for event-driven UIs that call `launch`.

`Kurama::from_backend` takes any `ModelBackend`. `.ephemeral()` keeps the session in memory. `.no_tools()` is chat-only. `.persist(dir)` stores sessions under `dir` and relocates Codex/Claude bridge cache there too. `.auto()` only changes the mode: pair it with `.allow_writes(["."]).allow_commands(["cargo", "git"])` or Auto denies every mutation.

Claude and Codex helpers are infallible constructors; they resolve cache paths at `build()`:

```rust
Kurama::claude_cli().workspace(".").yolo().prompt("…").await?;
```

## Agent

`Agent` fills in the rest. An embedder implements at most `ModelBackend` and maybe `Tool`:

```rust
use kurama_sdk::Agent;

let mut agent = Agent::new()
    .backend(my_backend)
    .tool(my_tool)
    .workspace(".")
    .yolo()
    .build()?;

let reply = agent.prompt("fix the tests").await?;
agent.resume(reply.session_id.to_string()).await?;
agent.prompt("continue from there").await?;
```

`resume` refuses a session from another workspace or profile, and it does not restore YOLO unless this Agent was built with `.yolo()` / `ExecutionMode::Yolo`.

Defaults: `DefaultPolicy` in supervised mode, `MemoryStore`, a no-op event sink, `NoDelegation`, and `RandomIds`. `.orchestrate()` installs `SmartOrchestrator` and builds the orchestration context from the active profile, workspace write scope, and configured roles. `.delegate()` requires `.orchestrate()`. `.config(&kurama_config)` copies role routes, escalations, auto boundaries, and concurrency from `~/.kurama/config.toml`.

Streaming, approvals, and children stay one method down. Break on `Done` (and `Error`); a later `next()` returns `None`:

```rust
let mut turn = agent.turn("use sub-agents to split the work").await?;
while let Some(event) = turn.next().await? {
    match event {
        Event::Text(text) => print!("{text}"),
        Event::Approval(req) => turn.approve_once(req.operation_id).await?,
        Event::Done(done) => {
            println!("\n{}", done.text);
            break;
        }
        Event::Error(message) => return Err(KuramaError::Model(message)),
        _ => {}
    }
}
```

Event-driven UIs (the Kurama CLI) call `agent.launch(metadata, replay)` and drive `Handle` plus the typed event stream.

## Explicit composition

`AgentBuilder` still requires every component:

```rust
use std::sync::Arc;
use kurama_sdk::{AgentBuilder, ModelProfile};

let runtime = AgentBuilder::new()
    .profile(ModelProfile::new("custom", "model", 32_000, 4_000), Arc::new(MyBackend::new()))
    .tool(Arc::new(MyTool::new()))
    .policy(Arc::new(MyPolicy::new()))
    .store(Arc::new(MyStore::new()))
    .sink(Arc::new(MySink::new()))
    .orchestrator(Arc::new(MyOrchestrator::new()))
    .ids(Arc::new(MyIds::new()))
    .build()?;
```

Duplicate model profile or tool names, missing required components, an unavailable active profile, and zero channel limits are errors.

## Providers and Tools

First-party adapters cover OpenAI, Anthropic, OpenAI-compatible HTTP endpoints, Codex CLI, and Claude CLI. The provider factory consumes non-secret profile configuration plus credentials resolved outside the protocol boundary.

Custom SDK tools do not enter the standard Kurama CLI automatically. The CLI registers exactly `read`, `write`, `bash`, and `web-search`; another tool affects its schema and binary only when it is explicitly compiled and registered.

## Compatibility

Kurama follows semantic versioning with `0.x` expectations. Breaking public API changes require a minor-version bump and migration notes. Patch releases preserve the public contracts, while trait additions are driven by working first-party implementations rather than speculative extension points.
