use kurama_protocol::{
    id::SessionId,
    policy::ExecutionMode,
    session::{MAX_GOAL_OBJECTIVE_CHARS, SessionGoal},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub accepts_arguments: bool,
    pub requires_arguments: bool,
}

pub const COMMAND_SPECS: [CommandSpec; 18] = [
    CommandSpec {
        name: "model",
        description: "select or list profiles",
        accepts_arguments: true,
        requires_arguments: false,
    },
    CommandSpec {
        name: "agents",
        description: "inspect and control sub-agents",
        accepts_arguments: false,
        requires_arguments: false,
    },
    CommandSpec {
        name: "todo",
        description: "show the session todo list",
        accepts_arguments: false,
        requires_arguments: false,
    },
    CommandSpec {
        name: "goal",
        description: "set, view, pause, resume, or clear a goal",
        accepts_arguments: true,
        requires_arguments: false,
    },
    CommandSpec {
        name: "mode",
        description: "switch supervised or auto mode",
        accepts_arguments: true,
        requires_arguments: true,
    },
    CommandSpec {
        name: "connect",
        description: "add a provider profile",
        accepts_arguments: false,
        requires_arguments: false,
    },
    CommandSpec {
        name: "sessions",
        description: "list recent project sessions",
        accepts_arguments: false,
        requires_arguments: false,
    },
    CommandSpec {
        name: "resume",
        description: "resume a saved session",
        accepts_arguments: true,
        requires_arguments: true,
    },
    CommandSpec {
        name: "new",
        description: "start a new session",
        accepts_arguments: false,
        requires_arguments: false,
    },
    CommandSpec {
        name: "context",
        description: "inspect assembled context estimates and compaction coverage",
        accepts_arguments: false,
        requires_arguments: false,
    },
    CommandSpec {
        name: "compact",
        description: "compact the current context",
        accepts_arguments: false,
        requires_arguments: false,
    },
    CommandSpec {
        name: "status",
        description: "show model, mode, usage, and git",
        accepts_arguments: false,
        requires_arguments: false,
    },
    CommandSpec {
        name: "copy",
        description: "copy the latest assistant reply",
        accepts_arguments: false,
        requires_arguments: false,
    },
    CommandSpec {
        name: "diff",
        description: "review changes and prepare hunk feedback",
        accepts_arguments: false,
        requires_arguments: false,
    },
    CommandSpec {
        name: "queue",
        description: "edit, remove, or resume pending follow-ups",
        accepts_arguments: false,
        requires_arguments: false,
    },
    CommandSpec {
        name: "verify",
        description: "list project checks or run one by name",
        accepts_arguments: true,
        requires_arguments: false,
    },
    CommandSpec {
        name: "help",
        description: "list slash commands",
        accepts_arguments: false,
        requires_arguments: false,
    },
    CommandSpec {
        name: "exit",
        description: "exit and print resume details",
        accepts_arguments: false,
        requires_arguments: false,
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Agents,
    Todo,
    Goal(GoalAction),
    Verify(Option<String>),
    Model(Option<String>),
    Connect,
    Sessions,
    Resume(SessionId),
    New,
    Context,
    Compact,
    Copy,
    Diff,
    Queue,
    Status,
    Mode(ExecutionMode),
    Help,
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalAction {
    View,
    Set(String),
    Edit(String),
    Pause,
    Resume,
    Clear,
}

pub fn command_suggestions(input: &str) -> Vec<CommandSpec> {
    let Some(first_line) = input.lines().next() else {
        return Vec::new();
    };
    let Some(command) = first_line.strip_prefix('/') else {
        return Vec::new();
    };
    let filter = command.split_whitespace().next().unwrap_or("");
    let mut exact = Vec::new();
    let mut prefixes = Vec::new();
    for spec in COMMAND_SPECS.iter().copied() {
        if spec.name == filter {
            exact.push(spec);
        } else if spec.name.starts_with(filter) {
            prefixes.push(spec);
        }
    }
    exact.extend(prefixes);
    exact
}

pub fn command_missing_required_arguments(input: &str) -> bool {
    let mut parts = input.split_whitespace();
    let Some(name) = parts.next().and_then(|name| name.strip_prefix('/')) else {
        return false;
    };
    parts.next().is_none()
        && COMMAND_SPECS
            .iter()
            .any(|spec| spec.name == name && spec.requires_arguments)
}

pub fn parse_command(input: &str) -> Result<Command, String> {
    let mut parts = input.split_whitespace();
    let name = parts.next().ok_or_else(|| "empty command".to_owned())?;
    let remainder = parts.collect::<Vec<_>>();

    match (name, remainder.as_slice()) {
        ("/agents", []) => Ok(Command::Agents),
        ("/todo", []) => Ok(Command::Todo),
        ("/goal", _) => parse_goal(input),
        ("/verify", []) => Ok(Command::Verify(None)),
        ("/verify", [name]) => Ok(Command::Verify(Some((*name).to_owned()))),
        ("/model", []) => Ok(Command::Model(None)),
        ("/model", [profile]) => Ok(Command::Model(Some((*profile).to_owned()))),
        ("/connect", []) => Ok(Command::Connect),
        ("/sessions", []) => Ok(Command::Sessions),
        ("/resume", [session_id]) => Ok(Command::Resume(SessionId::from(*session_id))),
        ("/new", []) => Ok(Command::New),
        ("/context", []) => Ok(Command::Context),
        ("/status", []) => Ok(Command::Status),
        ("/compact", []) => Ok(Command::Compact),
        ("/copy", []) => Ok(Command::Copy),
        ("/diff", []) => Ok(Command::Diff),
        ("/queue", []) => Ok(Command::Queue),
        ("/help", []) => Ok(Command::Help),
        ("/mode", ["supervised"]) => Ok(Command::Mode(ExecutionMode::Supervised)),
        ("/mode", ["auto"]) => Ok(Command::Mode(ExecutionMode::Auto)),
        ("/exit", []) => Ok(Command::Exit),
        ("/mode", ["yolo"]) => Err("YOLO is launch-only; restart with --yolo".into()),
        ("/restart", _) => Err("/restart is intentionally unsupported; use /new or /resume".into()),
        _ => Err(format!("unknown or invalid command: {input}")),
    }
}

fn parse_goal(input: &str) -> Result<Command, String> {
    let rest = input
        .strip_prefix("/goal")
        .ok_or_else(|| "unknown or invalid command: /goal".to_owned())?
        .trim();
    if rest.is_empty() {
        return Ok(Command::Goal(GoalAction::View));
    }
    let action = match rest.split_once(char::is_whitespace) {
        Some(("set", objective)) => GoalAction::Set(validate_goal_objective(objective)?),
        Some(("edit", objective)) => GoalAction::Edit(validate_goal_objective(objective)?),
        None if rest == "set" => {
            return Err("usage: /goal set <objective>".into());
        }
        None if rest == "edit" => {
            return Err("usage: /goal edit <objective>".into());
        }
        None if rest == "view" => GoalAction::View,
        None if rest == "pause" => GoalAction::Pause,
        None if rest == "resume" => GoalAction::Resume,
        None if rest == "clear" => GoalAction::Clear,
        _ => GoalAction::Set(validate_goal_objective(rest)?),
    };
    Ok(Command::Goal(action))
}

fn validate_goal_objective(objective: &str) -> Result<String, String> {
    SessionGoal::validate_objective(objective.to_owned()).map_err(|error| {
        if objective.trim().is_empty() {
            "goal objective must be non-empty".into()
        } else if objective.trim().chars().count() > MAX_GOAL_OBJECTIVE_CHARS {
            format!("goal objective cannot exceed {MAX_GOAL_OBJECTIVE_CHARS} characters")
        } else {
            error.to_string()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_product_commands_and_rejects_unsupported_actions() {
        assert_eq!(parse_command("/agents").unwrap(), Command::Agents);
        assert_eq!(
            parse_command("/model openai-main").unwrap(),
            Command::Model(Some("openai-main".into()))
        );
        assert_eq!(
            parse_command("/resume ses_deadbeef").unwrap(),
            Command::Resume(SessionId::from("ses_deadbeef"))
        );
        assert_eq!(
            parse_command("/mode auto").unwrap(),
            Command::Mode(ExecutionMode::Auto)
        );
        assert_eq!(parse_command("/help").unwrap(), Command::Help);
        assert_eq!(parse_command("/copy").unwrap(), Command::Copy);
        assert_eq!(parse_command("/diff").unwrap(), Command::Diff);
        assert_eq!(parse_command("/status").unwrap(), Command::Status);
        assert_eq!(
            parse_command("/goal").unwrap(),
            Command::Goal(GoalAction::View)
        );
        assert_eq!(
            parse_command("/goal keep tests green").unwrap(),
            Command::Goal(GoalAction::Set("keep tests green".into()))
        );
        assert_eq!(
            parse_command("/goal set ship the e2e pass").unwrap(),
            Command::Goal(GoalAction::Set("ship the e2e pass".into()))
        );
        assert_eq!(
            parse_command("/goal view").unwrap(),
            Command::Goal(GoalAction::View)
        );
        assert_eq!(
            parse_command("/goal pause").unwrap(),
            Command::Goal(GoalAction::Pause)
        );
        assert!(parse_command("/goal set").is_err());
        assert_eq!(parse_command("/exit").unwrap(), Command::Exit);
        assert!(parse_command("/restart").is_err());
        assert!(parse_command("/mode yolo").is_err());
    }
}
