use std::{io, thread};

use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event},
    execute,
    terminal::{
        Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
        LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    },
};
use tokio::sync::mpsc;
use unicode_segmentation::GraphemeCursor;

pub(crate) fn previous_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let original = cursor.min(text.len());
    let mut cursor = original;
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let mut boundary = GraphemeCursor::new(cursor, text.len(), true);
    if cursor < original && boundary.is_boundary(text, 0) == Ok(true) {
        return cursor;
    }
    boundary.prev_boundary(text, 0).ok().flatten().unwrap_or(0)
}

pub(crate) fn next_grapheme_boundary(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    GraphemeCursor::new(cursor, text.len(), true)
        .next_boundary(text, 0)
        .ok()
        .flatten()
        .unwrap_or(text.len())
}

pub(crate) fn grapheme_boundary_at_or_after(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while !text.is_char_boundary(cursor) {
        cursor += 1;
    }
    let mut boundary = GraphemeCursor::new(cursor, text.len(), true);
    if boundary.is_boundary(text, 0) == Ok(true) {
        cursor
    } else {
        boundary
            .next_boundary(text, 0)
            .ok()
            .flatten()
            .unwrap_or(text.len())
    }
}

pub(crate) fn grapheme_display_width(grapheme: &str, column: usize) -> usize {
    if grapheme == "\t" {
        4 - column % 4
    } else {
        ratatui::text::Span::raw(grapheme).width()
    }
}

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
        EnterAlternateScreen,
        Clear(ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
        crossterm::cursor::Hide,
        EnableBracketedPaste,
        // Report motion only while a button is held, so dragging selects text.
        crossterm::style::Print("\x1b[?1002h\x1b[?1006h"),
        DisableLineWrap
    )
}

fn write_exit_commands<W: io::Write>(mut writer: W) -> io::Result<()> {
    execute!(
        writer,
        EnableLineWrap,
        crossterm::style::Print("\x1b[?1006l\x1b[?1002l"),
        DisableBracketedPaste,
        LeaveAlternateScreen,
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
    fn grapheme_boundaries_keep_combining_sequences_and_joined_emoji_whole() {
        let text = "Ae\u{301}👩‍👩‍👧‍👦界";
        let stops = [0, 1, "Ae\u{301}".len(), "Ae\u{301}👩‍👩‍👧‍👦".len(), text.len()];
        for pair in stops.windows(2) {
            assert_eq!(super::next_grapheme_boundary(text, pair[0]), pair[1]);
            assert_eq!(super::previous_grapheme_boundary(text, pair[1]), pair[0]);
            for cursor in pair[0] + 1..pair[1] {
                assert_eq!(super::previous_grapheme_boundary(text, cursor), pair[0]);
                assert_eq!(super::next_grapheme_boundary(text, cursor), pair[1]);
            }
        }
        assert_eq!(super::previous_grapheme_boundary("", usize::MAX), 0);
        assert_eq!(super::next_grapheme_boundary("", usize::MAX), 0);
    }

    #[test]
    fn terminal_commands_own_and_restore_fullscreen_lifecycle() {
        let mut enter = Vec::new();
        write_enter_commands(&mut enter).expect("enter commands");
        let mut exit = Vec::new();
        write_exit_commands(&mut exit).expect("exit commands");
        let enter = String::from_utf8(enter).expect("ANSI entry commands");
        let exit = String::from_utf8(exit).expect("ANSI exit commands");

        assert!(
            enter.starts_with("\x1b[?1049h\x1b[2J\x1b[1;1H"),
            "clear and home only after saving the primary screen"
        );
        assert!(enter.contains("\x1b[?25l"));
        assert!(enter.contains("\x1b[?2004h"));
        assert!(enter.contains("\x1b[?7l"));
        assert!(exit.contains("\x1b[?7h"));
        assert!(exit.contains("\x1b[?2004l"));
        for mode in [1002, 1006] {
            assert_eq!(enter.matches(&format!("\x1b[?{mode}h")).count(), 1);
            assert_eq!(exit.matches(&format!("\x1b[?{mode}l")).count(), 1);
        }
        for mode in [1000, 1003, 1015] {
            assert!(!enter.contains(&format!("\x1b[?{mode}h")));
        }
        assert_eq!(enter.matches("\x1b[?1049h").count(), 1);
        assert_eq!(exit.matches("\x1b[?1049l").count(), 1);
        assert!(
            exit.ends_with("\x1b[?1049l\x1b[?25h"),
            "restore the primary screen before showing its cursor"
        );
        assert!(!enter.contains("\x1b[3J") && !exit.contains("\x1b[3J"));
        assert!(
            !enter.contains("\x1b[6n"),
            "startup never waits for a CPR reply"
        );
    }
}
