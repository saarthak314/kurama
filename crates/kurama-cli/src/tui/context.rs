use std::cell::Cell;

use crossterm::event::KeyCode;
use kurama_protocol::runtime::ContextInspection;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear},
};

use super::{
    theme::{ACCENT, RED, TEXT},
    transcript::{for_each_wrapped_line, sanitize_terminal_text, truncate_display},
};

#[derive(Default)]
pub(crate) struct ContextView {
    pub(crate) loading: bool,
    lines: Vec<(String, bool)>,
    scroll: usize,
    page: Cell<usize>,
    max_scroll: Cell<usize>,
}

impl ContextView {
    pub(crate) fn update(&mut self, inspection: ContextInspection) {
        self.loading = false;
        self.scroll = 0;
        let mut lines = vec![
            (
                "Estimates of the next assembled request, not provider usage.".into(),
                false,
            ),
            (
                format!("Input limit: {} tokens", inspection.max_input_tokens),
                false,
            ),
            (
                format!(
                    "Reserved output: {} tokens",
                    inspection.reserved_output_tokens
                ),
                false,
            ),
            (
                format!("Usable input: {} tokens", inspection.usable_tokens),
                false,
            ),
            (
                format!("Estimated input: {} tokens", inspection.estimated_tokens),
                false,
            ),
            (String::new(), false),
            ("Estimated input by category".into(), false),
        ];
        for category in inspection.categories {
            lines.push((
                format!("  {}: {} tokens", category.name, category.tokens),
                false,
            ));
        }
        lines.push((String::new(), false));
        lines.push((
            format!(
                "Completed turns: {} total · {} included recent · {} omitted",
                inspection.total_completed_turns,
                inspection.included_recent_turns,
                inspection.omitted_turns,
            ),
            false,
        ));
        lines.push((
            match inspection.summary_covered_through_sequence {
                Some(sequence) => format!("Existing summary covers through event {sequence}"),
                None => "Existing summary: none".into(),
            },
            false,
        ));
        match inspection.compaction {
            Some(preview) => {
                lines.push((
                    format!(
                        "Compaction candidate: {} events through event {}",
                        preview.event_count, preview.covered_through_sequence
                    ),
                    false,
                ));
                lines.push((
                    format!(
                        "Compaction request estimate: {} tokens · {} its request budget",
                        preview.estimated_tokens,
                        if preview.fits_budget {
                            "fits"
                        } else {
                            "exceeds"
                        }
                    ),
                    !preview.fits_budget,
                ));
            }
            None => lines.push(("Compaction candidate: none".into(), false)),
        }
        if let Some(error) = inspection.assembly_error {
            lines.push((format!("Assembly error: {error}"), true));
        }
        lines.push((String::new(), false));
        lines.push((
            "Inspection does not compact or contact the model. /compact is a separate action."
                .into(),
            false,
        ));
        self.lines = lines
            .into_iter()
            .map(|(text, warning)| (sanitize_terminal_text(&text).into_owned(), warning))
            .collect();
    }

    pub(crate) fn handle_key(&mut self, key: KeyCode) {
        let page = self.page.get().max(1);
        self.scroll = match key {
            KeyCode::Up => self.scroll.saturating_sub(1),
            KeyCode::Down => self.scroll.saturating_add(1),
            KeyCode::PageUp => self.scroll.saturating_sub(page),
            KeyCode::PageDown => self.scroll.saturating_add(page),
            KeyCode::Home => 0,
            KeyCode::End => self.max_scroll.get(),
            _ => self.scroll,
        }
        .min(self.max_scroll.get());
    }
}

pub(crate) fn render_context(frame: &mut Frame<'_>, view: &ContextView, area: Rect) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .title(Span::styled(
            if view.loading {
                " /context · loading "
            } else {
                " /context · request estimates "
            },
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    let inner = if area.height >= 4 {
        block.inner(area)
    } else {
        area
    };
    if area.height >= 4 {
        frame.render_widget(block, area);
    }
    if inner.is_empty() {
        return;
    }
    let footer_height = u16::from(inner.height > 1);
    let height = usize::from(inner.height - footer_height);
    let width = usize::from(inner.width);
    let rows = |visit: &mut dyn FnMut(&str, bool)| {
        if view.lines.is_empty() {
            visit(
                if view.loading {
                    "Assembling context inspection…"
                } else {
                    "No inspection yet. Press r to refresh."
                },
                false,
            );
        } else {
            for (text, warning) in &view.lines {
                for_each_wrapped_line(text, width, |row| visit(row, *warning));
            }
        }
    };
    let mut count = 0_usize;
    rows(&mut |_, _| count += 1);
    let max_scroll = count.saturating_sub(height);
    view.max_scroll.set(max_scroll);
    view.page.set(height.saturating_sub(1).max(1));
    let start = view.scroll.min(max_scroll);
    let mut row = 0;
    rows(&mut |text, warning| {
        if row >= start && row < start + height {
            frame.render_widget(
                Line::styled(text, Style::default().fg(if warning { RED } else { TEXT })),
                Rect::new(inner.x, inner.y + (row - start) as u16, inner.width, 1),
            );
        }
        row += 1;
    });
    if footer_height > 0 {
        let controls = if inner.width >= 49 {
            "↑↓/PgUp/PgDn scroll · r refresh · Esc close"
        } else {
            "↑↓ scroll · r refresh · Esc close"
        };
        frame.render_widget(
            Line::styled(
                truncate_display(controls, width),
                Style::default().fg(ACCENT),
            ),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }
}
