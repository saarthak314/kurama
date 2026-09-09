use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

use super::{
    TuiState,
    theme::{ACCENT, DIM, TEXT},
    transcript::truncate_display,
};

pub(crate) const MAX_COMMAND_PALETTE_ROWS: usize = 8;

pub(crate) fn command_palette_height(state: &TuiState, available: u16) -> u16 {
    let count = if state.history_search_active() {
        state.history_matches().len().saturating_add(1)
    } else if !state.file_suggestions().is_empty() {
        state.file_suggestions().len()
    } else {
        state.command_suggestions().len()
    };
    if count == 0 {
        0
    } else {
        count
            .min(MAX_COMMAND_PALETTE_ROWS.saturating_add(1))
            .min(available as usize) as u16
    }
}

pub(crate) fn render_command_palette(frame: &mut Frame<'_>, state: &TuiState, area: Rect) {
    if area.is_empty() {
        return;
    }
    if state.history_search_active() {
        render_history_palette(frame, state, area);
        return;
    }
    let files = state.file_suggestions();
    if !files.is_empty() {
        render_named_rows(
            frame,
            area,
            &files
                .iter()
                .map(|path| (path.as_str(), ""))
                .collect::<Vec<_>>(),
            state.file_selection(),
        );
        return;
    }
    let suggestions = state.command_suggestions();
    if suggestions.is_empty() {
        return;
    }

    let selected = state
        .command_selection()
        .min(suggestions.len().saturating_sub(1));
    let visible = suggestions.len().min(area.height as usize);
    let start = selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(suggestions.len().saturating_sub(visible));
    let command_width = suggestions
        .iter()
        .skip(start)
        .take(visible)
        .map(|spec| spec.name.len().saturating_add(1))
        .max()
        .unwrap_or(0);
    let lines = suggestions
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, spec)| {
            let is_selected = index == selected;
            let marker = if is_selected { "› " } else { "  " };
            let name_width = command_width.saturating_sub(1);
            let command = format!("/{:<name_width$}", spec.name);
            let used = 2_usize.saturating_add(command_width);
            let description_width = (area.width as usize).saturating_sub(used.saturating_add(2));
            let description = if description_width >= 8 {
                truncate_display(spec.description, description_width)
            } else {
                String::new()
            };
            let mut spans = vec![
                Span::styled(
                    marker,
                    Style::default().fg(if is_selected { ACCENT } else { TEXT }),
                ),
                Span::styled(
                    command,
                    Style::default()
                        .fg(if is_selected { ACCENT } else { TEXT })
                        .add_modifier(if is_selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ];
            if !description.is_empty() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(description, Style::default().fg(DIM)));
            }
            Line::from(spans)
        })
        .collect::<Vec<_>>();

    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(
            area.x,
            area.bottom().saturating_sub(visible as u16),
            area.width,
            visible as u16,
        ),
    );
}

fn render_history_palette(frame: &mut Frame<'_>, state: &TuiState, area: Rect) {
    if area.is_empty() {
        return;
    }
    let title = format!("history  {}", state.history_search_query().unwrap_or(""));
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate_display(&title, area.width as usize),
            Style::default().fg(DIM),
        ))),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if area.height <= 1 {
        return;
    }
    let matches = state
        .history_matches()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let rows = matches
        .iter()
        .map(|prompt| (prompt.as_str(), ""))
        .collect::<Vec<_>>();
    render_named_rows(
        frame,
        Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(1),
        ),
        &rows,
        state.history_search_selection(),
    );
}

fn render_named_rows(frame: &mut Frame<'_>, area: Rect, rows: &[(&str, &str)], selected: usize) {
    if rows.is_empty() || area.is_empty() {
        return;
    }
    let selected = selected.min(rows.len().saturating_sub(1));
    let visible = rows.len().min(area.height as usize);
    let start = selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(rows.len().saturating_sub(visible));
    let lines = rows
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, (name, description))| {
            let is_selected = index == selected;
            let marker = if is_selected { "› " } else { "  " };
            let mut spans = vec![
                Span::styled(
                    marker,
                    Style::default().fg(if is_selected { ACCENT } else { TEXT }),
                ),
                Span::styled(
                    truncate_display(name, area.width.saturating_sub(2) as usize),
                    Style::default()
                        .fg(if is_selected { ACCENT } else { TEXT })
                        .add_modifier(if is_selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ];
            if !description.is_empty() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(*description, Style::default().fg(DIM)));
            }
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(
            area.x,
            area.bottom().saturating_sub(visible as u16),
            area.width,
            visible as u16,
        ),
    );
}
