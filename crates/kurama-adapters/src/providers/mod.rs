use kurama_protocol::{
    KuramaError,
    agent::{AgentBudget, AgentSpec, DelegationRequest, WriteScope},
    model::{ModelItem, ModelRequest},
    tool::ToolDescriptor,
};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

const DELEGATION_OPEN: &str = "<kurama_delegate>";
const DELEGATION_CLOSE: &str = "</kurama_delegate>";

#[cfg(feature = "anthropic")]
pub mod anthropic;
#[cfg(feature = "openai")]
pub mod openai;
#[cfg(feature = "openai-compatible")]
pub mod openai_compat;
pub mod sse;

pub fn endpoint_url(endpoint: &Url, resource: &str) -> Result<Url, KuramaError> {
    if !matches!(endpoint.scheme(), "http" | "https") {
        return Err(KuramaError::Configuration(format!(
            "unsupported provider URL scheme: {}",
            endpoint.scheme()
        )));
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err(KuramaError::Configuration(
            "provider endpoint must not contain credentials".into(),
        ));
    }
    let mut base = endpoint.clone();
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    base.join(resource)
        .map_err(|error| KuramaError::Configuration(format!("provider endpoint: {error}")))
}

#[cfg(feature = "openai")]
pub fn responses_input(request: &ModelRequest) -> Vec<Value> {
    request
        .items
        .iter()
        .map(|item| match item {
            ModelItem::User { text } => message("user", "input_text", text),
            ModelItem::Assistant { text } => message("assistant", "output_text", text),
            ModelItem::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } => json!({
                "type": "function_call_output",
                "call_id": call_id.as_ref(),
                "output": if *is_error { format!("ERROR: {content}") } else { content.clone() }
            }),
            other => message("user", "input_text", &context_text(other)),
        })
        .collect()
}

#[cfg(feature = "openai-compatible")]
pub fn chat_messages(request: &ModelRequest) -> Vec<Value> {
    let mut messages = vec![json!({"role": "system", "content": request.system})];
    messages.extend(request.items.iter().map(|item| match item {
        ModelItem::User { text } => json!({"role": "user", "content": text}),
        ModelItem::Assistant { text } => json!({"role": "assistant", "content": text}),
        ModelItem::ToolResult {
            call_id,
            content,
            is_error,
            ..
        } => json!({
            "role": "tool",
            "tool_call_id": call_id.as_ref(),
            "content": if *is_error { format!("ERROR: {content}") } else { content.clone() }
        }),
        other => json!({"role": "user", "content": context_text(other)}),
    }));
    messages
}

#[cfg(feature = "anthropic")]
pub fn anthropic_messages(request: &ModelRequest) -> Vec<Value> {
    request
        .items
        .iter()
        .map(|item| match item {
            ModelItem::User { text } => json!({"role": "user", "content": text}),
            ModelItem::Assistant { text } => json!({"role": "assistant", "content": text}),
            ModelItem::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } => json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": call_id.as_ref(),
                    "content": content,
                    "is_error": is_error
                }]
            }),
            other => json!({"role": "user", "content": context_text(other)}),
        })
        .collect()
}

#[cfg(any(
    feature = "openai",
    feature = "anthropic",
    feature = "openai-compatible"
))]
pub fn responses_tools(tools: &[ToolDescriptor]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
                "strict": true
            })
        })
        .collect()
}

#[cfg(feature = "openai-compatible")]
pub fn chat_tools(tools: &[ToolDescriptor]) -> Vec<Value> {
    responses_tools(tools)
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool["name"],
                    "description": tool["description"],
                    "parameters": tool["parameters"],
                    "strict": true
                }
            })
        })
        .collect()
}

#[cfg(feature = "anthropic")]
pub fn anthropic_tools(tools: &[ToolDescriptor]) -> Vec<Value> {
    responses_tools(tools)
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool["name"],
                "description": tool["description"],
                "input_schema": tool["parameters"],
                "strict": true
            })
        })
        .collect()
}

pub fn provider_instructions(request: &ModelRequest) -> String {
    let Some(delegation) = request.delegation.as_ref() else {
        return request.system.clone();
    };
    format!(
        "{}\n\nDelegation is not a tool. To delegate, return exactly {DELEGATION_OPEN}JSON{DELEGATION_CLOSE} as the entire assistant text, with JSON matching this schema: {}. Do not add prose or Markdown around the control block. Kurama assigns child roles and profiles; dependencies must name exact prerequisite objective strings.",
        request.system, delegation.parameters
    )
}

pub fn normalize_delegation_events(
    events: Vec<kurama_protocol::model::ModelEvent>,
    delegation_enabled: bool,
) -> Result<Vec<kurama_protocol::model::ModelEvent>, KuramaError> {
    use kurama_protocol::model::{FinishReason, ModelEvent};

    let text = events
        .iter()
        .filter_map(|event| match event {
            ModelEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    if !text.contains(DELEGATION_OPEN) && !text.contains(DELEGATION_CLOSE) {
        return Ok(events);
    }
    if !delegation_enabled {
        return Err(KuramaError::Protocol(
            "provider returned delegation while disabled".into(),
        ));
    }
    let trimmed = text.trim();
    let Some(payload) = trimmed
        .strip_prefix(DELEGATION_OPEN)
        .and_then(|value| value.strip_suffix(DELEGATION_CLOSE))
    else {
        return Err(KuramaError::Protocol(
            "delegation control block must be the entire response".into(),
        ));
    };
    if payload.contains(DELEGATION_OPEN) || payload.contains(DELEGATION_CLOSE) {
        return Err(KuramaError::Protocol(
            "delegation control block must not be nested".into(),
        ));
    }
    let arguments = serde_json::from_str(payload)
        .map_err(|error| KuramaError::Protocol(format!("invalid delegation JSON: {error}")))?;
    let mut delegation = Some(ModelEvent::Delegation {
        request: delegation_from_arguments(arguments)?,
    });
    let mut normalized = Vec::with_capacity(events.len());
    for event in events {
        match event {
            ModelEvent::TextDelta { .. } => {
                if let Some(event) = delegation.take() {
                    normalized.push(event);
                }
            }
            ModelEvent::ResponseCompleted { cursor, .. } => {
                if let Some(event) = delegation.take() {
                    normalized.push(event);
                }
                normalized.push(ModelEvent::ResponseCompleted {
                    cursor,
                    finish_reason: FinishReason::ToolCalls,
                });
            }
            event => normalized.push(event),
        }
    }
    if let Some(event) = delegation {
        normalized.push(event);
    }
    Ok(normalized)
}

pub fn delegation_from_arguments(arguments: Value) -> Result<DelegationRequest, KuramaError> {
    let request: ModelDelegationRequest = serde_json::from_value(arguments)
        .map_err(|error| KuramaError::Protocol(format!("invalid delegation request: {error}")))?;
    Ok(DelegationRequest {
        agents: request
            .agents
            .into_iter()
            .map(|agent| AgentSpec {
                role: String::new(),
                objective: agent.objective,
                profile: None,
                context_refs: agent.context_refs,
                write_scope: agent.write_scope,
                budget: agent.budget,
                depends_on: agent.depends_on,
            })
            .collect(),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelDelegationRequest {
    agents: Vec<ModelAgentSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelAgentSpec {
    objective: String,
    #[serde(default)]
    context_refs: Vec<String>,
    write_scope: WriteScope,
    budget: AgentBudget,
    #[serde(default)]
    depends_on: Vec<String>,
}

#[cfg(feature = "openai")]
fn message(role: &str, content_type: &str, text: &str) -> Value {
    json!({
        "role": role,
        "content": [{"type": content_type, "text": text}]
    })
}

fn context_text(item: &ModelItem) -> String {
    match item {
        ModelItem::Summary { text, .. } => format!("Context summary:\n{text}"),
        ModelItem::Evidence { path, content, .. } => format!("Evidence from {path}:\n{content}"),
        ModelItem::AgentResult {
            agent_id,
            summary,
            changed_files,
            ..
        } => format!(
            "Child agent {agent_id} result:\n{summary}\nChanged files: {}",
            changed_files.join(", ")
        ),
        ModelItem::TodoList { items } => {
            let mut lines = vec!["Session todo list:".to_owned()];
            for item in items {
                lines.push(format!(
                    "- [{}] {}: {}",
                    match item.status {
                        kurama_protocol::session::TodoStatus::Pending => " ",
                        kurama_protocol::session::TodoStatus::InProgress => ">",
                        kurama_protocol::session::TodoStatus::Completed => "x",
                        kurama_protocol::session::TodoStatus::Cancelled => "-",
                    },
                    item.id,
                    item.content
                ));
            }
            lines.join("\n")
        }
        ModelItem::Goal { goal, continuation } => {
            let mut text = format!(
                "Active goal ({}):\n{}",
                goal.status.as_str(),
                goal.objective
            );
            if *continuation {
                text.push_str(
                    "\n\nContinue this goal. Do not shrink it. Call update_goal with status \"complete\" only when current evidence proves every requirement. Call update_goal with status \"blocked\" only after the same blocker repeats for three consecutive goal turns. Otherwise keep working.",
                );
            }
            text
        }
        _ => unreachable!("message-like items are handled directly"),
    }
}

pub(crate) fn event_stream(
    events: Vec<Result<kurama_protocol::model::ModelEvent, KuramaError>>,
) -> kurama_protocol::traits::ModelStream {
    Box::pin(futures_util::stream::iter(events))
}
