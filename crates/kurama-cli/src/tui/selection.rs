use std::sync::Arc;

use ratatui::{Frame, layout::Rect, style::Color, text::Span};
use unicode_segmentation::UnicodeSegmentation;

use super::{theme::ACCENT, transcript::TranscriptLine};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TranscriptPoint {
    pub(crate) row: usize,
    pub(crate) column: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptSelection {
    pub(crate) anchor: TranscriptPoint,
    pub(crate) focus: TranscriptPoint,
    pub(crate) dragging: bool,
    pub(crate) dragged: bool,
    pub(crate) pressed_link: Option<Arc<str>>,
    pub(crate) copied: bool,
}

impl TranscriptSelection {
    pub(crate) fn new(anchor: TranscriptPoint, pressed_link: Option<Arc<str>>) -> Self {
        Self {
            anchor,
            focus: anchor,
            dragging: true,
            dragged: false,
            pressed_link,
            copied: false,
        }
    }

    pub(crate) fn update(&mut self, point: TranscriptPoint) {
        self.focus = point;
        self.dragged = true;
    }

    pub(crate) fn finish(&mut self, point: TranscriptPoint) {
        self.focus = point;
        self.dragging = false;
    }

    fn bounds(&self) -> (TranscriptPoint, TranscriptPoint) {
        (self.anchor.min(self.focus), self.anchor.max(self.focus))
    }

    pub(crate) fn text(&self, lines: &[TranscriptLine]) -> String {
        if !self.dragged {
            return String::new();
        }
        let (start, end) = self.bounds();
        let mut selected = String::new();
        let mut joined = String::new();
        let end_row = end.row.saturating_add(1).min(lines.len());
        for (row, line) in lines.iter().enumerate().take(end_row).skip(start.row) {
            if row > start.row {
                selected.push('\n');
            }
            // A grapheme may straddle a style boundary. Segment the visible row,
            // not its spans, and borrow the overwhelmingly common single-span row.
            let text = match line.text.spans.as_slice() {
                [] => "",
                [span] => span.content.as_ref(),
                spans => {
                    joined.clear();
                    for span in spans {
                        joined.push_str(&span.content);
                    }
                    joined.as_str()
                }
            };
            let first_column = if row == start.row { start.column } else { 0 };
            let last_column = if row == end.row {
                end.column
            } else {
                usize::MAX
            };
            if first_column == 0 && last_column == usize::MAX {
                selected.push_str(text);
                continue;
            }
            let mut column = 0_usize;
            for grapheme in text.graphemes(true) {
                if column > last_column {
                    break;
                }
                let next_column = column.saturating_add(Span::raw(grapheme).width());
                if next_column > first_column && next_column > column {
                    selected.push_str(grapheme);
                }
                column = next_column;
            }
        }
        selected
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, first_row: usize) {
        if !self.dragged {
            return;
        }
        let visible = area.intersection(frame.area());
        let (start, end) = self.bounds();
        let buffer = frame.buffer_mut();
        for y in visible.y..visible.bottom() {
            let Some(row) = first_row.checked_add(usize::from(y - area.y)) else {
                continue;
            };
            if row < start.row || row > end.row {
                continue;
            }
            let first_column = if row == start.row { start.column } else { 0 };
            let last_column = if row == end.row {
                end.column
            } else {
                usize::MAX
            };
            if first_column >= usize::from(visible.right().saturating_sub(area.x)) {
                continue;
            }
            let mut x = visible.x;
            while x < visible.right() && usize::from(x - area.x) <= last_column {
                let column = usize::from(x - area.x);
                let width = Span::raw(buffer[(x, y)].symbol()).width().max(1);
                let next_column = column.saturating_add(width);
                let next_x = (usize::from(x) + width).min(usize::from(visible.right())) as u16;
                // Include both cells of a wide glyph even if only its trailing
                // cell is selected; otherwise terminals paint half a character.
                if next_column > first_column {
                    for cell_x in x..next_x {
                        buffer[(cell_x, y)].set_fg(Color::Black).set_bg(ACCENT);
                    }
                }
                x = next_x;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Style, text::Line};

    use super::*;

    fn point(row: usize, column: usize) -> TranscriptPoint {
        TranscriptPoint { row, column }
    }

    fn drag(anchor: TranscriptPoint, focus: TranscriptPoint) -> TranscriptSelection {
        let mut selection = TranscriptSelection::new(anchor, None);
        selection.update(focus);
        selection.finish(focus);
        selection
    }

    fn render_selection(selection: &TranscriptSelection, first_row: usize) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(10, 5)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Line::styled("a界e\u{301}z", Style::default().fg(Color::Green)),
                    Rect::new(2, 1, 6, 1),
                );
                frame.render_widget(Line::raw("second"), Rect::new(2, 2, 6, 1));
                selection.render(frame, Rect::new(2, 1, 6, 2), first_row);
            })
            .unwrap()
            .buffer
            .clone()
    }

    #[test]
    fn copy_preserves_whitespace_and_graphemes_across_styles_in_both_directions() {
        let lines = [
            TranscriptLine::from(Line::from(vec![
                Span::raw(" a界e"),
                Span::styled("\u{301}👨‍👩‍👧‍👦 ", Style::default().fg(Color::Green)),
            ])),
            TranscriptLine::from(Line::raw("")),
            TranscriptLine::from(Line::raw("  last  ")),
        ];
        for (anchor, focus) in [(point(0, 3), point(2, 3)), (point(2, 3), point(0, 3))] {
            assert_eq!(drag(anchor, focus).text(&lines), "界e\u{301}👨‍👩‍👧‍👦 \n\n  la");
        }
        assert_eq!(drag(point(0, 4), point(0, 4)).text(&lines), "e\u{301}");
        assert_eq!(drag(point(0, 6), point(0, 6)).text(&lines), "👨‍👩‍👧‍👦");
    }

    #[test]
    fn copy_clamps_to_content_without_trimming_source_whitespace() {
        let lines = [
            TranscriptLine::from(Line::raw("  first  ")),
            TranscriptLine::from(Line::raw("last ")),
        ];
        assert_eq!(
            drag(point(0, 0), point(usize::MAX, usize::MAX)).text(&lines),
            "  first  \nlast "
        );
        assert_eq!(drag(point(0, 200), point(1, 1)).text(&lines), "\nla");
        assert!(drag(point(20, 0), point(30, 0)).text(&lines).is_empty());
        assert!(drag(point(0, 0), point(0, 1)).text(&[]).is_empty());
    }

    #[test]
    fn plain_click_does_not_copy_or_highlight_a_character() {
        let mut click = TranscriptSelection::new(point(7, 1), None);
        let before = render_selection(&click, 7);
        click.finish(point(7, 1));
        assert!(
            click
                .text(&[TranscriptLine::from(Line::raw("text"))])
                .is_empty()
        );
        assert_eq!(before, render_selection(&click, 7));
        assert!(before.content.iter().all(|cell| cell.bg == Color::Reset));
    }

    #[test]
    fn highlight_uses_absolute_rows_and_preserves_off_range_cells_and_symbols() {
        let first_row = usize::from(u16::MAX) + 10;
        let before = render_selection(&TranscriptSelection::new(point(0, 0), None), first_row);
        for (anchor, focus) in [
            (point(first_row, 2), point(first_row + 1, 1)),
            (point(first_row + 1, 1), point(first_row, 2)),
        ] {
            let highlighted = render_selection(&drag(anchor, focus), first_row);
            for y in 0..5 {
                for x in 0..10 {
                    let selected =
                        (y == 1 && (3..8).contains(&x)) || (y == 2 && (2..4).contains(&x));
                    let cell = &highlighted[(x, y)];
                    if selected {
                        assert_eq!(cell.fg, Color::Black);
                        assert_eq!(cell.bg, ACCENT);
                        assert_eq!(cell.symbol(), before[(x, y)].symbol());
                    } else {
                        assert_eq!(cell, &before[(x, y)]);
                    }
                }
            }
        }
    }

    #[test]
    fn inclusive_wide_cell_selection_highlights_the_whole_glyph_only() {
        let highlighted = render_selection(&drag(point(7, 2), point(7, 2)), 7);
        for x in 0..10 {
            assert_eq!(
                highlighted[(x, 1)].bg,
                if (3..5).contains(&x) {
                    ACCENT
                } else {
                    Color::Reset
                }
            );
        }
        assert_eq!(highlighted[(3, 1)].symbol(), "界");
    }

    #[test]
    fn highlight_clips_offscreen_rows_and_extreme_endpoints() {
        let mut terminal = Terminal::new(TestBackend::new(4, 2)).unwrap();
        terminal
            .draw(|frame| {
                drag(point(0, 0), point(usize::MAX, usize::MAX)).render(
                    frame,
                    Rect::new(2, 1, 20, 20),
                    100,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..2 {
            for x in 0..4 {
                assert_eq!(
                    buffer[(x, y)].bg,
                    if y == 1 && x >= 2 {
                        ACCENT
                    } else {
                        Color::Reset
                    }
                );
            }
        }
    }
}
