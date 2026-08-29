use std::sync::Arc;

#[cfg(feature = "codex-bridge")]
use std::path::PathBuf;

use kurama_protocol::{
    KuramaError,
    config::{ProfileConfig, ProfileKind},
    traits::ModelBackend,
};

use crate::{
    config::AppPaths,
    credentials::{CredentialResolver, SessionSecrets},
    http::HttpClient,
};

pub struct ProviderFactory {
    #[cfg(any(
        feature = "openai",
        feature = "anthropic",
        feature = "openai-compatible"
    ))]
    http: HttpClient,
    #[cfg(any(feature = "codex-bridge", feature = "claude-bridge"))]
    paths: AppPaths,
    #[cfg(any(
        feature = "openai",
        feature = "anthropic",
        feature = "openai-compatible"
    ))]
    credentials: CredentialResolver,
}

impl ProviderFactory {
    pub fn new(http: HttpClient, paths: AppPaths, credentials: CredentialResolver) -> Self {
        #[cfg(not(any(
            feature = "openai",
            feature = "anthropic",
            feature = "openai-compatible"
        )))]
        let _ = (&http, credentials);
        #[cfg(not(any(feature = "codex-bridge", feature = "claude-bridge")))]
        let _ = &paths;
        Self {
            #[cfg(any(
                feature = "openai",
                feature = "anthropic",
                feature = "openai-compatible"
            ))]
            http,
            #[cfg(any(feature = "codex-bridge", feature = "claude-bridge"))]
            paths,
            #[cfg(any(
                feature = "openai",
                feature = "anthropic",
                feature = "openai-compatible"
            ))]
            credentials,
        }
    }

    pub fn build(
        &self,
        profile_name: &str,
        profile: &ProfileConfig,
        session_secrets: &SessionSecrets,
    ) -> Result<Arc<dyn ModelBackend>, KuramaError> {
        validate_profile_name(profile_name)?;
        match profile.kind {
            ProfileKind::OpenAi => self.openai(profile_name, profile, session_secrets),
            ProfileKind::Anthropic => self.anthropic(profile_name, profile, session_secrets),
            ProfileKind::OpenAiCompatible => {
                self.openai_compatible(profile_name, profile, session_secrets)
            }
            ProfileKind::CodexCli => self.codex(profile_name, profile),
            ProfileKind::ClaudeCli => self.claude(profile),
        }
    }

    #[cfg(any(
        feature = "openai",
        feature = "anthropic",
        feature = "openai-compatible"
    ))]
    fn credential(
        &self,
        profile_name: &str,
        profile: &ProfileConfig,
        session_secrets: &SessionSecrets,
    ) -> Result<Option<String>, KuramaError> {
        self.credentials
            .resolve_optional(profile_name, profile.auth.as_ref(), session_secrets)
            .map(|secret| secret.map(|secret| secret.expose().to_owned()))
    }

    #[cfg(feature = "openai")]
    fn openai(
        &self,
        profile_name: &str,
        profile: &ProfileConfig,
        session_secrets: &SessionSecrets,
    ) -> Result<Arc<dyn ModelBackend>, KuramaError> {
        let secret = self
            .credential(profile_name, profile, session_secrets)?
            .ok_or_else(|| KuramaError::Configuration("OpenAI profile requires auth".into()))?;
        let endpoint = profile
            .endpoint
            .as_deref()
            .unwrap_or("https://api.openai.com/v1");
        Ok(Arc::new(
            crate::providers::openai::OpenAiBackend::from_endpoint(
                self.http.clone(),
                endpoint,
                secret,
            )?,
        ))
    }

    #[cfg(not(feature = "openai"))]
    fn openai(
        &self,
        _profile_name: &str,
        _profile: &ProfileConfig,
        _session_secrets: &SessionSecrets,
    ) -> Result<Arc<dyn ModelBackend>, KuramaError> {
        Err(disabled("openai"))
    }

    #[cfg(feature = "anthropic")]
    fn anthropic(
        &self,
        profile_name: &str,
        profile: &ProfileConfig,
        session_secrets: &SessionSecrets,
    ) -> Result<Arc<dyn ModelBackend>, KuramaError> {
        let secret = self
            .credential(profile_name, profile, session_secrets)?
            .ok_or_else(|| KuramaError::Configuration("Anthropic profile requires auth".into()))?;
        let endpoint = profile
            .endpoint
            .as_deref()
            .unwrap_or("https://api.anthropic.com/v1");
        Ok(Arc::new(
            crate::providers::anthropic::AnthropicBackend::from_endpoint(
                self.http.clone(),
                endpoint,
                secret,
            )?,
        ))
    }

    #[cfg(not(feature = "anthropic"))]
    fn anthropic(
        &self,
        _profile_name: &str,
        _profile: &ProfileConfig,
        _session_secrets: &SessionSecrets,
    ) -> Result<Arc<dyn ModelBackend>, KuramaError> {
        Err(disabled("anthropic"))
    }

    #[cfg(feature = "openai-compatible")]
    fn openai_compatible(
        &self,
        profile_name: &str,
        profile: &ProfileConfig,
        session_secrets: &SessionSecrets,
    ) -> Result<Arc<dyn ModelBackend>, KuramaError> {
        let endpoint = profile.endpoint.as_deref().ok_or_else(|| {
            KuramaError::Configuration("OpenAI-compatible profile requires endpoint".into())
        })?;
        let secret = self.credential(profile_name, profile, session_secrets)?;
        Ok(Arc::new(
            crate::providers::openai_compat::OpenAiCompatBackend::from_endpoint(
                self.http.clone(),
                endpoint,
                secret,
            )?,
        ))
    }

    #[cfg(not(feature = "openai-compatible"))]
    fn openai_compatible(
        &self,
        _profile_name: &str,
        _profile: &ProfileConfig,
        _session_secrets: &SessionSecrets,
    ) -> Result<Arc<dyn ModelBackend>, KuramaError> {
        Err(disabled("openai-compatible"))
    }

    #[cfg(feature = "codex-bridge")]
    fn codex(
        &self,
        profile_name: &str,
        profile: &ProfileConfig,
    ) -> Result<Arc<dyn ModelBackend>, KuramaError> {
        reject_cli_auth(profile)?;
        let (bridge_dir, schema_path) = self.bridge_paths(profile_name);
        let bridge = crate::bridges::codex::CodexBridge::new(bridge_dir, schema_path)
            .with_program(profile.command.as_deref().unwrap_or("codex"));
        Ok(Arc::new(bridge))
    }

    #[cfg(not(feature = "codex-bridge"))]
    fn codex(
        &self,
        _profile_name: &str,
        _profile: &ProfileConfig,
    ) -> Result<Arc<dyn ModelBackend>, KuramaError> {
        Err(disabled("codex-bridge"))
    }

    #[cfg(feature = "claude-bridge")]
    fn claude(&self, profile: &ProfileConfig) -> Result<Arc<dyn ModelBackend>, KuramaError> {
        reject_cli_auth(profile)?;
        let schema_path = self.paths.cache().join("bridge/control-v1.json");
        let bridge = crate::bridges::claude::ClaudeBridge::new(schema_path)
            .with_program(profile.command.as_deref().unwrap_or("claude"));
        Ok(Arc::new(bridge))
    }

    #[cfg(not(feature = "claude-bridge"))]
    fn claude(&self, _profile: &ProfileConfig) -> Result<Arc<dyn ModelBackend>, KuramaError> {
        Err(disabled("claude-bridge"))
    }

    #[cfg(feature = "codex-bridge")]
    fn bridge_paths(&self, profile_name: &str) -> (PathBuf, PathBuf) {
        let root = self.paths.cache().join("bridge");
        (
            root.join("sessions").join(profile_name),
            root.join("control-v1.json"),
        )
    }
}

fn validate_profile_name(profile_name: &str) -> Result<(), KuramaError> {
    if profile_name.is_empty()
        || !profile_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(KuramaError::Configuration(
            "profile name is unsafe for adapter cache paths".into(),
        ));
    }
    Ok(())
}

#[cfg(any(feature = "codex-bridge", feature = "claude-bridge"))]
fn reject_cli_auth(profile: &ProfileConfig) -> Result<(), KuramaError> {
    if profile.auth.is_some() {
        Err(KuramaError::Configuration(
            "subscription CLI profiles must use the CLI credential store".into(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(all(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible",
    feature = "codex-bridge",
    feature = "claude-bridge"
)))]
fn disabled(feature: &str) -> KuramaError {
    KuramaError::Configuration(format!("adapter feature {feature} is disabled"))
}
