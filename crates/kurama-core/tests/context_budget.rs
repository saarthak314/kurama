use kurama_core::context::{ContextManager, ContextPolicy};
use kurama_protocol::{
    id::SessionId,
    model::{ModelItem, ModelProfile},
    policy::ExecutionMode,
    session::{EventEnvelope, SessionEvent, SessionMetadata},
};
use serde_json::json;

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
        .assemble(&profile, Vec::new(), false, "/workspace/project")
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

#[test]
fn delegation_schema_describes_a_round_trippable_request() {
    let mut manager = ContextManager::new(ContextPolicy::default());
    manager.replay(long_session());
    let profile = ModelProfile::new("test", "frontier", 128_000, 8_000);
    let schema = manager
        .assemble(&profile, Vec::new(), true, "/workspace/project")
        .expect("assemble context")
        .request
        .delegation
        .expect("delegation schema")
        .parameters;

    assert_eq!(
        schema,
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["agents"],
            "properties": {
                "agents": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 8,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": [
                            "objective",
                            "context_refs",
                            "write_scope",
                            "budget",
                            "depends_on"
                        ],
                        "properties": {
                            "objective": {"type": "string"},
                            "context_refs": {
                                "type": "array",
                                "items": {"type": "string"}
                            },
                            "write_scope": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["roots", "files"],
                                "properties": {
                                    "roots": {
                                        "type": "array",
                                        "items": {"type": "string"}
                                    },
                                    "files": {
                                        "type": "array",
                                        "items": {"type": "string"}
                                    }
                                }
                            },
                            "budget": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": [
                                    "max_input_tokens",
                                    "max_output_tokens",
                                    "max_turns",
                                    "max_seconds"
                                ],
                                "properties": {
                                    "max_input_tokens": {
                                        "type": "integer",
                                        "minimum": 1,
                                        "maximum": 80000
                                    },
                                    "max_output_tokens": {
                                        "type": "integer",
                                        "minimum": 1,
                                        "maximum": 8000
                                    },
                                    "max_turns": {
                                        "type": "integer",
                                        "minimum": 1,
                                        "maximum": 12
                                    },
                                    "max_seconds": {
                                        "type": "integer",
                                        "minimum": 1,
                                        "maximum": 1800
                                    }
                                }
                            },
                            "depends_on": {
                                "type": "array",
                                "description": "Exact objective strings of prerequisite agents.",
                                "items": {"type": "string"}
                            }
                        }
                    }
                }
            }
        })
    );

    let representative = json!({
        "agents": [
            {
                "objective": "Implement the delegation contract",
                "context_refs": ["crates/kurama-core/src/context.rs"],
                "write_scope": {
                    "roots": ["crates/kurama-core/src"],
                    "files": ["crates/kurama-core/tests/context_budget.rs"]
                },
                "budget": {
                    "max_input_tokens": 12000,
                    "max_output_tokens": 2000,
                    "max_turns": 4,
                    "max_seconds": 300
                },
                "depends_on": []
            },
            {
                "objective": "Review the implementation",
                "context_refs": [],
                "write_scope": {"roots": [], "files": []},
                "budget": {
                    "max_input_tokens": 8000,
                    "max_output_tokens": 1000,
                    "max_turns": 2,
                    "max_seconds": 120
                },
                "depends_on": ["implementer"]
            }
        ]
    });
    assert!(schema_is_valid(&schema, &representative));
}

fn schema_is_valid(schema: &serde_json::Value, value: &serde_json::Value) -> bool {
    let item = &schema["properties"]["agents"]["items"];
    let required = item["required"].as_array().expect("required");
    value["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .all(|agent| {
            required
                .iter()
                .all(|key| agent.get(key.as_str().expect("required key")).is_some())
                && agent
                    .as_object()
                    .expect("agent")
                    .keys()
                    .all(|key| item["properties"].get(key).is_some())
        })
}
