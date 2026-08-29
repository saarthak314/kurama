#![cfg_attr(
    not(any(
        feature = "tools",
        feature = "fs-store",
        feature = "codex-bridge",
        feature = "claude-bridge"
    )),
    forbid(unsafe_code)
)]

mod id;

#[cfg(all(
    feature = "native-credentials",
    feature = "http",
    any(
        feature = "openai",
        feature = "anthropic",
        feature = "openai-compatible",
        feature = "codex-bridge",
        feature = "claude-bridge"
    )
))]
mod factory;

#[cfg(feature = "native-credentials")]
mod config;
#[cfg(any(
    feature = "native-credentials",
    feature = "tools",
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
mod credentials;
#[cfg(any(feature = "native-credentials", feature = "tools"))]
mod redact;
#[cfg(feature = "fs-store")]
mod storage;

#[cfg(any(feature = "codex-bridge", feature = "claude-bridge"))]
mod bridges;
#[cfg(feature = "http")]
mod http;
#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
mod providers;
#[cfg(feature = "tools")]
mod tools;

#[cfg(any(feature = "codex-bridge", feature = "claude-bridge"))]
pub use bridges::BridgeCommand;
#[cfg(feature = "claude-bridge")]
pub use bridges::claude::ClaudeBridge;
#[cfg(feature = "codex-bridge")]
pub use bridges::codex::CodexBridge;
#[cfg(any(feature = "codex-bridge", feature = "claude-bridge"))]
pub use bridges::control::{bridge_prompt, control_schema, parse_control, write_control_schema};
#[cfg(feature = "native-credentials")]
pub use config::{AppPaths, ConfigRepository};
#[cfg(any(
    feature = "native-credentials",
    feature = "tools",
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
pub use credentials::{
    CredentialResolver, SecretValue, SessionSecrets, format_auth_ref, parse_auth_ref,
};
#[cfg(all(
    feature = "native-credentials",
    feature = "http",
    any(
        feature = "openai",
        feature = "anthropic",
        feature = "openai-compatible",
        feature = "codex-bridge",
        feature = "claude-bridge"
    )
))]
pub use factory::ProviderFactory;
#[cfg(feature = "http")]
pub use http::{HttpClient, HttpErrorClass, HttpFailure, bounded_redacted, bounded_redacted_error};
pub use id::RandomIds;
#[cfg(feature = "anthropic")]
pub use providers::anthropic::AnthropicBackend;
#[cfg(feature = "openai")]
pub use providers::openai::OpenAiBackend;
#[cfg(feature = "openai-compatible")]
pub use providers::openai_compat::OpenAiCompatBackend;
#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
pub use providers::sse::{SseDecoder, SseEvent};
#[cfg(any(feature = "native-credentials", feature = "tools"))]
pub use redact::{RedactionMetadata, Redactor};
#[cfg(feature = "fs-store")]
pub use storage::FsSessionStore;
#[cfg(feature = "tools")]
pub use tools::{
    BashTool, BoundedOutput, BoundedText, GuardedPath, PathGuard, ReadTool, WriteTool, html_to_text,
};
#[cfg(all(feature = "tools", feature = "http"))]
pub use tools::{
    JsonSearchBackend, OpenAiNativeSearch, SearchBackend, SearchResult, WebSearchTool,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
