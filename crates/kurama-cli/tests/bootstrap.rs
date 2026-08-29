use std::{collections::BTreeMap, path::PathBuf};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use kurama_adapters::{AppPaths, ConfigRepository, FsSessionStore, SessionSecrets};
use kurama_cli::{
    app::App,
    args::{Args, ResumeChoice},
    tui::Overlay,
};
use kurama_protocol::{
    config::{AuthRef, KuramaConfig, OrchestrationConfig, ProfileConfig, ProfileKind},
    policy::{AutoBoundaries, ExecutionMode},
    runtime::EngineCommand,
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

fn bridge_profile(model: &str) -> ProfileConfig {
    ProfileConfig {
        kind: ProfileKind::CodexCli,
        model: model.into(),
        endpoint: None,
        auth: None,
        command: Some("codex".into()),
        max_input_tokens: 100_000,
        max_output_tokens: 10_000,
        escalation_profiles: Vec::new(),
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
    let mut app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");
    assert_eq!(app.state.overlay(), Overlay::Onboarding);
    assert!(!app.is_connected());

    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("select connection");

    assert_eq!(app.state.overlay(), Overlay::Onboarding);
    assert!(app.state.status.contains("profile name"));
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

#[tokio::test]
async fn resume_uses_the_recorded_profile() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    let mut config = bridge_config();
    config
        .profiles
        .insert("archive".into(), bridge_profile("archive-model"));
    repository.write_config(&config).expect("write config");
    let store = FsSessionStore::open(paths.root().to_path_buf()).expect("store");
    let metadata = SessionMetadata {
        id: "s_archive".into(),
        created_at_ms: 1,
        project_root: project
            .canonicalize()
            .expect("canonical")
            .display()
            .to_string(),
        profile: "archive".into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    };
    store.create(&metadata).expect("create session");
    store
        .append(&EventEnvelope::new(
            0,
            1,
            metadata.id.clone(),
            None,
            SessionEvent::SessionStarted { metadata },
        ))
        .expect("append start");

    let app = App::bootstrap_with_paths(
        &Args {
            resume: Some(ResumeChoice::Id("s_archive".into())),
            ..Args::default()
        },
        project,
        paths,
        SessionSecrets::default(),
    )
    .expect("bootstrap");

    assert_eq!(app.state.profile, "archive");
    assert_eq!(app.state.model, "archive-model");
}

#[tokio::test]
async fn remembered_safe_mode_is_restored_on_the_next_launch() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    repository
        .remember_mode(ExecutionMode::Auto)
        .expect("remember mode");

    let app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");

    assert_eq!(app.state.mode, ExecutionMode::Auto);
}

#[tokio::test]
async fn session_commands_request_an_in_process_restart() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let mut app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");

    type_command(&mut app, "/resume s_previous");
    let exit = app
        .handle_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("submit command");

    assert!(!exit, "the TUI waits for the runtime shutdown event");
    assert!(matches!(
        app.state.sent_commands().last(),
        Some(EngineCommand::Shutdown)
    ));
    assert_eq!(
        app.restart_args().and_then(|args| args.resume.as_ref()),
        Some(&ResumeChoice::Id("s_previous".into()))
    );
}

#[tokio::test]
async fn invalid_slash_commands_report_status_without_exiting() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let mut app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");

    type_command(&mut app, "/does-not-exist");
    let exit = app
        .handle_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("invalid command remains inside the TUI");

    assert!(!exit);
    assert!(app.state.status.contains("unknown or invalid command"));
}

#[tokio::test]
async fn live_controls_list_context_persist_mode_and_switch_profile() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    let mut config = bridge_config();
    config
        .profiles
        .insert("archive".into(), bridge_profile("archive-model"));
    repository.write_config(&config).expect("write config");
    let mut app = App::bootstrap_with_paths(
        &Args::default(),
        project,
        paths.clone(),
        SessionSecrets::default(),
    )
    .expect("bootstrap");
    let session_id = app.session_id().expect("session id").to_string();

    submit_command(&mut app, "/sessions");
    assert!(app.state.status.contains(&session_id));
    submit_command(&mut app, "/context");
    assert!(app.state.status.contains("100000 token input limit"));
    assert!(app.state.status.contains(&session_id));
    submit_command(&mut app, "/mode auto");
    assert_eq!(
        repository.read_state().expect("state").last_mode,
        Some(ExecutionMode::Auto)
    );

    submit_command(&mut app, "/model archive");
    assert_eq!(
        app.restart_args().and_then(|args| args.profile.as_deref()),
        Some("archive")
    );
    assert!(matches!(
        app.state.sent_commands().last(),
        Some(EngineCommand::Shutdown)
    ));
}

#[test]
fn onboarding_writes_a_cli_profile_and_requests_restart() {
    let (_temp, paths, project) = fixture();
    let mut app = App::bootstrap_with_paths(
        &Args::default(),
        project,
        paths.clone(),
        SessionSecrets::default(),
    )
    .expect("bootstrap");

    press_enter(&mut app);
    press_enter(&mut app);
    type_command(&mut app, "frontier-model");
    let exit = press_enter(&mut app);

    assert!(exit);
    let config = ConfigRepository::open(paths)
        .expect("repository")
        .read_config()
        .expect("read config")
        .expect("config");
    let profile = config.profiles.get("codex").expect("codex profile");
    assert_eq!(profile.kind, ProfileKind::CodexCli);
    assert_eq!(profile.model, "frontier-model");
    assert_eq!(profile.command.as_deref(), Some("codex"));
    assert_eq!(
        app.restart_args().and_then(|args| args.profile.as_deref()),
        Some("codex")
    );
}

#[test]
fn session_auth_profiles_prompt_for_a_masked_runtime_secret() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    let mut config = bridge_config();
    config.profiles.insert(
        "remote".into(),
        ProfileConfig {
            kind: ProfileKind::OpenAi,
            model: "remote-model".into(),
            endpoint: None,
            auth: Some(AuthRef::Session),
            command: None,
            max_input_tokens: 100_000,
            max_output_tokens: 10_000,
            escalation_profiles: Vec::new(),
        },
    );
    config.default_profile = Some("remote".into());
    repository.write_config(&config).expect("write config");
    let mut app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");

    assert!(!app.is_connected());
    assert_eq!(app.state.overlay(), Overlay::Onboarding);
    assert!(app.state.onboarding.prompt().contains("remote"));
    type_command(&mut app, "top-secret");
    assert_eq!(app.state.onboarding.display_input(), "••••••••••");
    let exit = press_enter(&mut app);

    assert!(exit);
    assert!(app.restart_args().is_some());
}

#[tokio::test]
async fn inactive_session_profile_does_not_block_the_selected_profile() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    let mut config = bridge_config();
    config.profiles.insert(
        "remote".into(),
        ProfileConfig {
            kind: ProfileKind::OpenAi,
            model: "remote-model".into(),
            endpoint: None,
            auth: Some(AuthRef::Session),
            command: None,
            max_input_tokens: 100_000,
            max_output_tokens: 10_000,
            escalation_profiles: Vec::new(),
        },
    );
    repository.write_config(&config).expect("write config");

    let app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");

    assert!(app.is_connected());
    assert_eq!(app.state.profile, "work");
}

#[tokio::test]
async fn connect_adds_a_profile_without_stopping_the_active_session() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let mut app = App::bootstrap_with_paths(
        &Args::default(),
        project.clone(),
        paths,
        SessionSecrets::default(),
    )
    .expect("bootstrap");

    submit_command(&mut app, "/connect");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
        .expect("select Claude");
    press_enter(&mut app);
    press_enter(&mut app);
    type_command(&mut app, "claude-model");
    press_enter(&mut app);

    assert!(app.is_connected());
    assert_eq!(app.state.profile, "work");
    assert!(app.restart_args().is_none());
    assert!(
        !app.state
            .sent_commands()
            .iter()
            .any(|command| matches!(command, EngineCommand::Shutdown))
    );
    assert_eq!(
        repository
            .resolve_profile(&project, None)
            .expect("resolve profile")
            .as_deref(),
        Some("work")
    );
    assert!(
        repository
            .read_config()
            .expect("read config")
            .expect("config")
            .profiles
            .contains_key("claude")
    );
}

#[tokio::test]
async fn reopening_connect_resets_a_cancelled_wizard() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let mut app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");

    submit_command(&mut app, "/connect");
    press_enter(&mut app);
    type_command(&mut app, "discarded");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
        .expect("cancel wizard");
    submit_command(&mut app, "/connect");

    assert!(app.state.onboarding.is_selecting_connection());
    assert_eq!(app.state.onboarding.display_input(), "");
}

fn type_command(app: &mut App, command: &str) {
    for character in command.chars() {
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::NONE,
        )))
        .expect("type command");
    }
}

fn submit_command(app: &mut App, command: &str) {
    type_command(app, command);
    press_enter(app);
}

fn press_enter(app: &mut App) -> bool {
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )))
    .expect("submit command")
}
