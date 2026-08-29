use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
};

use kurama_protocol::{agent::AgentState, policy::ExecutionMode, tool::Operation};

use super::{Overlay, TranscriptKind, TuiState};

const BORDER: Color = Color::Rgb(48, 53, 64);
const DIM: Color = Color::Rgb(126, 132, 146);
const TEXT: Color = Color::Rgb(224, 226, 232);
const RED: Color = Color::Rgb(255, 92, 82);
const AMBER: Color = Color::Rgb(220, 178, 73);
const GREEN: Color = Color::Rgb(111, 207, 151);
const BLUE: Color = Color::Rgb(116, 177, 255);
const TOOL_PREVIEW_LINES: usize = 6;
const TOOL_PREVIEW_HEAD: usize = 3;
const TOOL_PREVIEW_TAIL: usize = 2;
const APPROVAL_MAX_HEIGHT: usize = 14;
const APPROVAL_EDITOR_MIN_LINES: usize = 4;

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
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
    let approval_height = approval_height(state, frame.area().width);
    let input_height = if approval_height > 0 {
        approval_height
    } else {
        composer_height(state)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let header = Line::from(vec![
        Span::styled("●  ", Style::default().fg(RED)),
        Span::styled(
            "KURAMA",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {} · {}/{} · {}",
                state.project,
                state.profile,
                state.model,
                mode_label(state.mode)
            ),
            Style::default().fg(DIM),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(BORDER))
                .padding(Padding::new(2, 1, 0, 0)),
        ),
        chunks[0],
    );

    let transcript_width = chunks[1].width.saturating_sub(4) as usize;
    frame.render_widget(
        Paragraph::new(Text::from(transcript_lines(state, transcript_width)))
            .scroll((state.scroll, 0))
            .block(Block::default().padding(Padding::new(2, 2, 1, 0))),
        chunks[1],
    );

    if approval_height > 0 {
        if let Some(position) = render_approval(frame, state, chunks[2]) {
            frame.set_cursor_position(position);
        }
    } else {
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
                            Span::styled(
                                "›  ",
                                Style::default().fg(RED).add_modifier(Modifier::BOLD),
                            ),
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
    let area = inset(frame.area(), 4, 2);
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

fn render_approval(frame: &mut Frame<'_>, state: &TuiState, area: Rect) -> Option<Position> {
    let Some(approval) = &state.approval else {
        return None;
    };
    let area = inset(area, 2, 0);
    let layout = approval_layout(approval, area.width as usize, area.height as usize);
    frame.render_widget(Paragraph::new(layout.lines), area);
    layout.cursor.map(|(row, column)| {
        Position::new(
            area.x
                .saturating_add(column.min(area.width.saturating_sub(1))),
            area.y
                .saturating_add(row.min(area.height.saturating_sub(1))),
        )
    })
}

struct ApprovalLayout {
    lines: Vec<Line<'static>>,
    cursor: Option<(u16, u16)>,
}

fn approval_layout(
    approval: &super::ApprovalState,
    width: usize,
    max_height: usize,
) -> ApprovalLayout {
    let title = if approval.editing {
        "Edit arguments"
    } else {
        "Approval required"
    };
    let title = Line::from(vec![
        Span::styled("• ", Style::default().fg(RED).add_modifier(Modifier::BOLD)),
        Span::styled(
            title,
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
    ]);
    let summary = indented_prose_lines(&approval.request.summary, width, Style::default().fg(TEXT));
    let detail = indented_lines(
        &approval_detail(&approval.request.operation),
        width,
        Style::default().fg(DIM),
    );
    let controls = if approval.editing {
        indented_prose_lines("Enter submit · Esc return", width, Style::default().fg(DIM))
    } else {
        indented_prose_lines(
            "a approve once · d deny · e edit",
            width,
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        )
    };
    let max_height = max_height.max(1);
    let body_height = max_height.saturating_sub(1 + controls.len());
    let mut lines = vec![title];

    if approval.editing {
        let context_height = summary.len().saturating_add(detail.len());
        let editor_height = hard_wrap(&approval.editor, width.saturating_sub(2).max(1)).len();
        let context_budget = if context_height.saturating_add(editor_height) <= body_height {
            context_height
        } else {
            context_height.min(
                body_height.saturating_sub(
                    editor_height
                        .min(APPROVAL_EDITOR_MIN_LINES)
                        .min(body_height),
                ),
            )
        };
        lines.extend(bounded_approval_context(summary, detail, context_budget));
        let editor_budget = body_height.saturating_sub(context_budget);
        let editor_lines = editor_preview(
            &approval.editor,
            width.saturating_sub(2).max(1),
            editor_budget,
        );
        let cursor_row = lines
            .len()
            .saturating_add(editor_lines.len().saturating_sub(1)) as u16;
        let cursor_column = editor_lines
            .last()
            .map_or(2, |line| 2 + Line::from(line.as_str()).width() as u16);
        lines.extend(editor_lines.into_iter().map(|line| {
            Line::from(vec![
                Span::raw("  "),
                Span::styled(line, Style::default().fg(TEXT)),
            ])
        }));
        lines.extend(controls);
        ApprovalLayout {
            lines,
            cursor: Some((cursor_row, cursor_column)),
        }
    } else {
        lines.extend(bounded_approval_context(summary, detail, body_height));
        lines.extend(controls);
        ApprovalLayout {
            lines,
            cursor: None,
        }
    }
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

fn indented_lines(value: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    hard_wrap(value, width.saturating_sub(2).max(1))
        .into_iter()
        .map(|line| Line::from(vec![Span::raw("  "), Span::styled(line, style)]))
        .collect()
}

fn indented_prose_lines(value: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    word_wrap(value, width.saturating_sub(2).max(1))
        .into_iter()
        .map(|line| Line::from(vec![Span::raw("  "), Span::styled(line, style)]))
        .collect()
}

fn bounded_approval_context(
    summary: Vec<Line<'static>>,
    detail: Vec<Line<'static>>,
    height: usize,
) -> Vec<Line<'static>> {
    if summary.len().saturating_add(detail.len()) <= height {
        return summary.into_iter().chain(detail).collect();
    }

    let detail_reserve = usize::from(!detail.is_empty() && height > 1);
    let summary_height = summary.len().min(height.saturating_sub(detail_reserve));
    let detail_height = detail.len().min(height.saturating_sub(summary_height));
    summary
        .into_iter()
        .take(summary_height)
        .chain(detail.into_iter().take(detail_height))
        .collect()
}

fn render_agents(frame: &mut Frame<'_>, state: &TuiState) {
    frame.render_widget(Clear, frame.area());
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

fn panel_block() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .padding(Padding::new(1, 1, 0, 0))
}

fn transcript_lines(state: &TuiState, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in &state.transcript {
        match entry.kind {
            TranscriptKind::User => push_prefixed_lines(
                &mut lines,
                &entry.body,
                "› ",
                "  ",
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
                Style::default().fg(TEXT),
                width,
            ),
            TranscriptKind::Assistant => {
                for line in hard_wrap(&entry.body, width) {
                    lines.push(Line::from(Span::styled(line, Style::default().fg(TEXT))));
                }
            }
            TranscriptKind::Tool => {
                push_prefixed_lines(
                    &mut lines,
                    &format!("Ran {}", tool_name(&entry.label)),
                    "• ",
                    "  ",
                    Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                    width,
                );
                let output_width = width.saturating_sub(4).max(1);
                for (index, line) in tool_output_preview(&entry.body, output_width)
                    .into_iter()
                    .enumerate()
                {
                    lines.push(Line::from(vec![
                        Span::styled(
                            if index == 0 { "  └ " } else { "    " },
                            Style::default().fg(DIM),
                        ),
                        Span::styled(line, Style::default().fg(DIM)),
                    ]));
                }
            }
            TranscriptKind::System => push_prefixed_lines(
                &mut lines,
                &format!("{} · {}", entry.label, entry.body),
                "• ",
                "  ",
                Style::default().fg(DIM),
                Style::default().fg(DIM),
                width,
            ),
        }
        lines.push(Line::from(""));
    }
    lines
}

fn push_prefixed_lines(
    lines: &mut Vec<Line<'static>>,
    body: &str,
    first_prefix: &'static str,
    continuation_prefix: &'static str,
    prefix_style: Style,
    body_style: Style,
    width: usize,
) {
    let prefix_width = Line::from(first_prefix).width();
    for (index, line) in hard_wrap(body, width.saturating_sub(prefix_width).max(1))
        .into_iter()
        .enumerate()
    {
        lines.push(Line::from(vec![
            Span::styled(
                if index == 0 {
                    first_prefix
                } else {
                    continuation_prefix
                },
                prefix_style,
            ),
            Span::styled(line, body_style),
        ]));
    }
}

fn hard_wrap(value: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut wrapped = Vec::new();
    for source_line in value.split('\n') {
        if source_line.is_empty() {
            wrapped.push(String::new());
            continue;
        }

        let mut line = String::new();
        let mut line_width = 0_usize;
        for character in source_line.chars() {
            let character_width = Line::from(character.to_string()).width();
            if line_width > 0 && line_width.saturating_add(character_width) > width {
                wrapped.push(std::mem::take(&mut line));
                line_width = 0;
            }
            line.push(character);
            line_width = line_width.saturating_add(character_width);
        }
        wrapped.push(line);
    }
    wrapped
}

fn word_wrap(value: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut wrapped = Vec::new();
    for source_line in value.split('\n') {
        let mut line = String::new();
        let mut line_width = 0_usize;
        for word in source_line.split_whitespace() {
            let word_width = Line::from(word).width();
            if word_width > width {
                if !line.is_empty() {
                    wrapped.push(std::mem::take(&mut line));
                    line_width = 0;
                }
                let mut chunks = hard_wrap(word, width);
                if let Some(last) = chunks.pop() {
                    wrapped.extend(chunks);
                    line_width = Line::from(last.as_str()).width();
                    line = last;
                }
            } else if line.is_empty() {
                line.push_str(word);
                line_width = word_width;
            } else if line_width.saturating_add(1 + word_width) <= width {
                line.push(' ');
                line.push_str(word);
                line_width = line_width.saturating_add(1 + word_width);
            } else {
                wrapped.push(std::mem::take(&mut line));
                line.push_str(word);
                line_width = word_width;
            }
        }
        if !line.is_empty() {
            wrapped.push(line);
        } else if source_line.is_empty() {
            wrapped.push(String::new());
        }
    }
    wrapped
}

fn tool_output_preview(output: &str, width: usize) -> Vec<String> {
    let mut wrapped = hard_wrap(output, width);
    if wrapped.len() <= TOOL_PREVIEW_LINES {
        return wrapped;
    }

    let omitted = wrapped.len() - TOOL_PREVIEW_HEAD - TOOL_PREVIEW_TAIL;
    let tail = wrapped.split_off(wrapped.len() - TOOL_PREVIEW_TAIL);
    wrapped.truncate(TOOL_PREVIEW_HEAD);
    wrapped.push(format!("… {omitted} lines omitted …"));
    wrapped.extend(tail);
    wrapped
}

fn editor_preview(editor: &str, width: usize, max_lines: usize) -> Vec<String> {
    if max_lines == 0 {
        return Vec::new();
    }
    let wrapped = hard_wrap(editor, width);
    if wrapped.len() <= max_lines {
        return wrapped;
    }

    let omitted = wrapped.len() - max_lines.saturating_sub(1);
    let mut visible = vec![format!("… {omitted} lines above …")];
    visible.extend(
        wrapped
            .into_iter()
            .skip(omitted)
            .take(max_lines.saturating_sub(1)),
    );
    visible
}

fn tool_name(label: &str) -> String {
    let mut parts = label.split('/').map(str::trim);
    let first = parts.next().unwrap_or("tool");
    let name = if first.eq_ignore_ascii_case("tool") {
        parts.next().unwrap_or(first)
    } else {
        first
    };
    name.to_ascii_lowercase()
}

fn approval_height(state: &TuiState, terminal_width: u16) -> u16 {
    if !matches!(state.overlay, Overlay::Approval | Overlay::ApprovalEdit) {
        return 0;
    }

    state.approval.as_ref().map_or(0, |approval| {
        approval_layout(
            approval,
            terminal_width.saturating_sub(4) as usize,
            APPROVAL_MAX_HEIGHT,
        )
        .lines
        .len() as u16
    })
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
    Style::default().fg(match mode {
        ExecutionMode::Supervised => GREEN,
        ExecutionMode::Auto => AMBER,
        ExecutionMode::Yolo => RED,
    })
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
