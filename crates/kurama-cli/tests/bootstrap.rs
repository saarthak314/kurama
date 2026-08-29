use std::{collections::BTreeMap, path::PathBuf};

use kurama_adapters::{AppPaths, ConfigRepository, FsSessionStore, SessionSecrets};
use kurama_cli::{
    app::App,
    args::{Args, ResumeChoice},
    tui::Overlay,
};
use kurama_protocol::{
    config::{KuramaConfig, OrchestrationConfig, ProfileConfig, ProfileKind},
    policy::{AutoBoundaries, ExecutionMode},
    session::{EventEnvelope, SessionEvent, SessionMetadata},
    traits::SessionStore,
};
use tempfile::TempDir;

fn bridge_config() -> KuramaConfig {
    KuramaConfig {
        version: 1,
        default_profile: Some("work".into()),
        default_mode: ExecutionMode::Supervised,
        profiles: BTreeMap::from([(
            "work".into(),
            ProfileConfig {
                kind: ProfileKind::CodexCli,
                model: "frontier".into(),
                endpoint: None,
                auth: None,
                command: Some("codex".into()),
                max_input_tokens: 100_000,
                max_output_tokens: 10_000,
                escalation_profiles: Vec::new(),
            },
        )]),
        roles: BTreeMap::new(),
        orchestration: OrchestrationConfig::default(),
        auto: AutoBoundaries::default(),
        search: None,
    }
}

fn fixture() -> (TempDir, AppPaths, PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = AppPaths::from_root(temp.path().join(".kurama"));
    let project = temp.path().join("project");
    std::fs::create_dir(&project).expect("project");
    (temp, paths, project)
}

#[test]
fn missing_configuration_opens_onboarding_without_a_runtime() {
    let (_temp, paths, project) = fixture();
    let app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");
    assert_eq!(app.state.overlay(), Overlay::Onboarding);
    assert!(!app.is_connected());
}

#[tokio::test]
async fn configured_profile_composes_the_real_four_tool_runtime() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");

    let app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");
    assert!(app.is_connected());
    assert_eq!(app.state.profile, "work");
    assert_eq!(app.state.model, "frontier");
    assert_eq!(App::tool_names(), ["bash", "read", "web-search", "write"]);
}

#[tokio::test]
async fn continue_resumes_the_project_session_and_downgrades_old_yolo() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let store = FsSessionStore::open(paths.root().to_path_buf()).expect("store");
    let metadata = SessionMetadata {
        id: "s_previous".into(),
        created_at_ms: 1,
        project_root: project
            .canonicalize()
            .expect("canonical")
            .display()
            .to_string(),
        profile: "work".into(),
        mode: ExecutionMode::Yolo,
        redaction_best_effort: true,
    };
    store.create(&metadata).expect("create session");
    store
        .append(&EventEnvelope::new(
            0,
            1,
            metadata.id.clone(),
            None,
            SessionEvent::SessionStarted {
                metadata: metadata.clone(),
            },
        ))
        .expect("append start");
    repository
        .remember_latest_session(&project, &metadata.id)
        .expect("remember session");

    let app = App::bootstrap_with_paths(
        &Args {
            resume: Some(ResumeChoice::Continue),
            ..Args::default()
        },
        project,
        paths,
        SessionSecrets::default(),
    )
    .expect("bootstrap");
    assert_eq!(app.session_id().map(AsRef::as_ref), Some("s_previous"));
    assert_eq!(app.state.mode, ExecutionMode::Supervised);
    assert!(app.state.status.contains("Previous run used YOLO"));
}
