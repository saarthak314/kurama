mod agents;
mod approval;
mod input;
mod onboarding;
mod render;
mod state;
mod transcript;

pub use agents::{AgentRow, sort_agents};
pub use approval::ApprovalState;
pub use input::{TerminalGuard, spawn_input_thread};
pub use onboarding::{OnboardingState, OnboardingSubmission};
pub(crate) use render::SURFACE;
pub use render::render;
pub use state::{ActivityState, Overlay, ToolLifecycle, ToolTranscript, TranscriptEntry, TuiState};
pub use transcript::{TranscriptDetail, transcript_lines};
