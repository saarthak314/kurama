mod agents;
mod approval;
mod input;
mod onboarding;
mod render;
mod state;

pub use agents::{AgentRow, sort_agents};
pub use approval::ApprovalState;
pub use input::{TerminalGuard, spawn_input_thread};
pub use onboarding::{OnboardingState, OnboardingSubmission};
pub use render::render;
pub(crate) use render::{SURFACE, transcript_lines};
pub use state::{Overlay, TranscriptEntry, TranscriptKind, TuiState};
