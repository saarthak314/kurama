use kurama_protocol::{
    KuramaError,
    agent::DelegationRequest,
    model::{DelegationSchema, ModelItem, ModelRequest},
    tool::ToolDescriptor,
};
use serde_json::{Value, json};
use url::Url;

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
pub fn responses_tools(
    tools: &[ToolDescriptor],
    delegation: Option<&DelegationSchema>,
) -> Vec<Value> {
    let mut values = tools
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
        .collect::<Vec<_>>();
    if let Some(delegation) = delegation {
        values.push(json!({
            "type": "function",
            "name": "__kurama_delegate",
            "description": "Delegate explicit work to bounded child agents.",
            "parameters": delegation.parameters,
            "strict": true
        }));
    }
    values
}

#[cfg(feature = "openai-compatible")]
pub fn chat_tools(tools: &[ToolDescriptor], delegation: Option<&DelegationSchema>) -> Vec<Value> {
    responses_tools(tools, delegation)
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
pub fn anthropic_tools(
    tools: &[ToolDescriptor],
    delegation: Option<&DelegationSchema>,
) -> Vec<Value> {
    responses_tools(tools, delegation)
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

pub fn delegation_from_arguments(arguments: Value) -> Result<DelegationRequest, KuramaError> {
    serde_json::from_value(arguments)
        .map_err(|error| KuramaError::Protocol(format!("invalid delegation request: {error}")))
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
        _ => unreachable!("message-like items are handled directly"),
    }
}

pub(crate) fn event_stream(
    events: Vec<Result<kurama_protocol::model::ModelEvent, KuramaError>>,
) -> kurama_protocol::traits::ModelStream {
    Box::pin(futures_util::stream::iter(events))
}
