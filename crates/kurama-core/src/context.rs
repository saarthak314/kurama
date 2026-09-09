use kurama_protocol::{
    KuramaError,
    agent::AgentBudget,
    id::{AgentId, SessionId},
    model::{DelegationSchema, ModelItem, ModelProfile, ModelRequest},
    session::{EventEnvelope, SessionEvent, latest_todos},
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
}

impl ContextManager {
    pub fn new(policy: ContextPolicy) -> Self {
        Self {
            policy,
            canonical: Vec::new(),
            summary: None,
            identity: None,
        }
    }

    pub fn replay(&mut self, events: Vec<EventEnvelope>) {
        self.canonical.clear();
        self.summary = None;
        self.identity = None;
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
        self.canonical.push(event);
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

        let turns = split_turns(&self.canonical);
        let current_items = turns
            .iter()
            .rev()
            .find(|turn| !turn.complete)
            .map_or_else(Vec::new, |turn| self.items_for_events(&turn.events));
        let current_tokens = estimate_items(&current_items)?;
        if fixed_tokens.saturating_add(current_tokens) > usable_tokens {
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
                usable_tokens.saturating_sub(current_tokens),
                &mut report.summary_tokens,
            )?;
        }

        let mut recent: Vec<_> = turns
            .iter()
            .filter(|turn| turn.complete)
            .rev()
            .take(self.policy.recent_turns)
            .collect();
        recent.reverse();
        for turn in recent {
            let turn_items = self.items_for_events(&turn.events);
            push_items(
                &mut items,
                turn_items,
                &mut used,
                usable_tokens.saturating_sub(current_tokens),
                &mut report.recent_turn_tokens,
            )?;
        }

        push_required_items(
            &mut items,
            current_items,
            &mut used,
            &mut report.current_turn_tokens,
        )?;

        let todos = latest_todos(&self.canonical);
        if !todos.is_empty() {
            let mut todo_tokens = 0;
            push_if_fits(
                &mut items,
                ModelItem::TodoList { items: todos },
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

    fn items_for_events(&self, events: &[&EventEnvelope]) -> Vec<ModelItem> {
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

    fn evidence_items(&self) -> Vec<ModelItem> {
        let mut evidence = Vec::new();
        for event in &self.canonical {
            let SessionEvent::ToolCompleted { result, .. } = &event.event else {
                continue;
            };
            let Some(entries) = result
                .metadata
                .get("evidence")
                .and_then(|value| value.as_array())
            else {
                continue;
            };
            for entry in entries {
                let Some(path) = entry.get("path").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let Some(content) = entry.get("content").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                evidence.push(ModelItem::Evidence {
                    path: path.into(),
                    content: content.into(),
                    blob: result.blob_refs.first().cloned(),
                });
            }
        }
        evidence
    }

    fn retained_turn_start(&self) -> usize {
        let turns = split_turns(&self.canonical);
        let complete: Vec<_> = turns.iter().filter(|turn| turn.complete).collect();
        if complete.len() <= self.policy.recent_turns {
            return 0;
        }
        complete[complete.len() - self.policy.recent_turns].start
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

#[derive(Debug)]
struct Turn<'a> {
    start: usize,
    events: Vec<&'a EventEnvelope>,
    complete: bool,
}

fn split_turns(events: &[EventEnvelope]) -> Vec<Turn<'_>> {
    let mut turns = Vec::new();
    let mut current: Option<Turn<'_>> = None;
    for (index, event) in events.iter().enumerate() {
        if matches!(event.event, SessionEvent::UserMessage { .. }) {
            if let Some(turn) = current.take() {
                turns.push(turn);
            }
            current = Some(Turn {
                start: index,
                events: Vec::new(),
                complete: false,
            });
        }
        if let Some(turn) = current.as_mut() {
            turn.events.push(event);
            if matches!(
                event.event,
                SessionEvent::TurnCompleted | SessionEvent::TurnFailed { .. }
            ) {
                turn.complete = true;
                turns.push(current.take().expect("current turn"));
            }
        }
    }
    if let Some(turn) = current {
        turns.push(turn);
    }
    turns
}

fn push_items(
    destination: &mut Vec<ModelItem>,
    source: Vec<ModelItem>,
    used: &mut u64,
    budget: u64,
    category: &mut u64,
) -> Result<(), KuramaError> {
    for item in source {
        if !push_if_fits(destination, item, used, budget, category)? {
            break;
        }
    }
    Ok(())
}

fn push_required_items(
    destination: &mut Vec<ModelItem>,
    items: Vec<ModelItem>,
    used: &mut u64,
    category: &mut u64,
) -> Result<(), KuramaError> {
    for item in items {
        let tokens = estimate_serialized(serde_json::to_vec(&item))?;
        destination.push(item);
        *used += tokens;
        *category += tokens;
    }
    Ok(())
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
}
