use std::{collections::VecDeque, time::Instant};
use unicode_segmentation::UnicodeSegmentation;

use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph},
};

use kurama_protocol::{agent::AgentState, session::TodoStatus};

use super::{
    Overlay, ResponsiveLayout, TranscriptEntry, TuiState, activity_line,
    agents::state_label,
    command_palette_height,
    composer::{
        approval_height, composer_height, render_approval, render_composer, render_footer,
        render_queue,
    },
    layout::{main_area, queue_height},
    render_command_palette,
    theme::{ACCENT, AMBER, BORDER, DIM, GREEN, RED, TEXT},
    transcript::{
        TranscriptDetail, for_each_wrapped_line, render_transcript_view, sanitize_terminal_text,
        startup_lines, transcript_lines, truncate_display,
    },
    worked_for_line,
};

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    render_with_transcript(frame, state, None);
}

pub(crate) fn render_with_transcript(
    frame: &mut Frame<'_>,
    state: &TuiState,
    prepared_transcript: Option<&[Line<'static>]>,
) {
    if frame.area().is_empty() {
        state.transcript_width.set(0);
        state.viewport_height.set(0);
        return;
    }
    if state.transcript_view_expanded() {
        render_transcript_view(frame, state, prepared_transcript);
        return;
    }
    render_main(frame, state, prepared_transcript);
    match state.overlay {
        Overlay::None | Overlay::Approval | Overlay::ApprovalEdit => {}
        Overlay::Shortcuts => {}
        Overlay::Onboarding => {
            if let Some(position) = render_onboarding(frame, state) {
                frame.set_cursor_position(position);
            }
        }
        Overlay::Agents => render_agents(frame, state),
        Overlay::Todos => render_todos(frame, state),
        Overlay::AgentInspect | Overlay::AgentMessage | Overlay::ConfirmAgentCancel => {
            if let Some(position) = render_agent_inspect(frame, state) {
                frame.set_cursor_position(position);
            }
        }
    }
}

fn render_main(
    frame: &mut Frame<'_>,
    state: &TuiState,
    prepared_transcript: Option<&[Line<'static>]>,
) {
    let frame_area = frame.area();
    let area = main_area(frame_area);
    if area.is_empty() {
        return;
    }
    let approval_visible = matches!(state.overlay, Overlay::Approval | Overlay::ApprovalEdit);
    let shortcuts_visible = state.overlay == Overlay::Shortcuts;
    let input_height = if approval_visible {
        approval_height(state, area.width)
    } else if shortcuts_visible {
        10.min(area.height).max(5)
    } else {
        composer_height(state, area.width)
    };
    let now = Instant::now();
    let activity = if state.overlay == Overlay::None {
        activity_line(state.activity(), area.width as usize, now).or_else(|| {
            state
                .last_turn_elapsed()
                .and_then(|elapsed| worked_for_line(elapsed, area.width as usize))
        })
    } else {
        None
    };
    let layout = ResponsiveLayout::for_area(
        area,
        input_height,
        activity.is_some(),
        queue_height(state, area.width),
    );
    state.transcript_width.set(layout.transcript.width);
    state.viewport_height.set(layout.transcript.height);

    if !layout.transcript.is_empty() {
        let transcript_width = layout.transcript.width as usize;
        let rendered_transcript;
        let transcript = if let Some(transcript) = prepared_transcript {
            transcript
        } else {
            rendered_transcript = transcript_lines(
                state.live_transcript(),
                transcript_width,
                TranscriptDetail::Compact,
            );
            &rendered_transcript
        };
        let viewport_height = layout.transcript.height as usize;
        let scroll = state
            .scroll
            .min(transcript.len().saturating_sub(viewport_height));
        let start = transcript
            .len()
            .saturating_sub(viewport_height.saturating_add(scroll));
        for (row, line) in transcript
            .iter()
            .skip(start)
            .take(viewport_height)
            .enumerate()
        {
            frame.render_widget(
                line,
                Rect::new(
                    layout.transcript.x,
                    layout.transcript.y + row as u16,
                    layout.transcript.width,
                    1,
                ),
            );
        }
    }

    if let Some(activity) = activity
        && !layout.activity.is_empty()
    {
        frame.render_widget(Paragraph::new(activity), layout.activity);
    }

    if !layout.queue.is_empty() {
        render_queue(frame, state, layout.queue);
    }

    if approval_visible {
        if let Some(position) = render_approval(frame, state, layout.input) {
            frame.set_cursor_position(position);
        }
    } else if shortcuts_visible {
        render_shortcuts(frame, state, layout.input);
    } else if state.overlay == Overlay::None
        && let Some(position) = render_composer(frame, state, layout.input)
    {
        frame.set_cursor_position(position);
    } else {
        render_composer(frame, state, layout.input);
    }

    render_footer(
        frame,
        state,
        layout.footer,
        !approval_visible && state.overlay == Overlay::None,
    );

    let palette_height = command_palette_height(state, layout.input.y.saturating_sub(area.y));
    if palette_height > 0 {
        render_command_palette(
            frame,
            state,
            Rect::new(
                area.x,
                layout.input.y.saturating_sub(palette_height),
                area.width,
                palette_height,
            ),
        );
    }
}

fn render_shortcuts(frame: &mut Frame<'_>, state: &TuiState, area: Rect) {
    let _ = state;
    if area.is_empty() {
        return;
    }
    frame.render_widget(Clear, area);
    let lines = [
        ("ctrl+c", "interrupt, then clear, then exit"),
        ("esc", "close overlay, then interrupt"),
        ("enter", "send"),
        ("ctrl+j", "newline"),
        ("ctrl+o", "expand transcript"),
        ("ctrl+t", "todo list"),
        ("ctrl+r", "search history"),
        ("shift+tab", "cycle supervised/auto"),
    ]
    .into_iter()
    .map(|(key, hint)| {
        Line::from(vec![
            Span::styled(format!(" {key:<12} "), Style::default().fg(ACCENT)),
            Span::styled(hint, Style::default().fg(DIM)),
        ])
    })
    .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" shortcuts ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(BORDER))
                .padding(Padding::new(1, 1, 0, 0)),
        ),
        area,
    );
}

fn render_onboarding(frame: &mut Frame<'_>, state: &TuiState) -> Option<Position> {
    frame.render_widget(Clear, frame.area());
    let mut area = inset(frame.area(), 2, 1);
    if area.is_empty() {
        return None;
    }
    if area.height >= 16
        && let Some(TranscriptEntry::Startup {
            version,
            model,
            project,
            mode,
        }) = state.transcript.first()
    {
        let banner = startup_lines(version, model, project, *mode, area.width as usize);
        let height = banner.len().min(area.height.saturating_sub(8) as usize) as u16;
        for (row, line) in banner.iter().take(height as usize).enumerate() {
            frame.render_widget(line, Rect::new(area.x, area.y + row as u16, area.width, 1));
        }
        area.y += height;
        area.height -= height;
    }
    let block = modal_block(area);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return None;
    }
    let width = inner.width as usize;
    let height = inner.height as usize;
    if state.onboarding.is_selecting_connection() {
        let header = usize::from(height >= 4);
        let footer = usize::from(height >= 2);
        let visible = height.saturating_sub(header + footer).max(1);
        if header != 0 {
            frame.render_widget(
                Line::from(Span::styled(
                    truncate("How should Kurama connect?", width),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
        }
        let options = state.onboarding.options();
        let selected = state
            .onboarding
            .selected()
            .min(options.len().saturating_sub(1));
        let start = selected
            .saturating_sub(visible / 2)
            .min(options.len().saturating_sub(visible));
        for (row, option) in options.iter().enumerate().skip(start).take(visible) {
            let prefix = truncate(if row == selected { "› " } else { "  " }, width.min(2));
            let available = width.saturating_sub(Line::from(prefix.as_str()).width());
            let line = Line::from(vec![
                Span::styled(prefix, Style::default().fg(ACCENT)),
                Span::styled(
                    truncate(&format!("{}  {option}", row + 1), available),
                    if row == selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(DIM)
                    },
                ),
            ]);
            frame.render_widget(
                line,
                Rect::new(
                    inner.x,
                    inner.y + (header + row - start) as u16,
                    inner.width,
                    1,
                ),
            );
        }
        if footer != 0 {
            let hint = if width >= 38 {
                "↑↓ choose · enter confirm · esc close"
            } else {
                "↑↓ enter esc"
            };
            frame.render_widget(
                Line::from(Span::styled(
                    truncate(hint, width),
                    Style::default().fg(DIM),
                )),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        }
        return None;
    }

    let show_error = state.onboarding.error().is_some() && height >= 2;
    let footer = usize::from(height >= 3);
    let title = usize::from(height > 1 + footer + usize::from(show_error));
    if title != 0 {
        frame.render_widget(
            Line::from(Span::styled(
                truncate(&state.onboarding.prompt(), width),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
    }
    let gutter = inner.width.saturating_sub(2).min(2);
    let (shown, cursor_column) = state
        .onboarding
        .display_input_tail(width.saturating_sub(gutter as usize));
    let input_row = inner.y + title as u16;
    frame.render_widget(
        Line::from(vec![
            Span::styled(truncate("› ", gutter as usize), Style::default().fg(ACCENT)),
            Span::raw(shown),
        ]),
        Rect::new(inner.x, input_row, inner.width, 1),
    );
    if show_error {
        frame.render_widget(
            Line::from(Span::styled(
                truncate(state.onboarding.error().unwrap_or_default(), width),
                Style::default().fg(RED),
            )),
            Rect::new(inner.x, input_row + 1, inner.width, 1),
        );
    }
    if footer != 0 {
        frame.render_widget(
            Line::from(Span::styled(
                truncate("enter confirm · esc back", width),
                Style::default().fg(DIM),
            )),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }
    Some(Position::new(
        inner.x + (gutter as usize + cursor_column).min(width - 1) as u16,
        input_row,
    ))
}

fn render_agents(frame: &mut Frame<'_>, state: &TuiState) {
    frame.render_widget(Clear, frame.area());
    let area = inset(frame.area(), 1, 0);
    if area.is_empty() {
        return;
    }
    let block = modal_block(area);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let width = inner.width as usize;
    let header = usize::from(inner.height >= 3);
    let footer = usize::from(inner.height >= 2);
    let list_height = (inner.height as usize)
        .saturating_sub(header + footer)
        .max(1);
    if header != 0 {
        let title = format!(
            "/agents  {} running · {} queued",
            state.running_agents, state.queued_agents
        );
        frame.render_widget(
            Line::from(Span::styled(
                truncate(&title, width),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
    }
    if state.agents.is_empty() {
        frame.render_widget(
            Line::from(Span::styled(
                truncate("No sub-agents", width),
                Style::default().fg(DIM),
            )),
            Rect::new(inner.x, inner.y + header as u16, inner.width, 1),
        );
    } else {
        let selected = state.selected_agent.min(state.agents.len() - 1);
        let start = selected
            .saturating_sub(list_height / 2)
            .min(state.agents.len().saturating_sub(list_height));
        for (row, agent) in state
            .agents
            .iter()
            .enumerate()
            .skip(start)
            .take(list_height)
        {
            frame.render_widget(
                agent_row(agent, row == selected, width),
                Rect::new(
                    inner.x,
                    inner.y + (header + row - start) as u16,
                    inner.width,
                    1,
                ),
            );
        }
    }
    if footer != 0 {
        let controls = if width >= 60 {
            "enter inspect · m message · x cancel · ↑↓ select · esc close"
        } else {
            "↵ inspect · m · x · ↑↓ · esc"
        };
        frame.render_widget(
            Line::from(Span::styled(
                truncate(controls, width),
                Style::default().fg(DIM),
            )),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }
}

fn render_todos(frame: &mut Frame<'_>, state: &TuiState) {
    let frame_area = frame.area();
    if frame_area.is_empty() {
        state.viewport_height.set(0);
        return;
    }
    let height = state
        .todos
        .len()
        .saturating_add(4)
        .max(5)
        .min(frame_area.height as usize) as u16;
    let mut area = inset(frame_area, 1, 0);
    area.y += area.height.saturating_sub(height) / 2;
    area.height = height;
    let block = modal_block(area);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        state.viewport_height.set(0);
        return;
    }
    let width = inner.width as usize;
    let header = usize::from(inner.height >= 3);
    let footer = usize::from(inner.height >= 2);
    let visible = (inner.height as usize)
        .saturating_sub(header + footer)
        .max(1);
    state.viewport_height.set(visible as u16);
    if header != 0 {
        let completed = state
            .todos
            .iter()
            .filter(|item| matches!(item.status, TodoStatus::Completed | TodoStatus::Cancelled))
            .count();
        frame.render_widget(
            Line::from(Span::styled(
                truncate(
                    &format!("/todo  {completed}/{} complete", state.todos.len()),
                    width,
                ),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
    }
    if state.todos.is_empty() {
        frame.render_widget(
            Line::from(Span::styled(
                truncate("No todo items", width),
                Style::default().fg(DIM),
            )),
            Rect::new(inner.x, inner.y + header as u16, inner.width, 1),
        );
    } else {
        let selected = state.selected_todo.min(state.todos.len() - 1);
        let start = selected
            .saturating_sub(visible / 2)
            .min(state.todos.len().saturating_sub(visible));
        for (row, item) in state.todos.iter().enumerate().skip(start).take(visible) {
            let marker = match item.status {
                TodoStatus::Completed => "[x]",
                TodoStatus::Cancelled => "[-]",
                TodoStatus::InProgress => "[>]",
                TodoStatus::Pending => "[ ]",
            };
            let prefix = if row == selected { "› " } else { "  " };
            let label = format!(
                "{prefix}{marker} {}",
                truncate(&item.content, width.saturating_sub(6))
            );
            let style = if row == selected {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else if matches!(item.status, TodoStatus::Completed | TodoStatus::Cancelled) {
                Style::default().fg(DIM)
            } else {
                Style::default()
            };
            frame.render_widget(
                Line::from(Span::styled(truncate(&label, width), style)),
                Rect::new(
                    inner.x,
                    inner.y + (header + row - start) as u16,
                    inner.width,
                    1,
                ),
            );
        }
    }
    if footer != 0 {
        frame.render_widget(
            Line::from(Span::styled(
                truncate("↑↓ scroll · esc close", width),
                Style::default().fg(DIM),
            )),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }
}

fn agent_row(agent: &crate::tui::AgentRow, selected: bool, width: usize) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let marker = truncate(if selected { "› " } else { "  " }, width.min(2));
    let marker_width = Line::from(marker.as_str()).width();
    let state_text = truncate(
        &state_label(&agent.state).to_uppercase(),
        width.saturating_sub(marker_width),
    );
    let state_color = match agent.state {
        AgentState::Running => ACCENT,
        AgentState::Completed => GREEN,
        AgentState::Failed => RED,
        AgentState::Queued | AgentState::Cancelled => DIM,
    };
    let read_only = if agent.is_read_only() && width >= 42 {
        "  read-only"
    } else {
        ""
    };
    let wrapping = if agent.activity.eq_ignore_ascii_case("wrapping up") && width >= 72 {
        "  wrapping up"
    } else {
        ""
    };
    let budget =
        width.saturating_sub(marker_width + state_text.len() + read_only.len() + wrapping.len());
    let id = truncate(agent.id.as_ref(), budget.min(10));
    let body = if width >= 72 {
        format!(
            "{} {} {}  {}",
            id,
            truncate(&agent.role, 12),
            truncate(&agent.profile, 12),
            truncate(&agent.task, budget)
        )
    } else if width >= 32 {
        format!(
            "{} {}  {}",
            id,
            truncate(&agent.role, 12),
            truncate(&agent.task, budget)
        )
    } else {
        id
    };
    let body = truncate(&body, budget.saturating_sub(usize::from(budget > 0)));
    let padding = " ".repeat(budget.saturating_sub(Line::from(body.as_str()).width()));
    Line::from(vec![
        Span::styled(marker, Style::default().fg(ACCENT)),
        Span::styled(body, Style::default().fg(if selected { TEXT } else { DIM })),
        Span::raw(padding),
        Span::styled(
            state_text,
            Style::default()
                .fg(state_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(read_only, Style::default().fg(DIM)),
        Span::styled(wrapping, Style::default().fg(AMBER)),
    ])
}

fn render_agent_inspect(frame: &mut Frame<'_>, state: &TuiState) -> Option<Position> {
    frame.render_widget(Clear, frame.area());
    let area = inset(frame.area(), 1, 0);
    if area.is_empty() {
        return None;
    }
    let Some(agent) = state.selected_agent() else {
        render_agents(frame, state);
        return None;
    };
    let block = modal_block(area);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return None;
    }
    let width = inner.width as usize;
    let header = 2 * usize::from(inner.height >= 4) + usize::from(inner.height >= 8);
    if header > 0 {
        frame.render_widget(
            agent_row(agent, true, width),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        let metadata = format!(
            "{} / {}",
            truncate(&agent.role, width / 2),
            truncate(&agent.profile, width / 2)
        );
        frame.render_widget(
            Line::from(Span::styled(
                truncate(&metadata, width),
                Style::default().fg(DIM),
            )),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );
    }
    if header > 2 {
        let activity = format!("Recent activity · {}", truncate(&agent.activity, width));
        frame.render_widget(
            Line::from(Span::styled(
                truncate(&activity, width),
                Style::default().fg(DIM),
            )),
            Rect::new(inner.x, inner.y + 2, inner.width, 1),
        );
    }
    let body_height = (inner.height as usize).saturating_sub(header + 1);
    let mut visible = Vec::with_capacity(body_height);
    for entry in agent.transcript.iter().rev().take(body_height) {
        let remaining = body_height - visible.len();
        let mut tail = VecDeque::<String>::with_capacity(remaining);
        for_each_wrapped_line(entry, width, |line| {
            let mut row = if tail.len() == remaining {
                tail.pop_front().unwrap_or_default()
            } else {
                String::new()
            };
            row.clear();
            row.push_str(line);
            tail.push_back(row);
        });
        visible.extend(tail.into_iter().rev());
        if visible.len() == body_height {
            break;
        }
    }
    for (row, line) in visible.into_iter().rev().enumerate() {
        frame.render_widget(
            Line::from(line),
            Rect::new(inner.x, inner.y + (header + row) as u16, inner.width, 1),
        );
    }
    let action_area = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
    if state.overlay == Overlay::AgentMessage {
        let gutter = inner.width.saturating_sub(2).min(2);
        let (shown, column) = editor_window(
            &state.agent_message,
            state.agent_message_cursor,
            width - gutter as usize,
        );
        frame.render_widget(
            Line::from(vec![
                Span::styled(truncate("› ", gutter as usize), Style::default().fg(ACCENT)),
                Span::raw(shown),
            ]),
            action_area,
        );
        return Some(Position::new(
            inner.x + (gutter as usize + column).min(width - 1) as u16,
            action_area.y,
        ));
    }
    let action = if state.overlay == Overlay::ConfirmAgentCancel {
        if width >= 30 {
            format!(
                "Cancel {}? y confirm · n/esc return",
                truncate(agent.id.as_ref(), width.saturating_sub(29))
            )
        } else {
            "y/n cancel · esc back".into()
        }
    } else {
        "m message · x cancel · esc agents".into()
    };
    frame.render_widget(
        Line::from(Span::styled(
            truncate(&action, width),
            Style::default().fg(if state.overlay == Overlay::ConfirmAgentCancel {
                AMBER
            } else {
                DIM
            }),
        )),
        action_area,
    );
    None
}

fn truncate(value: &str, width: usize) -> String {
    truncate_display(&sanitize_terminal_text(value), width)
}

fn inset(area: Rect, horizontal: u16, vertical: u16) -> Rect {
    let horizontal = area
        .width
        .saturating_sub(4)
        .min(horizontal.saturating_mul(2));
    let vertical = area
        .height
        .saturating_sub(3)
        .min(vertical.saturating_mul(2));
    Rect::new(
        area.x.saturating_add(horizontal.div_ceil(2)),
        area.y.saturating_add(vertical.div_ceil(2)),
        area.width.saturating_sub(horizontal),
        area.height.saturating_sub(vertical),
    )
}

fn modal_block(area: Rect) -> Block<'static> {
    if area.width < 4 || area.height < 3 {
        return Block::default();
    }
    let padding = u16::from(area.width >= 8);
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .padding(Padding::new(padding, padding, 0, 0))
}

fn editor_window(value: &str, cursor: usize, width: usize) -> (String, usize) {
    if width == 0 {
        return (String::new(), 0);
    }
    let mut cursor = cursor.min(value.len());
    while !value.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let clean = sanitize_terminal_text(value);
    let cursor = sanitize_terminal_text(&value[..cursor])
        .len()
        .min(clean.len());
    let start = clean[..cursor].rfind('\n').map_or(0, |index| index + 1);
    let end = clean[cursor..]
        .find('\n')
        .map_or(clean.len(), |index| cursor + index);
    let line = &clean[start..end];
    let cursor = cursor - start;
    let cell_width = |grapheme: &str| {
        if grapheme == "\t" {
            1
        } else {
            Line::from(grapheme).width()
        }
    };
    let cursor_column = line
        .grapheme_indices(true)
        .take_while(|(index, _)| *index < cursor)
        .map(|(_, grapheme)| cell_width(grapheme))
        .sum::<usize>();
    let desired_start = cursor_column.saturating_sub(width - 1);
    let mut skipped = 0;
    let mut used = 0;
    let mut shown = String::new();
    for grapheme in line.graphemes(true) {
        let cells = cell_width(grapheme);
        if skipped < desired_start {
            skipped += cells;
            continue;
        }
        if used + cells > width {
            if used == 0 && cells > width {
                shown.push('�');
            }
            break;
        }
        shown.push_str(if grapheme == "\t" { " " } else { grapheme });
        used += cells;
    }
    (shown, cursor_column.saturating_sub(skipped).min(width - 1))
}
