use kurama_protocol::config::{AuthRef, ProfileConfig, ProfileKind};
use unicode_segmentation::UnicodeSegmentation;
use zeroize::Zeroize;

use super::{
    input::{grapheme_boundary_at_or_after, next_grapheme_boundary, previous_grapheme_boundary},
    transcript::sanitize_terminal_text,
};

const OPTIONS: [&str; 5] = [
    "Codex subscription",
    "Claude subscription",
    "OpenAI API key",
    "Anthropic API key",
    "OpenAI-compatible or local endpoint",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnboardingStage {
    Connection,
    Profile,
    Endpoint,
    Model,
    Secret,
    Credential,
}

#[derive(Clone)]
pub enum OnboardingSubmission {
    Profile {
        name: String,
        profile: ProfileConfig,
        secret: Option<String>,
    },
    Credential {
        profile: String,
        secret: String,
    },
}

#[derive(Clone)]
pub struct OnboardingState {
    selected: usize,
    stage: OnboardingStage,
    input: String,
    cursor: usize,
    profile_name: String,
    endpoint: Option<String>,
    model: String,
    credential_profile: Option<String>,
    error: Option<String>,
}

impl Default for OnboardingState {
    fn default() -> Self {
        Self::new()
    }
}

impl OnboardingState {
    pub fn new() -> Self {
        Self {
            selected: 0,
            stage: OnboardingStage::Connection,
            input: String::new(),
            cursor: 0,
            profile_name: String::new(),
            endpoint: None,
            model: String::new(),
            credential_profile: None,
            error: None,
        }
    }

    pub fn credential(profile: impl Into<String>) -> Self {
        Self {
            selected: 0,
            stage: OnboardingStage::Credential,
            input: String::new(),
            cursor: 0,
            profile_name: String::new(),
            endpoint: None,
            model: String::new(),
            credential_profile: Some(profile.into()),
            error: None,
        }
    }

    pub const fn options(&self) -> [&'static str; 5] {
        OPTIONS
    }

    pub const fn selected(&self) -> usize {
        self.selected
    }

    pub fn select_next(&mut self) {
        if self.stage == OnboardingStage::Connection {
            self.selected = (self.selected + 1).min(OPTIONS.len() - 1);
        }
    }

    pub fn select_previous(&mut self) {
        if self.stage == OnboardingStage::Connection {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    pub fn select_index(&mut self, index: usize) {
        if self.stage == OnboardingStage::Connection && index < OPTIONS.len() {
            self.selected = index;
        }
    }

    pub fn selected_option(&self) -> &'static str {
        OPTIONS[self.selected]
    }

    pub fn is_selecting_connection(&self) -> bool {
        self.stage == OnboardingStage::Connection
    }

    pub fn prompt(&self) -> String {
        match self.stage {
            OnboardingStage::Connection => "How should Kurama connect?".into(),
            OnboardingStage::Profile => "Profile name".into(),
            OnboardingStage::Endpoint => "OpenAI-compatible endpoint".into(),
            OnboardingStage::Model => "Model identifier".into(),
            OnboardingStage::Secret => {
                "API key (leave blank only for unauthenticated endpoints)".into()
            }
            OnboardingStage::Credential => format!(
                "Session credential for {}",
                self.credential_profile.as_deref().unwrap_or("profile")
            ),
        }
    }

    pub fn step_label(&self) -> &'static str {
        match self.stage {
            OnboardingStage::Connection => "setup",
            OnboardingStage::Profile => "profile",
            OnboardingStage::Endpoint => "endpoint",
            OnboardingStage::Model => "model",
            OnboardingStage::Secret | OnboardingStage::Credential => "credential",
        }
    }

    pub fn display_input(&self) -> String {
        if self.is_secret() {
            "•".repeat(self.input.graphemes(true).count())
        } else {
            sanitize_terminal_text(&self.input).into_owned()
        }
    }

    pub fn display_input_window(&self, width: usize) -> (String, usize) {
        let available = width.saturating_sub(1);
        if available == 0 {
            return (String::new(), 0);
        }
        let secret = self.is_secret();
        let display_width = |grapheme: &str| {
            if secret {
                1
            } else {
                ratatui::text::Span::raw(grapheme).width()
            }
        };
        let mut before = self.input[..self.cursor]
            .grapheme_indices(true)
            .rev()
            .peekable();
        let mut start = self.cursor;
        let mut column = 0;
        while let Some(&(offset, grapheme)) = before.peek() {
            let cells = display_width(grapheme);
            if column + cells > available / 2 {
                break;
            }
            start = offset;
            column += cells;
            before.next();
        }
        let mut end = self.cursor;
        let mut used = column;
        for (offset, grapheme) in self.input[self.cursor..].grapheme_indices(true) {
            let cells = display_width(grapheme);
            if used + cells > available {
                break;
            }
            end = self.cursor + offset + grapheme.len();
            used += cells;
        }
        for (offset, grapheme) in before {
            let cells = display_width(grapheme);
            if used + cells > available {
                break;
            }
            start = offset;
            column += cells;
            used += cells;
        }
        let display = if secret {
            "•".repeat(used)
        } else {
            sanitize_terminal_text(&self.input[start..end]).into_owned()
        };
        (display, column)
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_error(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
    }

    pub fn push(&mut self, character: char) {
        if self.stage != OnboardingStage::Connection && !character.is_control() {
            self.input.insert(self.cursor, character);
            self.cursor =
                grapheme_boundary_at_or_after(&self.input, self.cursor + character.len_utf8());
            self.error = None;
        }
    }

    pub fn insert_str(&mut self, text: &str) {
        if self.stage == OnboardingStage::Connection {
            return;
        }
        let mut filtered = String::new();
        let text = if text.chars().any(char::is_control) {
            filtered.extend(text.chars().filter(|character| !character.is_control()));
            filtered.as_str()
        } else {
            text
        };
        self.input.insert_str(self.cursor, text);
        self.cursor = grapheme_boundary_at_or_after(&self.input, self.cursor + text.len());
        filtered.zeroize();
        self.error = None;
    }

    pub fn backspace(&mut self) {
        let previous = previous_grapheme_boundary(&self.input, self.cursor);
        self.remove_input_range(previous, self.cursor);
    }

    pub fn delete_forward(&mut self) {
        let next = next_grapheme_boundary(&self.input, self.cursor);
        self.remove_input_range(self.cursor, next);
    }

    pub fn move_left(&mut self) {
        self.cursor = previous_grapheme_boundary(&self.input, self.cursor);
    }

    pub fn move_right(&mut self) {
        self.cursor = next_grapheme_boundary(&self.input, self.cursor);
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.input.len();
    }

    fn remove_input_range(&mut self, start: usize, end: usize) {
        self.input[start..end].zeroize();
        self.input.replace_range(start..end, "");
        self.cursor = grapheme_boundary_at_or_after(&self.input, start);
        self.error = None;
    }

    pub fn go_back(&mut self) -> bool {
        self.error = None;
        match self.stage {
            OnboardingStage::Connection | OnboardingStage::Credential => false,
            OnboardingStage::Profile => {
                self.stage = OnboardingStage::Connection;
                self.input.clear();
                self.cursor = 0;
                true
            }
            OnboardingStage::Endpoint => {
                self.stage = OnboardingStage::Profile;
                self.input = self.profile_name.clone();
                self.move_end();
                true
            }
            OnboardingStage::Model if self.selected == 4 => {
                self.stage = OnboardingStage::Endpoint;
                self.input = self.endpoint.clone().unwrap_or_default();
                self.move_end();
                true
            }
            OnboardingStage::Model => {
                self.stage = OnboardingStage::Profile;
                self.input = self.profile_name.clone();
                self.move_end();
                true
            }
            OnboardingStage::Secret => {
                self.input.zeroize();
                self.stage = OnboardingStage::Model;
                self.input = self.model.clone();
                self.move_end();
                true
            }
        }
    }

    pub fn begin(&mut self) {
        if self.stage != OnboardingStage::Connection {
            return;
        }
        self.stage = OnboardingStage::Profile;
        self.input = match self.selected {
            0 => "codex",
            1 => "claude",
            2 => "openai",
            3 => "anthropic",
            _ => "compatible",
        }
        .into();
        self.move_end();
    }

    pub fn submit(&mut self) -> Result<Option<OnboardingSubmission>, String> {
        match self.stage {
            OnboardingStage::Connection => {
                self.begin();
                Ok(None)
            }
            OnboardingStage::Profile => {
                let profile = self.input.trim();
                if !valid_profile_name(profile) {
                    return Err("profile name must use letters, digits, '.', '-', or '_'".into());
                }
                self.profile_name = profile.into();
                self.input.clear();
                self.cursor = 0;
                self.stage = if self.selected == 4 {
                    OnboardingStage::Endpoint
                } else {
                    OnboardingStage::Model
                };
                Ok(None)
            }
            OnboardingStage::Endpoint => {
                let endpoint = self.input.trim();
                if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
                    return Err("endpoint must start with http:// or https://".into());
                }
                self.endpoint = Some(endpoint.trim_end_matches('/').into());
                self.input.clear();
                self.cursor = 0;
                self.stage = OnboardingStage::Model;
                Ok(None)
            }
            OnboardingStage::Model => {
                let model = self.input.trim();
                if model.is_empty() {
                    return Err("model identifier is required".into());
                }
                self.model = model.into();
                self.input.clear();
                self.cursor = 0;
                if self.selected <= 1 {
                    Ok(Some(self.profile_submission(None)))
                } else {
                    self.stage = OnboardingStage::Secret;
                    Ok(None)
                }
            }
            OnboardingStage::Secret => {
                let mut secret = std::mem::take(&mut self.input);
                self.cursor = 0;
                if self.selected != 4 && secret.trim().is_empty() {
                    secret.zeroize();
                    return Err("API key is required".into());
                }
                let secret = (!secret.is_empty()).then_some(secret);
                Ok(Some(self.profile_submission(secret)))
            }
            OnboardingStage::Credential => {
                let secret = std::mem::take(&mut self.input);
                self.cursor = 0;
                if secret.is_empty() {
                    return Err("session credential is required".into());
                }
                Ok(Some(OnboardingSubmission::Credential {
                    profile: self
                        .credential_profile
                        .clone()
                        .ok_or_else(|| "credential profile is unavailable".to_owned())?,
                    secret,
                }))
            }
        }
    }

    fn is_secret(&self) -> bool {
        matches!(
            self.stage,
            OnboardingStage::Secret | OnboardingStage::Credential
        )
    }

    fn profile_submission(&self, secret: Option<String>) -> OnboardingSubmission {
        let kind = match self.selected {
            0 => ProfileKind::CodexCli,
            1 => ProfileKind::ClaudeCli,
            2 => ProfileKind::OpenAi,
            3 => ProfileKind::Anthropic,
            _ => ProfileKind::OpenAiCompatible,
        };
        let command = match kind {
            ProfileKind::CodexCli => Some("codex".into()),
            ProfileKind::ClaudeCli => Some("claude".into()),
            _ => None,
        };
        OnboardingSubmission::Profile {
            name: self.profile_name.clone(),
            profile: ProfileConfig {
                kind,
                model: self.model.clone(),
                endpoint: self.endpoint.clone(),
                auth: secret.as_ref().map(|_| AuthRef::Session),
                command,
                max_input_tokens: 128_000,
                max_output_tokens: 16_000,
                escalation_profiles: Vec::new(),
            },
            secret,
        }
    }
}

impl Drop for OnboardingState {
    fn drop(&mut self) {
        self.input.zeroize();
    }
}

fn valid_profile_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_stay_masked_and_delete_as_graphemes() {
        let mut state = OnboardingState::credential("remote");
        state.insert_str("Ae\u{301}👩‍👩‍👧‍👦");
        let (display, column) = state.display_input_window(3);
        assert_eq!(display, "••");
        assert_eq!(column, 2);
        state.backspace();
        state.backspace();
        let Some(OnboardingSubmission::Credential { secret, .. }) = state.submit().unwrap() else {
            panic!("expected credential submission");
        };
        assert_eq!(secret, "A");
    }

    #[test]
    fn long_onboarding_input_keeps_its_tail_and_cursor_in_bounds() {
        let mut state = OnboardingState::new();
        state.begin();
        state.input = format!("{}界e\u{301}", "a".repeat(32_000));
        state.move_end();
        let (display, column) = state.display_input_window(4);
        assert_eq!(display, "界e\u{301}");
        assert_eq!(column, 3);
        assert_eq!(state.display_input_window(0), (String::new(), 0));
    }

    #[test]
    fn navigation_and_deletion_keep_combining_and_joined_graphemes_whole() {
        let mut state = OnboardingState::credential("remote");
        state.insert_str("Ae\u{301}👩‍👩‍👧‍👦Z");
        state.move_home();
        state.move_left();
        state.move_right();
        state.move_right();
        state.move_left();
        state.delete_forward();
        state.move_right();
        state.backspace();
        state.move_end();
        state.move_right();
        state.delete_forward();
        let Some(OnboardingSubmission::Credential { secret, .. }) = state.submit().unwrap() else {
            panic!("expected credential submission");
        };
        assert_eq!(secret, "AZ");
    }

    #[test]
    fn credentials_can_be_corrected_in_the_middle() {
        let mut state = OnboardingState::credential("remote");
        state.insert_str("sk-abXdef");
        state.move_home();
        for _ in 0..5 {
            state.move_right();
        }
        state.delete_forward();
        state.insert_str("c\n\r");
        state.backspace();
        state.push('c');
        state.move_end();
        state.backspace();
        state.move_home();
        state.backspace();
        let Some(OnboardingSubmission::Credential { secret, .. }) = state.submit().unwrap() else {
            panic!("expected credential submission");
        };
        assert_eq!(secret, "sk-abcde");
    }

    #[test]
    fn inserted_base_character_moves_past_existing_combining_marks() {
        let mut state = OnboardingState::credential("remote");
        state.insert_str("\u{301}Z");
        state.move_home();
        state.push('e');
        state.push('X');
        let Some(OnboardingSubmission::Credential { secret, .. }) = state.submit().unwrap() else {
            panic!("expected credential submission");
        };
        assert_eq!(secret, "e\u{301}XZ");
    }

    #[test]
    fn pasted_joined_emoji_is_inserted_before_moving_to_a_grapheme_boundary() {
        let mut state = OnboardingState::credential("remote");
        state.insert_str("👩👩Z");
        state.move_home();
        state.move_right();
        state.insert_str("\u{200d}👧");
        state.push('X');
        let Some(OnboardingSubmission::Credential { secret, .. }) = state.submit().unwrap() else {
            panic!("expected credential submission");
        };
        assert_eq!(secret, "👩‍👧X👩Z");
    }

    #[test]
    fn deleting_a_separator_moves_past_newly_joined_regional_indicators() {
        let mut state = OnboardingState::credential("remote");
        state.insert_str("🇦x🇧");
        state.move_home();
        state.move_right();
        state.delete_forward();
        state.push('X');
        let Some(OnboardingSubmission::Credential { secret, .. }) = state.submit().unwrap() else {
            panic!("expected credential submission");
        };
        assert_eq!(secret, "🇦🇧X");
    }

    #[test]
    fn masked_windows_follow_the_cursor_at_narrow_widths() {
        let mut state = OnboardingState::credential("remote");
        state.insert_str("Ae\u{301}👩‍👩‍👧‍👦XYZ");
        state.move_home();
        for _ in 0..3 {
            state.move_right();
        }
        for (width, expected, column) in [
            (0, "", 0),
            (1, "", 0),
            (2, "•", 0),
            (3, "••", 1),
            (4, "•••", 1),
            (5, "••••", 2),
        ] {
            assert_eq!(state.display_input_window(width), (expected.into(), column));
        }
        state.move_home();
        assert_eq!(state.display_input_window(4), ("•••".into(), 0));
        state.move_end();
        assert_eq!(state.display_input_window(4), ("•••".into(), 3));
    }

    #[test]
    fn visible_windows_keep_wide_graphemes_and_cursor_columns_intact() {
        let mut state = OnboardingState::new();
        state.begin();
        state.input.clear();
        state.move_home();
        state.insert_str("A界e\u{301}👩‍👩‍👧‍👦XYZ");
        state.move_home();
        for _ in 0..3 {
            state.move_right();
        }
        assert_eq!(state.display_input_window(4), ("e\u{301}👩‍👩‍👧‍👦".into(), 1));
        assert_eq!(state.display_input_window(2), ("e\u{301}".into(), 1));
        state.move_home();
        assert_eq!(state.display_input_window(4), ("A界".into(), 0));
    }

    #[test]
    fn stage_transitions_reset_cursor_and_restored_values_append_at_the_end() {
        let mut state = OnboardingState::new();
        state.select_index(4);
        state.begin();
        state.push('x');
        assert_eq!(state.display_input(), "compatiblex");
        assert!(state.submit().unwrap().is_none());
        state.insert_str("http://localhost");
        assert!(state.submit().unwrap().is_none());
        state.insert_str("model");
        assert!(state.submit().unwrap().is_none());
        state.insert_str("discarded-secret");
        state.move_home();
        assert!(state.go_back());
        state.push('x');
        assert_eq!(state.display_input(), "modelx");
        assert!(state.go_back());
        state.push('/');
        assert_eq!(state.display_input(), "http://localhost/");
        assert!(state.go_back());
        state.push('y');
        assert_eq!(state.display_input(), "compatiblexy");
        assert!(state.go_back());
        state.begin();
        state.push('z');
        assert_eq!(state.display_input(), "compatiblez");
    }

    #[test]
    fn submitted_and_rejected_credentials_leave_an_editable_empty_input() {
        let mut state = OnboardingState::credential("remote");
        state.insert_str("first");
        let Some(OnboardingSubmission::Credential { secret, .. }) = state.submit().unwrap() else {
            panic!("expected credential submission");
        };
        assert_eq!(secret, "first");
        assert!(state.submit().is_err());
        state.push('x');
        let Some(OnboardingSubmission::Credential { secret, .. }) = state.submit().unwrap() else {
            panic!("expected credential submission");
        };
        assert_eq!(secret, "x");
    }
}
