use kurama_protocol::{
    KuramaError,
    session::GoalStatus,
    tool::{Operation, ToolContext, ToolDescriptor, ToolInvocation, ToolResult},
    traits::{BoxFuture, CancelSignal, Tool},
};

pub struct GoalTool;

impl Tool for GoalTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "update_goal".into(),
            description: "Mark the parent session goal complete or blocked. Call complete only when current evidence proves every requirement. Call blocked only after the same blocker repeats for three consecutive goal turns.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["complete", "blocked"]
                    },
                    "reason": {
                        "type": "string",
                        "description": "Evidence for completion, or the repeated blocker."
                    }
                },
                "required": ["status", "reason"],
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
            let update = parse_update(&invocation.arguments)?;
            Ok(ToolResult {
                call_id: invocation.call_id,
                output: format!("goal {} — {}", update.status.as_str(), update.reason),
                is_error: false,
                metadata: serde_json::json!({
                    "tool_name": "update_goal",
                    "status": update.status.as_str(),
                    "reason": update.reason,
                }),
                truncated: false,
                blob_refs: Vec::new(),
            })
        })
    }
}

pub(crate) struct GoalUpdate {
    pub status: GoalStatus,
    pub reason: String,
}

pub(crate) fn update_from_result_or_arguments(
    result: &ToolResult,
    arguments: &serde_json::Value,
) -> Result<GoalUpdate, KuramaError> {
    if let Some(status) = result
        .metadata
        .get("status")
        .and_then(serde_json::Value::as_str)
        && let Some(reason) = result
            .metadata
            .get("reason")
            .and_then(serde_json::Value::as_str)
    {
        return Ok(GoalUpdate {
            status: parse_status(status)?,
            reason: reason.trim().to_owned(),
        });
    }
    parse_update(arguments)
}

fn parse_update(value: &serde_json::Value) -> Result<GoalUpdate, KuramaError> {
    let status = value
        .get("status")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| KuramaError::Protocol("update_goal requires status".into()))?;
    let reason = value
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if reason.is_empty() {
        return Err(KuramaError::Protocol(
            "update_goal requires a non-empty reason".into(),
        ));
    }
    Ok(GoalUpdate {
        status: parse_status(status)?,
        reason,
    })
}

fn parse_status(status: &str) -> Result<GoalStatus, KuramaError> {
    match status {
        "complete" | "achieved" => Ok(GoalStatus::Achieved),
        "blocked" => Ok(GoalStatus::Blocked),
        other => Err(KuramaError::Protocol(format!(
            "unknown update_goal status: {other}"
        ))),
    }
}
