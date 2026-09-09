use std::{io, thread};

use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event},
    execute,
    terminal::{
        DisableLineWrap, EnableLineWrap, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use tokio::sync::mpsc;

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = write_enter_commands(io::stdout()) {
            let _ = disable_raw_mode();
            let _ = write_exit_commands(io::stdout());
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = write_exit_commands(io::stdout());
    }
}

fn write_enter_commands<W: io::Write>(mut writer: W) -> io::Result<()> {
    execute!(
        writer,
        crossterm::cursor::Hide,
        EnableBracketedPaste,
        DisableLineWrap
    )
}

fn write_exit_commands<W: io::Write>(mut writer: W) -> io::Result<()> {
    execute!(
        writer,
        LeaveAlternateScreen,
        EnableLineWrap,
        DisableBracketedPaste,
        crossterm::cursor::Show
    )
}

pub fn spawn_input_thread(capacity: usize) -> mpsc::Receiver<Event> {
    let (sender, receiver) = mpsc::channel(capacity);
    thread::Builder::new()
        .name("kurama-terminal-input".into())
        .spawn(move || {
            while let Ok(event) = event::read() {
                if sender.blocking_send(event).is_err() {
                    break;
                }
            }
        })
        .expect("spawn terminal input thread");
    receiver
}

#[cfg(test)]
mod tests {
    use super::{write_enter_commands, write_exit_commands};

    #[test]
    fn terminal_commands_disable_and_restore_line_wrapping() {
        let mut enter = Vec::new();
        write_enter_commands(&mut enter).expect("enter commands");
        let mut exit = Vec::new();
        write_exit_commands(&mut exit).expect("exit commands");

        assert!(enter.windows(5).any(|window| window == b"\x1b[?7l"));
        assert!(exit.windows(5).any(|window| window == b"\x1b[?7h"));
        assert!(
            exit.windows(8).any(|window| window == b"\x1b[?1049l"),
            "leave alternate screen on exit"
        );
    }
}
