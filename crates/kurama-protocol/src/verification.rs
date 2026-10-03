use crate::{id::OperationId, session::BlobRef};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationRecipe {
    pub command: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_cwd() -> String {
    ".".into()
}

fn default_timeout_ms() -> u64 {
    60_000
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    NotRun,
    Running,
    Passed,
    Failed,
    Cancelled,
    Denied,
    Interrupted,
}

impl VerificationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotRun => "not_run",
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Denied => "denied",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VerificationReport {
    pub name: String,
    pub command: String,
    pub cwd: String,
    pub timeout_ms: u64,
    pub status: VerificationStatus,
    pub operation_id: Option<OperationId>,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub output_refs: Vec<BlobRef>,
    pub message: Option<String>,
}

impl VerificationReport {
    pub fn not_run(name: String, recipe: &VerificationRecipe) -> Self {
        Self {
            name,
            command: recipe.command.clone(),
            cwd: recipe.cwd.clone(),
            timeout_ms: recipe.timeout_ms,
            status: VerificationStatus::NotRun,
            operation_id: None,
            started_at_ms: None,
            finished_at_ms: None,
            exit_code: None,
            output_refs: Vec::new(),
            message: None,
        }
    }
}
