use std::{collections::BTreeMap, path::PathBuf};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use kurama_adapters::{AppPaths, ConfigRepository, FsSessionStore, SessionSecrets};
use kurama_cli::{
    app::App,
    args::{Args, ResumeChoice},
    tui::{ActivityState, Overlay, ToolLifecycle, TranscriptEntry},
};
use kurama_protocol::{
    config::{AuthRef, KuramaConfig, OrchestrationConfig, ProfileConfig, ProfileKind},
    id::{CallId, OperationId},
    policy::{AutoBoundaries, ExecutionMode},
    runtime::{EngineCommand, RuntimeEvent},
    session::{EventEnvelope, SessionEvent, SessionMetadata},
    tool::{Operation, ToolResult},
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
    assert_eq!(app.state.onboarding.prompt(), "Profile name");
    assert!(matches!(
        app.state.transcript.as_slice(),
        [TranscriptEntry::Startup {
            version,
            project,
            mode: ExecutionMode::Supervised,
        }] if version == env!("CARGO_PKG_VERSION") && project.ends_with("/project")
    ));
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
    assert!(matches!(
        app.state.transcript.first(),
        Some(TranscriptEntry::Startup {
            version,
            project,
            mode: ExecutionMode::Supervised,
        }) if version == env!("CARGO_PKG_VERSION") && project.ends_with("/project")
    ));
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
    assert!(transcript_has_notice(&app, "Previous run used YOLO"));
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
async fn resume_hydrates_the_visible_transcript_once() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let store = FsSessionStore::open(paths.root().to_path_buf()).expect("store");
    let metadata = SessionMetadata {
        id: "s_transcript".into(),
        created_at_ms: 1,
        project_root: project
            .canonicalize()
            .expect("canonical")
            .display()
            .to_string(),
        profile: "work".into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    };
    store.create(&metadata).expect("create session");
    let output_blob = store
        .put_blob(b"head\nfull middle output\ntail\n")
        .expect("store output blob");
    for (sequence, event) in [
        SessionEvent::SessionStarted { metadata },
        SessionEvent::UserMessage {
            text: "inspect the parser".into(),
        },
        SessionEvent::AssistantMessage {
            text: "checking it".into(),
        },
        SessionEvent::ToolProposed {
            operation_id: OperationId::from("operation"),
            call_id: CallId::from("call"),
            operation: Operation::Read {
                path: "parser.rs".into(),
                external: false,
            },
        },
        SessionEvent::ToolCompleted {
            operation_id: OperationId::from("operation"),
            result: ToolResult {
                call_id: CallId::from("call"),
                output: "head\n[omitted]\ntail\n".into(),
                is_error: false,
                metadata: serde_json::json!({
                    "tool_name": "read",
                    "display_blobs": {
                        "output": output_blob.clone()
                    }
                }),
                truncated: true,
                blob_refs: Vec::new(),
            },
        },
        SessionEvent::TurnFailed {
            error: "provider disconnected".into(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        store
            .append(&EventEnvelope::new(
                sequence as u64,
                sequence as u64 + 1,
                "s_transcript".into(),
                None,
                event,
            ))
            .expect("append replay event");
    }

    let mut app = App::bootstrap_with_paths(
        &Args {
            resume: Some(ResumeChoice::Id("s_transcript".into())),
            ..Args::default()
        },
        project,
        paths,
        SessionSecrets::default(),
    )
    .expect("bootstrap");

    let durable_replay = store
        .replay(&"s_transcript".into())
        .expect("durable replay");
    let durable_result = durable_replay
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolCompleted { result, .. } => Some(result),
            _ => None,
        })
        .expect("durable tool result");
    assert_eq!(durable_result.output, "head\n[omitted]\ntail\n");
    assert!(durable_result.metadata.get("display_output").is_none());
    assert!(
        !serde_json::to_string(&durable_replay)
            .expect("serialize durable replay")
            .contains("full middle output")
    );

    assert!(matches!(
        &app.state.transcript[..],
        [
            TranscriptEntry::Startup {
                version,
                project,
                mode: ExecutionMode::Supervised,
            },
            TranscriptEntry::UserTurn { body: user },
            TranscriptEntry::AssistantMessage { body: assistant },
            TranscriptEntry::ToolCall(tool),
            TranscriptEntry::Error { body: error },
        ] if version == env!("CARGO_PKG_VERSION")
            && project.ends_with("/project")
            && user == "inspect the parser"
            && assistant == "checking it"
            && tool.name == "read"
            && tool.output == "head\nfull middle output\ntail\n"
            && tool.lifecycle == ToolLifecycle::Completed
            && error == "provider disconnected"
    ));

    app.state.apply_runtime_event(RuntimeEvent::AssistantDelta {
        text: "retrying now".into(),
    });
    assert_eq!(
        app.state
            .transcript
            .iter()
            .filter(|entry| matches!(
                entry,
                TranscriptEntry::AssistantMessage { body } if body == "checking it"
            ))
            .count(),
        1
    );
    assert!(matches!(
        app.state.transcript.last(),
        Some(TranscriptEntry::AssistantMessage { body }) if body == "retrying now"
    ));
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
    assert!(transcript_has_notice(&app, "resuming session s_previous"));
}

#[tokio::test]
async fn invalid_slash_commands_append_error_without_exiting() {
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
    assert_eq!(app.state.activity(), &ActivityState::Idle);
    assert!(transcript_has_error(&app, "unknown or invalid command"));
}

#[tokio::test]
async fn slash_palette_completes_selection_and_waits_for_required_arguments() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let mut app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");

    type_command(&mut app, "/res");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)))
        .expect("complete command");
    assert_eq!(app.state.composer, "/resume ");

    press_enter(&mut app);
    assert_eq!(app.state.composer, "/resume ");
    assert!(!transcript_has_error(&app, "unknown or invalid command"));

    app.state.composer.clear();
    app.state.cursor = 0;
    type_command(&mut app, "/");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
        .expect("select next command");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)))
        .expect("complete selected command");
    assert_eq!(app.state.composer, "/agents");
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
    assert!(transcript_has_notice(&app, &session_id));
    submit_command(&mut app, "/context");
    assert!(transcript_has_notice(&app, "100000 token input limit"));
    assert!(transcript_has_notice(&app, &session_id));
    submit_command(&mut app, "/mode auto");
    assert_eq!(
        repository.read_state().expect("state").last_mode,
        Some(ExecutionMode::Auto)
    );
    assert!(transcript_has_notice(&app, "mode auto"));

    submit_command(&mut app, "/model archive");
    assert_eq!(
        app.restart_args().and_then(|args| args.profile.as_deref()),
        Some("archive")
    );
    assert!(matches!(
        app.state.sent_commands().last(),
        Some(EngineCommand::Shutdown)
    ));
    assert!(transcript_has_notice(&app, "switching to profile archive"));
}

#[tokio::test]
async fn normal_submit_queues_the_turn_and_sets_thinking() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let mut app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");

    submit_command(&mut app, "inspect the repository");

    assert!(matches!(
        app.state.activity(),
        ActivityState::Thinking { .. }
    ));
    assert!(matches!(
        app.state.sent_commands().last(),
        Some(EngineCommand::SubmitTurn { text, .. }) if text == "inspect the repository"
    ));
}

#[tokio::test]
async fn follow_up_turns_wait_for_the_active_turn_and_dispatch_in_order() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let mut app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");
    app.state.set_thinking();

    submit_command(&mut app, "second turn");
    submit_command(&mut app, "third turn");

    assert_eq!(app.state.pending_turn_count(), 2);
    assert!(app.state.sent_commands().is_empty());
    assert!(
        !app.state.transcript.iter().any(
            |entry| matches!(entry, TranscriptEntry::UserTurn { body } if body == "second turn")
        )
    );

    app.state.apply_runtime_event(RuntimeEvent::TurnCompleted);
    assert_eq!(app.state.pending_turn_count(), 1);
    assert!(matches!(
        app.state.sent_commands().last(),
        Some(EngineCommand::SubmitTurn { text, .. }) if text == "second turn"
    ));

    app.state.take_commands();
    app.state.apply_runtime_event(RuntimeEvent::TurnCompleted);
    assert_eq!(app.state.pending_turn_count(), 0);
    assert!(matches!(
        app.state.sent_commands().last(),
        Some(EngineCommand::SubmitTurn { text, .. }) if text == "third turn"
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
    assert!(transcript_has_notice(&app, "added profile claude"));
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
        .expect("back to connection");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
        .expect("close wizard");
    submit_command(&mut app, "/connect");

    assert!(app.state.onboarding.is_selecting_connection());
    assert_eq!(app.state.onboarding.display_input(), "");
}

#[tokio::test]
async fn at_sign_tab_completes_a_project_file() {
    let (_temp, paths, project) = fixture();
    std::fs::create_dir_all(project.join("src")).expect("src");
    std::fs::write(project.join("src/lib.rs"), "fn x() {}").expect("write");
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let mut app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");

    type_command(&mut app, "@lib");
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)))
        .expect("complete file");
    assert_eq!(app.state.composer, "@src/lib.rs ");
}

#[tokio::test]
async fn pasting_an_image_path_attaches_a_mention() {
    let (_temp, paths, project) = fixture();
    let png = project.join("shot.png");
    std::fs::write(&png, b"\x89PNG\r\n\x1a\nrest").expect("png");
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let mut app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");

    app.handle_event(Event::Paste(png.to_string_lossy().into_owned()))
        .expect("paste image");
    assert!(
        app.state.composer.starts_with("@.kurama/paste/") && app.state.composer.ends_with(".png "),
        "{}",
        app.state.composer
    );
    assert!(transcript_has_notice(&app, "attached .kurama/paste/"));
}

#[tokio::test]
async fn ctrl_r_searches_composer_history() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let mut app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");

    app.state.remember_prompt("open the palette");
    app.state.remember_prompt("fix the failing tests");
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    )))
    .expect("start history search");
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('f'),
        KeyModifiers::NONE,
    )))
    .expect("filter history");
    assert_eq!(app.state.history_matches(), ["fix the failing tests"]);
    press_enter(&mut app);
    assert_eq!(app.state.composer, "fix the failing tests");
    assert!(!app.state.history_search_active());
}

#[tokio::test]
async fn escape_dismisses_file_mentions_without_clearing_composer() {
    let (_temp, paths, project) = fixture();
    std::fs::create_dir_all(project.join("src")).expect("src");
    std::fs::write(project.join("src/lib.rs"), "fn x() {}").expect("write");
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let mut app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");

    type_command(&mut app, "@lib");
    assert!(
        !app.state.file_suggestions().is_empty(),
        "{:?}",
        app.state.file_suggestions()
    );
    app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
        .expect("dismiss files");
    assert!(app.state.file_suggestions().is_empty());
    assert_eq!(app.state.composer, "@lib");
}

#[tokio::test]
async fn history_search_ignores_composer_control_keys() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let mut app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");

    app.state.remember_prompt("fix the failing tests");
    app.state.composer = "draft".into();
    app.state.cursor = 5;
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    )))
    .expect("start history search");
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    )))
    .expect("ignore ctrl+u");
    assert_eq!(app.state.composer, "draft");
    assert!(app.state.history_search_active());
}

#[tokio::test]
async fn paste_during_history_search_extends_the_query() {
    let (_temp, paths, project) = fixture();
    let repository = ConfigRepository::open(paths.clone()).expect("repository");
    repository
        .write_config(&bridge_config())
        .expect("write config");
    let mut app =
        App::bootstrap_with_paths(&Args::default(), project, paths, SessionSecrets::default())
            .expect("bootstrap");

    app.state.remember_prompt("fix the failing tests");
    app.handle_event(Event::Key(KeyEvent::new(
        KeyCode::Char('r'),
        KeyModifiers::CONTROL,
    )))
    .expect("start history search");
    app.handle_event(Event::Paste("fail".into()))
        .expect("paste query");
    assert_eq!(app.state.history_search_query(), Some("fail"));
    assert_eq!(app.state.history_matches(), ["fix the failing tests"]);
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

fn transcript_has_notice(app: &App, needle: &str) -> bool {
    app.state
        .transcript
        .iter()
        .any(|entry| matches!(entry, TranscriptEntry::Notice { body, .. } if body.contains(needle)))
}

fn transcript_has_error(app: &App, needle: &str) -> bool {
    app.state
        .transcript
        .iter()
        .any(|entry| matches!(entry, TranscriptEntry::Error { body } if body.contains(needle)))
}
