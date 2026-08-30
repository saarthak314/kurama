use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

use super::{TuiState, transcript::truncate_display};

pub(crate) const MAX_COMMAND_PALETTE_ROWS: usize = 8;

pub(crate) fn command_palette_height(state: &TuiState, available: u16) -> u16 {
    if state.command_suggestions().is_empty() {
        0
    } else {
        MAX_COMMAND_PALETTE_ROWS.min(available as usize) as u16
    }
}

pub(crate) fn render_command_palette(frame: &mut Frame<'_>, state: &TuiState, area: Rect) {
    if area.is_empty() {
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
                    Style::default().fg(if is_selected {
                        Color::Cyan
                    } else {
                        Color::Reset
                    }),
                ),
                Span::styled(
                    command,
                    Style::default()
                        .fg(if is_selected {
                            Color::Cyan
                        } else {
                            Color::Reset
                        })
                        .add_modifier(if is_selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ];
            if !description.is_empty() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    description,
                    Style::default().add_modifier(Modifier::DIM),
                ));
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
