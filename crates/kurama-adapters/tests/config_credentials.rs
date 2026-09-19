#![cfg(feature = "native-credentials")]

use std::{collections::BTreeMap, path::PathBuf};

use kurama_adapters::{
    AppPaths, ConfigRepository, CredentialResolver, Redactor, SecretValue, SessionSecrets,
    parse_auth_ref,
};
use kurama_protocol::{
    KuramaError,
    config::{
        AuthRef, KuramaConfig, MutableState, OrchestrationConfig, ProfileConfig, ProfileKind,
        SearchConfig,
    },
    id::SessionId,
    policy::{AutoBoundaries, ExecutionMode},
};

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
}

#[test]
fn independent_repositories_preserve_concurrent_state_updates() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = AppPaths::from_root(temp.path().join("state"));
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&fixture_config("global"))
        .expect("config");
    let projects: Vec<_> = (0..8)
        .map(|index| {
            let path = temp.path().join(format!("project-{index}"));
            std::fs::create_dir(&path).expect("project");
            path.canonicalize().expect("canonical project")
        })
        .collect();
    let barrier = std::sync::Barrier::new(projects.len());
    let (failures, errors) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        for (index, project) in projects.iter().enumerate() {
            let paths = &paths;
            let barrier = &barrier;
            let failures = &failures;
            scope.spawn(move || {
                let repository = ConfigRepository::open(paths.clone());
                if let Err(error) = &repository {
                    failures
                        .send(format!("open {index}: {error}"))
                        .expect("failure receiver");
                }
                for round in 0..8 {
                    barrier.wait();
                    if let Ok(repository) = &repository {
                        let outcome = repository
                            .remember_project_profile(project, "project")
                            .map_err(|error| format!("profile {index}/{round}: {error}"))
                            .and_then(|()| {
                                repository
                                    .remember_latest_session(
                                        project,
                                        &SessionId::from(format!("session-{index}-{round}")),
                                    )
                                    .map_err(|error| format!("session {index}/{round}: {error}"))
                            });
                        if let Err(error) = outcome {
                            failures.send(error.to_string()).expect("failure receiver");
                        }
                    }
                }
            });
        }
    });
    drop(failures);
    let errors = errors.into_iter().collect::<Vec<_>>();
    assert!(
        errors.is_empty(),
        "concurrent state updates failed: {errors:?}"
    );
    let state = ConfigRepository::open(paths)
        .expect("reopen")
        .read_state()
        .expect("state");
    for (index, project) in projects.iter().enumerate() {
        assert_eq!(
            state.project_profiles.get(project).map(String::as_str),
            Some("project")
        );
        assert_eq!(
            state.latest_sessions.get(project),
            Some(&SessionId::from(format!("session-{index}-7")))
        );
    }
}

#[test]
fn concurrent_first_openers_observe_complete_initial_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = AppPaths::from_root(temp.path().join("new-root"));
    let barrier = std::sync::Barrier::new(16);
    std::thread::scope(|scope| {
        for _ in 0..16 {
            let paths = &paths;
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                let repository = ConfigRepository::open(paths.clone()).expect("concurrent open");
                assert!(repository.read_config().expect("initial config").is_none());
                let state = repository.read_state().expect("complete initial state");
                assert!(state.project_profiles.is_empty());
                assert!(state.latest_sessions.is_empty());
            });
        }
    });
}

#[cfg(unix)]
#[test]
fn config_reads_reject_swapped_leaves_and_keep_the_opened_root() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("root");
    let outside = temp.path().join("outside");
    let repository = ConfigRepository::open(AppPaths::from_root(root.clone())).expect("repository");
    let other =
        ConfigRepository::open(AppPaths::from_root(outside.clone())).expect("outside repository");
    other
        .remember_mode(ExecutionMode::Auto)
        .expect("outside mode");
    for name in ["config.toml", "state.json"] {
        let original = std::fs::read(root.join(name)).expect("original");
        std::fs::remove_file(root.join(name)).expect("remove leaf");
        symlink(outside.join(name), root.join(name)).expect("swap leaf");
        assert!(if name == "config.toml" {
            repository.read_config().is_err()
        } else {
            repository.read_state().is_err()
        });
        std::fs::remove_file(root.join(name)).expect("remove link");
        std::fs::write(root.join(name), original).expect("restore leaf");
    }
    let moved = temp.path().join("opened-root");
    std::fs::rename(&root, &moved).expect("move root");
    symlink(&outside, &root).expect("swap ancestor");
    repository
        .remember_mode(ExecutionMode::Supervised)
        .expect("update opened root");
    assert_eq!(
        repository.read_state().expect("opened state").last_mode,
        Some(ExecutionMode::Supervised)
    );
    assert_eq!(
        other.read_state().expect("outside untouched").last_mode,
        Some(ExecutionMode::Auto)
    );
    assert_eq!(
        ConfigRepository::open(AppPaths::from_root(moved))
            .expect("reopen original")
            .read_state()
            .expect("durable original")
            .last_mode,
        Some(ExecutionMode::Supervised)
    );
}

#[test]
fn bootstrap_selection_is_all_or_nothing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = AppPaths::from_root(temp.path().join("state"));
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&fixture_config("global"))
        .expect("config");
    repository
        .remember_session(
            temp.path(),
            "global",
            &SessionId::from("old"),
            Some(ExecutionMode::Supervised),
        )
        .expect("initial selection");
    assert!(
        repository
            .remember_session(
                temp.path(),
                "project",
                &SessionId::from("new"),
                Some(ExecutionMode::Yolo)
            )
            .is_err()
    );
    let state = ConfigRepository::open(paths)
        .expect("reopen")
        .read_state()
        .expect("state");
    let project = temp.path().canonicalize().expect("project");
    assert_eq!(
        state.project_profiles.get(&project).map(String::as_str),
        Some("global")
    );
    assert_eq!(
        state.latest_sessions.get(&project),
        Some(&SessionId::from("old"))
    );
    assert_eq!(state.last_mode, Some(ExecutionMode::Supervised));
    repository
        .remember_session(
            temp.path(),
            "project",
            &SessionId::from("launch-only"),
            None,
        )
        .expect("launch-only selection");
    let state = repository.read_state().expect("launch-only state");
    assert_eq!(
        state.project_profiles.get(&project).map(String::as_str),
        Some("project")
    );
    assert_eq!(
        state.latest_sessions.get(&project),
        Some(&SessionId::from("launch-only"))
    );
    assert_eq!(state.last_mode, Some(ExecutionMode::Supervised));
}
