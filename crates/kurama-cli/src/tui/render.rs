use std::time::Instant;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
};

use kurama_protocol::agent::AgentState;

use super::{
    Overlay, ResponsiveLayout, TranscriptEntry, TuiState, activity_line, command_palette_height,
    composer::{approval_height, composer_height, render_approval, render_composer, render_footer},
    layout::main_area,
    render_command_palette,
    transcript::{
        TranscriptDetail, render_transcript_view, startup_lines, transcript_lines, truncate_display,
    },
    worked_for_line,
};

const BORDER: Color = Color::DarkGray;
const DIM: Color = Color::DarkGray;
const TEXT: Color = Color::Reset;
pub(crate) const SURFACE: Color = Color::Reset;
const ACCENT: Color = Color::Cyan;
const AMBER: Color = Color::Yellow;
const GREEN: Color = Color::Cyan;

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
        Overlay::Onboarding => render_onboarding(frame, state),
        Overlay::Agents => render_agents(frame, state),
        Overlay::AgentInspect | Overlay::AgentMessage | Overlay::ConfirmAgentCancel => {
            render_agent_inspect(frame, state)
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
    let layout = ResponsiveLayout::for_area(area, input_height, activity.is_some());

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

fn render_onboarding(frame: &mut Frame<'_>, state: &TuiState) {
    frame.render_widget(Clear, frame.area());
    let mut area = inset(frame.area(), 4, 2);
    if area.is_empty() {
        return;
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
        let consumed = banner_height.saturating_add(1).min(area.height);
        area.y = area.y.saturating_add(consumed);
        area.height = area.height.saturating_sub(consumed);
    }
    if area.is_empty() {
        return;
    }
    if !state.onboarding.is_selecting_connection() {
        let input = state.onboarding.display_input();
        let lines = vec![
            Line::from(Span::styled(
                state.onboarding.step_label(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                state.onboarding.prompt(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "›  ",
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
            Line::from(""),
            Line::from(Span::styled(
                "Enter confirms · Esc closes setup · secrets remain masked",
                Style::default().fg(DIM),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(BORDER))
                    .padding(Padding::new(2, 2, 1, 1)),
            ),
            area,
        );
        return;
    }
    let mut lines = vec![
        Line::from(Span::styled(
            state.onboarding.step_label(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "How should Kurama connect?",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Choose once. Projects remember the profile, not the secret.",
            Style::default().fg(DIM),
        )),
        Line::from(""),
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
                    .add_modifier(Modifier::BOLD),
            ),
            if selected {
                Span::styled("   SELECTED", Style::default().fg(ACCENT))
            } else {
                Span::raw("")
            },
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        connection_note(state.onboarding.selected()).trim_start(),
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "REMOTE-FIRST",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "   Kurama ships no model runtime. Local models connect through an existing endpoint.",
            Style::default().fg(DIM),
        ),
    ]));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER))
                .padding(Padding::new(2, 2, 1, 1)),
        ),
        area,
    );
}

fn render_agents(frame: &mut Frame<'_>, state: &TuiState) {
    frame.render_widget(Clear, frame.area());
    let frame_area = frame.area();
    let area = inset(
        frame_area,
        if frame_area.width >= 60 { 3 } else { 1 },
        if frame_area.height >= 20 { 2 } else { 1 },
    );
    if area.is_empty() {
        return;
    }
    let horizontal_padding = if area.width >= 48 { 2 } else { 1 };
    let vertical_padding = if area.height >= 10 { 1 } else { 0 };
    let block = Block::default()
        .borders(Borders::ALL)
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
            "ID        ROLE          PROFILE      TASK                  STATE",
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
        "enter  inspect   ↑↓  select   esc  close"
    } else {
        "↵ inspect  ↑↓  esc"
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
        AgentState::Failed | AgentState::Cancelled => DIM,
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

fn render_agent_inspect(frame: &mut Frame<'_>, state: &TuiState) {
    frame.render_widget(Clear, frame.area());
    let area = inset(frame.area(), 3, 2);
    if area.is_empty() {
        return;
    }
    let Some(agent) = state.selected_agent() else {
        return;
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("/AGENTS  /  ", Style::default().fg(ACCENT)),
            Span::styled(
                agent.id.to_string(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            agent.role.to_uppercase(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            agent.task.as_str(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("{}  ·  {:?}", agent.profile, agent.state),
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "CURRENT",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            agent.activity.as_str(),
            Style::default().fg(TEXT),
        )),
        Line::from(""),
    ];
    for line in &agent.transcript {
        lines.push(Line::from(Span::styled(
            format!("│ {line}"),
            Style::default().fg(TEXT),
        )));
        lines.push(Line::from("│"));
    }
    if state.overlay == Overlay::AgentMessage {
        lines.push(Line::from(Span::styled(
            format!("m  {}", state.agent_message),
            Style::default().fg(TEXT),
        )));
    } else if state.overlay == Overlay::ConfirmAgentCancel {
        lines.push(Line::from(Span::styled(
            format!("Cancel {}?  y confirm  ·  n/esc return", agent.id),
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "esc  agents     m  message     x  cancel agent",
            Style::default().fg(DIM),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER))
                .padding(Padding::new(2, 2, 1, 1)),
        ),
        area,
    );
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
