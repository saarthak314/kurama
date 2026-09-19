use kurama_protocol::{
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
fn old_read_operations_migrate_to_path_lists() {
    use kurama_protocol::tool::Operation;
    let operation: Operation =
        serde_json::from_str(r#"{"type":"read","path":"src/lib.rs","external":false}"#)
            .expect("legacy operation");
    assert_eq!(
        operation,
        Operation::Read {
            paths: vec!["src/lib.rs".into()],
            external: false,
        }
    );
    let encoded = serde_json::to_value(&operation).expect("encode current operation");
    assert_eq!(encoded["paths"], serde_json::json!(["src/lib.rs"]));
    assert!(encoded.get("path").is_none());
    let multi = Operation::Read {
        paths: vec!["src/lib.rs".into(), "src/main.rs".into()],
        external: true,
    };
    assert_eq!(
        serde_json::from_value::<Operation>(serde_json::to_value(&multi).unwrap()).unwrap(),
        multi
    );
}

#[test]
fn user_message_authorization_survives_serialization_and_old_logs_default_off() {
    let legacy: SessionEvent =
        serde_json::from_str(r#"{"type":"user_message","text":"inspect the parser"}"#)
            .expect("legacy user message");
    assert_eq!(
        legacy,
        SessionEvent::UserMessage {
            text: "inspect the parser".into(),
            explicit_delegation: false,
        }
    );
    let authorized = SessionEvent::UserMessage {
        text: "inspect the parser".into(),
        explicit_delegation: true,
    };
    assert_eq!(
        serde_json::from_value::<SessionEvent>(serde_json::to_value(&authorized).unwrap()).unwrap(),
        authorized
    );
}

#[test]
fn todo_ids_cannot_differ_only_by_surrounding_whitespace() {
    use kurama_protocol::session::{TodoItem, TodoStatus};
    let canonical = TodoItem {
        id: "task".into(),
        content: "inspect parser".into(),
        status: TodoStatus::Pending,
    };
    assert!(TodoItem::validate_list(std::slice::from_ref(&canonical)).is_ok());
    let ambiguous = TodoItem {
        id: " task ".into(),
        ..canonical.clone()
    };
    assert!(TodoItem::validate_list(std::slice::from_ref(&ambiguous)).is_err());
    assert!(TodoItem::validate_list(&[canonical, ambiguous]).is_err());
}
