use kurama_core::context::{CompactionRequest, ContextManager, ContextPolicy, estimate_text};
use kurama_protocol::{
    id::{CallId, OperationId, SessionId},
    model::{ModelItem, ModelProfile},
    policy::ExecutionMode,
    session::{
        EventEnvelope, GoalStatus, SessionEvent, SessionGoal, SessionMetadata, TodoItem, TodoStatus,
    },
    tool::ToolResult,
};
use serde_json::json;

fn event(sequence: u64, event: SessionEvent) -> EventEnvelope {
    EventEnvelope::new(sequence, sequence, SessionId::from("session"), None, event)
}

fn compaction_events(request: &CompactionRequest) -> Vec<EventEnvelope> {
    request
        .items
        .iter()
        .find_map(|item| match item {
            ModelItem::User { text } => Some(serde_json::from_str(text).expect("durable events")),
            _ => None,
        })
        .expect("new compaction events")
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

    assert!(matches!(error, kurama_protocol::KuramaError::Session(_)));
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
fn repeated_compaction_carries_summary_outside_the_new_prefix() {
    let mut manager = ContextManager::new(ContextPolicy::default());
    for (turn, name) in ["A", "B", "C", "D", "E"].into_iter().enumerate() {
        let sequence = turn as u64 * 3;
        manager.record(event(
            sequence,
            SessionEvent::UserMessage { text: name.into() },
        ));
        manager.record(event(
            sequence + 1,
            SessionEvent::AssistantMessage {
                text: format!("decision {name}"),
            },
        ));
        manager.record(event(sequence + 2, SessionEvent::TurnCompleted));
    }
    let first = manager.compaction_request().expect("compact A");
    assert_eq!(first.covered_through_sequence, 2);
    let summary = format!("decision A: {}", "durable detail ".repeat(80));
    manager.record(event(
        15,
        SessionEvent::ContextCompacted {
            covered_through_sequence: first.covered_through_sequence,
            summary: summary.clone(),
            // Estimates must measure the actual input, not trust stored counts.
            tokens: 1,
        },
    ));
    assert!(manager.compaction_request().is_none());
    manager.record(event(16, SessionEvent::UserMessage { text: "F".into() }));
    manager.record(event(
        17,
        SessionEvent::AssistantMessage {
            text: "decision F".into(),
        },
    ));
    manager.record(event(18, SessionEvent::TurnCompleted));

    let second = manager
        .compaction_request()
        .expect("compact B with A summary");
    assert_eq!(second.covered_through_sequence, 5);
    assert_eq!(
        compaction_events(&second)
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![3, 4, 5]
    );
    assert!(matches!(
        &second.items[0],
        ModelItem::Summary { text, covered_through_sequence: 2, .. } if text == &summary
    ));
    let input = serde_json::to_string(&second.items).expect("serialize model data");
    assert_eq!(input.matches("decision A").count(), 1);
    assert!(!second.prompt.contains("decision A"));
    assert!(second.estimated_tokens >= estimate_text(&summary));

    let assembled = manager
        .assemble(
            &ModelProfile::new("test", "frontier", 128_000, 8_000),
            Vec::new(),
            false,
            ".",
        )
        .expect("assemble retained turns");
    let users: Vec<_> = assembled
        .request
        .items
        .iter()
        .filter_map(|item| match item {
            ModelItem::User { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(users, vec!["C", "D", "E", "F"]);
}

#[test]
fn compaction_excludes_superseded_summary_events_from_new_data() {
    let mut manager = ContextManager::new(ContextPolicy {
        recent_turns: 0,
        ..ContextPolicy::default()
    });
    manager.replay(vec![
        event(0, SessionEvent::UserMessage { text: "A".into() }),
        event(1, SessionEvent::TurnCompleted),
        event(
            2,
            SessionEvent::ContextCompacted {
                covered_through_sequence: 1,
                summary: "stale decision".into(),
                tokens: 5,
            },
        ),
        event(
            3,
            SessionEvent::UserMessage {
                text: "B replaces A".into(),
            },
        ),
        event(4, SessionEvent::TurnCompleted),
        event(
            5,
            SessionEvent::ContextCompacted {
                covered_through_sequence: 4,
                summary: "current decision".into(),
                tokens: 5,
            },
        ),
        event(6, SessionEvent::UserMessage { text: "C".into() }),
        event(7, SessionEvent::TurnCompleted),
    ]);
    let request = manager.compaction_request().expect("compact C");
    assert_eq!(request.covered_through_sequence, 7);
    assert_eq!(
        compaction_events(&request)
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![6, 7]
    );
    let input = serde_json::to_string(&request.items).expect("serialize model data");
    assert_eq!(input.matches("current decision").count(), 1);
    assert!(!input.contains("stale decision"));
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
    assert_eq!(
        manager
            .compaction_request()
            .expect("compact older continuation turns")
            .covered_through_sequence,
        8
    );
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

#[test]
fn oversized_recent_turn_is_omitted_whole_without_blocking_later_turns() {
    let mut manager = ContextManager::new(ContextPolicy {
        max_input_tokens: 2_000,
        reserve_output_tokens: 0,
        recent_turns: 2,
        ..ContextPolicy::default()
    });
    manager.replay(vec![
        event(
            0,
            SessionEvent::UserMessage {
                text: "orphaned question".into(),
            },
        ),
        event(
            1,
            SessionEvent::AssistantMessage {
                text: "orphaned answer".into(),
            },
        ),
        event(
            2,
            SessionEvent::ToolCompleted {
                operation_id: OperationId::from("large-operation"),
                result: ToolResult::success(CallId::from("large-call"), "x".repeat(9_000)),
            },
        ),
        event(3, SessionEvent::TurnCompleted),
        event(
            4,
            SessionEvent::UserMessage {
                text: "small question".into(),
            },
        ),
        event(
            5,
            SessionEvent::AssistantMessage {
                text: "small answer".into(),
            },
        ),
        event(6, SessionEvent::TurnCompleted),
        event(
            7,
            SessionEvent::UserMessage {
                text: "current question".into(),
            },
        ),
    ]);

    let assembled = manager
        .assemble(
            &ModelProfile::new("test", "frontier", 2_000, 0),
            Vec::new(),
            false,
            ".",
        )
        .expect("assemble with an oversized historical turn");
    assert_eq!(
        assembled.request.items,
        vec![
            ModelItem::User {
                text: "small question".into()
            },
            ModelItem::Assistant {
                text: "small answer".into()
            },
            ModelItem::User {
                text: "current question".into()
            },
        ]
    );
}

#[test]
fn malformed_boundaries_preserve_latest_incomplete_and_failed_turns() {
    let mut manager = ContextManager::new(ContextPolicy {
        recent_turns: 1,
        ..ContextPolicy::default()
    });
    let profile = ModelProfile::new("test", "frontier", 128_000, 8_000);
    let mut events = vec![
        event(0, SessionEvent::TurnCompleted),
        event(
            1,
            SessionEvent::UserMessage {
                text: "interrupted".into(),
            },
        ),
        event(
            2,
            SessionEvent::AssistantMessage {
                text: "partial answer".into(),
            },
        ),
        event(
            3,
            SessionEvent::UserMessage {
                text: "failed question".into(),
            },
        ),
        event(
            4,
            SessionEvent::AssistantMessage {
                text: "failed answer".into(),
            },
        ),
        event(
            5,
            SessionEvent::TurnFailed {
                error: "provider disconnected".into(),
            },
        ),
        event(6, SessionEvent::TurnCompleted),
    ];
    for entry in &events {
        manager.record(entry.clone());
    }
    assert_eq!(
        manager
            .assemble(&profile, Vec::new(), false, ".")
            .expect("assemble failed turn")
            .request
            .items,
        vec![
            ModelItem::User {
                text: "failed question".into()
            },
            ModelItem::Assistant {
                text: "failed answer".into()
            },
            ModelItem::User {
                text: "interrupted".into()
            },
            ModelItem::Assistant {
                text: "partial answer".into()
            },
        ]
    );

    let continuation = event(
        7,
        SessionEvent::AssistantMessage {
            text: "continuation".into(),
        },
    );
    manager.record(continuation.clone());
    events.push(continuation);
    assert_eq!(
        manager
            .assemble(&profile, Vec::new(), false, ".")
            .expect("assemble open continuation")
            .request
            .items,
        vec![
            ModelItem::User {
                text: "failed question".into()
            },
            ModelItem::Assistant {
                text: "failed answer".into()
            },
            ModelItem::Assistant {
                text: "continuation".into()
            },
        ]
    );

    let completed = event(8, SessionEvent::TurnCompleted);
    manager.record(completed.clone());
    events.push(completed);
    let expected = vec![
        ModelItem::Assistant {
            text: "continuation".into(),
        },
        ModelItem::User {
            text: "interrupted".into(),
        },
        ModelItem::Assistant {
            text: "partial answer".into(),
        },
    ];
    assert_eq!(
        manager
            .assemble(&profile, Vec::new(), false, ".")
            .expect("assemble completed continuation")
            .request
            .items,
        expected
    );
    manager.replay(events);
    assert_eq!(
        manager
            .assemble(&profile, Vec::new(), false, ".")
            .expect("assemble replayed boundaries")
            .request
            .items,
        expected
    );
}

#[test]
fn clearing_and_replay_remove_stale_goal_todo_evidence_and_turns() {
    let mut manager = ContextManager::new(ContextPolicy::default());
    let profile = ModelProfile::new("test", "frontier", 128_000, 8_000);
    let todo = TodoItem {
        id: "todo".into(),
        content: "latest task".into(),
        status: TodoStatus::InProgress,
    };
    let goal = SessionGoal::new("latest goal").expect("valid goal");
    let mut result = ToolResult::success(CallId::from("evidence-call"), "tool output");
    result.metadata = json!({"evidence": [{"path": "src/lib.rs", "content": "fact"}]});
    manager.replay(vec![
        event(
            0,
            SessionEvent::GoalUpdated {
                goal: SessionGoal::new("old goal").expect("valid goal"),
            },
        ),
        event(
            1,
            SessionEvent::TodoUpdated {
                items: vec![TodoItem {
                    content: "old task".into(),
                    ..todo.clone()
                }],
            },
        ),
        event(
            2,
            SessionEvent::ToolCompleted {
                operation_id: OperationId::from("evidence-operation"),
                result,
            },
        ),
        event(3, SessionEvent::TurnCompleted),
        event(
            4,
            SessionEvent::UserMessage {
                text: "interrupted".into(),
            },
        ),
        event(
            5,
            SessionEvent::UserMessage {
                text: "open".into(),
            },
        ),
        event(
            6,
            SessionEvent::ContextCompacted {
                covered_through_sequence: 3,
                summary: "old summary".into(),
                tokens: 4,
            },
        ),
        event(7, SessionEvent::GoalUpdated { goal: goal.clone() }),
        event(
            8,
            SessionEvent::TodoUpdated {
                items: vec![todo.clone()],
            },
        ),
    ]);
    let items = manager
        .assemble(&profile, Vec::new(), false, ".")
        .expect("assemble latest state")
        .request
        .items;
    assert_eq!(
        items
            .iter()
            .filter_map(|item| match item {
                ModelItem::Goal { goal, .. } => Some(goal),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![&goal]
    );
    assert_eq!(
        items
            .iter()
            .filter_map(|item| match item {
                ModelItem::TodoList { items } => Some(items),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![&vec![todo]]
    );
    assert!(items.iter().any(|item| matches!(item, ModelItem::Evidence { path, content, .. } if path == "src/lib.rs" && content == "fact")));

    manager.record(event(9, SessionEvent::GoalCleared));
    manager.record(event(10, SessionEvent::TodoUpdated { items: Vec::new() }));
    let cleared = manager
        .assemble(&profile, Vec::new(), false, ".")
        .expect("assemble cleared state");
    assert!(
        !cleared
            .request
            .items
            .iter()
            .any(|item| matches!(item, ModelItem::Goal { .. } | ModelItem::TodoList { .. }))
    );

    manager.replay(vec![event(
        0,
        SessionEvent::UserMessage {
            text: "replacement".into(),
        },
    )]);
    assert_eq!(
        manager
            .assemble(&profile, Vec::new(), false, ".")
            .expect("assemble replacement history")
            .request
            .items,
        vec![ModelItem::User {
            text: "replacement".into()
        }]
    );
    assert!(manager.compaction_request().is_none());
    manager.replay(Vec::new());
    assert!(matches!(
        manager.assemble(&profile, Vec::new(), false, "."),
        Err(kurama_protocol::KuramaError::Session(_))
    ));
}

#[test]
fn evidence_skips_malformed_entries_and_stops_at_the_budget_boundary() {
    let mut result = ToolResult::success(CallId::from("call"), "output");
    result.metadata = json!({"evidence": [
        {"path": "missing-content"},
        {"path": 7, "content": "invalid path"},
        {"path": "first", "content": "small fact"},
        {"path": "oversized", "content": "x".repeat(9_000)},
        {"path": "after-boundary", "content": "must not be included"}
    ]});
    let mut manager = ContextManager::new(ContextPolicy {
        max_input_tokens: 2_000,
        reserve_output_tokens: 0,
        recent_turns: 0,
        ..ContextPolicy::default()
    });
    manager.replay(vec![
        event(
            0,
            SessionEvent::ToolCompleted {
                operation_id: OperationId::from("operation"),
                result,
            },
        ),
        event(1, SessionEvent::TurnCompleted),
    ]);
    let items = manager
        .assemble(
            &ModelProfile::new("test", "frontier", 2_000, 0),
            Vec::new(),
            false,
            ".",
        )
        .expect("assemble bounded evidence")
        .request
        .items;
    assert_eq!(
        items,
        vec![ModelItem::Evidence {
            path: "first".into(),
            content: "small fact".into(),
            blob: None
        }]
    );
}

#[test]
fn zero_recent_turns_compacts_completed_history_but_not_the_current_turn() {
    let mut manager = ContextManager::new(ContextPolicy {
        recent_turns: 0,
        ..ContextPolicy::default()
    });
    manager.replay(vec![
        event(
            0,
            SessionEvent::UserMessage {
                text: "completed question".into(),
            },
        ),
        event(
            1,
            SessionEvent::AssistantMessage {
                text: "completed answer".into(),
            },
        ),
        event(2, SessionEvent::TurnCompleted),
    ]);
    let completed = manager
        .compaction_request()
        .expect("compact all completed turns");
    assert_eq!(completed.covered_through_sequence, 2);
    assert_eq!(
        compaction_events(&completed)
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );

    manager.record(event(
        3,
        SessionEvent::UserMessage {
            text: "current question".into(),
        },
    ));
    assert_eq!(
        manager
            .compaction_request()
            .expect("retain current turn")
            .covered_through_sequence,
        2
    );
    manager.apply_compaction(2, "completed facts".into(), 4);
    assert!(manager.compaction_request().is_none());

    manager.record(event(
        4,
        SessionEvent::TurnFailed {
            error: "failed".into(),
        },
    ));
    let next = manager
        .compaction_request()
        .expect("compact newly completed turn");
    assert_eq!(next.covered_through_sequence, 4);
    assert_eq!(
        compaction_events(&next)
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
    manager.record(event(
        5,
        SessionEvent::ContextCompacted {
            covered_through_sequence: 4,
            summary: "all completed facts".into(),
            tokens: 4,
        },
    ));
    assert!(manager.compaction_request().is_none());
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
