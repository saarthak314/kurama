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
    #[cfg(feature = "codex-bridge")]
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
        #[cfg(not(feature = "codex-bridge"))]
        let _ = &paths;
        Self {
            #[cfg(any(
                feature = "openai",
                feature = "anthropic",
                feature = "openai-compatible"
            ))]
            http,
            #[cfg(feature = "codex-bridge")]
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
        if profile_name.trim().is_empty() {
            return Err(KuramaError::Configuration(
                "profile name cannot be empty".into(),
            ));
        }
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
    ) -> Result<Option<zeroize::Zeroizing<String>>, KuramaError> {
        self.credentials
            .resolve_optional(profile_name, profile.auth.as_ref(), session_secrets)
            .map(|secret| secret.map(|secret| secret.into_zeroizing()))
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
            .with_program(cli_command(profile)?);
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
        let bridge =
            crate::bridges::claude::ClaudeBridge::new().with_program(cli_command(profile)?);
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
            root.join("sessions")
                .join(profile_cache_key(profile_name).as_ref()),
            root.join("control-v1.json"),
        )
    }
}

#[cfg(feature = "codex-bridge")]
fn profile_cache_key(profile_name: &str) -> std::borrow::Cow<'_, str> {
    if !matches!(profile_name, "." | "..")
        && profile_name.len() <= 255
        && profile_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return std::borrow::Cow::Borrowed(profile_name);
    }
    use sha2::{Digest, Sha256};
    // Preserve existing safe directories so persisted CLI cursors keep their
    // workspace; reserve a prefix outside the old alphabet for encoded names.
    std::borrow::Cow::Owned(crate::id::hexadecimal(
        "~",
        Sha256::digest(profile_name.as_bytes()).as_ref(),
    ))
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

#[cfg(any(feature = "codex-bridge", feature = "claude-bridge"))]
fn cli_command(profile: &ProfileConfig) -> Result<&str, KuramaError> {
    profile
        .command
        .as_deref()
        .filter(|command| !command.trim().is_empty())
        .ok_or_else(|| KuramaError::Configuration("CLI profile requires a nonempty command".into()))
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
