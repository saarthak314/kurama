use std::borrow::Cow;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

use super::{
    TuiState,
    theme::{ACCENT, DIM},
    transcript::{sanitize_terminal_text, truncate_display},
};

pub(crate) const MAX_COMMAND_PALETTE_ROWS: usize = 8;

pub(crate) enum Palette<'a> {
    History(Vec<&'a str>),
    Files(Vec<&'a str>),
    Commands(Vec<crate::commands::CommandSpec>),
}

impl<'a> Palette<'a> {
    pub(crate) fn prepare(state: &'a TuiState) -> Self {
        if state.history_search_active() {
            return Self::History(state.history_matches());
        }
        let files = state.file_suggestions();
        if files.is_empty() {
            Self::Commands(state.command_suggestions())
        } else {
            Self::Files(files)
        }
    }

    pub(crate) fn height(&self, available: u16) -> u16 {
        let count = match self {
            Self::History(rows) => rows.len() + 1,
            Self::Files(rows) => rows.len(),
            Self::Commands(rows) => rows.len(),
        };
        count
            .min(MAX_COMMAND_PALETTE_ROWS + 1)
            .min(available as usize) as u16
    }
}

pub(crate) fn command_palette_height(state: &TuiState, available: u16) -> u16 {
    if available == 0 {
        return 0;
    }
    Palette::prepare(state).height(available)
}

pub(crate) fn render_command_palette(
    frame: &mut Frame<'_>,
    state: &TuiState,
    palette: &Palette<'_>,
    area: Rect,
) {
    if area.is_empty() {
        return;
    }
    let suggestions = match palette {
        Palette::History(matches) => {
            render_history_palette(frame, state, matches, area);
            return;
        }
        Palette::Files(files) => {
            render_named_rows(frame, area, files, state.file_selection());
            return;
        }
        Palette::Commands(suggestions) => suggestions,
    };
    if suggestions.is_empty() {
        return;
    }
    let selected = state.command_selection().min(suggestions.len() - 1);
    let visible = suggestions.len().min(area.height as usize);
    let start = visible_start(selected, suggestions.len(), visible);
    let gutter = if area.width >= 4 { 2 } else { 0 };
    let content_width = area.width as usize - gutter;
    let command_width = suggestions
        .iter()
        .skip(start)
        .take(visible)
        .map(|spec| spec.name.len() + 1)
        .max()
        .unwrap_or(0)
        .min(content_width);
    let lines = suggestions
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, spec)| {
            let active = index == selected;
            let style = selection_style(active);
            let name = truncate_display(&format!("/{}", spec.name), command_width);
            let used = Span::raw(name.as_str()).width();
            let mut spans = vec![
                Span::styled(marker(active, gutter), style),
                Span::styled(name, style),
            ];
            let description_width = content_width.saturating_sub(command_width + 2);
            if description_width >= 8 {
                spans.push(Span::raw(
                    " ".repeat(command_width.saturating_sub(used) + 2),
                ));
                spans.push(Span::styled(
                    truncate_display(spec.description, description_width),
                    Style::default().fg(DIM),
                ));
            }
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    render_rows(frame, area, lines);
}

fn render_history_palette(frame: &mut Frame<'_>, state: &TuiState, matches: &[&str], area: Rect) {
    if area.height == 1 && !matches.is_empty() {
        render_named_rows(frame, area, matches, state.history_search_selection());
        return;
    }
    let title = if matches.is_empty() {
        format!(
            "history · no matches  {}",
            single_line(state.history_search_query().unwrap_or(""))
        )
    } else {
        format!(
            "history  {}",
            single_line(state.history_search_query().unwrap_or(""))
        )
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Span::styled(
            truncate_display(&title, area.width as usize),
            Style::default().fg(DIM),
        )),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if area.height > 1 {
        render_named_rows(
            frame,
            Rect::new(area.x, area.y + 1, area.width, area.height - 1),
            matches,
            state.history_search_selection(),
        );
    }
}

fn render_named_rows<T: AsRef<str>>(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[T],
    selected: usize,
) {
    if rows.is_empty() || area.is_empty() {
        return;
    }
    let selected = selected.min(rows.len() - 1);
    let visible = rows.len().min(area.height as usize);
    let start = visible_start(selected, rows.len(), visible);
    let gutter = if area.width >= 4 { 2 } else { 0 };
    let lines = rows
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, name)| {
            let active = index == selected;
            let style = selection_style(active);
            Line::from(vec![
                Span::styled(marker(active, gutter), style),
                Span::styled(
                    truncate_display(&single_line(name.as_ref()), area.width as usize - gutter),
                    style,
                ),
            ])
        })
        .collect::<Vec<_>>();
    render_rows(frame, area, lines);
}

fn render_rows(frame: &mut Frame<'_>, area: Rect, lines: Vec<Line<'_>>) {
    let height = lines.len() as u16;
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(area.x, area.bottom() - height, area.width, height),
    );
}

fn visible_start(selected: usize, count: usize, visible: usize) -> usize {
    selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(count.saturating_sub(visible))
}

fn marker(active: bool, gutter: usize) -> &'static str {
    if gutter == 0 {
        ""
    } else if active {
        "› "
    } else {
        "  "
    }
}

fn selection_style(active: bool) -> Style {
    if active {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn single_line(text: &str) -> Cow<'_, str> {
    let text = sanitize_terminal_text(text);
    if text.contains(['\n', '\r', '\t']) {
        Cow::Owned(text.replace(['\n', '\r', '\t'], " "))
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    #[test]
    fn one_row_palette_shows_selected_item_without_multiline_spill() {
        let rows = ["first", "selected\npath"];
        for width in [1, 20] {
            let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
            terminal
                .draw(|frame| render_named_rows(frame, frame.area(), &rows, usize::MAX))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let text = buffer
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            if width == 1 {
                assert_eq!(buffer[(0, 0)].fg, ACCENT);
                assert!(!text.trim().is_empty());
            } else {
                assert!(text.contains("selected path"));
                assert!(!text.contains("first"));
            }
        }
    }
}
