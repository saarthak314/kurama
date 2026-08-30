use std::time::Instant;

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::{ActivityState, transcript::truncate_display};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const INTERRUPT_HINT: &str = "Esc to interrupt";

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
        SPINNER_FRAMES[(elapsed.as_millis() / 32 % SPINNER_FRAMES.len() as u128) as usize];
    let elapsed = compact_elapsed(elapsed.as_secs());
    let suffix = format!(" · {elapsed}");
    let prefix = format!("{spinner} {verb}");
    let action = if let Some(detail) = detail {
        let available = width.saturating_sub(Line::from(format!("{prefix}  {suffix}")).width());
        let detail = truncate_display(detail, available);
        if detail.is_empty() {
            prefix
        } else {
            format!("{prefix} {detail}")
        }
    } else {
        prefix
    };
    let base = format!("{action}{suffix}");

    if Line::from(base.as_str()).width() > width {
        return Some(Line::from(Span::styled(
            truncate_display(&base, width),
            Style::default().fg(Color::Cyan),
        )));
    }

    let mut spans = vec![
        Span::styled(format!("{spinner} "), Style::default().fg(Color::Cyan)),
        Span::raw(action.trim_start_matches(spinner).trim_start().to_owned()),
        Span::styled(suffix, Style::default().add_modifier(Modifier::DIM)),
    ];
    let base_width = Line::from(spans.clone()).width();
    let hint = format!("  {INTERRUPT_HINT}");
    if base_width.saturating_add(Line::from(hint.as_str()).width()) <= width {
        spans.push(Span::styled(
            hint,
            Style::default().add_modifier(Modifier::DIM),
        ));
    }

    Some(Line::from(spans))
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
