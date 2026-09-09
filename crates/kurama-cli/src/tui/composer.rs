use std::path::Path;

use kurama_protocol::{policy::ExecutionMode, tool::Operation};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;

use super::{
    ApprovalState, Overlay, TuiState,
    theme::{ACCENT, AMBER, BORDER, DIM, RED, TEXT},
    transcript::{hard_wrap, truncate_display, word_wrap},
};

const PROMPT: &str = "› ";
const PROMPT_WIDTH: usize = 2;
const MAX_COMPOSER_HEIGHT: usize = 8;
const MAX_APPROVAL_HEIGHT: usize = 14;
const BOX_BORDERS: u16 = 2;

fn boxed(width: u16, height: u16) -> bool {
    width >= 4 && height >= 3
}

fn inner_width(width: u16) -> usize {
    width.saturating_sub(BOX_BORDERS).max(1) as usize
}

pub(crate) fn composer_height(state: &TuiState, width: u16) -> u16 {
    let inner = composer_visual(&state.composer, state.cursor, inner_width(width))
        .lines
        .len()
        .clamp(1, MAX_COMPOSER_HEIGHT) as u16;
    inner.saturating_add(BOX_BORDERS)
}

pub(crate) fn approval_height(state: &TuiState, width: u16) -> u16 {
    if !matches!(state.overlay, Overlay::Approval | Overlay::ApprovalEdit) {
        return 0;
    }
    state.approval.as_ref().map_or(0, |approval| {
        let inner = approval_layout(approval, inner_width(width), MAX_APPROVAL_HEIGHT)
            .lines
            .len() as u16;
        inner.saturating_add(BOX_BORDERS).max(3)
    })
}

pub(crate) fn render_composer(
    frame: &mut Frame<'_>,
    state: &TuiState,
    area: Rect,
) -> Option<Position> {
    if area.is_empty() {
        return None;
    }

    let (content, origin) = if boxed(area.width, area.height) {
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER));
        if state.composer.is_empty() && area.width >= 42 {
            block = block.title_bottom(
                Line::from(Span::styled(
                    " enter send · shift+enter newline · ? shortcuts ",
                    Style::default().fg(DIM),
                ))
                .right_aligned(),
            );
        }
        let inner = block.inner(area);
        frame.render_widget(block, area);
        (inner, inner)
    } else {
        (area, area)
    };
    if content.is_empty() {
        return None;
    }

    if state.composer.is_empty() {
        let placeholder = "Ask Kurama to do anything";
        let available = content.width.saturating_sub(PROMPT_WIDTH as u16) as usize;
        let placeholder = if Line::from(placeholder).width() <= available {
            placeholder
        } else {
            ""
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(PROMPT, Style::default().fg(ACCENT)),
                Span::styled(placeholder, Style::default().fg(DIM)),
            ])),
            content,
        );
        return Some(Position::new(
            origin
                .x
                .saturating_add((PROMPT_WIDTH as u16).min(origin.width.saturating_sub(1))),
            origin.y,
        ));
    }

    let visual = composer_visual(&state.composer, state.cursor, content.width as usize);
    let visible_height = content.height as usize;
    let start = visual
        .cursor_row
        .saturating_add(1)
        .saturating_sub(visible_height)
        .min(visual.lines.len().saturating_sub(visible_height));
    let lines = visual
        .lines
        .iter()
        .skip(start)
        .take(visible_height)
        .enumerate()
        .map(|(index, line)| {
            Line::from(vec![
                Span::styled(
                    if start + index == 0 { PROMPT } else { "  " },
                    Style::default().fg(ACCENT),
                ),
                Span::styled(line.clone(), Style::default().fg(TEXT)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(Text::from(lines)), content);

    let cursor_row = visual.cursor_row.saturating_sub(start) as u16;
    Some(Position::new(
        origin.x.saturating_add(
            (PROMPT_WIDTH as u16)
                .saturating_add(visual.cursor_column as u16)
                .min(origin.width.saturating_sub(1)),
        ),
        origin
            .y
            .saturating_add(cursor_row.min(origin.height.saturating_sub(1))),
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
    let (content, origin) = if boxed(area.width, area.height) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(AMBER));
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
    let width = area.width as usize;
    let lines = state
        .pending_prompts()
        .take(area.height as usize)
        .map(|prompt| {
            Line::from(Span::styled(
                truncate_display(&format!("queued  {prompt}"), width),
                Style::default().fg(DIM),
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);
}

pub(crate) fn render_footer(frame: &mut Frame<'_>, state: &TuiState, area: Rect, _show_help: bool) {
    if area.is_empty() {
        return;
    }

    let profile = FooterItem::dim(format!("{}/{}", state.profile, state.model));
    let project = FooterItem::dim(project_label(&state.project));
    let mode = FooterItem::new(mode_label(state.mode).to_owned(), mode_style(state.mode));
    let context = state.context_label().map(FooterItem::dim);
    let mut items = vec![profile, project];
    if let Some(context) = context {
        items.push(context);
    }
    items.push(mode);
    if footer_width(&items) > area.width as usize {
        items.remove(1);
    }
    if footer_width(&items) > area.width as usize {
        items.remove(0);
    }

    if footer_width(&items) > area.width as usize {
        items[0].text = truncate_display(&items[0].text, area.width as usize);
    }
    let mut spans = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(item.text, item.style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

struct ComposerVisual {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_column: usize,
}

fn composer_visual(input: &str, cursor: usize, width: usize) -> ComposerVisual {
    let content_width = width.saturating_sub(PROMPT_WIDTH).max(1);
    let mut cursor = cursor.min(input.len());
    while !input.is_char_boundary(cursor) {
        cursor = cursor.saturating_sub(1);
    }

    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_width = 0_usize;
    let mut cursor_row = 0_usize;
    let mut cursor_column = 0_usize;
    let mut cursor_recorded = false;

    for (index, grapheme) in input.grapheme_indices(true) {
        let grapheme_width = Line::from(grapheme).width();
        if grapheme != "\n"
            && line_width > 0
            && line_width.saturating_add(grapheme_width) > content_width
        {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
        }
        if index == cursor {
            cursor_row = lines.len();
            cursor_column = line_width;
            cursor_recorded = true;
        } else if cursor > index && cursor < index.saturating_add(grapheme.len()) {
            cursor_row = lines.len();
            cursor_column = line_width
                .saturating_add(Line::from(&grapheme[..cursor.saturating_sub(index)]).width());
            cursor_recorded = true;
        }
        if grapheme == "\n" {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
        } else {
            line.push_str(grapheme);
            line_width = line_width.saturating_add(grapheme_width);
        }
    }

    if !cursor_recorded {
        if cursor == input.len() && line_width >= content_width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
        }
        cursor_row = lines.len();
        cursor_column = line_width;
    }
    lines.push(line);

    ComposerVisual {
        lines,
        cursor_row,
        cursor_column,
    }
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

    let control_height = if max_height == 1 {
        1
    } else {
        max_height.saturating_sub(1)
    };
    let controls = approval_controls(approval.editing, width, control_height);
    let body_height = max_height.saturating_sub(controls.len());
    let detail_text = approval_detail(&approval.request.operation);
    let detail = indented_lines(&detail_text, width, Style::default(), false);
    let summary = indented_lines(
        &approval.request.summary,
        width,
        Style::default().add_modifier(Modifier::DIM),
        true,
    );
    let mut title_spans = vec![Span::styled(
        "Action required",
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
    )];
    if approval.editing && width >= 31 {
        title_spans.push(Span::styled(
            " · Edit arguments",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    let title = Line::from(title_spans);

    if approval.editing {
        let editor_width = width.saturating_sub(2).max(1);
        let validation_height = usize::from(approval.validation_error.is_some() && body_height > 0);
        let content_height = body_height.saturating_sub(validation_height);
        let editor_visual =
            approval_editor_visual(&approval.editor, approval.editor_cursor, editor_width);
        let editor_full_height = editor_visual.lines.len();
        let editor_reserve = usize::from(editor_full_height > 0 && content_height > 1);
        let detail_height = detail
            .len()
            .min(content_height.saturating_sub(editor_reserve));
        let editor_budget = editor_full_height.min(content_height.saturating_sub(detail_height));
        let mut optional_height = content_height
            .saturating_sub(detail_height)
            .saturating_sub(editor_budget);
        let show_title = optional_height > 0;
        optional_height = optional_height.saturating_sub(usize::from(show_title));
        let summary_height = summary.len().min(optional_height);
        let mut lines = Vec::new();
        if show_title {
            lines.push(title);
        }
        lines.extend(bounded_detail_lines(&detail_text, width, detail_height));
        lines.extend(summary.into_iter().take(summary_height));
        let editor = editor_preview(editor_visual, editor_budget);
        let cursor = editor.cursor.map(|(row, column)| {
            let row = lines.len().saturating_add(row) as u16;
            let column = 2_u16.saturating_add(column as u16);
            (row, column)
        });
        lines.extend(
            editor
                .lines
                .into_iter()
                .map(|line| Line::from(vec![Span::raw("  "), Span::raw(line)])),
        );
        if let Some(error) = &approval.validation_error {
            lines.push(approval_validation_line(error, width));
        }
        lines.extend(controls);
        debug_assert!(lines.len() <= max_height);
        ApprovalLayout { lines, cursor }
    } else {
        let detail_height = detail.len().min(body_height);
        let mut optional_height = body_height.saturating_sub(detail_height);
        let show_title = optional_height > 0;
        optional_height = optional_height.saturating_sub(usize::from(show_title));
        let summary_height = summary.len().min(optional_height);
        let mut lines = Vec::new();
        if show_title {
            lines.push(title);
        }
        lines.extend(bounded_detail_lines(&detail_text, width, detail_height));
        lines.extend(summary.into_iter().take(summary_height));
        lines.extend(controls);
        debug_assert!(lines.len() <= max_height);
        ApprovalLayout {
            lines,
            cursor: None,
        }
    }
}

fn approval_controls(editing: bool, width: usize, max_lines: usize) -> Vec<Line<'static>> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }

    let full_labels = if editing {
        ["Enter submit", "Esc return"].as_slice()
    } else {
        ["a approve once", "s approve session", "d deny", "e edit"].as_slice()
    };
    let style = if editing {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(AMBER).add_modifier(Modifier::BOLD)
    };
    let full_combined = full_labels.join("  ");
    if Line::from(full_combined.as_str()).width().saturating_add(2) <= width {
        return vec![Line::from(vec![
            Span::raw("  "),
            Span::styled(full_combined, style),
        ])];
    }

    let stacked = full_labels
        .iter()
        .copied()
        .map(|label| Line::from(vec![Span::raw("  "), Span::styled(label, style)]))
        .collect::<Vec<_>>();
    if stacked.len() <= max_lines && stacked.iter().all(|line| line.width() <= width) {
        return stacked;
    }

    let compact_labels = if editing {
        ["Enter", "Esc"].as_slice()
    } else {
        ["a approve", "s session", "d deny", "e edit"].as_slice()
    };
    let compact_combined = compact_labels.join(if editing { "/" } else { "  " });
    if Line::from(compact_combined.as_str()).width() <= width {
        return vec![Line::from(Span::styled(compact_combined, style))];
    }

    let compact_stacked = compact_labels
        .iter()
        .copied()
        .map(|label| Line::from(Span::styled(label, style)))
        .collect::<Vec<_>>();
    if compact_stacked.len() <= max_lines
        && compact_stacked.iter().all(|line| line.width() <= width)
    {
        return compact_stacked;
    }

    let shortest = if editing {
        "↵⎋"
    } else if width >= Line::from("a/s/d/e").width() {
        "a/s/d/e"
    } else if width >= Line::from("asde").width() {
        "asde"
    } else {
        "asd"
    };
    vec![Line::from(Span::styled(
        truncate_display(shortest, width),
        style,
    ))]
}

fn indented_lines(value: &str, width: usize, style: Style, prose: bool) -> Vec<Line<'static>> {
    let width = width.saturating_sub(2).max(1);
    let lines = if prose {
        word_wrap(value, width)
    } else {
        hard_wrap(value, width)
    };
    lines
        .into_iter()
        .map(|line| Line::from(vec![Span::raw("  "), Span::styled(line, style)]))
        .collect()
}

fn bounded_detail_lines(value: &str, width: usize, max_lines: usize) -> Vec<Line<'static>> {
    if max_lines == 0 || width == 0 {
        return Vec::new();
    }
    let content_width = width.saturating_sub(2).max(1);
    let wrapped = hard_wrap(value, content_width);
    if wrapped.len() <= max_lines {
        return wrapped
            .into_iter()
            .map(|line| Line::from(vec![Span::raw("  "), Span::raw(line)]))
            .collect();
    }
    if max_lines == 1 {
        return vec![Line::from(vec![
            Span::raw("  "),
            Span::raw(middle_truncate(value, content_width)),
        ])];
    }
    if max_lines == 2 {
        let first = truncate_display(&wrapped[0], content_width);
        let last = format!(
            "…{}",
            truncate_tail(
                wrapped.last().map_or("", String::as_str),
                content_width.saturating_sub(1)
            )
        );
        return [first, last]
            .into_iter()
            .map(|line| Line::from(vec![Span::raw("  "), Span::raw(line)]))
            .collect();
    }

    let visible_lines = max_lines.saturating_sub(1);
    let head_lines = visible_lines.div_ceil(2);
    let tail_lines = visible_lines.saturating_sub(head_lines);
    let omitted = wrapped.len().saturating_sub(visible_lines);
    let mut lines = wrapped.iter().take(head_lines).cloned().collect::<Vec<_>>();
    lines.push(truncate_display(
        &format!("… {omitted} lines omitted …"),
        content_width,
    ));
    lines.extend(
        wrapped
            .iter()
            .skip(wrapped.len().saturating_sub(tail_lines))
            .cloned(),
    );
    lines
        .into_iter()
        .map(|line| Line::from(vec![Span::raw("  "), Span::raw(line)]))
        .collect()
}

fn middle_truncate(value: &str, width: usize) -> String {
    if Line::from(value).width() <= width {
        return value.to_owned();
    }
    if width <= 1 {
        return "…".repeat(width);
    }
    let head_width = 4.min(width.saturating_sub(1));
    let tail_width = width.saturating_sub(head_width).saturating_sub(1);
    format!(
        "{}…{}",
        truncate_display(value, head_width),
        truncate_tail(value, tail_width)
    )
}

fn truncate_tail(value: &str, width: usize) -> String {
    let mut tail = Vec::new();
    let mut used = 0_usize;
    for grapheme in value.graphemes(true).rev() {
        let grapheme_width = Line::from(grapheme).width();
        if used.saturating_add(grapheme_width) > width {
            break;
        }
        tail.push(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    tail.into_iter().rev().collect()
}

struct EditorPreview {
    lines: Vec<String>,
    cursor: Option<(usize, usize)>,
}

fn approval_editor_visual(editor: &str, cursor: usize, width: usize) -> ComposerVisual {
    const CURSOR_MARKER: char = '\u{2063}';

    let mut cursor = cursor.min(editor.len());
    while !editor.is_char_boundary(cursor) {
        cursor = cursor.saturating_sub(1);
    }
    let mut marked = editor.to_owned();
    marked.insert(cursor, CURSOR_MARKER);
    let mut lines = hard_wrap(&marked, width);
    for (row, line) in lines.iter_mut().enumerate() {
        if let Some(column) = line.find(CURSOR_MARKER) {
            let cursor_column = Line::from(&line[..column]).width();
            line.remove(column);
            return ComposerVisual {
                lines,
                cursor_row: row,
                cursor_column,
            };
        }
    }
    ComposerVisual {
        lines,
        cursor_row: 0,
        cursor_column: 0,
    }
}

fn editor_preview(visual: ComposerVisual, max_lines: usize) -> EditorPreview {
    if max_lines == 0 {
        return EditorPreview {
            lines: Vec::new(),
            cursor: None,
        };
    }
    if visual.lines.len() <= max_lines {
        return EditorPreview {
            lines: visual.lines,
            cursor: Some((visual.cursor_row, visual.cursor_column)),
        };
    }
    if max_lines == 1 {
        let row = visual.cursor_row.min(visual.lines.len().saturating_sub(1));
        return EditorPreview {
            lines: vec![visual.lines[row].clone()],
            cursor: Some((0, visual.cursor_column)),
        };
    }

    let content_lines = max_lines.saturating_sub(1);
    let start = visual
        .cursor_row
        .saturating_add(1)
        .saturating_sub(content_lines)
        .min(visual.lines.len().saturating_sub(content_lines));
    if start == 0 {
        return EditorPreview {
            lines: visual.lines.into_iter().take(max_lines).collect(),
            cursor: Some((visual.cursor_row, visual.cursor_column)),
        };
    }

    let mut lines = vec![format!("… {start} lines above …")];
    lines.extend(visual.lines.into_iter().skip(start).take(content_lines));
    EditorPreview {
        lines,
        cursor: Some((
            visual.cursor_row.saturating_sub(start).saturating_add(1),
            visual.cursor_column,
        )),
    }
}

fn approval_validation_line(error: &str, width: usize) -> Line<'static> {
    let message = format!("Invalid JSON · {error}");
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            truncate_display(&message, width.saturating_sub(2)),
            Style::default().fg(RED),
        ),
    ])
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

struct FooterItem {
    text: String,
    style: Style,
}

impl FooterItem {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    fn dim(text: impl Into<String>) -> Self {
        Self::new(text, Style::default().add_modifier(Modifier::DIM))
    }

    fn width(&self) -> usize {
        Line::from(self.text.as_str()).width()
    }
}

fn footer_width(items: &[FooterItem]) -> usize {
    items
        .iter()
        .map(FooterItem::width)
        .sum::<usize>()
        .saturating_add(items.len().saturating_sub(1) * 2)
}

fn project_label(project: &str) -> String {
    Path::new(project)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(project)
        .to_owned()
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

    use super::*;

    #[test]
    fn single_line_approval_uses_key_only_controls_at_width_five() {
        assert_eq!(single_line_controls(false, 5), "asde");
    }

    #[test]
    fn single_line_approval_uses_key_only_controls_at_width_four() {
        assert_eq!(single_line_controls(false, 4), "asde");
    }

    #[test]
    fn single_line_approval_uses_key_only_controls_at_width_three() {
        assert_eq!(single_line_controls(false, 3), "asd");
    }

    #[test]
    fn single_line_edit_approval_keeps_submit_and_return_at_width_two() {
        assert_eq!(single_line_controls(true, 2), "↵⎋");
    }

    fn single_line_controls(editing: bool, width: usize) -> String {
        let request = ApprovalRequest {
            operation_id: OperationId::from("o_test"),
            operation: Operation::Bash {
                command: "cargo test -p kurama-cli".into(),
                cwd: ".".into(),
                class: CommandClass::ReadOnly,
                timeout_ms: 30_000,
            },
            summary: "Run the focused CLI tests".into(),
            arguments: serde_json::json!({"command":"cargo test -p kurama-cli"}),
        };
        let mut approval = ApprovalState::new(request);
        approval.editing = editing;
        let layout = approval_layout(&approval, width, 1);

        assert_eq!(layout.lines.len(), 1);
        assert!(layout.lines[0].width() <= width);
        layout.lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }
}
