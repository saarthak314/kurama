mod bash;
#[cfg(all(feature = "claude-bridge", feature = "http"))]
mod claude_search;
#[cfg(all(feature = "codex-bridge", feature = "http"))]
mod codex_search;
mod html_text;
mod limits;
#[cfg(all(
    feature = "http",
    any(feature = "codex-bridge", feature = "claude-bridge")
))]
mod native_search;
mod path_guard;
mod read;
#[cfg(feature = "http")]
mod web_search;
mod write;

pub use bash::BashTool;
#[cfg(all(feature = "claude-bridge", feature = "http"))]
pub use claude_search::ClaudeNativeSearch;
#[cfg(all(feature = "codex-bridge", feature = "http"))]
pub use codex_search::CodexNativeSearch;
pub use html_text::html_to_text;
pub use limits::{BoundedOutput, BoundedText};
pub use path_guard::{GuardedPath, PathGuard};
pub use read::ReadTool;
#[cfg(feature = "http")]
pub use web_search::{
    JsonSearchBackend, OpenAiNativeSearch, SearchBackend, SearchResult, WebSearchTool,
};
pub use write::WriteTool;
