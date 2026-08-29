#![cfg(feature = "native-credentials")]

#[path = "../src/config.rs"]
mod config;
#[path = "../src/credentials.rs"]
mod credentials;
#[path = "../src/redact.rs"]
mod redact;

use std::{collections::BTreeMap, path::PathBuf};

use config::{AppPaths, ConfigRepository};
use credentials::{CredentialResolver, SecretValue, SessionSecrets, parse_auth_ref};
use kurama_protocol::{
    KuramaError,
    config::{
        AuthRef, KuramaConfig, MutableState, OrchestrationConfig, ProfileConfig, ProfileKind,
        SearchConfig,
    },
    id::SessionId,
    policy::{AutoBoundaries, ExecutionMode},
};
use redact::Redactor;

fn profile(kind: ProfileKind, auth: Option<AuthRef>) -> ProfileConfig {
    ProfileConfig {
        kind,
        model: "frontier-model".into(),
        endpoint: Some("https://example.invalid/v1".into()),
        auth,
        command: None,
        max_input_tokens: 200_000,
        max_output_tokens: 12_000,
        escalation_profiles: Vec::new(),
    }
}

fn fixture_config(default_profile: &str) -> KuramaConfig {
    let profiles = [
        (
            "global".into(),
            profile(
                ProfileKind::OpenAi,
                Some(AuthRef::Environment {
                    name: "OPENAI_API_KEY".into(),
                }),
            ),
        ),
        (
            "project".into(),
            profile(ProfileKind::OpenAiCompatible, None),
        ),
        ("cli".into(), profile(ProfileKind::OpenAiCompatible, None)),
    ]
    .into_iter()
    .collect();

    KuramaConfig {
        version: 1,
        default_profile: Some(default_profile.into()),
        default_mode: ExecutionMode::Supervised,
        profiles,
        roles: BTreeMap::new(),
        orchestration: OrchestrationConfig { max_concurrency: 4 },
        auto: AutoBoundaries {
            write_roots: vec![PathBuf::from(".")],
            allowed_commands: vec!["cargo".into(), "rg".into()],
            allowed_hosts: vec!["api.openai.com".into()],
        },
        search: Some(SearchConfig::Provider),
    }
}

#[test]
fn project_profile_beats_global_default_and_cli_beats_project() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = AppPaths::from_root(temp.path().join(".kurama"));
    let repository = ConfigRepository::open(paths).expect("repository");
    repository
        .write_config(&fixture_config("global"))
        .expect("config");
    repository
        .remember_project_profile(temp.path(), "project")
        .expect("remember");

    assert_eq!(
        repository
            .resolve_profile(temp.path(), None)
            .expect("project resolve"),
        Some("project".into())
    );
    assert_eq!(
        repository
            .resolve_profile(temp.path(), Some("cli"))
            .expect("cli resolve"),
        Some("cli".into())
    );

    let reopened = ConfigRepository::open(repository.paths().clone()).expect("reopen");
    assert_eq!(
        reopened
            .resolve_profile(temp.path(), None)
            .expect("persisted resolve"),
        Some("project".into())
    );
}

#[test]
fn config_roundtrip_uses_v1_wire_shape_and_state_stays_non_secret() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repository = ConfigRepository::open(AppPaths::from_root(temp.path().join(".kurama")))
        .expect("repository");
    let expected = fixture_config("global");
    repository.write_config(&expected).expect("write config");

    assert_eq!(
        repository.read_config().expect("read config"),
        Some(expected)
    );
    let encoded = std::fs::read_to_string(repository.paths().config()).expect("config text");
    assert!(encoded.contains("auth = \"env:OPENAI_API_KEY\""));
    assert!(encoded.contains("[policy.auto]"));
    assert!(!encoded.contains("Environment"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            std::fs::metadata(repository.paths().root())
                .expect("root metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(repository.paths().config())
                .expect("config metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    repository
        .remember_latest_session(temp.path(), &SessionId::from("s_latest"))
        .expect("latest session");
    repository
        .remember_mode(ExecutionMode::Auto)
        .expect("remember mode");
    let state = repository.read_state().expect("state");
    assert_eq!(state.last_mode, Some(ExecutionMode::Auto));
    assert_eq!(
        state
            .latest_sessions
            .get(&std::fs::canonicalize(temp.path()).expect("canonical project")),
        Some(&SessionId::from("s_latest"))
    );
}

#[test]
fn invalid_config_versions_refs_auth_and_persisted_yolo_are_rejected() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repository = ConfigRepository::open(AppPaths::from_root(temp.path().join(".kurama")))
        .expect("repository");
    let cases = [
        "version = 2\ndefault_mode = \"supervised\"\n",
        "version = 1\ndefault_mode = \"yolo\"\n",
        "version = 1\ndefault_mode = \"supervised\"\ndefault_profile = \"missing\"\n",
        concat!(
            "version = 1\ndefault_mode = \"supervised\"\n",
            "[profiles.primary]\nkind = \"open_ai\"\nmodel = \"m\"\n",
            "auth = \"sk-plaintext\"\nmax_input_tokens = 1\nmax_output_tokens = 1\n"
        ),
        concat!(
            "version = 1\ndefault_mode = \"supervised\"\n",
            "[orchestration]\nmax_concurrency = 9\n"
        ),
    ];

    for source in cases {
        std::fs::write(repository.paths().config(), source).expect("fixture config");
        assert!(
            matches!(repository.read_config(), Err(KuramaError::Configuration(_))),
            "accepted invalid config: {source}"
        );
    }

    assert!(matches!(
        repository.write_state(&MutableState {
            last_mode: Some(ExecutionMode::Yolo),
            ..MutableState::default()
        }),
        Err(KuramaError::Configuration(_))
    ));
}

#[test]
fn auth_references_accept_only_safe_non_secret_forms() {
    assert_eq!(
        parse_auth_ref("env:OPENAI_API_KEY").expect("env"),
        AuthRef::Environment {
            name: "OPENAI_API_KEY".into()
        }
    );
    assert_eq!(
        parse_auth_ref("keychain:openai/main").expect("keychain"),
        AuthRef::Keychain {
            service: "openai".into(),
            account: "main".into()
        }
    );
    assert_eq!(
        parse_auth_ref("session").expect("session"),
        AuthRef::Session
    );

    for invalid in [
        "",
        "env:",
        "env:BAD-NAME",
        "keychain:missing-account",
        "keychain:/account",
        "keychain:service/",
        "sk-plaintext",
    ] {
        assert!(parse_auth_ref(invalid).is_err(), "accepted {invalid}");
    }
}

#[test]
fn session_credentials_are_resolved_without_serializing_secret_values() {
    let mut sessions = SessionSecrets::default();
    sessions.insert("primary", SecretValue::new("session-secret".into()));
    assert!(sessions.contains("primary"));
    let resolver = CredentialResolver;
    let resolved = resolver
        .resolve("primary", &AuthRef::Session, &sessions)
        .expect("resolve session secret");
    assert_eq!(resolved.expose(), "session-secret");
    assert_eq!(format!("{resolved:?}"), "SecretValue([REDACTED])");
    assert!(
        resolver
            .resolve_optional("primary", None, &sessions)
            .expect("optional credential")
            .is_none()
    );

    assert!(matches!(
        resolver.resolve(
            "primary",
            &AuthRef::Environment {
                name: "__KURAMA_TEST_CREDENTIAL_MUST_NOT_EXIST__".into(),
            },
            &sessions,
        ),
        Err(KuramaError::Configuration(_))
    ));
    assert!(sessions.remove("primary").is_some());

    let _: fn(&CredentialResolver, &str, &str, &SecretValue) -> Result<(), KuramaError> =
        CredentialResolver::write_native;
}

#[test]
fn registered_secrets_are_replaced_exactly_longest_first() {
    let mut redactor = Redactor::default();
    redactor.register(SecretValue::new("secret-value".into()));
    redactor.register(SecretValue::new("secret-value-extended".into()));
    redactor.register(SecretValue::new("short".into()));

    assert_eq!(
        redactor.redact("Bearer secret-value-extended; token=secret-value; short"),
        "Bearer [REDACTED]; token=[REDACTED]; short"
    );
    let mut json = serde_json::json!({
        "header": "secret-value",
        "nested": ["prefix secret-value-extended suffix", 1]
    });
    redactor.redact_json(&mut json);
    assert_eq!(json["header"], "[REDACTED]");
    assert_eq!(json["nested"][0], "prefix [REDACTED] suffix");

    let encoded = serde_json::to_string(&redactor.safe_metadata()).expect("metadata");
    assert_eq!(encoded, "{\"registered_patterns\":2}");
    assert!(!encoded.contains("secret-value"));
    assert_eq!(
        redactor.redact_bytes(b"raw secret-value bytes"),
        b"raw [REDACTED] bytes"
    );

    let _: fn() -> Result<AppPaths, KuramaError> = AppPaths::discover;
}
