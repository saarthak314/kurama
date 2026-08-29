use kurama_protocol::{id::SessionId, policy::ExecutionMode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Agents,
    Model(Option<String>),
    Connect,
    Sessions,
    Resume(SessionId),
    New,
    Context,
    Compact,
    Mode(ExecutionMode),
}

pub fn parse_command(input: &str) -> Result<Command, String> {
    let mut parts = input.split_whitespace();
    let name = parts.next().ok_or_else(|| "empty command".to_owned())?;
    let remainder = parts.collect::<Vec<_>>();

    match (name, remainder.as_slice()) {
        ("/agents", []) => Ok(Command::Agents),
        ("/model", []) => Ok(Command::Model(None)),
        ("/model", [profile]) => Ok(Command::Model(Some((*profile).to_owned()))),
        ("/connect", []) => Ok(Command::Connect),
        ("/sessions", []) => Ok(Command::Sessions),
        ("/resume", [session_id]) => Ok(Command::Resume(SessionId::from(*session_id))),
        ("/new", []) => Ok(Command::New),
        ("/context", []) => Ok(Command::Context),
        ("/compact", []) => Ok(Command::Compact),
        ("/mode", ["supervised"]) => Ok(Command::Mode(ExecutionMode::Supervised)),
        ("/mode", ["auto"]) => Ok(Command::Mode(ExecutionMode::Auto)),
        ("/mode", ["yolo"]) => Err("YOLO is launch-only; restart with --yolo".into()),
        ("/restart", _) => Err("/restart is intentionally unsupported; use /new or /resume".into()),
        _ => Err(format!("unknown or invalid command: {input}")),
    }
}
