use kurama_core::context::{ContextManager, ContextPolicy};
use kurama_protocol::{
    id::SessionId,
    model::{ModelItem, ModelProfile},
    policy::ExecutionMode,
    session::{EventEnvelope, SessionEvent, SessionMetadata},
};

fn event(sequence: u64, event: SessionEvent) -> EventEnvelope {
    EventEnvelope::new(sequence, sequence, SessionId::from("session"), None, event)
}

fn long_session() -> Vec<EventEnvelope> {
    let metadata = SessionMetadata {
        id: SessionId::from("session"),
        created_at_ms: 0,
        project_root: ".".into(),
        profile: "test".into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    };
    let mut events = vec![event(0, SessionEvent::SessionStarted { metadata })];
    for turn in 0..10 {
        events.push(event(
            events.len() as u64,
            SessionEvent::UserMessage {
                text: format!("question {turn} {}", "x".repeat(150)),
            },
        ));
        events.push(event(
            events.len() as u64,
            SessionEvent::AssistantMessage {
                text: format!("answer {turn} {}", "y".repeat(150)),
            },
        ));
        events.push(event(events.len() as u64, SessionEvent::TurnCompleted));
    }
    events
}

#[test]
fn keeps_recent_turns_and_summary_within_budget() {
    let events = long_session();
    let mut manager = ContextManager::new(ContextPolicy {
        max_input_tokens: 1_000,
        reserve_output_tokens: 200,
        compact_at_percent: 75,
        recent_turns: 3,
        max_tool_result_tokens: 120,
    });
    manager.replay(events);
    manager.apply_compaction(18, "durable facts".into(), 4);
    let profile = ModelProfile::new("test", "frontier", 1_000, 200);
    let assembled = manager
        .assemble(&profile, Vec::new(), false)
        .expect("assemble context");
    assert!(assembled.estimated_tokens <= 800);
    assert!(
        assembled
            .request
            .items
            .iter()
            .any(|item| matches!(item, ModelItem::Summary { .. }))
    );
}

#[test]
fn compaction_preserves_canonical_events() {
    let events = long_session();
    let mut manager = ContextManager::new(ContextPolicy::default());
    manager.replay(events.clone());
    let request = manager.compaction_request().expect("compaction request");
    manager.apply_compaction(request.covered_through_sequence, "facts".into(), 2);
    assert_eq!(manager.canonical_event_count(), events.len());
    assert_eq!(manager.report().summary_tokens, 2);
}
