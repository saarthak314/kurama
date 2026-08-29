use kurama_protocol::{
    agent::{AgentBudget, AgentSpec, WriteScope},
    id::{AgentId, SessionId},
    policy::ExecutionMode,
    session::{EventEnvelope, SCHEMA_VERSION, SessionEvent},
};

#[test]
fn session_event_roundtrips_with_version_and_lineage() {
    let event = EventEnvelope::new(
        7,
        42,
        SessionId::from("session-a"),
        Some(AgentId::from("child-a")),
        SessionEvent::ModeSelected {
            mode: ExecutionMode::Auto,
        },
    );
    let encoded = serde_json::to_string(&event).expect("encode");
    let decoded: EventEnvelope = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded.schema_version, SCHEMA_VERSION);
    assert_eq!(decoded.sequence, 7);
    assert_eq!(decoded.agent_id.as_deref(), Some("child-a"));
}

#[test]
fn unspecified_child_write_scope_is_read_only() {
    let spec = AgentSpec {
        role: "reviewer".into(),
        objective: "inspect the diff".into(),
        profile: None,
        context_refs: Vec::new(),
        write_scope: WriteScope::default(),
        budget: AgentBudget::default(),
        depends_on: Vec::new(),
    };
    assert!(spec.write_scope.is_read_only());
}
