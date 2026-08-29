#![cfg(all(
    feature = "native-credentials",
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible",
    feature = "codex-bridge",
    feature = "claude-bridge"
))]

use kurama_adapters::{AppPaths, CredentialResolver, HttpClient, ProviderFactory, SessionSecrets};
use kurama_protocol::config::{AuthRef, ProfileConfig, ProfileKind};

#[test]
fn factory_maps_every_profile_kind_without_provider_flags() {
    let temp = tempfile::tempdir().expect("tempdir");
    let factory = ProviderFactory::new(
        HttpClient::new(),
        AppPaths::from_root(temp.path().join(".kurama")),
        CredentialResolver,
    );
    let sessions = SessionSecrets::default();

    let cases = [
        (ProfileKind::OpenAiCompatible, "openai_compatible"),
        (ProfileKind::CodexCli, "codex_cli"),
        (ProfileKind::ClaudeCli, "claude_cli"),
    ];
    for (kind, expected) in cases {
        let profile = profile(kind);
        let backend = factory.build("test", &profile, &sessions).expect("backend");
        assert_eq!(backend.backend_name(), expected);
    }
}

#[test]
fn native_byok_profiles_require_resolvable_credentials() {
    let temp = tempfile::tempdir().expect("tempdir");
    let factory = ProviderFactory::new(
        HttpClient::new(),
        AppPaths::from_root(temp.path().join(".kurama")),
        CredentialResolver,
    );
    let profile = profile(ProfileKind::OpenAi);
    let error = match factory.build("missing", &profile, &SessionSecrets::default()) {
        Ok(_) => panic!("missing credential accepted"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        kurama_protocol::KuramaError::Configuration(_)
    ));
}

fn profile(kind: ProfileKind) -> ProfileConfig {
    let (endpoint, auth, command) = match kind {
        ProfileKind::OpenAi => (
            Some("https://api.openai.com/v1".into()),
            Some(AuthRef::Session),
            None,
        ),
        ProfileKind::Anthropic => (
            Some("https://api.anthropic.com/v1".into()),
            Some(AuthRef::Session),
            None,
        ),
        ProfileKind::OpenAiCompatible => (Some("http://localhost:11434/v1".into()), None, None),
        ProfileKind::CodexCli => (None, None, Some("codex".into())),
        ProfileKind::ClaudeCli => (None, None, Some("claude".into())),
    };
    ProfileConfig {
        kind,
        model: "test-model".into(),
        endpoint,
        auth,
        command,
        max_input_tokens: 32_000,
        max_output_tokens: 4_000,
        escalation_profiles: Vec::new(),
    }
}
