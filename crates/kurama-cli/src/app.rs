use std::{io, path::PathBuf};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use kurama_protocol::{
    policy::ApprovalResponse,
    runtime::{EngineCommand, RuntimeEvent},
};
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend},
};
use tokio::sync::mpsc;

use crate::{
    args::Args,
    commands::{Command, parse_command},
    tui::{Overlay, TerminalGuard, TuiState, render, spawn_input_thread},
};

pub struct App {
    pub state: TuiState,
}

impl App {
    pub fn bootstrap(args: &Args, cwd: PathBuf) -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set".to_owned())?;
        let config_exists = home.join(".kurama/config.toml").is_file();
        let project = cwd.display().to_string();
        let state = if config_exists || args.profile.is_some() {
            TuiState::new(
                args.profile.as_deref().unwrap_or("default"),
                "configured model",
                project,
                if args.yolo {
                    kurama_protocol::policy::ExecutionMode::Yolo
                } else {
                    kurama_protocol::policy::ExecutionMode::Supervised
                },
            )
        } else {
            TuiState::onboarding(project)
        };
        Ok(Self { state })
    }

    pub const fn tool_names() -> [&'static str; 4] {
        ["bash", "read", "web-search", "write"]
    }

    pub async fn run(mut self) -> Result<(), String> {
        let _guard = TerminalGuard::enter().map_err(|error| error.to_string())?;
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend).map_err(|error| error.to_string())?;
        let mut input = spawn_input_thread(32);
        terminal
            .draw(|frame| render(frame, &self.state))
            .map_err(|error| error.to_string())?;

        while let Some(event) = input.recv().await {
            let should_exit = self.handle_event(event)?;
            terminal
                .draw(|frame| render(frame, &self.state))
                .map_err(|error| error.to_string())?;
            if should_exit {
                break;
            }
        }
        Ok(())
    }

    pub fn handle_event(&mut self, event: Event) -> Result<bool, String> {
        let Event::Key(key) = event else {
            return Ok(false);
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.state.take_commands();
            return Ok(true);
        }

        match self.state.overlay {
            Overlay::Onboarding => self.handle_onboarding_key(key),
            Overlay::Approval | Overlay::ApprovalEdit => self.handle_approval_key(key),
            Overlay::Agents
            | Overlay::AgentInspect
            | Overlay::AgentMessage
            | Overlay::ConfirmAgentCancel => self.handle_agents_key(key),
            Overlay::None => self.handle_main_key(key)?,
        }
        Ok(false)
    }

    fn handle_main_key(&mut self, key: KeyEvent) -> Result<(), String> {
        match key.code {
            KeyCode::Char(character) => {
                self.state.composer.insert(self.state.cursor, character);
                self.state.cursor += character.len_utf8();
            }
            KeyCode::Backspace if self.state.cursor > 0 => {
                let previous = self.state.composer[..self.state.cursor]
                    .char_indices()
                    .last()
                    .map(|(index, _)| index)
                    .unwrap_or(0);
                self.state.composer.drain(previous..self.state.cursor);
                self.state.cursor = previous;
            }
            KeyCode::Delete if self.state.cursor < self.state.composer.len() => {
                let next = self.state.cursor
                    + self.state.composer[self.state.cursor..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(0);
                self.state.composer.drain(self.state.cursor..next);
            }
            KeyCode::Left => {
                self.state.cursor = self.state.composer[..self.state.cursor]
                    .char_indices()
                    .last()
                    .map(|(index, _)| index)
                    .unwrap_or(0);
            }
            KeyCode::Right if self.state.cursor < self.state.composer.len() => {
                self.state.cursor += self.state.composer[self.state.cursor..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(0);
            }
            KeyCode::PageUp => self.state.scroll = self.state.scroll.saturating_add(5),
            KeyCode::PageDown => self.state.scroll = self.state.scroll.saturating_sub(5),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.state.composer.insert(self.state.cursor, '\n');
                self.state.cursor += 1;
            }
            KeyCode::Enter => self.submit_composer()?,
            _ => {}
        }
        Ok(())
    }

    fn submit_composer(&mut self) -> Result<(), String> {
        let text = std::mem::take(&mut self.state.composer);
        self.state.cursor = 0;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        if trimmed.starts_with('/') {
            match parse_command(trimmed)? {
                Command::Agents => self.state.open_agents(),
                Command::Model(profile) => {
                    self.state.status = profile.map_or_else(
                        || "select model".into(),
                        |profile| format!("model profile: {profile}"),
                    )
                }
                Command::Connect => self.state.overlay = Overlay::Onboarding,
                Command::Sessions => self.state.status = "session browser".into(),
                Command::Resume(session) => self.state.status = format!("resume {session}"),
                Command::New => self.state.status = "new session".into(),
                Command::Context => self.state.status = "context report".into(),
                Command::Compact => self.state.queue_command(EngineCommand::Compact),
                Command::Mode(mode) => {
                    self.state.mode = mode;
                    self.state.queue_command(EngineCommand::SetMode(mode));
                }
            }
        } else {
            self.state.push_user(trimmed);
            let explicit_delegation = delegation_intent(trimmed);
            self.state.queue_command(EngineCommand::SubmitTurn {
                text: trimmed.to_owned(),
                explicit_delegation,
            });
            self.state.status = "thinking".into();
        }
        Ok(())
    }

    fn handle_onboarding_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.state.onboarding.select_previous(),
            KeyCode::Down => self.state.onboarding.select_next(),
            KeyCode::Enter => {
                self.state.status = format!("selected {}", self.state.onboarding.selected_option());
                self.state.overlay = Overlay::None;
            }
            KeyCode::Esc => self.state.overlay = Overlay::None,
            _ => {}
        }
    }

    fn handle_approval_key(&mut self, key: KeyEvent) {
        match (self.state.overlay, key.code) {
            (Overlay::Approval, KeyCode::Char('a')) => {
                self.state.resolve_approval(ApprovalResponse::ApproveOnce)
            }
            (Overlay::Approval, KeyCode::Char('d')) => {
                self.state.resolve_approval(ApprovalResponse::Deny)
            }
            (Overlay::Approval, KeyCode::Char('e')) => self.state.begin_approval_edit(),
            (Overlay::ApprovalEdit, KeyCode::Char(character)) => {
                if let Some(approval) = &mut self.state.approval {
                    approval.editor.push(character);
                    if let Ok(arguments) = serde_json::from_str(&approval.editor) {
                        approval.arguments = arguments;
                    }
                }
            }
            (Overlay::ApprovalEdit, KeyCode::Backspace) => {
                if let Some(approval) = &mut self.state.approval {
                    approval.editor.pop();
                    if let Ok(arguments) = serde_json::from_str(&approval.editor) {
                        approval.arguments = arguments;
                    }
                }
            }
            (Overlay::ApprovalEdit, KeyCode::Enter) => {
                let _ = self.state.submit_approval_edit();
            }
            (_, KeyCode::Esc) => self.state.close_overlay(),
            _ => {}
        }
    }

    fn handle_agents_key(&mut self, key: KeyEvent) {
        match (self.state.overlay, key.code) {
            (Overlay::Agents, KeyCode::Up) => self.state.select_previous_agent(),
            (Overlay::Agents, KeyCode::Down) => self.state.select_next_agent(),
            (Overlay::Agents, KeyCode::Enter) => self.state.inspect_selected_agent(),
            (Overlay::Agents | Overlay::AgentInspect, KeyCode::Char('m')) => {
                self.state.begin_agent_message()
            }
            (Overlay::AgentMessage, KeyCode::Char(character)) => {
                self.state.agent_message.push(character);
            }
            (Overlay::AgentMessage, KeyCode::Backspace) => {
                self.state.agent_message.pop();
            }
            (Overlay::AgentMessage, KeyCode::Enter) => self.state.submit_agent_message(),
            (Overlay::Agents | Overlay::AgentInspect, KeyCode::Char('x')) => {
                self.state.request_agent_cancel()
            }
            (Overlay::ConfirmAgentCancel, KeyCode::Char('y')) => self.state.confirm_agent_cancel(),
            (Overlay::ConfirmAgentCancel, KeyCode::Char('n')) | (_, KeyCode::Esc) => {
                self.state.close_overlay()
            }
            _ => {}
        }
    }
}

fn delegation_intent(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    [
        "sub-agent",
        "subagent",
        "delegate",
        "parallelize",
        "parallelise",
        "use agents",
        "split this between",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

pub async fn run(args: Args) -> Result<(), String> {
    App::bootstrap(
        &args,
        std::env::current_dir().map_err(|error| error.to_string())?,
    )?
    .run()
    .await
}

pub fn prompt_bundle() -> String {
    let schemas = serde_json::json!([
        {"name":"read","parameters":{"type":"object","properties":{"paths":{"type":"array"},"start_line":{"type":"integer"},"max_lines":{"type":"integer"}},"required":["paths"],"additionalProperties":false}},
        {"name":"write","parameters":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"},"patch":{"type":"string"},"expected_hash":{"type":"string"}},"required":["path"],"additionalProperties":false}},
        {"name":"bash","parameters":{"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"},"timeout_ms":{"type":"integer"}},"required":["command","cwd","timeout_ms"],"additionalProperties":false}},
        {"name":"web-search","parameters":{"oneOf":[{"type":"object","properties":{"operation":{"const":"search"},"query":{"type":"string"},"limit":{"type":"integer"},"contains_workspace_data":{"type":"boolean"}},"required":["operation","query","limit","contains_workspace_data"],"additionalProperties":false},{"type":"object","properties":{"operation":{"const":"open"},"url":{"type":"string"}},"required":["operation","url"],"additionalProperties":false}]}}
    ]);
    format!("{}\n{}", kurama_core::prompts::SYSTEM_PROMPT, schemas)
}

pub async fn run_with<B>(
    mut app: App,
    terminal: &mut Terminal<B>,
    mut input: mpsc::Receiver<Event>,
    mut runtime_events: mpsc::Receiver<RuntimeEvent>,
) -> Result<App, String>
where
    B: Backend,
{
    terminal
        .draw(|frame| render(frame, &app.state))
        .map_err(|error| error.to_string())?;

    loop {
        tokio::select! {
            event = input.recv() => {
                let Some(event) = event else {
                    if runtime_events.is_closed() {
                        break;
                    }
                    continue;
                };
                if app.handle_event(event)? {
                    break;
                }
            }
            event = runtime_events.recv() => {
                let Some(event) = event else {
                    if input.is_closed() {
                        break;
                    }
                    continue;
                };
                app.state.apply_runtime_event(event);
            }
        }
        terminal
            .draw(|frame| render(frame, &app.state))
            .map_err(|error| error.to_string())?;
    }
    Ok(app)
}
