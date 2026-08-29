use std::{collections::BTreeMap, sync::Arc, time::Duration};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use kurama_adapters::{BashTool, HttpClient, ReadTool, WebSearchTool, WriteTool};
use kurama_cli::{
    app::{App, run_with},
    tui::TuiState,
};
use kurama_core::{
    orchestrator::SmartOrchestrator,
    policy::DefaultPolicy,
    testing::{CollectingSink, MemoryStore, ScriptedBackend, SequenceIds},
};
use kurama_protocol::{
    agent::{AgentBudget, AgentSpec, DelegationRequest, OrchestrationContext, WriteScope},
    model::{FinishReason, ModelEvent, ModelProfile},
    policy::{AutoBoundaries, ExecutionMode},
    session::{SessionEvent, SessionMetadata},
    traits::{Orchestrator, SessionStore, Tool},
};
use kurama_sdk::AgentBuilder;
use ratatui::{Terminal, backend::TestBackend};
use tokio::sync::mpsc;

#[tokio::test]
async fn fake_provider_runs_tools_approval_child_panel_and_exact_once_resume() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("input.txt"), "inspect me\n").expect("fixture");
    let backend = Arc::new(ScriptedBackend::new(vec![
        tool_round(
            "read_call",
            "read",
            serde_json::json!({
                "files": [{"path": "input.txt", "start_line": 1, "end_line": 10}]
            }),
        ),
        tool_round(
            "write_call",
            "write",
            serde_json::json!({
                "path": "updated.txt",
                "content": "updated\n"
            }),
        ),
        vec![
            Ok(ModelEvent::Delegation {
                request: DelegationRequest {
                    agents: vec![AgentSpec {
                        role: "reviewer".into(),
                        objective: "inspect the completed update".into(),
                        profile: None,
                        context_refs: vec!["updated.txt".into()],
                        write_scope: WriteScope::default(),
                        budget: AgentBudget {
                            max_input_tokens: 16_000,
                            max_output_tokens: 2_000,
                            ..AgentBudget::default()
                        },
                        depends_on: Vec::new(),
                    }],
                },
            }),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::ToolCalls,
            }),
        ],
        vec![
            Ok(ModelEvent::TextDelta {
                text: "child checked the update".into(),
            }),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            }),
        ],
        vec![
            Ok(ModelEvent::TextDelta {
                text: "updated".into(),
            }),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            }),
        ],
        tool_round(
            "write_call",
            "write",
            serde_json::json!({
                "path": "updated.txt",
                "content": "duplicated\n"
            }),
        ),
        vec![
            Ok(ModelEvent::TextDelta {
                text: "resumed without repeating the write".into(),
            }),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            }),
        ],
    ]));
    let store = Arc::new(MemoryStore::default());
    let ids = Arc::new(SequenceIds::new(1));
    let profile = ModelProfile::new("fixture", "frontier", 32_000, 4_000);
    let orchestrator = Arc::new(SmartOrchestrator::new(ids.clone()));
    let workspace_root = workspace.path().to_path_buf();
    let write_scope = WriteScope {
        roots: vec![workspace_root.clone()],
        files: Vec::new(),
    };
    let orchestration = OrchestrationContext {
        parent_profile: profile.clone(),
        profiles: BTreeMap::from([("fixture".into(), profile.clone())]),
        role_routes: BTreeMap::new(),
        role_escalations: BTreeMap::new(),
        profile_escalations: BTreeMap::new(),
        parent_write_scope: write_scope.clone(),
        max_concurrency: 2,
        depth: 0,
        yolo: false,
    };
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(BashTool::default()),
        Arc::new(ReadTool::default()),
        Arc::new(WebSearchTool::new(HttpClient::new(), None)),
        Arc::new(WriteTool::default()),
    ];
    let mut builder = AgentBuilder::new()
        .profile(profile.clone(), backend)
        .active_profile("fixture")
        .policy(Arc::new(DefaultPolicy::new(
            ExecutionMode::Supervised,
            AutoBoundaries::default(),
        )))
        .store(store.clone())
        .sink(Arc::new(CollectingSink::default()))
        .orchestrator(orchestrator.clone())
        .ids(ids)
        .write_scope(write_scope)
        .orchestration_context(orchestration);
    for tool in tools {
        builder = builder.tool(tool);
    }
    let runtime = builder.build().expect("runtime");
    assert_eq!(
        runtime.registered_tools(),
        ["bash", "read", "web-search", "write"]
    );
    let metadata = SessionMetadata {
        id: "session".into(),
        created_at_ms: 1,
        project_root: workspace_root.display().to_string(),
        profile: "fixture".into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    };

    let (handle, runtime_events) = runtime
        .start(metadata.clone(), Vec::new())
        .expect("start runtime");
    let state = TuiState::new(
        "fixture",
        "frontier",
        workspace_root.display().to_string(),
        ExecutionMode::Supervised,
    );
    let app = App::from_runtime(
        state,
        handle,
        orchestrator.clone() as Arc<dyn Orchestrator>,
        metadata.id.clone(),
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).expect("terminal");
    let first_input = scripted_input(
        vec![
            (0, "use sub-agents to inspect and update the fixture"),
            (80, "a"),
            (120, "/agents"),
            (40, "\n"),
        ],
        80,
    );
    let app = tokio::time::timeout(
        Duration::from_secs(3),
        run_with(app, &mut terminal, first_input, runtime_events),
    )
    .await
    .expect("first run timeout")
    .expect("first run");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("updated.txt")).expect("updated file"),
        "updated\n"
    );
    assert!(!app.state.agents.is_empty());
    assert!(
        app.state
            .transcript
            .iter()
            .any(|entry| entry.body.contains("updated"))
    );
    assert!(
        store
            .events("session")
            .iter()
            .any(|event| matches!(event.event, SessionEvent::AgentCompleted { .. }))
    );

    tokio::time::sleep(Duration::from_millis(20)).await;
    let replay = store.replay(&metadata.id).expect("replay");
    let (handle, runtime_events) = runtime
        .start(metadata.clone(), replay)
        .expect("resume runtime");
    let state = TuiState::new(
        "fixture",
        "frontier",
        workspace_root.display().to_string(),
        ExecutionMode::Supervised,
    );
    let app = App::from_runtime(
        state,
        handle,
        orchestrator as Arc<dyn Orchestrator>,
        metadata.id.clone(),
    );
    let second_input = scripted_input(vec![(0, "resume check")], 100);
    let app = tokio::time::timeout(
        Duration::from_secs(3),
        run_with(app, &mut terminal, second_input, runtime_events),
    )
    .await
    .expect("resume timeout")
    .expect("resume run");
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("updated.txt")).expect("updated file"),
        "updated\n"
    );
    assert!(
        app.state
            .transcript
            .iter()
            .any(|entry| entry.body.contains("resumed without repeating"))
    );
    let write_completions = store
        .events("session")
        .iter()
        .filter(|event| matches!(
            &event.event,
            SessionEvent::ToolCompleted { result, .. } if result.call_id.as_ref() == "write_call"
        ))
        .count();
    assert_eq!(write_completions, 1);
}

fn tool_round(
    call_id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> Vec<Result<ModelEvent, kurama_protocol::KuramaError>> {
    vec![
        Ok(ModelEvent::ToolCall {
            call_id: call_id.into(),
            name: name.into(),
            arguments,
        }),
        Ok(ModelEvent::ResponseCompleted {
            cursor: None,
            finish_reason: FinishReason::ToolCalls,
        }),
    ]
}

fn scripted_input(
    steps: Vec<(u64, &'static str)>,
    shutdown_after_ms: u64,
) -> mpsc::Receiver<Event> {
    let (sender, receiver) = mpsc::channel(256);
    tokio::spawn(async move {
        for (delay_ms, text) in steps {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            if text == "\n" {
                if sender
                    .send(Event::Key(KeyEvent::new(
                        KeyCode::Enter,
                        KeyModifiers::NONE,
                    )))
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
            for character in text.chars() {
                if sender
                    .send(Event::Key(KeyEvent::new(
                        KeyCode::Char(character),
                        KeyModifiers::NONE,
                    )))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            if sender
                .send(Event::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                )))
                .await
                .is_err()
            {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(shutdown_after_ms)).await;
        let _ = sender
            .send(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
            .await;
    });
    receiver
}
