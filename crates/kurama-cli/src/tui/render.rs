use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
};

use kurama_protocol::{agent::AgentState, policy::ExecutionMode, tool::Operation};

use super::{Overlay, TranscriptKind, TuiState};

const BG: Color = Color::Rgb(7, 9, 12);
const PANEL: Color = Color::Rgb(15, 18, 24);
const BORDER: Color = Color::Rgb(48, 53, 64);
const DIM: Color = Color::Rgb(126, 132, 146);
const TEXT: Color = Color::Rgb(224, 226, 232);
const RED: Color = Color::Rgb(255, 92, 82);
const AMBER: Color = Color::Rgb(220, 178, 73);
const GREEN: Color = Color::Rgb(111, 207, 151);
const BLUE: Color = Color::Rgb(116, 177, 255);

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    frame.render_widget(
        Block::default().style(Style::default().bg(BG)),
        frame.area(),
    );
    render_main(frame, state);
    match state.overlay {
        Overlay::None => {}
        Overlay::Onboarding => render_onboarding(frame, state),
        Overlay::Approval | Overlay::ApprovalEdit => render_approval(frame, state),
        Overlay::Agents => render_agents(frame, state),
        Overlay::AgentInspect | Overlay::AgentMessage | Overlay::ConfirmAgentCancel => {
            render_agent_inspect(frame, state)
        }
    }
}

fn render_main(frame: &mut Frame<'_>, state: &TuiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(composer_height(state)),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let header = Line::from(vec![
        Span::styled("●  ", Style::default().fg(RED)),
        Span::styled(
            "KURAMA",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  /  ACTIVE SESSION", Style::default().fg(DIM)),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(BORDER))
                .padding(Padding::new(2, 1, 1, 0)),
        ),
        chunks[0],
    );

    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                mode_label(state.mode),
                mode_style(state.mode).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("     {}  /  {}", state.profile, state.model),
                Style::default().fg(DIM),
            ),
        ]),
        Line::from(""),
    ];
    for entry in &state.transcript {
        let color = match entry.kind {
            TranscriptKind::User => RED,
            TranscriptKind::Assistant => AMBER,
            TranscriptKind::Tool => BLUE,
            TranscriptKind::System => DIM,
        };
        lines.push(Line::from(Span::styled(
            format!("│ {}  /", entry.label),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            format!("│ {}", entry.body),
            Style::default().fg(TEXT),
        )));
        lines.push(Line::from("│"));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((state.scroll, 0))
            .block(Block::default().padding(Padding::new(2, 2, 1, 1))),
        chunks[1],
    );

    let composer = if state.composer.is_empty() {
        Text::from(Line::from(vec![
            Span::styled("›  ", Style::default().fg(RED).add_modifier(Modifier::BOLD)),
            Span::styled(
                "Message Kurama or type / for commands",
                Style::default().fg(DIM),
            ),
        ]))
    } else {
        Text::from(
            state
                .composer
                .split('\n')
                .map(|line| {
                    Line::from(vec![
                        Span::styled("›  ", Style::default().fg(RED).add_modifier(Modifier::BOLD)),
                        Span::styled(line, Style::default().fg(TEXT)),
                    ])
                })
                .collect::<Vec<_>>(),
        )
    };
    let composer_area = inset(chunks[2], 2, 0);
    let composer_block = panel_block();
    let composer_inner = composer_block.inner(composer_area);
    frame.render_widget(
        Paragraph::new(composer)
            .wrap(Wrap { trim: false })
            .block(composer_block),
        composer_area,
    );
    if state.overlay == Overlay::None
        && let Some(position) = composer_cursor_position(state, composer_inner)
    {
        frame.set_cursor_position(position);
    }

    let agents = format!(
        "agents {} running · {} queued",
        state.running_agents, state.queued_agents
    );
    let footer = if chunks[3].width < 100 {
        Line::from(vec![
            Span::styled(
                format!("  {}/{}", state.profile, state.model),
                Style::default().fg(DIM),
            ),
            Span::raw("  "),
            Span::styled(mode_label(state.mode), mode_style(state.mode)),
            Span::raw("  "),
            Span::styled(
                agents,
                Style::default().fg(if state.running_agents > 0 { RED } else { DIM }),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(format!("  {}", state.project), Style::default().fg(DIM)),
            Span::raw("    "),
            Span::styled(
                format!("{}/{}", state.profile, state.model),
                Style::default().fg(DIM),
            ),
            Span::raw("    "),
            Span::styled(mode_label(state.mode), mode_style(state.mode)),
            Span::raw("    "),
            Span::styled(
                agents,
                Style::default().fg(if state.running_agents > 0 { RED } else { DIM }),
            ),
            Span::raw("    "),
            Span::styled(state.status.as_str(), Style::default().fg(DIM)),
        ])
    };
    frame.render_widget(Paragraph::new(footer), chunks[3]);
}

fn render_onboarding(frame: &mut Frame<'_>, state: &TuiState) {
    frame.render_widget(Clear, frame.area());
    frame.render_widget(
        Block::default().style(Style::default().bg(BG)),
        frame.area(),
    );
    let area = inset(frame.area(), 4, 2);
    let mut lines = vec![
        Line::from(Span::styled(
            "STEP 1 / 3",
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
                    .bg(if selected {
                        Color::Rgb(78, 30, 28)
                    } else {
                        PANEL
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
        lines.push(Line::from(Span::styled(
            connection_note(index),
            Style::default().fg(DIM),
        )));
        lines.push(Line::from(""));
    }
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

fn render_approval(frame: &mut Frame<'_>, state: &TuiState) {
    frame.render_widget(Clear, frame.area());
    frame.render_widget(
        Block::default().style(Style::default().bg(BG)),
        frame.area(),
    );
    let area = inset(frame.area(), 4, 2);
    let Some(approval) = &state.approval else {
        return;
    };
    let detail = match &approval.request.operation {
        Operation::Read { path, .. } => format!("READ    {}", path.display()),
        Operation::Write { paths, .. } => format!(
            "WRITE   {}",
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Operation::Bash { command, .. } => format!("BASH    {command}"),
        Operation::WebSearch { query, .. } => format!("SEARCH  {query}"),
        Operation::WebOpen { url, .. } => format!("OPEN    {url}"),
    };
    let mut lines = vec![
        Line::from(Span::styled(
            "APPROVAL REQUIRED",
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            approval.request.summary.as_str(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(detail, Style::default().fg(TEXT))),
        Line::from(""),
    ];
    if approval.editing {
        lines.push(Line::from(Span::styled(
            "EDIT JSON",
            Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
        )));
        lines.extend(
            approval
                .editor
                .lines()
                .map(|line| Line::from(Span::styled(line.to_owned(), Style::default().fg(TEXT)))),
        );
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "enter  submit edited arguments    esc  return",
            Style::default().fg(DIM),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "A  APPROVE ONCE     D  DENY     E  EDIT JSON",
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "Edited arguments are reclassified and rechecked by policy.",
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

fn render_agents(frame: &mut Frame<'_>, state: &TuiState) {
    frame.render_widget(Clear, frame.area());
    frame.render_widget(
        Block::default().style(Style::default().bg(BG)),
        frame.area(),
    );
    let area = inset(frame.area(), 3, 2);
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
            "ID        ROLE          MODEL        TASK                  SCOPE          TIME    STATE",
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
                    "{:<10}{:<14}{:<13}{:<22}{:<15}{:<8}",
                    agent.id,
                    truncate(&agent.role, 12),
                    truncate(&agent.profile, 11),
                    truncate(&agent.task, 20),
                    truncate(&agent.scope, 13),
                    agent.elapsed
                ),
                Style::default()
                    .fg(if selected { TEXT } else { DIM })
                    .bg(if selected { PANEL } else { BG }),
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
    frame.render_widget(
        Block::default().style(Style::default().bg(BG)),
        frame.area(),
    );
    let area = inset(frame.area(), 3, 2);
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
            format!(
                "{}  ·  scope {}  ·  elapsed {}",
                agent.profile, agent.scope, agent.elapsed
            ),
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
            Style::default().fg(TEXT).bg(PANEL),
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

fn panel_block() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(PANEL))
        .padding(Padding::new(1, 1, 0, 0))
}

fn composer_height(state: &TuiState) -> u16 {
    (state.composer.lines().count().max(1) as u16 + 2).clamp(3, 8)
}

fn composer_cursor_position(state: &TuiState, area: Rect) -> Option<Position> {
    if area.is_empty() {
        return None;
    }

    let mut cursor = state.cursor.min(state.composer.len());
    while !state.composer.is_char_boundary(cursor) {
        cursor = cursor.saturating_sub(1);
    }

    let prefix_width = Line::from("›  ").width() as u16;
    let mut row = 0_u16;
    let mut lines = state.composer[..cursor].split('\n').peekable();
    while let Some(line) = lines.next() {
        let line_width = prefix_width.saturating_add(Line::from(line).width() as u16);
        if lines.peek().is_some() {
            row = row.saturating_add(line_width.div_ceil(area.width).max(1));
            continue;
        }

        row = row.saturating_add(line_width / area.width);
        let column = line_width % area.width;
        return Some(Position::new(
            area.x.saturating_add(column),
            area.y
                .saturating_add(row.min(area.height.saturating_sub(1))),
        ));
    }

    Some(Position::new(
        area.x
            .saturating_add(prefix_width.min(area.width.saturating_sub(1))),
        area.y,
    ))
}

fn mode_label(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Supervised => "SUPERVISED",
        ExecutionMode::Auto => "AUTO",
        ExecutionMode::Yolo => "YOLO",
    }
}

fn mode_style(mode: ExecutionMode) -> Style {
    Style::default()
        .fg(match mode {
            ExecutionMode::Supervised => GREEN,
            ExecutionMode::Auto => AMBER,
            ExecutionMode::Yolo => Color::Black,
        })
        .bg(if mode == ExecutionMode::Yolo { RED } else { BG })
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
    if value.chars().count() <= width {
        return value.to_owned();
    }
    value
        .chars()
        .take(width.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

fn inset(area: Rect, horizontal: u16, vertical: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(horizontal),
        y: area.y.saturating_add(vertical),
        width: area.width.saturating_sub(horizontal.saturating_mul(2)),
        height: area.height.saturating_sub(vertical.saturating_mul(2)),
    }
}
