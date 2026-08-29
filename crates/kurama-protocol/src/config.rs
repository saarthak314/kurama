use std::{collections::BTreeMap, path::PathBuf};

use crate::policy::{AutoBoundaries, ExecutionMode};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    OpenAi,
    Anthropic,
    OpenAiCompatible,
    CodexCli,
    ClaudeCli,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthRef {
    Environment { name: String },
    Keychain { service: String, account: String },
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProfileConfig {
    pub kind: ProfileKind,
    pub model: String,
    pub endpoint: Option<String>,
    pub auth: Option<AuthRef>,
    pub command: Option<String>,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    #[serde(default)]
    pub escalation_profiles: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RoleConfig {
    pub profile: String,
    #[serde(default)]
    pub escalation_profiles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OrchestrationConfig {
    pub max_concurrency: usize,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self { max_concurrency: 4 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SearchConfig {
    Provider,
    Json {
        endpoint: String,
        auth: Option<AuthRef>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KuramaConfig {
    pub version: u32,
    pub default_profile: Option<String>,
    pub default_mode: ExecutionMode,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileConfig>,
    #[serde(default)]
    pub roles: BTreeMap<String, RoleConfig>,
    #[serde(default)]
    pub orchestration: OrchestrationConfig,
    #[serde(default)]
    pub auto: AutoBoundaries,
    pub search: Option<SearchConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MutableState {
    pub project_profiles: BTreeMap<PathBuf, String>,
    pub latest_sessions: BTreeMap<PathBuf, crate::id::SessionId>,
    pub last_mode: Option<ExecutionMode>,
}
