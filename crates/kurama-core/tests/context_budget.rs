use kurama_core::context::{ContextManager, ContextPolicy};
use kurama_protocol::{
    id::{CallId, OperationId, SessionId},
    model::{ModelItem, ModelProfile},
    policy::ExecutionMode,
    session::{EventEnvelope, GoalStatus, SessionEvent, SessionGoal, SessionMetadata},
    tool::ToolResult,
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
fn current_turn_keeps_complete_tool_output_when_it_fits() {
    let output = (0..300)
        .map(|line| format!("tracked-file-{line}.rs"))
        .collect::<Vec<_>>()
        .join("\n");
    let metadata = SessionMetadata {
        id: SessionId::from("session"),
        created_at_ms: 0,
        project_root: ".".into(),
        profile: "test".into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    };
    let mut result = ToolResult::success(CallId::from("call"), output.clone());
    result.metadata = json!({"tool_name": "bash"});
    let events = vec![
        event(0, SessionEvent::SessionStarted { metadata }),
        event(
            1,
            SessionEvent::UserMessage {
                text: "Summarize the repository.".into(),
            },
        ),
        event(
            2,
            SessionEvent::ToolCompleted {
                operation_id: OperationId::from("operation"),
                result,
            },
        ),
    ];
    let mut manager = ContextManager::new(ContextPolicy {
        max_input_tokens: 16_000,
        reserve_output_tokens: 1_000,
        compact_at_percent: 75,
        recent_turns: 3,
    });
    manager.replay(events);

    let assembled = manager
        .assemble(
            &ModelProfile::new("test", "frontier", 16_000, 1_000),
            Vec::new(),
            false,
            "/workspace/project",
        )
        .expect("assemble context");

    assert!(assembled.request.items.iter().any(|item| {
        matches!(
            item,
            ModelItem::ToolResult { content, .. } if content == &output
        )
    }));
}

#[test]
fn current_turn_reports_context_overflow_instead_of_dropping_tool_output() {
    let metadata = SessionMetadata {
        id: SessionId::from("session"),
        created_at_ms: 0,
        project_root: ".".into(),
        profile: "test".into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    };
    let mut result = ToolResult::success(CallId::from("call"), "x".repeat(12_000));
    result.metadata = json!({"tool_name": "bash"});
    let mut manager = ContextManager::new(ContextPolicy {
        max_input_tokens: 1_000,
        reserve_output_tokens: 100,
        compact_at_percent: 75,
        recent_turns: 3,
    });
    manager.replay(vec![
        event(0, SessionEvent::SessionStarted { metadata }),
        event(
            1,
            SessionEvent::UserMessage {
                text: "Inspect everything.".into(),
            },
        ),
        event(
            2,
            SessionEvent::ToolCompleted {
                operation_id: OperationId::from("operation"),
                result,
            },
        ),
    ]);

    let error = manager
        .assemble(
            &ModelProfile::new("test", "frontier", 1_000, 100),
            Vec::new(),
            false,
            "/workspace/project",
        )
        .expect_err("oversized current turn must fail explicitly");

    assert!(error.to_string().contains("current turn exceeds"));
}

#[test]
fn assembled_items_keep_completed_history_before_the_current_turn() {
    let metadata = SessionMetadata {
        id: SessionId::from("session"),
        created_at_ms: 0,
        project_root: ".".into(),
        profile: "test".into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    };
    let mut result = ToolResult::success(CallId::from("call"), "current output");
    result.metadata = json!({"tool_name": "bash"});
    let mut manager = ContextManager::new(ContextPolicy::default());
    manager.replay(vec![
        event(0, SessionEvent::SessionStarted { metadata }),
        event(
            1,
            SessionEvent::UserMessage {
                text: "older question".into(),
            },
        ),
        event(
            2,
            SessionEvent::AssistantMessage {
                text: "older answer".into(),
            },
        ),
        event(3, SessionEvent::TurnCompleted),
        event(
            4,
            SessionEvent::UserMessage {
                text: "current question".into(),
            },
        ),
        event(
            5,
            SessionEvent::ToolCompleted {
                operation_id: OperationId::from("operation"),
                result,
            },
        ),
    ]);

    let items = manager
        .assemble(
            &ModelProfile::new("test", "frontier", 128_000, 8_000),
            Vec::new(),
            false,
            "/workspace/project",
        )
        .expect("assemble context")
        .request
        .items;

    assert!(matches!(&items[0], ModelItem::User { text } if text == "older question"));
    assert!(matches!(&items[1], ModelItem::Assistant { text } if text == "older answer"));
    assert!(matches!(&items[2], ModelItem::User { text } if text == "current question"));
    assert!(
        matches!(&items[3], ModelItem::ToolResult { content, .. } if content == "current output")
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

#[test]
fn delegation_schema_counts_toward_the_context_budget() {
    let mut manager = ContextManager::new(ContextPolicy::default());
    manager.replay(long_session());
    let profile = ModelProfile::new("test", "frontier", 128_000, 8_000);

    let without_delegation = manager
        .assemble(&profile, Vec::new(), false, "/workspace/project")
        .expect("assemble without delegation");
    let with_delegation = manager
        .assemble(&profile, Vec::new(), true, "/workspace/project")
        .expect("assemble with delegation");

    assert!(with_delegation.estimated_tokens > without_delegation.estimated_tokens);
}

#[test]
fn context_reserves_space_for_provider_request_envelopes() {
    let metadata = SessionMetadata {
        id: SessionId::from("session"),
        created_at_ms: 0,
        project_root: ".".into(),
        profile: "test".into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    };
    let mut manager = ContextManager::new(ContextPolicy::default());
    manager.replay(vec![event(0, SessionEvent::SessionStarted { metadata })]);

    let assembled = manager
        .assemble(
            &ModelProfile::new("test", "frontier", 128_000, 8_000),
            Vec::new(),
            false,
            "/workspace/project",
        )
        .expect("assemble context");

    assert!(assembled.estimated_tokens >= 512);
}

#[test]
fn goal_continuation_turns_stay_in_recent_history() {
    let metadata = SessionMetadata {
        id: SessionId::from("session"),
        created_at_ms: 0,
        project_root: ".".into(),
        profile: "test".into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    };
    let objective = "Keep the original goalpost intact across many turns.";
    let mut events = vec![
        event(0, SessionEvent::SessionStarted { metadata }),
        event(
            1,
            SessionEvent::GoalUpdated {
                goal: SessionGoal {
                    objective: objective.into(),
                    status: GoalStatus::Pursuing,
                    turns: 1,
                    blocked_streak: 0,
                },
            },
        ),
        event(
            2,
            SessionEvent::UserMessage {
                text: objective.into(),
            },
        ),
        event(
            3,
            SessionEvent::AssistantMessage {
                text: "checkpoint-0".into(),
            },
        ),
        event(4, SessionEvent::TurnCompleted),
    ];
    for turn in 1..6 {
        events.push(event(
            events.len() as u64,
            SessionEvent::AssistantMessage {
                text: format!("checkpoint-{turn}"),
            },
        ));
        events.push(event(events.len() as u64, SessionEvent::TurnCompleted));
    }
    let mut manager = ContextManager::new(ContextPolicy {
        max_input_tokens: 4_000,
        reserve_output_tokens: 200,
        compact_at_percent: 75,
        recent_turns: 3,
    });
    manager.replay(events);
    let items = manager
        .assemble(
            &ModelProfile::new("test", "frontier", 4_000, 200),
            Vec::new(),
            false,
            ".",
        )
        .expect("assemble")
        .request
        .items;
    assert!(items.iter().any(
        |item| matches!(item, ModelItem::Goal { goal, continuation: true } if goal.objective == objective)
    ));
    assert!(
        items
            .iter()
            .any(|item| matches!(item, ModelItem::Assistant { text } if text == "checkpoint-5"))
    );
    assert!(
        items
            .iter()
            .any(|item| matches!(item, ModelItem::Assistant { text } if text == "checkpoint-3"))
    );
    assert!(
        !items
            .iter()
            .any(|item| matches!(item, ModelItem::Assistant { text } if text == "checkpoint-0"))
    );
}

#[test]
fn tight_budget_still_keeps_the_active_goal() {
    let metadata = SessionMetadata {
        id: SessionId::from("session"),
        created_at_ms: 0,
        project_root: ".".into(),
        profile: "test".into(),
        mode: ExecutionMode::Supervised,
        redaction_best_effort: false,
    };
    let objective = "Do not drop this goal when history is large.";
    let padding = "progress ".repeat(80);
    let mut events = vec![event(0, SessionEvent::SessionStarted { metadata })];
    for turn in 0..6 {
        events.push(event(
            events.len() as u64,
            SessionEvent::UserMessage {
                text: format!("turn {turn} {padding}"),
            },
        ));
        events.push(event(
            events.len() as u64,
            SessionEvent::AssistantMessage {
                text: format!("answer {turn} {padding}"),
            },
        ));
        events.push(event(events.len() as u64, SessionEvent::TurnCompleted));
    }
    events.push(event(
        events.len() as u64,
        SessionEvent::GoalUpdated {
            goal: SessionGoal {
                objective: objective.into(),
                status: GoalStatus::Pursuing,
                turns: 6,
                blocked_streak: 0,
            },
        },
    ));
    let mut manager = ContextManager::new(ContextPolicy {
        max_input_tokens: 900,
        reserve_output_tokens: 100,
        compact_at_percent: 75,
        recent_turns: 4,
    });
    manager.replay(events);
    let items = manager
        .assemble(
            &ModelProfile::new("test", "frontier", 900, 100),
            Vec::new(),
            false,
            ".",
        )
        .expect("assemble")
        .request
        .items;
    assert!(
        items.iter().any(
            |item| matches!(item, ModelItem::Goal { goal, .. } if goal.objective == objective)
        )
    );
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
