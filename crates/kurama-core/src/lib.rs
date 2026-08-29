#![forbid(unsafe_code)]

pub mod agent_manager;
pub mod cancel;
pub mod context;
pub mod engine;
pub mod orchestrator;
pub mod policy;
pub mod prompts;
pub mod recovery;
pub mod testing;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
