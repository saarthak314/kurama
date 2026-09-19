#![cfg(all(
    feature = "native-credentials",
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible",
    feature = "codex-bridge",
    feature = "claude-bridge"
))]

use kurama_adapters::{
    AppPaths, ConfigRepository, CredentialResolver, HttpClient, ProviderFactory, SecretValue,
    SessionSecrets,
};
use kurama_protocol::config::{AuthRef, ProfileConfig, ProfileKind};

#[test]
fn factory_maps_every_profile_kind_without_provider_flags() {
    let temp = tempfile::tempdir().expect("tempdir");
    let factory = ProviderFactory::new(
        HttpClient::new(),
        AppPaths::from_root(temp.path().join(".kurama")),
        CredentialResolver,
    );
    let mut sessions = SessionSecrets::default();
    sessions.insert("team 日本語", SecretValue::new("test-secret".into()));

    let cases = [
        (ProfileKind::OpenAi, "openai"),
        (ProfileKind::Anthropic, "anthropic"),
        (ProfileKind::OpenAiCompatible, "openai_compatible"),
        (ProfileKind::CodexCli, "codex_cli"),
        (ProfileKind::ClaudeCli, "claude_cli"),
    ];
    for (kind, expected) in cases {
        let profile = profile(kind);
        let backend = factory
            .build("team 日本語", &profile, &sessions)
            .expect("backend");
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

#[test]
fn factory_requires_the_same_cli_command_as_config() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let paths = AppPaths::from_root(temporary.path().join("app"));
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    let factory = ProviderFactory::new(HttpClient::new(), paths, CredentialResolver);
    for kind in [ProfileKind::CodexCli, ProfileKind::ClaudeCli] {
        for command in [None, Some(" ".into())] {
            let mut profile = profile(kind.clone());
            profile.command = command;
            let config =
                config_with_profiles([("cli".into(), profile.clone())].into_iter().collect());
            assert!(repository.write_config(&config).is_err());
            assert!(matches!(
                factory.build("cli", &profile, &SessionSecrets::default()),
                Err(kurama_protocol::KuramaError::Configuration(_))
            ));
        }
    }
}

fn config_with_profiles(
    profiles: std::collections::BTreeMap<String, ProfileConfig>,
) -> kurama_protocol::config::KuramaConfig {
    kurama_protocol::config::KuramaConfig {
        version: 1,
        default_profile: None,
        default_mode: kurama_protocol::policy::ExecutionMode::Supervised,
        profiles,
        roles: Default::default(),
        orchestration: Default::default(),
        auto: Default::default(),
        search: None,
    }
}

struct NeverCancel;

impl kurama_protocol::traits::CancelSignal for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> kurama_protocol::traits::BoxFuture<'static, ()> {
        Box::pin(std::future::pending())
    }
}

#[cfg(unix)]
#[tokio::test]
async fn config_accepted_names_use_stable_contained_codex_cache_paths() {
    use futures_util::StreamExt;
    use kurama_protocol::model::{BackendCursor, ModelEvent, ModelProfile, ModelRequest};
    use std::os::unix::fs::PermissionsExt;
    let temporary = tempfile::tempdir().expect("tempdir");
    let paths = AppPaths::from_root(temporary.path().join("app"));
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    let executable = temporary.path().join("codex");
    let captured = temporary.path().join("workspace");
    std::fs::write(&executable, format!(r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
  if [ "$1" = '-C' ]; then printf '%s' "$2" > '{}'; fi
  shift
done
printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"kind\":\"final\",\"text\":\"done\"}}"}}}}'
printf '%s\n' '{{"type":"turn.completed"}}'
"#, captured.display())).expect("write fixture");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("fixture mode");
    let names = [
        ".".into(),
        "..".into(),
        "a.b".into(),
        "team profile".into(),
        "日本語".into(),
        "../escape".into(),
        "~reserved".into(),
        "x".repeat(300),
    ];
    let profiles = names
        .iter()
        .map(|name: &String| {
            let mut profile = profile(ProfileKind::CodexCli);
            profile.command = Some(executable.display().to_string());
            (name.clone(), profile)
        })
        .collect();
    repository
        .write_config(&config_with_profiles(profiles))
        .expect("config accepts profile names");
    let config = repository
        .read_config()
        .expect("read config")
        .expect("persisted config");
    let factory = ProviderFactory::new(HttpClient::new(), paths.clone(), CredentialResolver);
    let mut cache_paths = std::collections::HashSet::new();
    for name in names {
        let backend = factory
            .build(&name, &config.profiles[&name], &SessionSecrets::default())
            .expect("accepted config builds");
        let request = ModelRequest {
            session_id: "session".into(),
            agent_id: None,
            workspace_root: temporary.path().to_string_lossy().into_owned(),
            profile: ModelProfile::new(&name, "test", 32_000, 4_000),
            system: String::new(),
            items: Vec::new(),
            tools: Vec::new(),
            delegation: None,
            continuation: None,
        };
        let mut initial_path = None;
        for continuation in [
            None,
            Some(BackendCursor {
                backend: "codex_cli".into(),
                value: "thread".into(),
            }),
        ] {
            let mut request = request.clone();
            request.continuation = continuation;
            let stream = backend.stream(request, &NeverCancel).await.expect("stream");
            let events = stream.collect::<Vec<_>>().await;
            assert!(events.iter().all(Result::is_ok));
            assert!(events.iter().any(
                |event| matches!(event, Ok(ModelEvent::TextDelta { text }) if text == "done")
            ));
            let path = std::path::PathBuf::from(
                std::fs::read_to_string(&captured).expect("captured workspace"),
            );
            assert_eq!(
                path.parent(),
                Some(paths.cache().join("bridge/sessions").as_path())
            );
            assert!(path.is_dir());
            if name == "a.b" {
                assert_eq!(path.file_name().and_then(|name| name.to_str()), Some("a.b"));
            }
            if let Some(initial) = &initial_path {
                assert_eq!(&path, initial, "resume workspace changed");
            }
            initial_path = Some(path);
        }
        assert!(
            cache_paths.insert(initial_path.expect("initial workspace")),
            "profile cache collision"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn claude_stream_does_not_require_a_bridge_cache() {
    use futures_util::StreamExt;
    use kurama_protocol::model::{ModelEvent, ModelProfile, ModelRequest};
    use std::os::unix::fs::PermissionsExt;
    let temporary = tempfile::tempdir().expect("tempdir");
    let paths = AppPaths::from_root(temporary.path().join("app"));
    std::fs::create_dir_all(paths.cache().parent().expect("cache parent")).expect("app directory");
    std::fs::write(paths.cache(), "not a directory").expect("unusable cache");
    let executable = temporary.path().join("claude");
    std::fs::write(&executable, r#"#!/bin/sh
printf '%s\n' '{"type":"result","subtype":"success","structured_output":{"kind":"final","text":"done","calls":[],"agents":[]}}'
"#).expect("fixture");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("fixture mode");
    let mut profile = profile(ProfileKind::ClaudeCli);
    profile.command = Some(executable.display().to_string());
    let factory = ProviderFactory::new(HttpClient::new(), paths.clone(), CredentialResolver);
    let backend = factory
        .build("claude", &profile, &SessionSecrets::default())
        .expect("backend");
    let request = ModelRequest {
        session_id: "session".into(),
        agent_id: None,
        workspace_root: temporary.path().to_string_lossy().into_owned(),
        profile: ModelProfile::new("claude", "test", 32_000, 4_000),
        system: String::new(),
        items: Vec::new(),
        tools: Vec::new(),
        delegation: None,
        continuation: None,
    };
    let stream = backend
        .stream(request, &NeverCancel)
        .await
        .expect("Claude stream without cache");
    let events = stream.collect::<Vec<_>>().await;
    assert!(events.iter().all(Result::is_ok));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(ModelEvent::TextDelta { text }) if text == "done"))
    );
    assert_eq!(
        std::fs::read_to_string(paths.cache()).expect("unchanged cache sentinel"),
        "not a directory"
    );
}
