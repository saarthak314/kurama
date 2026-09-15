use std::ops::Range;

use kurama_protocol::{
    KuramaError,
    agent::AgentBudget,
    id::{AgentId, SessionId},
    model::{DelegationSchema, ModelItem, ModelProfile, ModelRequest},
    session::{EventEnvelope, SessionEvent},
    tool::ToolDescriptor,
};

use crate::prompts::SYSTEM_PROMPT;

const PROVIDER_ENVELOPE_RESERVE_TOKENS: u64 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextPolicy {
    pub max_input_tokens: u64,
    pub reserve_output_tokens: u64,
    pub compact_at_percent: u8,
    pub recent_turns: usize,
}

impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            max_input_tokens: 128_000,
            reserve_output_tokens: 8_000,
            compact_at_percent: 75,
            recent_turns: 4,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextReport {
    pub estimated_tokens: u64,
    pub system_tokens: u64,
    pub tool_tokens: u64,
    pub summary_tokens: u64,
    pub current_turn_tokens: u64,
    pub recent_turn_tokens: u64,
    pub evidence_tokens: u64,
    pub usable_tokens: u64,
    pub canonical_events: usize,
    pub compaction_recommended: bool,
}

#[derive(Debug, Clone)]
pub struct AssembledContext {
    pub request: ModelRequest,
    pub estimated_tokens: u64,
    pub report: ContextReport,
}

#[derive(Debug, Clone)]
pub struct CompactionRequest {
    pub covered_through_sequence: u64,
    pub events: Vec<EventEnvelope>,
    pub prompt: String,
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone)]
struct ActiveSummary {
    text: String,
    covered_through_sequence: u64,
    tokens: u64,
}

#[derive(Debug, Clone)]
pub struct ContextManager {
    policy: ContextPolicy,
    canonical: Vec<EventEnvelope>,
    summary: Option<ActiveSummary>,
    identity: Option<(SessionId, Option<AgentId>)>,
    completed_turns: Vec<Range<usize>>,
    open_turn: Option<Range<usize>>,
    // A user boundary can interrupt a turn without completing it. Keep the latest
    // such range as the current context when no newer turn remains open.
    interrupted_turn: Option<Range<usize>>,
    latest_user: Option<usize>,
    latest_goal: Option<usize>,
    latest_todos: Option<usize>,
    evidence_events: Vec<usize>,
}

impl ContextManager {
    pub fn new(policy: ContextPolicy) -> Self {
        Self {
            policy,
            canonical: Vec::new(),
            summary: None,
            identity: None,
            completed_turns: Vec::new(),
            open_turn: None,
            interrupted_turn: None,
            latest_user: None,
            latest_goal: None,
            latest_todos: None,
            evidence_events: Vec::new(),
        }
    }

    pub fn replay(&mut self, events: Vec<EventEnvelope>) {
        self.canonical.clear();
        self.summary = None;
        self.identity = None;
        self.completed_turns.clear();
        self.open_turn = None;
        self.interrupted_turn = None;
        self.latest_user = None;
        self.latest_goal = None;
        self.latest_todos = None;
        self.evidence_events.clear();
        for event in events {
            self.record(event);
        }
    }

    pub fn record(&mut self, event: EventEnvelope) {
        if self.identity.is_none() {
            self.identity = Some((event.session_id.clone(), event.agent_id.clone()));
        }
        if let SessionEvent::ContextCompacted {
            covered_through_sequence,
            summary,
            tokens,
        } = &event.event
        {
            self.summary = Some(ActiveSummary {
                text: summary.clone(),
                covered_through_sequence: *covered_through_sequence,
                tokens: *tokens,
            });
        }
        let index = self.canonical.len();
        match &event.event {
            SessionEvent::UserMessage { .. } => self.latest_user = Some(index),
            SessionEvent::GoalUpdated { .. } => self.latest_goal = Some(index),
            SessionEvent::GoalCleared => self.latest_goal = None,
            SessionEvent::TodoUpdated { .. } => self.latest_todos = Some(index),
            SessionEvent::ToolCompleted { result, .. }
                if result
                    .metadata
                    .get("evidence")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|entries| !entries.is_empty()) =>
            {
                self.evidence_events.push(index);
            }
            _ => {}
        }
        if matches!(event.event, SessionEvent::UserMessage { .. }) {
            if let Some(turn) = self.open_turn.take() {
                self.interrupted_turn = Some(turn);
            }
            self.open_turn = Some(index..index);
        } else if self.open_turn.is_none() && opens_context_turn(&event.event) {
            self.open_turn = Some(index..index);
        }
        if let Some(turn) = self.open_turn.as_mut() {
            turn.end = index + 1;
            if matches!(
                event.event,
                SessionEvent::TurnCompleted | SessionEvent::TurnFailed { .. }
            ) {
                self.completed_turns
                    .push(self.open_turn.take().expect("open turn"));
            }
        }
        self.canonical.push(event);
    }

    pub(crate) fn latest_user_text(&self) -> Option<&str> {
        match &self.canonical[self.latest_user?].event {
            SessionEvent::UserMessage { text } => Some(text),
            _ => None,
        }
    }

    fn current_turn(&self) -> Option<&Range<usize>> {
        self.open_turn.as_ref().or(self.interrupted_turn.as_ref())
    }

    pub fn canonical_event_count(&self) -> usize {
        self.canonical.len()
    }

    pub fn report(&self) -> ContextReport {
        ContextReport {
            summary_tokens: self.summary.as_ref().map_or(0, |summary| summary.tokens),
            canonical_events: self.canonical.len(),
            compaction_recommended: self.compaction_recommended(),
            ..ContextReport::default()
        }
    }

    pub fn project_summary(&self) -> String {
        self.summary
            .as_ref()
            .map_or_else(String::new, |summary| summary.text.clone())
    }

    pub fn apply_compaction(
        &mut self,
        covered_through_sequence: u64,
        summary: String,
        tokens: u64,
    ) {
        self.summary = Some(ActiveSummary {
            text: summary,
            covered_through_sequence,
            tokens,
        });
    }

    pub fn compaction_request(&self) -> Option<CompactionRequest> {
        let retained_start = self.retained_turn_start();
        if retained_start == 0 {
            return None;
        }
        let events: Vec<_> = self.canonical[..retained_start]
            .iter()
            .filter(|event| {
                self.summary
                    .as_ref()
                    .is_none_or(|summary| event.sequence > summary.covered_through_sequence)
            })
            .cloned()
            .collect();
        let covered_through_sequence = events.last()?.sequence;
        let serialized = serde_json::to_vec(&events).ok()?;
        Some(CompactionRequest {
            covered_through_sequence,
            estimated_tokens: estimate_bytes(serialized.len()),
            events,
            prompt: crate::prompts::COMPACTION_PROMPT.into(),
        })
    }

    pub fn assemble(
        &self,
        profile: &ModelProfile,
        tools: Vec<ToolDescriptor>,
        delegation_enabled: bool,
        workspace_root: &str,
    ) -> Result<AssembledContext, KuramaError> {
        let (session_id, agent_id) = self.identity.clone().ok_or_else(|| {
            KuramaError::Session("cannot assemble context without a session event".into())
        })?;
        let usable_tokens = self
            .policy
            .max_input_tokens
            .min(profile.max_input_tokens)
            .saturating_sub(
                self.policy
                    .reserve_output_tokens
                    .min(profile.max_output_tokens),
            );
        let system_tokens = estimate_text(SYSTEM_PROMPT);
        let tool_tokens = estimate_serialized(serde_json::to_vec(&tools))?;
        let request_shell = ModelRequest {
            session_id,
            agent_id,
            workspace_root: workspace_root.into(),
            profile: profile.clone(),
            system: SYSTEM_PROMPT.into(),
            items: Vec::new(),
            tools,
            delegation: delegation_enabled.then(|| DelegationSchema {
                parameters: delegation_schema(),
            }),
            continuation: None,
        };
        let fixed_tokens = estimate_serialized(serde_json::to_vec(&request_shell))?
            .saturating_add(PROVIDER_ENVELOPE_RESERVE_TOKENS);
        if fixed_tokens > usable_tokens {
            return Err(KuramaError::Session(
                "model request instructions and tool schemas exceed the input budget".into(),
            ));
        }

        let mut items = Vec::new();
        let mut used = fixed_tokens;
        let mut report = ContextReport {
            system_tokens,
            tool_tokens,
            usable_tokens,
            canonical_events: self.canonical.len(),
            ..ContextReport::default()
        };

        let current_items = self.current_turn().map_or_else(Vec::new, |turn| {
            self.items_for_events(&self.canonical[turn.clone()])
        });
        let current_tokens = estimate_items(&current_items)?;
        let goal_item = self.latest_goal.and_then(|index| {
            if let SessionEvent::GoalUpdated { goal } = &self.canonical[index].event {
                Some(ModelItem::Goal {
                    continuation: goal.status.is_active(),
                    goal: goal.clone(),
                })
            } else {
                None
            }
        });
        let goal_tokens = goal_item
            .as_ref()
            .map_or(Ok(0), |item| estimate_serialized(serde_json::to_vec(item)))?;
        let reserved_tail = current_tokens.saturating_add(goal_tokens);
        if fixed_tokens.saturating_add(reserved_tail) > usable_tokens {
            return Err(KuramaError::Session(
                "current turn exceeds the model input context budget; tool output was preserved"
                    .into(),
            ));
        }

        if let Some(summary) = &self.summary {
            let item = ModelItem::Summary {
                text: summary.text.clone(),
                covered_through_sequence: summary.covered_through_sequence,
                tokens: summary.tokens,
            };
            push_if_fits(
                &mut items,
                item,
                &mut used,
                usable_tokens.saturating_sub(reserved_tail),
                &mut report.summary_tokens,
            )?;
        }

        let recent_start = self
            .completed_turns
            .len()
            .saturating_sub(self.policy.recent_turns);
        for turn in &self.completed_turns[recent_start..] {
            let turn_items = self.items_for_events(&self.canonical[turn.clone()]);
            let turn_tokens = estimate_items(&turn_items)?;
            if used.saturating_add(turn_tokens) <= usable_tokens.saturating_sub(reserved_tail) {
                items.extend(turn_items);
                used += turn_tokens;
                report.recent_turn_tokens += turn_tokens;
            }
        }

        items.extend(current_items);
        used += current_tokens;
        report.current_turn_tokens += current_tokens;
        if let Some(goal_item) = goal_item {
            items.push(goal_item);
            used += goal_tokens;
            report.current_turn_tokens += goal_tokens;
        }

        if let Some(index) = self.latest_todos
            && let SessionEvent::TodoUpdated { items: todos } = &self.canonical[index].event
            && !todos.is_empty()
        {
            let mut todo_tokens = 0;
            push_if_fits(
                &mut items,
                ModelItem::TodoList {
                    items: todos.clone(),
                },
                &mut used,
                usable_tokens,
                &mut todo_tokens,
            )?;
        }

        for evidence in self.evidence_items() {
            if !push_if_fits(
                &mut items,
                evidence,
                &mut used,
                usable_tokens,
                &mut report.evidence_tokens,
            )? {
                break;
            }
        }

        report.estimated_tokens = used;
        report.compaction_recommended = used.saturating_mul(100)
            >= usable_tokens.saturating_mul(self.policy.compact_at_percent.into());
        Ok(AssembledContext {
            request: ModelRequest {
                items,
                ..request_shell
            },
            estimated_tokens: used,
            report,
        })
    }

    fn items_for_events(&self, events: &[EventEnvelope]) -> Vec<ModelItem> {
        events
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::UserMessage { text } => Some(ModelItem::User { text: text.clone() }),
                SessionEvent::AssistantMessage { text } => {
                    Some(ModelItem::Assistant { text: text.clone() })
                }
                SessionEvent::ToolCompleted { result, .. } => Some(ModelItem::ToolResult {
                    call_id: result.call_id.clone(),
                    name: result
                        .metadata
                        .get("tool_name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("tool")
                        .into(),
                    content: result.output.clone(),
                    is_error: result.is_error,
                    blob_refs: result.blob_refs.clone(),
                }),
                SessionEvent::AgentCompleted {
                    snapshot, summary, ..
                } => Some(ModelItem::AgentResult {
                    agent_id: snapshot.id.clone(),
                    summary: summary.clone(),
                    changed_files: snapshot.changed_files.clone(),
                    evidence_refs: Vec::new(),
                }),
                SessionEvent::AgentFailed {
                    snapshot, error, ..
                } => Some(ModelItem::AgentResult {
                    agent_id: snapshot.id.clone(),
                    summary: format!("Sub-agent failed: {error}"),
                    changed_files: snapshot.changed_files.clone(),
                    evidence_refs: Vec::new(),
                }),
                SessionEvent::AgentCancelled { snapshot, .. } => Some(ModelItem::AgentResult {
                    agent_id: snapshot.id.clone(),
                    summary: snapshot.last_error.clone().map_or_else(
                        || "Sub-agent was cancelled before completing.".into(),
                        |error| format!("Sub-agent was cancelled: {error}"),
                    ),
                    changed_files: snapshot.changed_files.clone(),
                    evidence_refs: Vec::new(),
                }),
                _ => None,
            })
            .collect()
    }

    fn evidence_items(&self) -> impl Iterator<Item = ModelItem> + '_ {
        self.evidence_events.iter().flat_map(|&index| {
            let SessionEvent::ToolCompleted { result, .. } = &self.canonical[index].event else {
                unreachable!("evidence index must reference a tool result");
            };
            result
                .metadata
                .get("evidence")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(move |entry| {
                    let path = entry.get("path")?.as_str()?;
                    let content = entry.get("content")?.as_str()?;
                    Some(ModelItem::Evidence {
                        path: path.into(),
                        content: content.into(),
                        blob: result.blob_refs.first().cloned(),
                    })
                })
        })
    }

    fn retained_turn_start(&self) -> usize {
        if self.completed_turns.len() <= self.policy.recent_turns {
            return 0;
        }
        if self.policy.recent_turns == 0 {
            return self.current_turn().map_or_else(
                || self.completed_turns.last().map_or(0, |turn| turn.end),
                |turn| turn.start,
            );
        }
        self.completed_turns[self.completed_turns.len() - self.policy.recent_turns].start
    }

    fn compaction_recommended(&self) -> bool {
        let bytes = serde_json::to_vec(&self.canonical).map_or(0, |bytes| bytes.len());
        let usable = self
            .policy
            .max_input_tokens
            .saturating_sub(self.policy.reserve_output_tokens);
        estimate_bytes(bytes).saturating_mul(100)
            >= usable.saturating_mul(self.policy.compact_at_percent.into())
    }
}

fn opens_context_turn(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::UserMessage { .. }
            | SessionEvent::AssistantMessage { .. }
            | SessionEvent::ToolProposed { .. }
            | SessionEvent::ToolInvocationRecorded { .. }
            | SessionEvent::ToolCompleted { .. }
            | SessionEvent::AgentQueued { .. }
            | SessionEvent::AgentStarted { .. }
            | SessionEvent::AgentProgress { .. }
            | SessionEvent::AgentCompleted { .. }
            | SessionEvent::AgentFailed { .. }
            | SessionEvent::AgentCancelled { .. }
            | SessionEvent::AgentMessage { .. }
    )
}

fn estimate_items(items: &[ModelItem]) -> Result<u64, KuramaError> {
    items.iter().try_fold(0_u64, |total, item| {
        estimate_serialized(serde_json::to_vec(item)).map(|tokens| total.saturating_add(tokens))
    })
}

fn push_if_fits(
    destination: &mut Vec<ModelItem>,
    item: ModelItem,
    used: &mut u64,
    budget: u64,
    category: &mut u64,
) -> Result<bool, KuramaError> {
    let tokens = estimate_serialized(serde_json::to_vec(&item))?;
    if used.saturating_add(tokens) > budget {
        return Ok(false);
    }
    destination.push(item);
    *used += tokens;
    *category += tokens;
    Ok(true)
}

pub fn estimate_text(text: &str) -> u64 {
    estimate_bytes(text.len())
}

fn estimate_serialized(serialized: Result<Vec<u8>, serde_json::Error>) -> Result<u64, KuramaError> {
    serialized
        .map(|bytes| estimate_bytes(bytes.len()))
        .map_err(|error| KuramaError::Protocol(error.to_string()))
}

fn estimate_bytes(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(3)
}

fn delegation_schema() -> serde_json::Value {
    let maximum_budget = AgentBudget::default();
    serde_json::json!({
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
                                    "maximum": maximum_budget.max_input_tokens
                                },
                                "max_output_tokens": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "maximum": maximum_budget.max_output_tokens
                                },
                                "max_turns": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "maximum": maximum_budget.max_turns
                                },
                                "max_seconds": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "maximum": maximum_budget.max_seconds
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
}

pub fn normalize_compaction_json(value: &str) -> Result<String, KuramaError> {
    let value: serde_json::Value = serde_json::from_str(value)
        .map_err(|error| KuramaError::Protocol(format!("invalid compaction JSON: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| KuramaError::Protocol("compaction response must be an object".into()))?;
    let mut output = Vec::new();
    for key in [
        "summary",
        "decisions",
        "open_tasks",
        "files",
        "operation_ids",
    ] {
        let value = object
            .get(key)
            .ok_or_else(|| KuramaError::Protocol(format!("compaction response missing {key}")))?;
        output.push(format!("{key}: {}", compact_json_value(value)?));
    }
    Ok(output.join("\n"))
}

fn compact_json_value(value: &serde_json::Value) -> Result<String, KuramaError> {
    match value {
        serde_json::Value::String(value) => Ok(value.clone()),
        _ => serde_json::to_string(value).map_err(|error| KuramaError::Protocol(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_is_conservative_and_deterministic() {
        assert_eq!(estimate_text("abcdef"), 2);
        assert_eq!(estimate_text("abcdefg"), 3);
    }

    #[test]
    fn compaction_json_requires_all_sections() {
        let value =
            r#"{"summary":"s","decisions":[],"open_tasks":[],"files":[],"operation_ids":[]}"#;
        assert!(
            normalize_compaction_json(value)
                .expect("normalize")
                .contains("summary: s")
        );
    }

    #[test]
    fn latest_user_is_preserved_between_turns_and_reset_by_replay() {
        let event = |sequence, event| {
            EventEnvelope::new(sequence, sequence, SessionId::from("session"), None, event)
        };
        let mut manager = ContextManager::new(ContextPolicy::default());
        manager.record(event(
            0,
            SessionEvent::UserMessage {
                text: "first".into(),
            },
        ));
        manager.record(event(
            1,
            SessionEvent::UserMessage {
                text: "latest".into(),
            },
        ));
        manager.record(event(2, SessionEvent::TurnCompleted));
        manager.record(event(
            3,
            SessionEvent::AssistantMessage {
                text: "continuation".into(),
            },
        ));
        assert_eq!(manager.latest_user_text(), Some("latest"));

        manager.replay(vec![event(
            0,
            SessionEvent::UserMessage {
                text: "replacement".into(),
            },
        )]);
        assert_eq!(manager.latest_user_text(), Some("replacement"));
        manager.replay(vec![event(0, SessionEvent::TurnCompleted)]);
        assert_eq!(manager.latest_user_text(), None);
        manager.replay(Vec::new());
        assert_eq!(manager.latest_user_text(), None);
    }
}
