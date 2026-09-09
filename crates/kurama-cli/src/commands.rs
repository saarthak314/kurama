use kurama_protocol::{id::SessionId, policy::ExecutionMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub accepts_arguments: bool,
    pub requires_arguments: bool,
}

pub const COMMAND_SPECS: [CommandSpec; 15] = [
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
        description: "show context and session details",
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
        description: "show the working tree diff",
        accepts_arguments: false,
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
    Model(Option<String>),
    Connect,
    Sessions,
    Resume(SessionId),
    New,
    Context,
    Compact,
    Copy,
    Diff,
    Status,
    Mode(ExecutionMode),
    Help,
    Exit,
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
        ("/help", []) => Ok(Command::Help),
        ("/mode", ["supervised"]) => Ok(Command::Mode(ExecutionMode::Supervised)),
        ("/mode", ["auto"]) => Ok(Command::Mode(ExecutionMode::Auto)),
        ("/exit", []) => Ok(Command::Exit),
        ("/mode", ["yolo"]) => Err("YOLO is launch-only; restart with --yolo".into()),
        ("/restart", _) => Err("/restart is intentionally unsupported; use /new or /resume".into()),
        _ => Err(format!("unknown or invalid command: {input}")),
    }
}
