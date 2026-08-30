use std::{io, thread};

use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use tokio::sync::mpsc;

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), crossterm::cursor::Hide, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), DisableBracketedPaste, crossterm::cursor::Show);
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, crossterm::cursor::Show);
    }
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
