use kurama_protocol::{
    KuramaError,
    session::{TodoItem, TodoStatus},
    tool::{Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolResult},
    traits::{BoxFuture, CancelSignal, Tool},
};

pub struct TodoTool;

impl Tool for TodoTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "todo".into(),
            description: "Replace the parent session todo list (max 20; at most one in_progress)."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "maxItems": 20,
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "content": {"type": "string"},
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed", "cancelled"]
                                }
                            },
                            "required": ["id", "content", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["items"],
                "additionalProperties": false
            }),
        }
    }

    fn classify(
        &self,
        context: &ToolContext,
        _invocation: &ToolInvocation,
    ) -> Result<Operation, KuramaError> {
        Ok(Operation::Read {
            path: context.workspace_root.clone(),
            external: false,
        })
    }

    fn execute<'a>(
        &'a self,
        _context: ToolContext,
        invocation: ToolInvocation,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ToolResult, KuramaError>> {
        Box::pin(async move {
            let items = parse_items(&invocation.arguments)?;
            TodoItem::validate_list(&items)?;
            let output = if items.is_empty() {
                "Todo list cleared.".into()
            } else {
                items
                    .iter()
                    .map(|item| {
                        let status = match item.status {
                            TodoStatus::Pending => "pending",
                            TodoStatus::InProgress => "in_progress",
                            TodoStatus::Completed => "completed",
                            TodoStatus::Cancelled => "cancelled",
                        };
                        format!("[{status}] {}: {}", item.id, item.content)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok(ToolResult {
                call_id: invocation.call_id,
                output,
                is_error: false,
                metadata: serde_json::json!({"tool_name": "todo", "items": items}),
                truncated: false,
                blob_refs: Vec::new(),
            })
        })
    }
}

pub(crate) fn items_from_result_or_arguments(
    result: &ToolResult,
    arguments: &serde_json::Value,
) -> Result<Vec<TodoItem>, KuramaError> {
    let items = result
        .metadata
        .get("items")
        .map_or_else(|| parse_items(arguments), parse_items)?;
    TodoItem::validate_list(&items)?;
    Ok(items)
}

fn parse_items(value: &serde_json::Value) -> Result<Vec<TodoItem>, KuramaError> {
    let items = value.get("items").unwrap_or(value).clone();
    serde_json::from_value(items)
        .map_err(|error| KuramaError::Protocol(format!("invalid todo items: {error}")))
}
