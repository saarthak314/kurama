# Rust SDK

Kurama's public `0.x` SDK is a compile-time composition boundary. `Agent` is the product API. `AgentBuilder` remains the explicit wiring harness used by tests and by embedders who want every seam. It is not a dynamic plugin system or stable binary ABI.

```text
kurama          ──────▶ kurama-sdk ─────▶ kurama-core ─────▶ kurama-protocol
kurama-cli      ──────▶ kurama-sdk
kurama-adapters ──────────────────────────────────────────▶ kurama-protocol
```

## Simple

`kurama` ships first-party backends and the four environment tools (`read`, `write`, `bash`, `web-search`). The engine also injects a parent-only `todo` tool for a session list; children do not get it. Coding work that writes files or runs mutating commands needs `.yolo()` (launch-only, same rule as the CLI) or a `Turn` loop that resolves approvals. Supervised `prompt` returns an error on the first `Ask` instead of blocking on stdin.

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

Defaults: `DefaultPolicy` in supervised mode, `MemoryStore`, a no-op event sink, `NoDelegation`, and `RandomIds`. `.orchestrate()` installs `SmartOrchestrator` and builds the orchestration context from the active profile, workspace write scope, and configured roles. Researcher, planner, and reviewer children are forced read-only; an implementer with an empty scope inherits the parent write scope. The parent may run up to three delegation waves in one user turn. Children wrap up at 80% of budget; a hard limit cancels unless a summary can be salvaged. `.delegate()` requires `.orchestrate()`. `.config(&kurama_config)` copies role routes, escalations, auto boundaries, and concurrency from `~/.kurama/config.toml`.

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

Assistant text is coalesced to at most 4 KiB UTF-8-safe chunks, with a 50 ms flush deadline while text is pending. This is an engine buffering bound, not an end-to-end provider latency guarantee. Drain runtime events continuously: the bounded channel applies backpressure rather than dropping text or terminal events.

`Handle::steer(text, explicit_delegation)` redirects an active turn at the next complete model/tool/delegation boundary; an idle handle starts a normal turn. Internal retries retain their in-flight request. `SteeringQueued` is a receipt, `SteeringApplied` marks durable application, and `SteeringRejected` returns input that could not be applied. These are nonterminal events; callers must preserve rejected text rather than resubmitting it automatically. Only applied steering enters durable history.

`Handle::inspect_context()` emits `ContextInspected` without calling the model or ending the turn. Its `ContextInspection` partitions an estimated request budget into categories, reports omitted/included completed turns and summary coverage, and previews explicit compaction. `assembly_error` explains a request that cannot fit while preserving inspection data. Token estimates are not provider billing counts.

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

### Custom session stores

`SessionStore::append` retains explicit-sequence validation. `append_next(&mut EventEnvelope)` assigns and commits the next sequence, returning that sequence in the envelope. The child engine and agent manager use this shared allocator because they interleave events in one child log. Filesystem and memory stores allocate under their storage lock without full replay. Existing custom stores inherit a compatibility implementation that serializes replay-plus-append within this process; override it with a transaction or shared storage lock for cross-process writers or efficient allocation. Store wrappers should forward `append_next` to the underlying allocator. Do not mix uncoordinated explicit-sequence appends with allocated appends to the same log.

Replay results must include any repair event committed during that replay so callers can append immediately from the returned sequence. `FsSessionStore::get_blob_tail(reference, max_bytes)` is an inherent filesystem helper, not a new `SessionStore` requirement: it verifies the complete blob stream and returns only its bounded suffix. `get_blob` still returns fully verified content.

Direct `kurama-core` compaction integrations now consume `CompactionRequest.items` rather than serializing an `events` field. These model-ready items include the prior summary as data plus newly covered events; do not omit the summary when constructing the backend request. The `Agent`/`Handle` embedding entry points are unchanged.

## Providers and Tools

First-party adapters cover OpenAI, Anthropic, OpenAI-compatible HTTP endpoints, Codex CLI, and Claude CLI. The provider factory consumes non-secret profile configuration plus credentials resolved outside the protocol boundary.

Custom SDK tools do not enter the standard Kurama CLI automatically. The CLI registers exactly `read`, `write`, `bash`, and `web-search`. The engine adds `todo` for the parent session only; it is not an environment tool and does not change that CLI registry.

## Compatibility

Kurama follows semantic versioning with `0.x` expectations. Breaking public API changes require a minor-version bump and migration notes. Patch releases preserve the public contracts, while trait additions are driven by working first-party implementations rather than speculative extension points.

The control additions extend `EngineCommand`, `RuntimeEvent`, SDK `Event`, and `SessionEvent`; downstream exhaustive matches must handle the new variants. `SessionEvent::UserSteered` stays within the original user turn and carries a default-false `explicit_delegation` flag. Direct compaction request constructors must also provide `CompactionRequest::event_count`. Older sessions remain readable; older binaries do not understand the new steering event.
