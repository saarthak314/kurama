use kurama_protocol::config::{AuthRef, ProfileConfig, ProfileKind};
use zeroize::Zeroize;

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
    profile_name: String,
    endpoint: Option<String>,
    model: String,
    credential_profile: Option<String>,
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
            profile_name: String::new(),
            endpoint: None,
            model: String::new(),
            credential_profile: None,
        }
    }

    pub fn credential(profile: impl Into<String>) -> Self {
        Self {
            selected: 0,
            stage: OnboardingStage::Credential,
            input: String::new(),
            profile_name: String::new(),
            endpoint: None,
            model: String::new(),
            credential_profile: Some(profile.into()),
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
            "•".repeat(self.input.chars().count())
        } else {
            self.input.clone()
        }
    }

    pub fn push(&mut self, character: char) {
        if self.stage != OnboardingStage::Connection && !character.is_control() {
            self.input.push(character);
        }
    }

    pub fn backspace(&mut self) {
        self.input.pop();
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
                if self.selected <= 1 {
                    Ok(Some(self.profile_submission(None)))
                } else {
                    self.stage = OnboardingStage::Secret;
                    Ok(None)
                }
            }
            OnboardingStage::Secret => {
                let secret = std::mem::take(&mut self.input);
                if self.selected != 4 && secret.trim().is_empty() {
                    return Err("API key is required".into());
                }
                let secret = (!secret.is_empty()).then_some(secret);
                Ok(Some(self.profile_submission(secret)))
            }
            OnboardingStage::Credential => {
                let secret = std::mem::take(&mut self.input);
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
