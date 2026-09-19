use std::time::Instant;
use unicode_segmentation::UnicodeSegmentation;

use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Padding, Paragraph},
};

use kurama_protocol::{agent::AgentState, session::TodoStatus};

use super::{
    Overlay, TranscriptEntry, TuiState, activity_line,
    agents::state_label,
    command_palette::Palette,
    composer::{render_approval, render_composer, render_footer, render_queue},
    context::render_context,
    diff::render_diff,
    layout::{main_area, main_layout},
    render_command_palette,
    theme::{ACCENT, AMBER, BORDER, DIM, GREEN, RED, TEXT},
    transcript::{
        TranscriptDetail, TranscriptLine, for_each_wrapped_line, prepared_transcript_lines,
        render_transcript_view, sanitize_terminal_text, startup_lines, truncate_display,
    },
    worked_for_line,
};

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    render_with_transcript(frame, state, None);
}

pub(crate) fn render_with_transcript(
    frame: &mut Frame<'_>,
    state: &TuiState,
    prepared_transcript: Option<&[TranscriptLine]>,
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
        Overlay::Queue => render_follow_ups(frame, state),
        Overlay::Context => render_context(frame, &state.context_view, main_area(frame.area())),
        Overlay::Diff => {
            let area = main_area(frame.area());
            if let Some(review) = &state.diff_review {
                render_diff(frame, review, area);
            } else {
                frame.render_widget(Clear, frame.area());
                let message = state
                    .diff_error
                    .as_deref()
                    .unwrap_or(if state.diff_loading {
                        "Loading staged, unstaged and untracked changes…"
                    } else {
                        "No diff loaded"
                    });
                let text = format!("{}\n\nEsc close", sanitize_terminal_text(message));
                frame.render_widget(
                    Paragraph::new(text)
                        .block(modal_block(area).title(" /diff "))
                        .wrap(ratatui::widgets::Wrap { trim: false }),
                    area,
                );
            }
        }
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
    prepared_transcript: Option<&[TranscriptLine]>,
) {
    let frame_area = frame.area();
    let area = main_area(frame_area);
    if area.is_empty() {
        return;
    }
    let approval_visible = matches!(state.overlay, Overlay::Approval | Overlay::ApprovalEdit);
    let shortcuts_visible = state.overlay == Overlay::Shortcuts;
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
    let layout = main_layout(frame_area, state);
    state.transcript_width.set(layout.transcript.width);
    state.viewport_height.set(layout.transcript.height);

    if !layout.transcript.is_empty() {
        let transcript_width = layout.transcript.width as usize;
        let rendered_transcript;
        let transcript = if let Some(transcript) = prepared_transcript {
            transcript
        } else {
            rendered_transcript = prepared_transcript_lines(
                &state.transcript,
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
            line.render(
                frame,
                Rect::new(
                    layout.transcript.x,
                    layout.transcript.y + row as u16,
                    layout.transcript.width,
                    1,
                ),
            );
        }
        if state.overlay == Overlay::None
            && let Some(selection) = &state.transcript_selection
        {
            selection.render(frame, layout.transcript, start);
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

    let palette_available = layout.input.y.saturating_sub(area.y);
    if palette_available == 0 {
        return;
    }
    let palette = Palette::prepare(state);
    let palette_height = palette.height(palette_available);
    if palette_height > 0 {
        render_command_palette(
            frame,
            state,
            &palette,
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
    if area.is_empty() {
        return;
    }
    frame.render_widget(Clear, area);
    let block = modal_block(area);
    let block = if area.height >= 3 && area.width >= 4 {
        block.title(" shortcuts ")
    } else {
        block
    };
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let mut lines = Vec::new();
    for (key, hint) in [
        ("ctrl+c", "interrupt, then clear, then exit"),
        ("esc", "close overlay, then interrupt"),
        ("enter", "send, or steer active work"),
        ("alt+enter", "queue a follow-up"),
        ("shift+enter", "newline (also Ctrl+J)"),
        ("↑↓", "prompt history"),
        ("alt+↑↓", "move within composer"),
        ("home/end", "logical line start/end"),
        ("ctrl+a/e", "whole prompt start/end"),
        ("/queue", "edit or remove follow-ups"),
        ("/diff", "review hunks and prepare feedback"),
        ("/context", "inspect request estimates"),
        ("ctrl+o", "expand transcript; Tab links, Enter open, y copy"),
        ("ctrl+t", "todo list"),
        ("ctrl+r", "search history"),
        ("shift+tab", "cycle supervised/auto"),
    ] {
        let text = if inner.width >= 60 {
            format!(" {key:<12} {hint}")
        } else {
            format!("{key} · {hint}")
        };
        for_each_wrapped_line(&text, inner.width as usize, |line| {
            lines.push(Line::from(line.to_owned()))
        });
    }
    let page = inner.height as usize;
    let max = lines.len().saturating_sub(page);
    let start = state.shortcuts_scroll.get().min(max);
    state.shortcuts_scroll.set(start);
    state.shortcuts_max_scroll.set(max);
    state.shortcuts_page_height.set(page.max(1));
    frame.render_widget(
        Paragraph::new(lines.into_iter().skip(start).take(page).collect::<Vec<_>>()),
        inner,
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
            let prefix = truncate(if row == selected { "> " } else { "  " }, width.min(2));
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
            render_footer(
                frame,
                state,
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
                false,
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
        .display_input_window(width.saturating_sub(gutter as usize));
    let input_row = inner.y + title as u16;
    frame.render_widget(
        Line::from(vec![
            Span::styled(truncate("> ", gutter as usize), Style::default().fg(ACCENT)),
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
        render_footer(
            frame,
            state,
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            false,
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
    state.agents_page_height.set(list_height);
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
        render_footer(
            frame,
            state,
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            false,
        );
    }
}

fn render_follow_ups(frame: &mut Frame<'_>, state: &TuiState) {
    frame.render_widget(Clear, frame.area());
    let area = main_area(frame.area());
    if area.is_empty() {
        return;
    }
    let block = modal_block(area);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let width = usize::from(inner.width);
    let header = u16::from(inner.height > 2);
    let footer = u16::from(inner.height > 1);
    let visible = usize::from(inner.height.saturating_sub(header + footer)).max(1);
    if header > 0 {
        let title = format!(
            "/queue  {} follow-ups · {}",
            state.pending_turn_count(),
            if state.queue_paused {
                "paused after interrupt"
            } else {
                "held while reviewing"
            }
        );
        frame.render_widget(
            Line::styled(
                truncate(&title, width),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
    }
    if state.pending_turn_count() == 0 {
        frame.render_widget(
            Line::styled(
                truncate(
                    "No follow-ups. Alt+Enter queues input while working.",
                    width,
                ),
                Style::default().fg(DIM),
            ),
            Rect::new(inner.x, inner.y + header, inner.width, 1),
        );
    } else {
        let selected = state.selected_follow_up.min(state.pending_turn_count() - 1);
        let start = selected
            .saturating_sub(visible / 2)
            .min(state.pending_turn_count().saturating_sub(visible));
        for (index, prompt) in state
            .pending_prompts()
            .enumerate()
            .skip(start)
            .take(visible)
        {
            let marker = if index == selected { ">" } else { " " };
            let safe = sanitize_terminal_text(prompt);
            let summary = safe.lines().next().unwrap_or("");
            let text = format!(
                "{marker} {}  {summary}{}",
                index + 1,
                if safe.contains('\n') { " …" } else { "" }
            );
            frame.render_widget(
                Line::styled(
                    truncate(&text, width),
                    Style::default().fg(if index == selected { ACCENT } else { DIM }),
                ),
                Rect::new(
                    inner.x,
                    inner.y + header + (index - start) as u16,
                    inner.width,
                    1,
                ),
            );
        }
    }
    if footer > 0 {
        render_footer(
            frame,
            state,
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            false,
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
            let prefix = if row == selected { "> " } else { "  " };
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
        render_footer(
            frame,
            state,
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            false,
        );
    }
}

fn agent_row(agent: &crate::tui::AgentRow, selected: bool, width: usize) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let marker = truncate(if selected { "> " } else { "  " }, width.min(2));
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
    let message_controls = usize::from(state.overlay == Overlay::AgentMessage && inner.height >= 2);
    let body_height = (inner.height as usize).saturating_sub(header + 1 + message_controls);
    let mut count = 0usize;
    for entry in &agent.transcript {
        for_each_wrapped_line(entry, width, |_| count += 1);
    }
    let max = count.saturating_sub(body_height);
    let scroll = state.agent_inspect_scroll.get().min(max);
    state.agent_inspect_scroll.set(scroll);
    state.agent_inspect_max_scroll.set(max);
    state.agent_inspect_page_height.set(body_height.max(1));
    let start = max.saturating_sub(scroll);
    let mut index = 0usize;
    for entry in &agent.transcript {
        for_each_wrapped_line(entry, width, |line| {
            if index >= start && index - start < body_height {
                frame.render_widget(
                    Line::from(line),
                    Rect::new(
                        inner.x,
                        inner.y + (header + index - start) as u16,
                        inner.width,
                        1,
                    ),
                );
            }
            index += 1;
        });
        if index >= start.saturating_add(body_height) {
            break;
        }
    }
    let action_area = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
    if state.overlay == Overlay::AgentMessage {
        if message_controls > 0 {
            render_footer(
                frame,
                state,
                Rect::new(inner.x, action_area.y - 1, inner.width, 1),
                false,
            );
        }
        let gutter = inner.width.saturating_sub(2).min(2);
        let (shown, column) = editor_window(
            &state.agent_message,
            state.agent_message_cursor,
            width - gutter as usize,
        );
        frame.render_widget(
            Line::from(vec![
                Span::styled(truncate("> ", gutter as usize), Style::default().fg(ACCENT)),
                Span::raw(shown),
            ]),
            action_area,
        );
        return Some(Position::new(
            inner.x + (gutter as usize + column).min(width - 1) as u16,
            action_area.y,
        ));
    }
    render_footer(frame, state, action_area, false);
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
    let padding = 1 + u16::from(area.width >= 8);
    Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
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

#[cfg(test)]
mod tests {
    use kurama_protocol::policy::ExecutionMode;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Color};

    use super::*;
    use crate::tui::{TranscriptPoint, TranscriptSelection};

    fn selected_frame(state: &TuiState, rows: &[TranscriptLine]) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        terminal
            .draw(|frame| render_with_transcript(frame, state, Some(rows)))
            .unwrap()
            .buffer
            .clone()
    }

    #[test]
    fn compact_and_expanded_views_highlight_absolute_rows_and_preserve_composer() {
        for expanded in [false, true] {
            let mut state = TuiState::new("work", "model", ".", ExecutionMode::Supervised);
            if expanded {
                state.toggle_transcript_view();
            }
            let frame_area = Rect::new(0, 0, 40, 16);
            let area = if expanded {
                let mut area = main_area(frame_area);
                area.height -= 1;
                area
            } else {
                main_layout(frame_area, &state).transcript
            };
            let rows = (0..usize::from(area.height) + 7)
                .map(|row| TranscriptLine::from(Line::raw(format!("row-{row:03}"))))
                .collect::<Vec<_>>();
            state.scroll = 4;
            let before = selected_frame(&state, &rows);
            let mut selection =
                TranscriptSelection::new(TranscriptPoint { row: 4, column: 1 }, None);
            selection.update(TranscriptPoint { row: 4, column: 2 });
            state.transcript_selection = Some(selection);
            let selected = selected_frame(&state, &rows);
            let feedback_row = if expanded { 15 } else { 14 };
            for y in 0..16 {
                for x in 0..40 {
                    let cell = &selected[(x, y)];
                    if y == area.y + 1 && (area.x + 1..=area.x + 2).contains(&x) {
                        assert_eq!(cell.fg, Color::Black);
                        assert_eq!(cell.bg, ACCENT);
                        assert_eq!(cell.symbol(), before[(x, y)].symbol());
                    } else if y != feedback_row {
                        assert_eq!(cell, &before[(x, y)]);
                    }
                }
            }
            state.overlay = Overlay::Shortcuts;
            let overlay = selected_frame(&state, &rows);
            state.transcript_selection = None;
            assert_eq!(overlay, selected_frame(&state, &rows));
        }
    }
}
