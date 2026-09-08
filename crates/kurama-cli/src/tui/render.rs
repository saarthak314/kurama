use std::time::Instant;

use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
};

use kurama_protocol::agent::AgentState;

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
        TranscriptDetail, render_transcript_view, startup_lines, transcript_lines, truncate_display,
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
        return;
    }
    if state.transcript_view_expanded() {
        render_transcript_view(frame, state);
        return;
    }
    render_main(frame, state, prepared_transcript);
    match state.overlay {
        Overlay::None | Overlay::Approval | Overlay::ApprovalEdit => {}
        Overlay::Shortcuts => render_shortcuts(frame, state),
        Overlay::Onboarding => {
            if let Some(position) = render_onboarding(frame, state) {
                frame.set_cursor_position(position);
            }
        }
        Overlay::Agents => render_agents(frame, state),
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
    let input_height = if approval_visible {
        approval_height(state, area.width)
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
        let transcript = transcript
            .iter()
            .skip(start)
            .take(viewport_height)
            .cloned()
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(Text::from(transcript)), layout.transcript);
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

fn render_shortcuts(frame: &mut Frame<'_>, state: &TuiState) {
    let _ = state;
    let area = inset(frame.area(), 6, 4);
    if area.is_empty() {
        return;
    }
    frame.render_widget(Clear, area);
    let lines = [
        ("ctrl+c", "interrupt, then clear, then exit"),
        ("esc", "interrupt a running turn"),
        ("enter", "send"),
        ("shift+enter", "newline"),
        ("ctrl+o", "expand transcript"),
        ("up/down", "prompt history"),
        ("/help", "slash commands"),
        ("?", "this overlay"),
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
    let mut area = inset(frame.area(), 4, 2);
    if area.is_empty() {
        return None;
    }
    if area.height >= 5
        && let Some(TranscriptEntry::Startup {
            version,
            project,
            mode,
        }) = state.transcript.first()
    {
        let banner_height = 2.min(area.height);
        frame.render_widget(
            Paragraph::new(startup_lines(version, project, *mode, area.width as usize)),
            Rect::new(area.x, area.y, area.width, banner_height),
        );
        area.y = area.y.saturating_add(banner_height);
        area.height = area.height.saturating_sub(banner_height);
    }
    if area.is_empty() {
        return None;
    }
    if !state.onboarding.is_selecting_connection() {
        let input = state.onboarding.display_input();
        let mut lines = vec![
            Line::from(Span::styled(
                state.onboarding.step_label(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                state.onboarding.prompt(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled(
                    "› ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if input.is_empty() {
                        " "
                    } else {
                        input.as_str()
                    },
                    Style::default().fg(TEXT),
                ),
            ]),
        ];
        if let Some(error) = state.onboarding.error() {
            lines.push(Line::from(Span::styled(error, Style::default().fg(RED))));
        }
        lines.push(Line::from(Span::styled(
            "Enter confirms · Esc closes setup · secrets remain masked",
            Style::default().fg(DIM),
        )));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(BORDER))
            .padding(Padding::new(1, 1, 0, 0));
        let inner = block.inner(area);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
        let prompt_row = 2_u16.min(inner.height.saturating_sub(1));
        let column = 2_u16.saturating_add(Line::from(input.as_str()).width() as u16);
        return Some(Position::new(
            inner
                .x
                .saturating_add(column.min(inner.width.saturating_sub(1))),
            inner.y.saturating_add(prompt_row),
        ));
    }
    let mut lines = vec![
        Line::from(Span::styled(
            state.onboarding.step_label(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "How should Kurama connect?",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Choose once. Projects remember the profile, not the secret.",
            Style::default().fg(DIM),
        )),
    ];
    for (index, option) in state.onboarding.options().iter().enumerate() {
        let selected = index == state.onboarding.selected();
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", index + 1),
                Style::default()
                    .fg(if selected { TEXT } else { DIM })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(
                format!("  {option}"),
                Style::default()
                    .fg(if selected { TEXT } else { DIM })
                    .add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ]));
    }
    lines.push(Line::from(Span::styled(
        connection_note(state.onboarding.selected()).trim_start(),
        Style::default().fg(DIM),
    )));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(BORDER))
                .padding(Padding::new(1, 1, 0, 0)),
        ),
        area,
    );
    None
}

fn render_agents(frame: &mut Frame<'_>, state: &TuiState) {
    frame.render_widget(Clear, frame.area());
    let frame_area = frame.area();
    let area = inset(frame_area, 1, 0);
    if area.is_empty() {
        return;
    }
    let horizontal_padding = if area.width >= 48 { 2 } else { 1 };
    let vertical_padding = if area.height >= 10 { 1 } else { 0 };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .padding(Padding::new(
            horizontal_padding,
            horizontal_padding,
            vertical_padding,
            vertical_padding,
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    let width = inner.width as usize;
    let height = inner.height as usize;
    let show_summary = height >= 9;
    let show_columns = width >= 72 && height >= 7;
    let fixed_lines = 2 + usize::from(show_summary) + usize::from(show_columns);
    let list_height = height.saturating_sub(fixed_lines).max(1);
    let selected = state
        .selected_agent
        .min(state.agents.len().saturating_sub(1));
    let start = selected
        .saturating_sub(list_height / 2)
        .min(state.agents.len().saturating_sub(list_height));
    let end = state.agents.len().min(start.saturating_add(list_height));

    let title = truncate(
        &format!(
            "/AGENTS    {} running · {} queued",
            state.running_agents, state.queued_agents
        ),
        width,
    );
    let mut lines = vec![Line::from(Span::styled(
        title,
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))];
    if show_summary {
        lines.push(Line::from(Span::styled(
            "Explicitly delegated children · no nested agents",
            Style::default().fg(DIM),
        )));
    }
    if show_columns {
        lines.push(Line::from(Span::styled(
            "  ID        ROLE          PROFILE      TASK                  STATE",
            Style::default().fg(DIM),
        )));
    }
    if state.agents.is_empty() {
        lines.push(Line::from(Span::styled(
            "No sub-agents",
            Style::default().fg(DIM),
        )));
    } else {
        lines.extend(
            state.agents[start..end]
                .iter()
                .enumerate()
                .map(|(offset, agent)| agent_row(agent, start + offset == selected, width)),
        );
    }
    while lines.len() + 1 < height {
        lines.push(Line::from(""));
    }
    let controls = if width >= 68 {
        "enter  inspect     m  message     x  cancel     ↑↓  select     esc  close"
    } else if width >= 36 {
        "enter inspect  m message  x cancel  esc"
    } else {
        "↵ m x esc"
    };
    lines.push(Line::from(Span::styled(
        truncate(controls, width),
        Style::default().fg(DIM),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn agent_row(agent: &crate::tui::AgentRow, selected: bool, width: usize) -> Line<'static> {
    let state_text = format!("{:?}", agent.state).to_uppercase();
    let state_color = match agent.state {
        AgentState::Running => ACCENT,
        AgentState::Queued => DIM,
        AgentState::Completed => GREEN,
        AgentState::Failed => RED,
        AgentState::Cancelled => DIM,
    };
    let marker = if selected { "▶ " } else { "  " };
    let body = if width >= 72 {
        let id = truncate(agent.id.as_ref(), 8);
        format!(
            "{:<10}{:<14}{:<13}{:<22}",
            id,
            truncate(&agent.role, 12),
            truncate(&agent.profile, 11),
            truncate(&agent.task, 20)
        )
    } else if width >= 42 {
        let reserved = marker.len() + state_text.len() + 4;
        let detail = truncate(
            &format!("{} · {} · {}", agent.id, agent.role, agent.task),
            width.saturating_sub(reserved),
        );
        format!("{detail}  ")
    } else {
        let reserved = marker.len() + state_text.len() + 2;
        let id = truncate(agent.id.as_ref(), width.saturating_sub(reserved));
        format!("{id}  ")
    };
    Line::from(vec![
        Span::styled(marker, Style::default().fg(ACCENT)),
        Span::styled(body, Style::default().fg(if selected { TEXT } else { DIM })),
        Span::styled(
            state_text,
            Style::default()
                .fg(state_color)
                .add_modifier(Modifier::BOLD),
        ),
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
    let state_color = match agent.state {
        AgentState::Running => ACCENT,
        AgentState::Queued => DIM,
        AgentState::Completed => GREEN,
        AgentState::Failed => RED,
        AgentState::Cancelled => DIM,
    };
    let mut body = vec![
        Line::from(vec![
            Span::styled("agents / ", Style::default().fg(ACCENT)),
            Span::styled(
                agent.id.to_string(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            agent.role.as_str(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(agent.task.as_str(), Style::default().fg(TEXT))),
        Line::from(vec![
            Span::styled(agent.profile.as_str(), Style::default().fg(DIM)),
            Span::raw("  ·  "),
            Span::styled(
                state_label(&agent.state).to_uppercase(),
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            agent.activity.as_str(),
            Style::default().fg(TEXT),
        )),
    ];
    for (index, line) in agent.transcript.iter().enumerate() {
        let last = index + 1 == agent.transcript.len();
        body.push(Line::from(Span::styled(
            format!("{} {line}", if last { "└" } else { "│" }),
            Style::default().fg(TEXT),
        )));
    }
    let action = if state.overlay == Overlay::AgentMessage {
        Line::from(vec![
            Span::styled("› ", Style::default().fg(ACCENT)),
            Span::styled(state.agent_message.as_str(), Style::default().fg(TEXT)),
        ])
    } else if state.overlay == Overlay::ConfirmAgentCancel {
        Line::from(Span::styled(
            format!("Cancel {}?  y confirm  ·  n/esc return", agent.id),
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(
            "esc  agents     m  message     x  cancel agent",
            Style::default().fg(DIM),
        ))
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    if inner.is_empty() {
        frame.render_widget(block, area);
        return None;
    }
    let action_height = 1.min(inner.height);
    let body_height = inner.height.saturating_sub(action_height) as usize;
    let start = body.len().saturating_sub(body_height);
    let mut lines = body
        .into_iter()
        .skip(start)
        .take(body_height)
        .collect::<Vec<_>>();
    while lines.len() + 1 < inner.height as usize {
        lines.push(Line::from(""));
    }
    let action_row = lines.len() as u16;
    lines.push(action);
    frame.render_widget(Paragraph::new(lines).block(block), area);
    if state.overlay == Overlay::AgentMessage {
        let column = 2_u16.saturating_add(
            Line::from(&state.agent_message[..state.agent_message_cursor]).width() as u16,
        );
        Some(Position::new(
            inner
                .x
                .saturating_add(column.min(inner.width.saturating_sub(1))),
            inner
                .y
                .saturating_add(action_row.min(inner.height.saturating_sub(1))),
        ))
    } else {
        None
    }
}

fn connection_note(index: usize) -> &'static str {
    match index {
        0 => "     Use the installed codex CLI · credentials stay inside Codex",
        1 => "     Use the installed claude CLI · credentials stay inside Claude",
        2 => "     Keychain, environment reference, or this session only",
        3 => "     Keychain, environment reference, or this session only",
        _ => "     Connect to an existing HTTP endpoint · no model runtime bundled",
    }
}

fn truncate(value: &str, width: usize) -> String {
    truncate_display(value, width)
}

fn inset(area: Rect, horizontal: u16, vertical: u16) -> Rect {
    let horizontal = horizontal.min(area.width / 2);
    let vertical = vertical.min(area.height / 2);
    Rect {
        x: area.x.saturating_add(horizontal),
        y: area.y.saturating_add(vertical),
        width: area.width.saturating_sub(horizontal.saturating_mul(2)),
        height: area.height.saturating_sub(vertical.saturating_mul(2)),
    }
}
