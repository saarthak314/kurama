use std::iter::Peekable;

use kurama_protocol::{policy::ExecutionMode, tool::Operation};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Padding, Paragraph},
};
use unicode_segmentation::{GraphemeIndices, UnicodeSegmentation};

use super::{
    ApprovalState, Overlay, TuiState,
    input::grapheme_display_width,
    selection::copied_feedback,
    theme::{ACCENT, AMBER, BORDER, DIM, RED},
    transcript::{sanitize_terminal_text, truncate_display},
};

const PROMPT: &str = "> ";
const PROMPT_WIDTH: usize = 2;
const MAX_COMPOSER_HEIGHT: usize = 8;
const MAX_APPROVAL_HEIGHT: usize = 14;
const CHROME_ROWS: u16 = 2;
const APPROVAL_CHOICES: [&str; 4] = ["a approve once", "s approve session", "d deny", "e edit"];

fn ruled(width: u16, height: u16) -> bool {
    width >= 4 && height >= 3
}

fn inner_width(width: u16) -> usize {
    width.saturating_sub(2).max(1) as usize
}

pub(crate) fn composer_height(state: &TuiState, width: u16) -> u16 {
    let rows = EditorRows::new(&state.composer, composer_text_width(width as usize))
        .take(MAX_COMPOSER_HEIGHT)
        .count();
    rows as u16 + 2
}

pub(crate) fn approval_height(state: &TuiState, width: u16) -> u16 {
    if !matches!(state.overlay, Overlay::Approval | Overlay::ApprovalEdit) {
        return 0;
    }
    state.approval.as_ref().map_or(0, |approval| {
        let content_width = inner_width(width);
        let text_width = composer_text_width(content_width);
        let (rows, overhead) = if approval.editing {
            (
                EditorRows::new(&approval.editor, text_width)
                    .take(MAX_APPROVAL_HEIGHT)
                    .count(),
                3 + usize::from(approval.validation_error.is_some()),
            )
        } else {
            let detail = approval_detail(&approval.request.operation);
            let controls = if content_width >= 19
                && APPROVAL_CHOICES
                    .iter()
                    .map(|label| label.len())
                    .sum::<usize>()
                    + 6
                    > content_width
            {
                4
            } else {
                1
            };
            let summary_rows = EditorRows::new(&approval.request.summary, text_width)
                .take(MAX_APPROVAL_HEIGHT)
                .count();
            (
                EditorRows::new(&detail, text_width)
                    .take(MAX_APPROVAL_HEIGHT)
                    .count(),
                1 + controls + summary_rows,
            )
        };
        (rows + overhead).min(MAX_APPROVAL_HEIGHT) as u16 + CHROME_ROWS
    })
}

fn composer_content_area(area: Rect) -> Rect {
    if area.height >= 3 {
        Rect::new(area.x, area.y + 1, area.width, area.height - 2)
    } else {
        area
    }
}

pub(crate) fn composer_cursor_at(
    state: &TuiState,
    area: Rect,
    position: Position,
    clamp: bool,
) -> Option<usize> {
    let content = composer_content_area(area);
    if content.is_empty() || (!clamp && !content.contains(position)) {
        return None;
    }
    let position = Position::new(
        position.x.clamp(content.x, content.right() - 1),
        position.y.clamp(content.y, content.bottom() - 1),
    );
    let width = composer_text_width(content.width as usize);
    let gutter = composer_gutter(content.width as usize);
    let measured = editor_measure(&state.composer, state.cursor, width);
    let row = composer_view_start(state, &measured, content.height as usize)
        + usize::from(position.y - content.y);
    let Some(row) = EditorRows::new(&state.composer, width).nth(row) else {
        return Some(state.composer.len());
    };
    let wanted = usize::from(position.x - content.x).saturating_sub(gutter);
    let mut column = 0;
    for (offset, grapheme) in row.text.grapheme_indices(true) {
        let next = column + grapheme_display_width(grapheme, column).min(width);
        if wanted < next {
            return Some(row.start + offset);
        }
        column = next;
    }
    Some(row.start + row.text.len())
}

pub(crate) fn render_composer(
    frame: &mut Frame<'_>,
    state: &TuiState,
    area: Rect,
) -> Option<Position> {
    state.composer_inner_width.set(area.width);
    if area.is_empty() {
        return None;
    }
    if area.height >= 3 {
        frame.render_widget(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(Style::default().fg(BORDER)),
            area,
        );
    }
    let content = composer_content_area(area);
    let gutter = composer_gutter(content.width as usize);
    let width = composer_text_width(content.width as usize);
    let measured = editor_measure(&state.composer, state.cursor, width);
    let start = composer_view_start(state, &measured, content.height as usize);
    state.composer_scroll.set(start);
    let lines = EditorRows::new(&state.composer, width)
        .skip(start)
        .take(content.height as usize)
        .enumerate()
        .map(|(index, row)| {
            let prefix = if gutter == 0 {
                ""
            } else if start + index == 0 {
                if gutter == 1 { ">" } else { PROMPT }
            } else {
                &"  "[..gutter]
            };
            let text = if state.composer.is_empty() {
                truncate_display(state.composer_placeholder(), width.saturating_sub(1))
            } else {
                editor_row_text(row.text, width)
            };
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(ACCENT)),
                Span::styled(
                    text,
                    if state.composer.is_empty() {
                        Style::default().fg(DIM)
                    } else {
                        Style::default()
                    },
                ),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(Text::from(lines)), content);
    if let Some(range) = state
        .composer_selection
        .as_ref()
        .and_then(|selection| selection.range(&state.composer))
    {
        let buffer = frame.buffer_mut();
        for (visible_row, row) in EditorRows::new(&state.composer, width)
            .skip(start)
            .take(content.height as usize)
            .enumerate()
        {
            let mut column = 0;
            for (offset, grapheme) in row.text.grapheme_indices(true) {
                let cells = grapheme_display_width(grapheme, column).min(width);
                if row.start + offset < range.end
                    && row.start + offset + grapheme.len() > range.start
                {
                    for cell in column..column + cells {
                        let x = content.x as usize + gutter + cell;
                        if x < content.right() as usize {
                            buffer[(x as u16, content.y + visible_row as u16)]
                                .set_fg(ratatui::style::Color::Black)
                                .set_bg(ACCENT);
                        }
                    }
                }
                column += cells;
            }
        }
    }
    Some(Position::new(
        content.x + (gutter + measured.cursor_column).min(content.width as usize - 1) as u16,
        content.y
            + measured
                .cursor_row
                .saturating_sub(start)
                .min(content.height as usize - 1) as u16,
    ))
}

pub(crate) fn render_approval(
    frame: &mut Frame<'_>,
    state: &TuiState,
    area: Rect,
) -> Option<Position> {
    let approval = state.approval.as_ref()?;
    if area.is_empty() {
        return None;
    }
    let (content, origin) = if ruled(area.width, area.height) {
        let block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(Style::default().fg(BORDER))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        (inner, inner)
    } else {
        (area, area)
    };
    if content.is_empty() {
        return None;
    }
    let layout = approval_layout(approval, content.width as usize, content.height as usize);
    frame.render_widget(Paragraph::new(layout.lines), content);
    layout.cursor.map(|(row, column)| {
        Position::new(
            origin
                .x
                .saturating_add(column.min(origin.width.saturating_sub(1))),
            origin
                .y
                .saturating_add(row.min(origin.height.saturating_sub(1))),
        )
    })
}

pub(crate) fn render_queue(frame: &mut Frame<'_>, state: &TuiState, area: Rect) {
    if area.is_empty() {
        return;
    }
    let mut area = area;
    if state.pending_steering > 0 {
        frame.render_widget(
            Line::styled(
                truncate_display(
                    &format!(
                        "steering queued ({}) · applies after this batch",
                        state.pending_steering
                    ),
                    area.width as usize,
                ),
                Style::default().fg(AMBER),
            ),
            Rect::new(area.x, area.y, area.width, 1),
        );
        area.y += 1;
        area.height -= 1;
        if area.is_empty() {
            return;
        }
    }
    let width = area.width as usize;
    let lines = state
        .pending_prompts()
        .take(area.height as usize)
        .map(|prompt| {
            let prefix = truncate_display(
                if state.queue_paused {
                    "paused  "
                } else {
                    "queued  "
                },
                width,
            );
            let available = width.saturating_sub(Span::raw(prefix.as_str()).width());
            let safe = sanitize_terminal_text(prompt);
            let text = if available == 0 {
                String::new()
            } else {
                let row = EditorRows::new(&safe, available)
                    .next()
                    .expect("editor has one row");
                editor_row_text(row.text, available)
            };
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(DIM)),
                Span::styled(text, Style::default().fg(DIM)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);
}

pub(crate) fn render_footer(frame: &mut Frame<'_>, state: &TuiState, area: Rect, show_help: bool) {
    if area.is_empty() {
        return;
    }
    let width = area.width as usize;
    let full_mode = mode_label(state.mode);
    let mode = if full_mode.len() <= width {
        full_mode
    } else {
        match state.mode {
            ExecutionMode::Supervised => "S",
            ExecutionMode::Auto => "A",
            ExecutionMode::Yolo => "Y",
        }
    };
    let cancel_prompt = (state.overlay == Overlay::ConfirmAgentCancel).then(|| {
        let agent = state
            .selected_agent()
            .map_or("agent", |agent| agent.id.as_ref());
        format!(
            "Cancel {}? y confirm · n/esc return",
            sanitize_terminal_text(agent)
        )
    });
    let hints = if !show_help {
        match state.overlay {
            Overlay::Approval => ["↑↓ choose · Enter confirm", "Enter confirm", "↵"],
            Overlay::ApprovalEdit => ["Enter submit · Esc back", "Enter/Esc", "↵"],
            Overlay::Shortcuts => ["Esc close shortcuts", "Esc close", "Esc"],
            Overlay::Onboarding if state.onboarding.is_selecting_connection() => [
                "↑↓ choose · Enter confirm · Esc close",
                "↑↓ Enter Esc",
                "↵/Esc",
            ],
            Overlay::Onboarding => ["Enter confirm · Esc back", "Enter/Esc", "↵"],
            Overlay::Agents => [
                "Enter inspect · m message · x cancel · ↑↓ select · Esc close",
                "↵ m x ↑↓ Esc",
                "↵/Esc",
            ],
            Overlay::Todos => ["↑↓ scroll · Esc close", "↑↓ Esc", "Esc"],
            Overlay::Queue => [
                "↑↓ select · Enter edit · Delete remove · s resume · Esc close",
                "↑↓ ↵ edit · Del remove · s run · Esc",
                "↵ edit",
            ],
            Overlay::Context => [
                "↑↓ scroll · r refresh · Esc close",
                "r refresh · Esc",
                "Esc",
            ],
            Overlay::Diff => ["n/p hunk · Enter feedback · Esc close", "n/p ↵ Esc", "Esc"],
            Overlay::AgentInspect => ["m message · x cancel · Esc agents", "m x Esc", "Esc"],
            Overlay::AgentMessage => ["Enter send · Esc back", "Enter/Esc", "↵"],
            Overlay::ConfirmAgentCancel => [
                cancel_prompt.as_deref().unwrap_or_default(),
                "y/n cancel · Esc back",
                "y/n",
            ],
            Overlay::None => ["", "", ""],
        }
    } else if let Some(dragging) = state
        .transcript_selection
        .as_ref()
        .filter(|selection| selection.dragged)
        .map(|selection| selection.dragging)
        .or_else(|| {
            state
                .composer_selection
                .as_ref()
                .filter(|selection| selection.dragged)
                .map(|selection| selection.dragging)
        })
    {
        if dragging {
            ["Release to copy selection", "Release to copy", "Copy"]
        } else {
            ["Ctrl+C copy selection · Esc clear", "Ctrl+C copy", "Copy"]
        }
    } else if state.editing_follow_up() {
        [
            "Enter save queued edit · Esc cancel (draft preserved)",
            "Enter save queue · Esc cancel",
            "↵ save",
        ]
    } else if state.editing_feedback() {
        if state.activity().is_animated() {
            [
                "Enter steer · Alt+Enter queue · Esc restore draft",
                "Enter steer · Alt+Enter queue",
                "↵ steer",
            ]
        } else {
            [
                "Enter send feedback · Esc restore draft",
                "Enter send · Esc cancel",
                "↵ send",
            ]
        }
    } else if state.history_search_active() {
        ["Enter use · Esc cancel", "Enter use", "↵"]
    } else if state.selected_command().is_some() {
        ["Enter run · Tab complete", "Tab complete", "Tab"]
    } else if state.selected_file().is_some() {
        ["Enter/Tab complete", "Tab complete", "Tab"]
    } else if state.scroll > 0 {
        ["Wheel scroll · Ctrl+L latest", "Ctrl+L latest", "^L"]
    } else if state.activity().is_animated() {
        if state.composer.is_empty() {
            [
                "Enter steer · Alt+Enter queue · Esc interrupt · /queue edit",
                "Enter steer · Alt+Enter queue",
                "Esc",
            ]
        } else {
            [
                "Enter steer · Alt+Enter queue · Esc interrupt · Shift+Enter newline",
                "Enter steer · Alt+Enter queue",
                "Esc",
            ]
        }
    } else if state.composer.is_empty() {
        [
            "? shortcuts · / commands · Ctrl+O transcript",
            "? shortcuts · / commands",
            "?",
        ]
    } else {
        [
            "Enter send · Ctrl+J newline · Ctrl+O transcript",
            "Enter send · Ctrl+J newline",
            "↵",
        ]
    };
    // Modal controls take precedence when a complete mode label cannot fit beside them.
    // Normal and approval footers always retain the safety mode, even at one column.
    let modal_controls = matches!(
        state.overlay,
        Overlay::Onboarding
            | Overlay::Agents
            | Overlay::Todos
            | Overlay::Queue
            | Overlay::Context
            | Overlay::Diff
            | Overlay::AgentInspect
            | Overlay::AgentMessage
            | Overlay::ConfirmAgentCancel
    );
    let show_mode =
        area.height > 1 || !modal_controls || mode.len() + Span::raw(hints[1]).width() + 2 <= width;
    let hint_width = if area.height > 1 {
        width
    } else {
        width.saturating_sub(if show_mode { mode.len() + 2 } else { 0 })
    };
    let hint = hints
        .into_iter()
        .find(|hint| Span::raw(*hint).width() <= hint_width)
        .unwrap_or("");
    if hint_width > 0 {
        let copied = show_help.then_some(state.copied_characters).flatten();
        let feedback = copied.map_or_else(
            || {
                Line::from(Span::styled(
                    hint,
                    Style::default().fg(if state.overlay == Overlay::ConfirmAgentCancel {
                        AMBER
                    } else {
                        DIM
                    }),
                ))
            },
            |characters| copied_feedback(characters, hint_width),
        );
        frame.render_widget(
            Paragraph::new(feedback),
            Rect::new(area.x, area.y, hint_width as u16, 1),
        );
    }
    if area.height == 1 {
        if show_mode {
            frame.render_widget(
                Span::styled(mode, mode_style(state.mode)),
                Rect::new(
                    area.right() - mode.len() as u16,
                    area.y,
                    mode.len() as u16,
                    1,
                ),
            );
        }
        return;
    }

    let context = state
        .context_label()
        .filter(|context| Span::raw(context.as_str()).width() + mode.len() + 2 <= width);
    let context_width = context
        .as_ref()
        .map_or(0, |text| Span::raw(text.as_str()).width());
    let status_width = width.saturating_sub(context_width + 2 * usize::from(context.is_some()));
    let branch_width = status_width.saturating_sub(mode.len() + 3);
    let mut spans = Vec::with_capacity(3);
    if branch_width > 0
        && let Some(branch) = state
            .git_branch
            .as_deref()
            .filter(|branch| !branch.is_empty())
    {
        spans.push(Span::styled(
            truncate_display(&sanitize_terminal_text(branch), branch_width),
            Style::default().fg(DIM),
        ));
        spans.push(Span::styled(" · ", Style::default().fg(DIM)));
    }
    spans.push(Span::styled(mode, mode_style(state.mode)));
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(area.x, area.y + 1, status_width as u16, 1),
    );
    if let Some(context) = context {
        frame.render_widget(
            Span::styled(context, Style::default().fg(DIM)),
            Rect::new(
                area.right() - context_width as u16,
                area.y + 1,
                context_width as u16,
                1,
            ),
        );
    }
}

fn composer_gutter(width: usize) -> usize {
    width.saturating_sub(2).min(PROMPT_WIDTH)
}

fn composer_text_width(width: usize) -> usize {
    width.saturating_sub(composer_gutter(width)).max(1)
}

struct EditorRow<'a> {
    text: &'a str,
    start: usize,
}

struct EditorRows<'a> {
    input: &'a str,
    graphemes: Peekable<GraphemeIndices<'a>>,
    width: usize,
    start: usize,
    finished: bool,
}

impl<'a> EditorRows<'a> {
    fn new(input: &'a str, width: usize) -> Self {
        Self {
            input,
            graphemes: input.grapheme_indices(true).peekable(),
            width: width.max(1),
            start: 0,
            finished: false,
        }
    }
}

impl<'a> Iterator for EditorRows<'a> {
    type Item = EditorRow<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let start = self.start;
        let mut column = 0_usize;
        while let Some(&(offset, grapheme)) = self.graphemes.peek() {
            if matches!(grapheme, "\n" | "\r\n" | "\r") {
                self.graphemes.next();
                self.start = offset + grapheme.len();
                return Some(EditorRow {
                    text: &self.input[start..offset],
                    start,
                });
            }
            let cells = grapheme_display_width(grapheme, column).min(self.width);
            if column > 0 && column.saturating_add(cells) > self.width {
                self.start = offset;
                return Some(EditorRow {
                    text: &self.input[start..offset],
                    start,
                });
            }
            self.graphemes.next();
            column = column.saturating_add(cells);
        }
        self.finished = column < self.width;
        self.start = self.input.len();
        Some(EditorRow {
            text: &self.input[start..],
            start,
        })
    }
}

struct EditorMeasurement {
    rows: usize,
    cursor_row: usize,
    cursor_column: usize,
}

impl EditorMeasurement {
    fn visible_start(&self, height: usize) -> usize {
        self.cursor_row
            .saturating_add(1)
            .saturating_sub(height)
            .min(self.rows.saturating_sub(height))
    }
}

fn composer_view_start(state: &TuiState, measured: &EditorMeasurement, height: usize) -> usize {
    if state
        .composer_selection
        .as_ref()
        .is_some_and(|selection| selection.dragging)
    {
        state
            .composer_scroll
            .get()
            .min(measured.rows.saturating_sub(height))
    } else {
        composer_visible_start(measured, height, state.composer_scroll.get())
    }
}

fn composer_visible_start(measured: &EditorMeasurement, height: usize, previous: usize) -> usize {
    let start = previous.min(measured.rows.saturating_sub(height));
    if measured.cursor_row < start {
        measured.cursor_row
    } else if measured.cursor_row >= start.saturating_add(height) {
        measured.cursor_row.saturating_add(1).saturating_sub(height)
    } else {
        start
    }
}

fn editor_measure(input: &str, cursor: usize, width: usize) -> EditorMeasurement {
    let cursor = cursor.min(input.len());
    let mut result = EditorMeasurement {
        rows: 0,
        cursor_row: 0,
        cursor_column: 0,
    };
    let mut selected = EditorRow { text: "", start: 0 };
    for row in EditorRows::new(input, width) {
        if row.start <= cursor {
            result.cursor_row = result.rows;
            selected = row;
        }
        result.rows += 1;
    }
    for (offset, grapheme) in selected.text.grapheme_indices(true) {
        if selected.start + offset + grapheme.len() > cursor {
            break;
        }
        result.cursor_column +=
            grapheme_display_width(grapheme, result.cursor_column).min(width.max(1));
    }
    result
}

fn editor_row_text(text: &str, width: usize) -> String {
    let mut rendered = String::with_capacity(text.len());
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        let cells = grapheme_display_width(grapheme, column);
        if grapheme == "\t" {
            rendered.extend(std::iter::repeat_n(' ', cells.min(width)));
        } else if cells > width {
            rendered.push('�');
        } else {
            rendered.extend(grapheme.chars().filter(|character| !character.is_control()));
        }
        column += cells.min(width);
    }
    rendered
}

pub(crate) fn composer_cursor_vertical(
    input: &str,
    cursor: usize,
    width: usize,
    delta: i32,
) -> Option<usize> {
    editor_cursor_vertical(input, cursor, composer_text_width(width), delta)
}

pub(crate) fn editor_cursor_vertical(
    input: &str,
    cursor: usize,
    width: usize,
    delta: i32,
) -> Option<usize> {
    let measured = editor_measure(input, cursor, width);
    let target = measured.cursor_row.checked_add_signed(delta as isize)?;
    let row = EditorRows::new(input, width).nth(target)?;
    let mut column = 0;
    let mut offset = row.start;
    for grapheme in row.text.graphemes(true) {
        let cells = grapheme_display_width(grapheme, column).min(width.max(1));
        if column + cells > measured.cursor_column {
            break;
        }
        column += cells;
        offset += grapheme.len();
    }
    Some(offset)
}

struct ApprovalLayout {
    lines: Vec<Line<'static>>,
    cursor: Option<(u16, u16)>,
}

fn approval_layout(approval: &ApprovalState, width: usize, max_height: usize) -> ApprovalLayout {
    if width == 0 || max_height == 0 {
        return ApprovalLayout {
            lines: Vec::new(),
            cursor: None,
        };
    }
    let title = || {
        Line::from(Span::styled(
            truncate_display(
                if approval.editing {
                    "Edit arguments"
                } else {
                    "Action required"
                },
                width,
            ),
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ))
    };
    if approval.editing {
        let gutter = composer_gutter(width);
        let editor_width = width.saturating_sub(gutter).max(1);
        approval.editor_width.set(editor_width);
        let validation_height = usize::from(approval.validation_error.is_some() && max_height >= 2);
        let controls = approval_controls(
            true,
            approval.selected,
            width,
            usize::from(max_height > validation_height + 1),
        );
        let available = max_height - controls.len() - validation_height;
        let header_height = if available >= 3 { 2 } else { 0 };
        let editor_height = available - header_height;
        let measured = editor_measure(&approval.editor, approval.editor_cursor, editor_width);
        let start = measured.visible_start(editor_height);
        let mut lines = Vec::with_capacity(max_height);
        if header_height > 0 {
            lines.push(title());
            lines.extend(bounded_detail_lines(
                &approval_detail(&approval.request.operation),
                width,
                1,
            ));
        }
        let cursor = Some((
            (lines.len() + measured.cursor_row.saturating_sub(start)) as u16,
            (gutter + measured.cursor_column).min(width - 1) as u16,
        ));
        lines.extend(
            EditorRows::new(&approval.editor, editor_width)
                .skip(start)
                .take(editor_height)
                .enumerate()
                .map(|(index, row)| {
                    Line::from(vec![
                        Span::styled(
                            if start + index == 0 {
                                &PROMPT[..gutter]
                            } else {
                                &"  "[..gutter]
                            },
                            Style::default().fg(ACCENT),
                        ),
                        Span::raw(editor_row_text(row.text, editor_width)),
                    ])
                }),
        );
        if validation_height > 0 {
            lines.push(approval_validation_line(
                approval.validation_error.as_deref().unwrap_or(""),
                width,
            ));
        }
        lines.extend(controls);
        ApprovalLayout { lines, cursor }
    } else {
        let controls = approval_controls(
            false,
            approval.selected,
            width,
            max_height.saturating_sub(1).max(1),
        );
        let body_height = max_height.saturating_sub(controls.len());
        let mut lines = Vec::with_capacity(max_height);
        if body_height >= 2 {
            lines.push(title());
        }
        let detail_height = body_height.saturating_sub(lines.len());
        lines.extend(bounded_detail_lines(
            &approval_detail(&approval.request.operation),
            width,
            detail_height,
        ));
        let remaining = body_height.saturating_sub(lines.len());
        if remaining > 0 {
            lines.extend(bounded_detail_lines(
                &approval.request.summary,
                width,
                remaining,
            ));
        }
        lines.extend(controls);
        ApprovalLayout {
            lines,
            cursor: None,
        }
    }
}

fn approval_controls(
    editing: bool,
    selected: usize,
    width: usize,
    max_lines: usize,
) -> Vec<Line<'static>> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    if editing {
        let hint = ["Enter submit · Esc back", "Enter/Esc", "↵⎋", "↵"]
            .into_iter()
            .find(|hint| Span::raw(*hint).width() <= width)
            .unwrap_or("");
        return vec![Line::from(Span::styled(hint, Style::default().fg(DIM)))];
    }
    let selected = selected.min(3);
    let choices = APPROVAL_CHOICES;
    let style = |index| {
        if index == selected {
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(DIM)
        }
    };
    let full_width = choices.iter().map(|label| label.len()).sum::<usize>() + 6;
    if full_width <= width {
        let mut spans = Vec::with_capacity(7);
        for (index, choice) in choices.into_iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(choice, style(index)));
        }
        return vec![Line::from(spans)];
    }
    if max_lines >= 4 && width >= 19 {
        return choices
            .into_iter()
            .enumerate()
            .map(|(index, choice)| {
                Line::from(vec![
                    Span::styled(if index == selected { "> " } else { "  " }, style(index)),
                    Span::styled(choice, style(index)),
                ])
            })
            .collect();
    }
    if width >= 4 {
        let mut spans = Vec::with_capacity(7);
        for (index, key) in ["a", "s", "d", "e"].into_iter().enumerate() {
            if index > 0 && width >= 7 {
                spans.push(Span::raw("/"));
            }
            spans.push(Span::styled(key, style(index)));
        }
        return vec![Line::from(spans)];
    }
    vec![Line::from(Span::styled(
        ["a", "s", "d", "e"][selected],
        style(selected),
    ))]
}

fn bounded_detail_lines(value: &str, width: usize, max_lines: usize) -> Vec<Line<'static>> {
    if max_lines == 0 || width == 0 {
        return Vec::new();
    }
    let value = sanitize_terminal_text(value);
    let gutter = composer_gutter(width);
    let content_width = width.saturating_sub(gutter).max(1);
    let count = EditorRows::new(&value, content_width).count();
    let prefix = &"  "[..gutter];
    let line = |text| Line::from(vec![Span::raw(prefix), Span::raw(text)]);
    if count <= max_lines {
        return EditorRows::new(&value, content_width)
            .map(|row| line(editor_row_text(row.text, content_width)))
            .collect();
    }
    if max_lines == 1 {
        return vec![line(middle_truncate(&value, content_width))];
    }
    let head = max_lines.div_ceil(2);
    let tail = max_lines - head;
    let mut lines = Vec::with_capacity(max_lines);
    for (index, row) in EditorRows::new(&value, content_width).enumerate() {
        if index < head {
            lines.push(line(editor_row_text(row.text, content_width)));
        } else if index >= count - tail {
            let text = editor_row_text(row.text, content_width);
            lines.push(line(if index == count - tail {
                format!("…{}", truncate_tail(&text, content_width.saturating_sub(1)))
            } else {
                text
            }));
        }
    }
    lines
}

fn middle_truncate(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let first = EditorRows::new(value, width)
        .next()
        .expect("editor has one row");
    let text = editor_row_text(first.text, width);
    if first.text.len() == value.len() {
        return text;
    }
    let head_width = 4.min(width.saturating_sub(1));
    let tail_width = width.saturating_sub(head_width + 1);
    format!(
        "{}…{}",
        truncate_display(&text, head_width),
        truncate_tail(value, tail_width)
    )
}

fn truncate_tail(value: &str, width: usize) -> String {
    let mut tail = Vec::new();
    let mut used = 0_usize;
    for grapheme in value.graphemes(true).rev() {
        let grapheme = if grapheme.chars().any(char::is_control) {
            " "
        } else {
            grapheme
        };
        let grapheme_width = Span::raw(grapheme).width();
        if used.saturating_add(grapheme_width) > width {
            break;
        }
        tail.push(grapheme);
        used += grapheme_width;
    }
    tail.into_iter().rev().collect()
}

fn approval_validation_line(error: &str, width: usize) -> Line<'static> {
    let error = sanitize_terminal_text(error);
    Line::from(Span::styled(
        truncate_display(&format!("Invalid JSON · {error}"), width),
        Style::default().fg(RED),
    ))
}

fn approval_detail(operation: &Operation) -> String {
    match operation {
        Operation::Read { path, .. } => format!("read  {}", path.display()),
        Operation::Write { paths, .. } => format!(
            "write  {}",
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Operation::Bash { command, .. } => format!("$ {command}"),
        Operation::WebSearch { query, .. } => format!("search  {query}"),
        Operation::WebOpen { url, .. } => format!("open  {url}"),
    }
}

fn mode_label(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Supervised => "supervised",
        ExecutionMode::Auto => "auto",
        ExecutionMode::Yolo => "yolo",
    }
}

fn mode_style(mode: ExecutionMode) -> Style {
    Style::default().fg(match mode {
        ExecutionMode::Supervised => ACCENT,
        ExecutionMode::Auto => ACCENT,
        ExecutionMode::Yolo => AMBER,
    })
}

#[cfg(test)]
mod tests {
    use kurama_protocol::{id::OperationId, policy::ApprovalRequest, tool::CommandClass};
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn approval() -> ApprovalState {
        ApprovalState::new(ApprovalRequest {
            operation_id: OperationId::from("o_test"),
            operation: Operation::Bash {
                command: "printf safe".into(),
                cwd: ".".into(),
                class: CommandClass::ReadOnly,
                timeout_ms: 30_000,
            },
            summary: "Inspect output".into(),
            arguments: serde_json::json!({"command": "printf safe"}),
        })
    }

    #[test]
    fn vertical_movement_preserves_display_columns_and_graphemes() {
        let input = "界x\nab\tz\ne\u{301}x";
        assert_eq!(
            editor_cursor_vertical(input, "界".len(), 80, 1),
            Some("界x\nab".len())
        );
        assert_eq!(
            editor_cursor_vertical(input, input.len(), 80, -1),
            Some("界x\nab".len())
        );
        let wrapped = "ab界e\u{301}xy";
        assert_eq!(
            editor_cursor_vertical(wrapped, wrapped.len() - 1, 4, -1),
            Some(2)
        );
    }

    #[test]
    fn approval_edit_keeps_cursor_and_validation_inside_tiny_layouts() {
        let mut approval = approval();
        approval.editing = true;
        approval.set_editor("界e\u{301}\t\r\n".repeat(100));
        approval.validation_error = Some("expected a value".into());
        for width in [1, 2, 3, 4, 12, 80] {
            for height in [1, 2, 3, 8] {
                let layout = approval_layout(&approval, width, height);
                assert!(layout.lines.len() <= height);
                assert!(layout.lines.iter().all(|line| line.width() <= width));
                let (row, column) = layout.cursor.expect("editing stays focused");
                assert!((row as usize) < layout.lines.len());
                assert!((column as usize) < width);
                if height >= 2 {
                    assert!(
                        layout
                            .lines
                            .iter()
                            .flat_map(|line| &line.spans)
                            .any(|span| span.style.fg == Some(RED)),
                        "validation must remain visible"
                    );
                }
            }
        }
    }

    #[test]
    fn approval_selection_remains_visible_when_actions_cannot_all_fit() {
        for selected in 0..4 {
            let lines = approval_controls(false, selected, 1, 1);
            assert_eq!(lines[0].width(), 1);
            assert_eq!(lines[0].spans[0].content, ["a", "s", "d", "e"][selected]);
            assert!(
                lines[0].spans[0]
                    .style
                    .add_modifier
                    .contains(Modifier::UNDERLINED)
            );
        }
    }

    #[test]
    fn approval_deletion_removes_whole_graphemes() {
        let mut approval = approval();
        approval.set_editor("Ae\u{301}👩‍👩‍👧‍👦");
        approval.backspace();
        assert_eq!(approval.editor, "Ae\u{301}");
        approval.move_left();
        assert_eq!(approval.editor_cursor, 1);
        approval.delete();
        assert_eq!(approval.editor, "A");
    }

    #[test]
    fn composer_cursor_stays_visible_for_wide_text_and_long_paste() {
        let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
        state.composer = format!("{}END", "界e\u{301}\t\r\n".repeat(4000));
        state.cursor = state.composer.len();
        for width in [0, 1, 2, 3, 4, 80] {
            for height in [0, 1, 2, 5] {
                let mut terminal =
                    Terminal::new(TestBackend::new(width, height)).expect("terminal");
                let mut cursor = None;
                terminal
                    .draw(|frame| cursor = render_composer(frame, &state, frame.area()))
                    .expect("draw");
                if width == 0 || height == 0 {
                    assert!(cursor.is_none());
                } else {
                    let cursor = cursor.expect("composer stays focused");
                    assert!(cursor.x < width && cursor.y < height);
                    if width == 80 {
                        let text = terminal
                            .backend()
                            .buffer()
                            .content
                            .iter()
                            .map(|cell| cell.symbol())
                            .collect::<String>();
                        assert!(text.contains("END"), "paste tail remains in view");
                    }
                }
            }
        }
    }
}
