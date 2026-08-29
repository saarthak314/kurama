use std::sync::Arc;

use kurama_core::orchestrator::SmartOrchestrator;
use kurama_core::testing::{
    AllowAllPolicy, CollectingSink, EchoTool, MemoryStore, NoDelegation, ScriptedBackend,
    SequenceIds,
};
use kurama_sdk::{
    AgentBudget, AgentBuilder, AgentSpec, AutoBoundaries, DelegationRequest, ExecutionMode,
    FinishReason, ModelEvent, ModelProfile, OrchestrationContext, RuntimeEvent, SessionEvent,
    SessionMetadata, SessionStore, WriteScope,
};
use std::{collections::BTreeMap, path::PathBuf};

#[test]
fn sdk_accepts_custom_runtime_parts() {
    let runtime = AgentBuilder::new()
        .profile(
            ModelProfile::new("custom", "frontier", 32_000, 4_000),
            Arc::new(ScriptedBackend::new(Vec::new())),
        )
        .tool(Arc::new(EchoTool))
        .policy(Arc::new(AllowAllPolicy))
        .store(Arc::new(MemoryStore::default()))
        .sink(Arc::new(CollectingSink::default()))
        .orchestrator(Arc::new(NoDelegation))
        .ids(Arc::new(SequenceIds::default()))
        .build()
        .expect("runtime");

    assert_eq!(runtime.active_profile(), "custom");
    assert_eq!(runtime.registered_tools(), vec!["echo"]);
}

#[test]
fn duplicate_tools_are_rejected() {
    let error = AgentBuilder::new()
        .tool(Arc::new(EchoTool))
        .tool(Arc::new(EchoTool))
        .build()
        .expect_err("duplicate tool");
    assert!(error.to_string().contains("duplicate tool"));
}

#[tokio::test]
async fn runtime_launches_explicit_depth_one_children() {
    let backend = Arc::new(ScriptedBackend::new(vec![
        vec![
            Ok(ModelEvent::Delegation {
                request: DelegationRequest {
                    agents: vec![AgentSpec {
                        role: "reviewer".into(),
                        objective: "inspect the fixture".into(),
                        profile: None,
                        context_refs: Vec::new(),
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
                text: "child complete".into(),
            }),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            }),
        ],
        vec![
            Ok(ModelEvent::TextDelta {
                text: "parent complete".into(),
            }),
            Ok(ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            }),
        ],
    ]));
    let store = Arc::new(MemoryStore::default());
    let ids = Arc::new(SequenceIds::default());
    let profile = ModelProfile::new("custom", "frontier", 32_000, 4_000);
    let orchestration = OrchestrationContext {
        parent_profile: profile.clone(),
        profiles: BTreeMap::from([("custom".into(), profile.clone())]),
        role_routes: BTreeMap::new(),
        role_escalations: BTreeMap::new(),
        profile_escalations: BTreeMap::new(),
        parent_write_scope: WriteScope {
            roots: vec![PathBuf::from(".")],
            files: Vec::new(),
        },
        max_concurrency: 1,
        depth: 0,
        yolo: false,
    };
    let runtime = AgentBuilder::new()
        .profile(profile, backend)
        .policy(Arc::new(AllowAllPolicy))
        .store(store.clone())
        .sink(Arc::new(CollectingSink::default()))
        .orchestrator(Arc::new(SmartOrchestrator::new(ids.clone())))
        .ids(ids)
        .auto_boundaries(AutoBoundaries::default())
        .orchestration_context(orchestration)
        .build()
        .expect("runtime");
    let session = SessionMetadata {
        id: "session".into(),
        created_at_ms: 0,
        project_root: ".".into(),
        profile: "custom".into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    };
    let (handle, mut events) = runtime.start(session, Vec::new()).expect("start");
    handle.submit("use sub-agents", true).await.expect("submit");
    loop {
        match events.recv().await.expect("event") {
            RuntimeEvent::TurnCompleted => break,
            RuntimeEvent::Error { message } => panic!("runtime error: {message}"),
            _ => {}
        }
    }

    let events = store.replay(&"session".into()).expect("replay");
    assert!(
        events
            .iter()
            .any(|event| matches!(event.event, SessionEvent::AgentCompleted { .. }))
    );
}
