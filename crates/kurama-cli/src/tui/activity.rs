use std::time::{Duration, Instant};

use ratatui::{
    style::Style,
    text::{Line, Span},
};

use super::{
    ActivityState,
    theme::{ACCENT, DIM},
    transcript::truncate_display,
};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const INTERRUPT_HINT: &str = "esc to interrupt";

pub fn activity_line(
    activity: &ActivityState,
    width: usize,
    now: Instant,
) -> Option<Line<'static>> {
    if width == 0 {
        return None;
    }

    let (started_at, verb, detail) = match activity {
        ActivityState::Thinking { started_at } => (*started_at, "Thinking", None),
        ActivityState::Working { label, started_at } => {
            (*started_at, "Working", Some(label.as_str()))
        }
        ActivityState::RunningTool { name, started_at } => {
            (*started_at, "Running", Some(name.as_str()))
        }
        ActivityState::Idle | ActivityState::AwaitingApproval | ActivityState::Interrupted => {
            return None;
        }
    };

    let elapsed = now.saturating_duration_since(started_at);
    let spinner =
        SPINNER_FRAMES[(elapsed.as_millis() / 100 % SPINNER_FRAMES.len() as u128) as usize];
    let elapsed = compact_elapsed(elapsed.as_secs());
    let action = detail.map_or_else(|| verb.to_owned(), |detail| format!("{verb} {detail}"));
    let full_suffix = format!(" ({elapsed} • {INTERRUPT_HINT})");
    if display_width(&action).saturating_add(display_width(&full_suffix)) + 2 <= width {
        return Some(styled_activity(spinner, action, full_suffix));
    }

    let elapsed_suffix = format!(" ({elapsed})");
    if display_width(&action).saturating_add(display_width(&elapsed_suffix)) + 2 <= width {
        return Some(styled_activity(spinner, action, elapsed_suffix));
    }

    let available = width.saturating_sub(2);
    let action = truncate_display(&action, available);
    Some(Line::from(vec![
        Span::styled(format!("{spinner} "), Style::default().fg(ACCENT)),
        Span::raw(action),
    ]))
}

pub fn worked_for_line(elapsed: Duration, width: usize) -> Option<Line<'static>> {
    if width == 0 {
        return None;
    }

    let label = format!("Worked for {}", compact_elapsed(elapsed.as_secs()));
    let style = Style::default().fg(DIM);
    let decorated_width = display_width(&label).saturating_add(3);
    if decorated_width > width {
        return Some(Line::from(Span::styled(
            truncate_display(&label, width),
            style,
        )));
    }

    Some(Line::from(vec![
        Span::styled("─ ", style),
        Span::styled(label, style),
        Span::styled(
            format!(" {}", "─".repeat(width.saturating_sub(decorated_width))),
            style,
        ),
    ]))
}

fn styled_activity(spinner: &str, action: String, suffix: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{spinner} "), Style::default().fg(ACCENT)),
        Span::raw(action),
        Span::styled(suffix, Style::default().fg(DIM)),
    ])
}

fn display_width(value: &str) -> usize {
    Line::from(value).width()
}

fn compact_elapsed(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds < 3_600 {
        return format!("{}m {:02}s", seconds / 60, seconds % 60);
    }
    format!(
        "{}h {:02}m {:02}s",
        seconds / 3_600,
        seconds % 3_600 / 60,
        seconds % 60
    )
}
