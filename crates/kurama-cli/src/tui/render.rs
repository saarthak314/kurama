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
    Overlay, ResponsiveLayout, TuiState, activity_line,
    composer::{approval_height, composer_height, render_approval, render_composer, render_footer},
    transcript::{TranscriptDetail, render_transcript_view, transcript_lines, truncate_display},
};

const BORDER: Color = Color::DarkGray;
const DIM: Color = Color::DarkGray;
const TEXT: Color = Color::Reset;
pub(crate) const SURFACE: Color = Color::Reset;
const RED: Color = Color::Red;
const AMBER: Color = Color::Yellow;
const GREEN: Color = Color::Cyan;

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    if frame.area().is_empty() {
        return;
    }
    if state.transcript_view_expanded() {
        render_transcript_view(frame, state);
        return;
    }
    render_main(frame, state);
    match state.overlay {
        Overlay::None | Overlay::Approval | Overlay::ApprovalEdit => {}
        Overlay::Onboarding => render_onboarding(frame, state),
        Overlay::Agents => render_agents(frame, state),
        Overlay::AgentInspect | Overlay::AgentMessage | Overlay::ConfirmAgentCancel => {
            render_agent_inspect(frame, state)
        }
    }
}

fn render_main(frame: &mut Frame<'_>, state: &TuiState) {
    let area = main_area(frame.area());
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
    let activity = (!approval_visible)
        .then(|| activity_line(state.activity(), area.width as usize, now))
        .flatten();
    let layout = ResponsiveLayout::for_area(area, input_height, activity.is_some());

    if !layout.transcript.is_empty() {
        let transcript = transcript_lines(
            state.live_transcript(),
            layout.transcript.width as usize,
            TranscriptDetail::Compact,
        );
        let viewport_height = layout.transcript.height as usize;
        let scroll = state
            .scroll
            .min(transcript.len().saturating_sub(viewport_height));
        let start = transcript
            .len()
            .saturating_sub(viewport_height.saturating_add(scroll));
        let transcript = transcript
            .into_iter()
            .skip(start)
            .take(viewport_height)
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
}

fn main_area(area: Rect) -> Rect {
    let horizontal = if area.width > 4 { 2 } else { 0 };
    Rect::new(
        area.x.saturating_add(horizontal),
        area.y,
        area.width.saturating_sub(horizontal.saturating_mul(2)),
        area.height,
    )
}

fn render_onboarding(frame: &mut Frame<'_>, state: &TuiState) {
    frame.render_widget(Clear, frame.area());
    let area = inset(frame.area(), 4, 2);
    if area.is_empty() {
        return;
    }
    if !state.onboarding.is_selecting_connection() {
        let input = state.onboarding.display_input();
        let lines = vec![
            Line::from(Span::styled(
                state.onboarding.step_label(),
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                state.onboarding.prompt(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("›  ", Style::default().fg(RED).add_modifier(Modifier::BOLD)),
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
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
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
                Span::styled("   SELECTED", Style::default().fg(RED))
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
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
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
    let area = inset(frame.area(), 3, 2);
    if area.is_empty() {
        return;
    }
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                "/AGENTS",
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "    {} running · {} queued",
                    state.running_agents, state.queued_agents
                ),
                Style::default().fg(DIM),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Sub-agents",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Explicitly delegated children. No nested agents.",
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "ID        ROLE          PROFILE      TASK                  STATE",
            Style::default().fg(DIM),
        )),
    ];
    for (index, agent) in state.agents.iter().enumerate() {
        let selected = index == state.selected_agent;
        let state_text = format!("{:?}", agent.state).to_uppercase();
        let state_color = match agent.state {
            AgentState::Running => RED,
            AgentState::Queued => AMBER,
            AgentState::Completed => GREEN,
            AgentState::Failed | AgentState::Cancelled => DIM,
        };
        lines.push(Line::from(vec![
            Span::styled(if selected { "▶ " } else { "  " }, Style::default().fg(RED)),
            Span::styled(
                format!(
                    "{:<10}{:<14}{:<13}{:<22}",
                    agent.id,
                    truncate(&agent.role, 12),
                    truncate(&agent.profile, 11),
                    truncate(&agent.task, 20)
                ),
                Style::default().fg(if selected { TEXT } else { DIM }),
            ),
            Span::styled(
                state_text,
                Style::default()
                    .fg(state_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines.push(Line::from(""));
    if let Some(agent) = state.selected_agent() {
        lines.push(Line::from(Span::styled(
            format!("SELECTED  {}", agent.id),
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            agent.activity.as_str(),
            Style::default().fg(TEXT),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "enter  inspect     m  message     x  cancel     ↑↓  select     esc  close",
        Style::default().fg(DIM),
    )));
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
            Span::styled("/AGENTS  /  ", Style::default().fg(RED)),
            Span::styled(
                agent.id.to_string(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            agent.role.to_uppercase(),
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
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
            Style::default().fg(AMBER).add_modifier(Modifier::BOLD),
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
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
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
