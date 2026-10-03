# SDKs

Kurama provides a native Rust API and [TypeScript/Python clients](#typescript-and-python) over its headless process. Rust owns execution, approvals, tools, persistence, recovery, and orchestration. The native `0.x` SDK is a compile-time composition boundary: `Agent` is the product API, while `AgentBuilder` exposes explicit wiring for embedders. Neither interface is a dynamic plugin system or stable binary ABI.

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

Defaults: supervised `DefaultPolicy`, `MemoryStore`, a no-op event sink, `NoDelegation`, and 128-bit `RandomIds`. `.orchestrate()` preserves a supplied orchestrator or installs `SmartOrchestrator`, and creates the child context from the active profile, workspace scope, and configured roles. Researcher, planner, and reviewer children are read-only; implementers inherit or narrow the parent scope. The parent may run three delegation waves per turn; children wrap up at 80% of budget and cannot spawn children. `.delegate()` requires `.orchestrate()`. Unknown configured role/escalation profiles fail setup rather than silently falling back. `.limits()` applies regardless of whether the backend/profile was registered first; zero token limits are rejected.

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

To abandon a partially consumed `Turn`, call `turn.cancel().await?`, then `turn.drain().await` before starting another prompt. Draining consumes the terminal event without accumulating more reply text; it does not itself request cancellation.

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

Provider streams retain an owned cancellation future after establishment. `SseDecoder` bounds each wire record to 1 MiB and accepts BOM/CR/LF/CRLF streams; provider normalizers reject incomplete tool calls. The default HTTP timeout is still a 120-second absolute request/body deadline, not an inactivity timer. `KuramaError::Provider` carries an authoritative retryability flag so permanent HTTP failures cannot be retried based on words in their diagnostic bodies.

`BoundedOutput::with_staging` records staging intent; it creates a file only when output really truncates. Deferred filesystem errors appear in `finish().staging_error`. Below-limit output has no staging path or blob reference. Byte and LF-delimited line limits include omission markers and, for Bash, stream labels and timeout notes. Complete raw streams remain retrievable when truncated; execution-error suffixes are separate metadata, not part of raw stream hashes.

Once Bash execution has started, cancellation waits for process-group teardown and returns an error `ToolResult` with `metadata.cancelled = true` and bounded partial output. Cancellation before execution still returns `KuramaError::Cancelled`. Direct tool consumers must inspect `is_error` and metadata rather than treating every returned result as success.

## Compatibility

Kurama follows semantic versioning with `0.x` expectations. Breaking public API changes require a minor-version bump and migration notes. Patch releases preserve the public contracts, while trait additions are driven by working first-party implementations rather than speculative extension points.

The control additions extend `EngineCommand`, `RuntimeEvent`, SDK `Event`, `SessionEvent`, and `KuramaError`; downstream exhaustive matches need the new variants. `UserMessage` and `UserSteered` carry default-false `explicit_delegation` flags. `UserSteered` stays within its original turn. `Operation::Read` now contains `paths: Vec<PathBuf>`; deserialization accepts old single-path records, while new records emit only `paths`. Direct compaction constructors provide `CompactionRequest::event_count`. Older sessions remain readable; older binaries do not understand new steering records.

Every registered profile must provide non-zero input and output token limits, including CLI-backed profiles. Configuration examples and verification fixtures specify both limits explicitly; an omitted limit is not an automatic model-capacity lookup.

Implement `CancelSignal::cancelled` as `BoxFuture<'static, ()>` by cloning owned cancellation state before constructing the future. `DefaultPolicy` is a unit struct: use `DefaultPolicy` rather than the removed constructor/accessor with stale mode state. Provider `parse_fixture` helpers were removed; tests and embedders use the production stream API.

OpenAI/Anthropic key constructors accept values convertible to `Zeroizing<String>`; compatible providers accept `Option<Zeroizing<String>>`. `SecretValue::into_zeroizing` transfers an existing protected allocation. `ClaudeBridge::new()` and `command_for(request, cursor)` no longer take an unused schema path. Codex uses immutable mode-specific schema files; the public schema writer rejects replacement with different content. Unsafe/long profile cache names use a reserved `~` plus SHA-256 component; existing safe cache paths remain stable.

Raw runtime consumers must also handle `Ready`, `VerificationUpdated`, and `VerificationsInspected`. `Ready` marks completion of startup recovery; the high-level Rust `Turn` interface consumes it internally. Verification records extend the durable event schema; older binaries cannot read these new records.

## TypeScript and Python

The process clients require Node 22+ or Python 3.11+ and have no runtime package dependencies. Configure Kurama once through its CLI, then omit `profile` to use the configured default. They locate the binary through an explicit `binary` option, then `KURAMA_BIN`, then `kurama` on `PATH`. Startup checks the protocol automatically. Credentials stay in existing configuration references or the child environment; no credential fields cross the protocol.

These clients require a binary supporting `--stdio` protocol version 1. The original `v0.2.0` release predates that interface. During development, build this checkout and pass `binary: "./target/release/kurama"` (TypeScript) or `binary="./target/release/kurama"` (Python). SDK packages do not automatically download or upgrade binaries.

Build distributable packages from the checkout:

```sh
cargo build --locked --release -p kurama-cli
npm --prefix sdk/typescript ci
mkdir -p target/sdk-packages
npm pack ./sdk/typescript --pack-destination target/sdk-packages
python3 -m pip wheel --no-deps --wheel-dir target/sdk-packages ./sdk/python
```

Install the resulting npm archive or Python wheel in the consuming application. Package sources are also installable from `sdk/typescript` and `sdk/python`; TypeScript's `prepack` script builds its declarations and JavaScript. Package publication is separate from building these artifacts.

### TypeScript

```typescript
import { Agent } from "@kurama/sdk";

await using agent = await Agent.open({ workspace: "." });
const reply = await agent.prompt("Explain the main entry point without editing files.");
console.log(reply.text);
```

`await using` uses TypeScript's async-disposal support. Ordinary `try/finally` with `await agent.close()` works too. `agent.stream(text)` is a native async iterable:

```typescript
for await (const event of agent.stream("Review the current changes.")) {
  if (event.type === "text") process.stdout.write(event.text);
  if (event.type === "approval") await agent.approve(event.request.operation_id, "deny");
}
```

The example explicitly denies operations rather than silently approving them. Interactive approval and cancellation examples are in `sdk/typescript/examples/`. Save `reply.sessionId` and reopen with `Agent.open({ sessionId })` to resume the same workspace/profile.

### Python

```python
import asyncio
from kurama import Agent

async def main():
    async with Agent(workspace=".") as agent:
        reply = await agent.prompt("Explain the main entry point without editing files.")
        print(reply.text)

asyncio.run(main())
```

Streaming exposes typed events with `event.type`, `event.text`, and `event.request`. Use the stream context manager when a loop might exit early:

```python
async with agent.stream("Review the current changes.") as events:
    async for event in events:
        if event.type == "text":
            print(event.text, end="", flush=True)
        elif event.type == "approval":
            await agent.approve(event.request.operation_id, "deny")
```

Python's bare `async for` does not close an iterator on `break`; `async with`, `await events.aclose()`, or closing the agent cancels/drains the active operation. Save `reply.session_id` and reopen with `Agent(session_id=...)` to resume. Runnable examples are in `sdk/python/examples/`.

### Approvals, errors, and checks

- Both clients default to supervised mode. `onApproval` / `on_approval` optionally accepts a synchronous or asynchronous callback returning `approve_once`, `approve_session`, `deny`, or an edited-arguments response. Without a callback, `prompt` and `verify` raise `ApprovalRequired` at an approval boundary, cancel/drain the operation, and leave the client reusable. Streaming instead exposes approval events for the application to handle explicitly.
- `cancel()` requests cancellation; it does not undo side effects. A stream emits a terminal `done` event with `completed`, `cancelled`, or `failed`. Convenience prompts raise `TurnFailed` for failed terminals and retain a partial reply. Server/protocol/process failures are typed errors, not empty successful replies.
- Only one execution is active per agent. `close()` owns bounded process cleanup; a blocked or broken peer cannot leave callers waiting indefinitely. Event buffers are bounded and fail explicitly on overflow instead of silently dropping authoritative events. `events()` exposes recovery and unsolicited session events separately from prompt streams.
- `verificationStatus()` / `verification_status()` lists named project recipes and their last results. `verify("quick")` runs one through the same Rust policy and returns a `VerificationReport`. A nonzero check exit yields `status="failed"`, not a protocol failure. See [verification configuration](configuration.md#verification-recipes).
- Existing auto boundaries still apply. Selecting `mode="yolo"` is explicit launch-only consent; clients pass the launch flag and cannot later escalate an existing process through the protocol.

### Transport and reproducible checks

`kurama --stdio` keeps stdout exclusively for versioned JSONL frames. The shared schema and cross-language fixtures are `protocol/sdk.schema.json` and `protocol/sdk.fixtures.json`. Applications use the client APIs rather than constructing those frames. Requests are correlated, recovery completes before initialization succeeds, and cancellation drains before another execution can start. Handshake deadlines do not limit time spent obtaining a recovery approval.

```sh
cargo build --locked -p kurama-cli --bin kurama --example sdk_contract
npm --prefix sdk/typescript test
PYTHONPATH=sdk/python/src python3 -m unittest discover -s sdk/python/tests -v
python3 scripts/check-sdk.py target/debug/kurama target/verification/sdk-contracts
```

The last command uses a local scripted provider and real Rust/TypeScript/Python clients. It exercises streaming, manual and missing-handler approvals, cancellation/reuse, delegation, resume, model-free verification, and matching conversational histories. CI also installs the built npm archive and wheel before running this scenario on Linux/macOS. Release jobs check the packaged binary's stdio handshake and publish `kurama-<version>-platforms.json` with protocol version, platform archives, and hashes.
