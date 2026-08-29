# Rust SDK

Kurama's public `0.x` SDK is a compile-time composition boundary. It re-exports the dependency-light protocol contracts and provides `AgentBuilder`; it is not a dynamic plugin system or stable binary ABI.

```text
kurama-cli ──────▶ kurama-sdk ─────▶ kurama-core ─────▶ kurama-protocol
kurama-adapters ──────────────────────────────────────▶ kurama-protocol
```

## Composition

Applications supply trait implementations and assemble one runtime through the same path used by the Kurama CLI:

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

The custom types implement the re-exported `ModelBackend`, `Tool`, `ApprovalPolicy`, `SessionStore`, `EventSink`, `Orchestrator`, and `IdGenerator` traits. Builder registration is explicit: duplicate model profile or tool names, missing required components, an unavailable active profile, and zero channel limits are errors.

`AgentRuntime::start(session_metadata, replay)` returns an engine handle and typed runtime event stream. Embedded consumers may replace model backends, tools, policies, stores, event sinks, orchestrators, ID generation, or the user interface without forking the engine.

## Providers and Tools

First-party adapters cover OpenAI, Anthropic, OpenAI-compatible HTTP endpoints, Codex CLI, and Claude CLI. The provider factory consumes non-secret profile configuration plus credentials resolved outside the protocol boundary.

Custom SDK tools do not enter the standard Kurama CLI automatically. The CLI registers exactly `read`, `write`, `bash`, and `web-search`; another tool affects its schema and binary only when it is explicitly compiled and registered.

## Compatibility

Kurama follows semantic versioning with `0.x` expectations. Breaking public API changes require a minor-version bump and migration notes. Patch releases preserve the public contracts, while trait additions are driven by working first-party implementations rather than speculative extension points.
