const OPTIONS: [&str; 5] = [
    "Codex subscription",
    "Claude subscription",
    "OpenAI API key",
    "Anthropic API key",
    "OpenAI-compatible or local endpoint",
];

#[derive(Debug, Clone, Default)]
pub struct OnboardingState {
    selected: usize,
}

impl OnboardingState {
    pub fn new() -> Self {
        Self::default()
    }

    pub const fn options(&self) -> [&'static str; 5] {
        OPTIONS
    }

    pub const fn selected(&self) -> usize {
        self.selected
    }

    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1).min(OPTIONS.len() - 1);
    }

    pub fn select_previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn selected_option(&self) -> &'static str {
        OPTIONS[self.selected]
    }
}
