use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Padding, Paragraph, Wrap},
};

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use unicode_segmentation::UnicodeSegmentation;

use kurama_protocol::{agent::AgentState, policy::ExecutionMode, tool::Operation};

use super::{Overlay, TranscriptEntry, TuiState};

const BORDER: Color = Color::Rgb(48, 53, 64);
const DIM: Color = Color::Rgb(126, 132, 146);
const TEXT: Color = Color::Rgb(224, 226, 232);
pub(crate) const SURFACE: Color = Color::Rgb(13, 16, 22);
const RED: Color = Color::Rgb(255, 92, 82);
const AMBER: Color = Color::Rgb(220, 178, 73);
const GREEN: Color = Color::Rgb(111, 207, 151);
const BLUE: Color = Color::Rgb(116, 177, 255);
const APPROVAL_MAX_HEIGHT: usize = 14;
const APPROVAL_EDITOR_MIN_LINES: usize = 4;

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    paint_surface(frame);
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

fn paint_surface(frame: &mut Frame<'_>) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(surface_style()), area);
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

    let transcript_block = Block::default().padding(Padding::new(2, 2, 1, 0));
    let transcript_area = transcript_block.inner(chunks[1]);
    let transcript_width = transcript_area.width as usize;
    let transcript = transcript_lines(state.live_transcript(), transcript_width);
    let viewport_height = transcript_area.height as usize;
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
    frame.render_widget(
        Paragraph::new(Text::from(transcript)).block(transcript_block),
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
    paint_surface(frame);
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
    paint_surface(frame);
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
    paint_surface(frame);
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

#[derive(Clone)]
struct StyledFragment {
    content: String,
    style: Style,
}

enum MarkdownBlock {
    Paragraph(Vec<StyledFragment>),
    Heading(HeadingLevel, Vec<StyledFragment>),
    Quote(Vec<MarkdownBlock>),
    Code {
        language: Option<String>,
        content: String,
    },
    List {
        start: Option<u64>,
        items: Vec<Vec<MarkdownBlock>>,
    },
    Rule,
    Table {
        alignments: Vec<Alignment>,
        header: Vec<Vec<StyledFragment>>,
        rows: Vec<Vec<Vec<StyledFragment>>>,
    },
}

#[derive(Clone, Copy, Default)]
struct MarkdownContext {
    quote_depth: usize,
    indent: usize,
}

fn markdown_lines(markdown: &str, width: usize) -> Vec<Line<'static>> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let events = Parser::new_ext(markdown, options).collect::<Vec<_>>();
    let mut index = 0;
    let blocks = parse_markdown_blocks(&events, &mut index, None);
    let mut lines = Vec::new();
    render_markdown_blocks(
        &blocks,
        width.max(1),
        MarkdownContext::default(),
        true,
        &mut lines,
    );
    while lines.last().is_some_and(|line| line.width() == 0) {
        lines.pop();
    }
    lines
}

fn parse_markdown_blocks<'a>(
    events: &[Event<'a>],
    index: &mut usize,
    end: Option<TagEnd>,
) -> Vec<MarkdownBlock> {
    let mut blocks = Vec::new();
    while *index < events.len() {
        let event = events[*index].clone();
        *index += 1;
        match event {
            Event::End(tag) if Some(tag) == end => break,
            Event::Start(Tag::Paragraph) => blocks.push(MarkdownBlock::Paragraph(
                parse_inline_fragments(events, index, TagEnd::Paragraph, text_style()),
            )),
            Event::Start(Tag::Heading { level, .. }) => {
                blocks.push(MarkdownBlock::Heading(
                    level,
                    parse_inline_fragments(events, index, TagEnd::Heading(level), text_style()),
                ));
            }
            Event::Start(Tag::BlockQuote(kind)) => blocks.push(MarkdownBlock::Quote(
                parse_markdown_blocks(events, index, Some(TagEnd::BlockQuote(kind))),
            )),
            Event::Start(Tag::CodeBlock(kind)) => {
                let language = match kind {
                    CodeBlockKind::Fenced(language) if !language.trim().is_empty() => {
                        language.split_whitespace().next().map(str::to_owned)
                    }
                    CodeBlockKind::Indented | CodeBlockKind::Fenced(_) => None,
                };
                blocks.push(MarkdownBlock::Code {
                    language,
                    content: parse_code_block(events, index),
                });
            }
            Event::Start(Tag::List(start)) => blocks.push(MarkdownBlock::List {
                start,
                items: parse_list_items(events, index, start.is_some()),
            }),
            Event::Start(Tag::Table(alignments)) => {
                let (header, rows) = parse_table(events, index);
                blocks.push(MarkdownBlock::Table {
                    alignments,
                    header,
                    rows,
                });
            }
            Event::Start(Tag::HtmlBlock) => {
                blocks.push(MarkdownBlock::Paragraph(vec![StyledFragment {
                    content: parse_html_block(events, index),
                    style: text_style(),
                }]))
            }
            Event::Rule => blocks.push(MarkdownBlock::Rule),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                blocks.push(MarkdownBlock::Paragraph(vec![StyledFragment {
                    content: text.into_string(),
                    style: text_style(),
                }]));
            }
            Event::Code(code) => blocks.push(MarkdownBlock::Paragraph(vec![StyledFragment {
                content: code.into_string(),
                style: inline_code_style(),
            }])),
            Event::SoftBreak | Event::HardBreak => {
                blocks.push(MarkdownBlock::Paragraph(Vec::new()));
            }
            Event::Start(_)
            | Event::End(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_) => {}
        }
    }
    blocks
}

fn parse_inline_fragments<'a>(
    events: &[Event<'a>],
    index: &mut usize,
    end: TagEnd,
    style: Style,
) -> Vec<StyledFragment> {
    let mut fragments = Vec::new();
    while *index < events.len() {
        let event = events[*index].clone();
        *index += 1;
        match event {
            Event::End(tag) if tag == end => break,
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                push_fragment(&mut fragments, text.into_string(), style);
            }
            Event::Code(code) => {
                push_fragment(
                    &mut fragments,
                    code.into_string(),
                    style.patch(inline_code_style()),
                );
            }
            Event::SoftBreak => push_fragment(&mut fragments, " ".into(), style),
            Event::HardBreak => push_fragment(&mut fragments, "\n".into(), style),
            Event::Start(Tag::Emphasis) => fragments.extend(parse_inline_fragments(
                events,
                index,
                TagEnd::Emphasis,
                style.add_modifier(Modifier::ITALIC),
            )),
            Event::Start(Tag::Strong) => fragments.extend(parse_inline_fragments(
                events,
                index,
                TagEnd::Strong,
                style.add_modifier(Modifier::BOLD),
            )),
            Event::Start(Tag::Strikethrough) => fragments.extend(parse_inline_fragments(
                events,
                index,
                TagEnd::Strikethrough,
                style.add_modifier(Modifier::CROSSED_OUT),
            )),
            Event::Start(Tag::Link { dest_url, .. }) => {
                let destination = dest_url.into_string();
                let linked = parse_inline_fragments(
                    events,
                    index,
                    TagEnd::Link,
                    style.fg(BLUE).add_modifier(Modifier::UNDERLINED),
                );
                let label = fragments_text(&linked);
                fragments.extend(linked);
                if !destination.is_empty() && label != destination {
                    push_fragment(
                        &mut fragments,
                        format!(" ({destination})"),
                        Style::default().fg(DIM),
                    );
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                let destination = dest_url.into_string();
                let alternate = parse_inline_fragments(
                    events,
                    index,
                    TagEnd::Image,
                    style.add_modifier(Modifier::ITALIC),
                );
                fragments.extend(alternate);
                if !destination.is_empty() {
                    push_fragment(
                        &mut fragments,
                        format!(" (image: {destination})"),
                        Style::default().fg(DIM),
                    );
                }
            }
            Event::TaskListMarker(checked) => push_fragment(
                &mut fragments,
                if checked { "☑ " } else { "☐ " }.into(),
                style.fg(if checked { GREEN } else { DIM }),
            ),
            Event::InlineMath(value)
            | Event::DisplayMath(value)
            | Event::FootnoteReference(value) => {
                push_fragment(&mut fragments, value.into_string(), style);
            }
            Event::Start(tag) => {
                fragments.extend(parse_inline_fragments(events, index, tag.to_end(), style));
            }
            Event::Rule | Event::End(_) => {}
        }
    }
    fragments
}

fn parse_code_block<'a>(events: &[Event<'a>], index: &mut usize) -> String {
    let mut content = String::new();
    while *index < events.len() {
        let event = events[*index].clone();
        *index += 1;
        match event {
            Event::End(TagEnd::CodeBlock) => break,
            Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) => {
                content.push_str(&text)
            }
            Event::SoftBreak | Event::HardBreak => content.push('\n'),
            Event::Start(_)
            | Event::End(_)
            | Event::Rule
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_) => {}
        }
    }
    content.trim_end_matches('\n').to_owned()
}

fn parse_html_block<'a>(events: &[Event<'a>], index: &mut usize) -> String {
    let mut content = String::new();
    while *index < events.len() {
        let event = events[*index].clone();
        *index += 1;
        match event {
            Event::End(TagEnd::HtmlBlock) => break,
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                content.push_str(&text);
            }
            Event::SoftBreak | Event::HardBreak => content.push('\n'),
            _ => {}
        }
    }
    content
}

fn parse_list_items<'a>(
    events: &[Event<'a>],
    index: &mut usize,
    ordered: bool,
) -> Vec<Vec<MarkdownBlock>> {
    let mut items = Vec::new();
    while *index < events.len() {
        let event = events[*index].clone();
        *index += 1;
        match event {
            Event::Start(Tag::Item) => {
                items.push(parse_markdown_blocks(events, index, Some(TagEnd::Item)))
            }
            Event::End(TagEnd::List(is_ordered)) if is_ordered == ordered => break,
            _ => {}
        }
    }
    items
}

fn parse_table<'a>(
    events: &[Event<'a>],
    index: &mut usize,
) -> (Vec<Vec<StyledFragment>>, Vec<Vec<Vec<StyledFragment>>>) {
    let mut header = Vec::new();
    let mut rows = Vec::new();
    while *index < events.len() {
        let event = events[*index].clone();
        *index += 1;
        match event {
            Event::Start(Tag::TableHead) => {
                header = parse_table_cells(events, index, TagEnd::TableHead);
            }
            Event::Start(Tag::TableRow) => {
                rows.push(parse_table_cells(events, index, TagEnd::TableRow));
            }
            Event::End(TagEnd::Table) => break,
            _ => {}
        }
    }
    (header, rows)
}

fn parse_table_cells<'a>(
    events: &[Event<'a>],
    index: &mut usize,
    end: TagEnd,
) -> Vec<Vec<StyledFragment>> {
    let mut cells = Vec::new();
    while *index < events.len() {
        let event = events[*index].clone();
        *index += 1;
        match event {
            Event::Start(Tag::TableCell) => cells.push(parse_inline_fragments(
                events,
                index,
                TagEnd::TableCell,
                text_style(),
            )),
            Event::End(tag) if tag == end => break,
            _ => {}
        }
    }
    cells
}

fn render_markdown_blocks(
    blocks: &[MarkdownBlock],
    width: usize,
    context: MarkdownContext,
    separated: bool,
    lines: &mut Vec<Line<'static>>,
) {
    for (index, block) in blocks.iter().enumerate() {
        if separated && index > 0 {
            push_markdown_blank(lines, context);
        }
        render_markdown_block(block, width, context, None, lines);
    }
}

fn render_markdown_block(
    block: &MarkdownBlock,
    width: usize,
    context: MarkdownContext,
    marker: Option<&str>,
    lines: &mut Vec<Line<'static>>,
) {
    match block {
        MarkdownBlock::Paragraph(fragments) => {
            render_fragments(fragments, width, context, marker, None, lines);
        }
        MarkdownBlock::Heading(level, fragments) => {
            let heading_style = Style::default()
                .fg(if *level == HeadingLevel::H1 {
                    RED
                } else {
                    TEXT
                })
                .add_modifier(Modifier::BOLD);
            let fragments = fragments
                .iter()
                .cloned()
                .map(|fragment| StyledFragment {
                    content: fragment.content,
                    style: fragment.style.patch(heading_style),
                })
                .collect::<Vec<_>>();
            render_fragments(&fragments, width, context, marker, None, lines);
        }
        MarkdownBlock::Quote(blocks) => {
            if marker.is_some() {
                render_fragments(&[], width, context, marker, None, lines);
            }
            let nested = MarkdownContext {
                quote_depth: context.quote_depth + 1,
                ..context
            };
            render_markdown_blocks(blocks, width, nested, true, lines);
        }
        MarkdownBlock::Code { language, content } => {
            if let Some(language) = language {
                render_fragments(
                    &[StyledFragment {
                        content: language.clone(),
                        style: Style::default().fg(DIM).add_modifier(Modifier::BOLD),
                    }],
                    width,
                    context,
                    marker,
                    None,
                    lines,
                );
            }
            let code_marker = if language.is_some() { None } else { marker };
            render_hard_fragments(
                &[StyledFragment {
                    content: content.clone(),
                    style: code_style(),
                }],
                width,
                context,
                code_marker,
                Some(("│ ", "│ ", Style::default().fg(DIM))),
                lines,
            );
        }
        MarkdownBlock::List { start, items } => {
            render_markdown_list(*start, items, width, context, marker, lines);
        }
        MarkdownBlock::Rule => {
            let (mut prefix, _) = markdown_prefixes(context, marker, None);
            let prefix_width = fragments_width(&prefix);
            prefix.push(StyledFragment {
                content: "─".repeat(width.saturating_sub(prefix_width).max(3)),
                style: Style::default().fg(BORDER),
            });
            lines.push(Line::from(fragments_into_spans(prefix)));
        }
        MarkdownBlock::Table {
            alignments,
            header,
            rows,
        } => render_markdown_table(alignments, header, rows, width, context, marker, lines),
    }
}

fn render_markdown_list(
    start: Option<u64>,
    items: &[Vec<MarkdownBlock>],
    width: usize,
    context: MarkdownContext,
    outer_marker: Option<&str>,
    lines: &mut Vec<Line<'static>>,
) {
    if outer_marker.is_some() {
        render_fragments(&[], width, context, outer_marker, None, lines);
    }
    for (item_index, item) in items.iter().enumerate() {
        let marker = start.map_or_else(
            || "• ".to_owned(),
            |number| format!("{}. ", number + item_index as u64),
        );
        if item.is_empty() {
            render_fragments(&[], width, context, Some(&marker), None, lines);
            continue;
        }
        for (block_index, block) in item.iter().enumerate() {
            if block_index == 0 {
                render_markdown_block(block, width, context, Some(&marker), lines);
            } else {
                render_markdown_block(
                    block,
                    width,
                    MarkdownContext {
                        indent: context.indent + display_width(&marker),
                        ..context
                    },
                    None,
                    lines,
                );
            }
        }
    }
}

fn render_fragments(
    fragments: &[StyledFragment],
    width: usize,
    context: MarkdownContext,
    marker: Option<&str>,
    decoration: Option<(&str, &str, Style)>,
    lines: &mut Vec<Line<'static>>,
) {
    let (mut first_prefix, mut continuation_prefix) =
        markdown_prefixes(context, marker, decoration);
    if let Some((first, continuation, style)) = decoration {
        first_prefix.push(StyledFragment {
            content: first.to_owned(),
            style,
        });
        continuation_prefix.push(StyledFragment {
            content: continuation.to_owned(),
            style,
        });
    }
    lines.extend(wrap_styled_fragments(
        fragments,
        width,
        &first_prefix,
        &continuation_prefix,
    ));
}

fn render_hard_fragments(
    fragments: &[StyledFragment],
    width: usize,
    context: MarkdownContext,
    marker: Option<&str>,
    decoration: Option<(&str, &str, Style)>,
    lines: &mut Vec<Line<'static>>,
) {
    let (mut first_prefix, mut continuation_prefix) =
        markdown_prefixes(context, marker, decoration);
    if let Some((first, continuation, style)) = decoration {
        first_prefix.push(StyledFragment {
            content: first.to_owned(),
            style,
        });
        continuation_prefix.push(StyledFragment {
            content: continuation.to_owned(),
            style,
        });
    }
    lines.extend(hard_wrap_styled_fragments(
        fragments,
        width,
        &first_prefix,
        &continuation_prefix,
    ));
}

fn markdown_prefixes(
    context: MarkdownContext,
    marker: Option<&str>,
    decoration: Option<(&str, &str, Style)>,
) -> (Vec<StyledFragment>, Vec<StyledFragment>) {
    let mut first = Vec::new();
    let mut continuation = Vec::new();
    if context.quote_depth > 0 {
        let quote = "│ ".repeat(context.quote_depth);
        let style = Style::default().fg(DIM);
        first.push(StyledFragment {
            content: quote.clone(),
            style,
        });
        continuation.push(StyledFragment {
            content: quote,
            style,
        });
    }
    if context.indent > 0 {
        let indent = " ".repeat(context.indent);
        first.push(StyledFragment {
            content: indent.clone(),
            style: text_style(),
        });
        continuation.push(StyledFragment {
            content: indent,
            style: text_style(),
        });
    }
    if let Some(marker) = marker {
        first.push(StyledFragment {
            content: marker.to_owned(),
            style: Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
        });
        continuation.push(StyledFragment {
            content: " ".repeat(display_width(marker)),
            style: text_style(),
        });
    }
    if decoration.is_some() {
        return (first, continuation);
    }
    (first, continuation)
}

fn wrap_styled_fragments(
    fragments: &[StyledFragment],
    width: usize,
    first_prefix: &[StyledFragment],
    continuation_prefix: &[StyledFragment],
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = first_prefix.to_vec();
    let mut current_width = fragments_width(&current);
    let mut content_width = 0_usize;
    let mut pending_whitespace = Vec::new();

    for token in styled_wrap_tokens(fragments) {
        match token {
            StyledWrapToken::Whitespace(whitespace) => pending_whitespace = whitespace,
            StyledWrapToken::Break => {
                lines.push(Line::from(fragments_into_spans(current)));
                current = continuation_prefix.to_vec();
                current_width = fragments_width(&current);
                content_width = 0;
                pending_whitespace.clear();
            }
            StyledWrapToken::Word(word) => {
                let word_width = fragments_width(&word);
                let whitespace_width = if content_width == 0 {
                    0
                } else {
                    fragments_width(&pending_whitespace)
                };
                if content_width > 0
                    && current_width
                        .saturating_add(whitespace_width)
                        .saturating_add(word_width)
                        > width
                {
                    lines.push(Line::from(fragments_into_spans(current)));
                    current = continuation_prefix.to_vec();
                    current_width = fragments_width(&current);
                    content_width = 0;
                }

                if content_width > 0 {
                    append_fragments(&mut current, &pending_whitespace);
                    current_width = current_width.saturating_add(whitespace_width);
                    content_width = content_width.saturating_add(whitespace_width);
                }
                pending_whitespace.clear();

                for fragment in word {
                    for grapheme in fragment.content.graphemes(true) {
                        let grapheme_width = display_width(grapheme);
                        if content_width > 0 && current_width.saturating_add(grapheme_width) > width
                        {
                            lines.push(Line::from(fragments_into_spans(current)));
                            current = continuation_prefix.to_vec();
                            current_width = fragments_width(&current);
                            content_width = 0;
                        }
                        push_fragment(&mut current, grapheme.to_owned(), fragment.style);
                        current_width = current_width.saturating_add(grapheme_width);
                        content_width = content_width.saturating_add(grapheme_width);
                    }
                }
            }
        }
    }

    if content_width > 0 || lines.is_empty() {
        lines.push(Line::from(fragments_into_spans(current)));
    }
    lines
}

fn hard_wrap_styled_fragments(
    fragments: &[StyledFragment],
    width: usize,
    first_prefix: &[StyledFragment],
    continuation_prefix: &[StyledFragment],
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = first_prefix.to_vec();
    let mut current_width = fragments_width(&current);
    let mut content_width = 0_usize;

    for fragment in fragments {
        for grapheme in fragment.content.graphemes(true) {
            if grapheme == "\n" {
                lines.push(Line::from(fragments_into_spans(current)));
                current = continuation_prefix.to_vec();
                current_width = fragments_width(&current);
                content_width = 0;
                continue;
            }
            let grapheme_width = display_width(grapheme);
            if content_width > 0 && current_width.saturating_add(grapheme_width) > width {
                lines.push(Line::from(fragments_into_spans(current)));
                current = continuation_prefix.to_vec();
                current_width = fragments_width(&current);
                content_width = 0;
            }
            push_fragment(&mut current, grapheme.to_owned(), fragment.style);
            current_width = current_width.saturating_add(grapheme_width);
            content_width = content_width.saturating_add(grapheme_width);
        }
    }

    if content_width > 0 || lines.is_empty() {
        lines.push(Line::from(fragments_into_spans(current)));
    }
    lines
}

enum StyledWrapToken {
    Word(Vec<StyledFragment>),
    Whitespace(Vec<StyledFragment>),
    Break,
}

fn styled_wrap_tokens(fragments: &[StyledFragment]) -> Vec<StyledWrapToken> {
    let mut tokens = Vec::new();
    let mut current = Vec::new();
    let mut current_is_whitespace = None;

    for fragment in fragments {
        for grapheme in fragment.content.graphemes(true) {
            if grapheme == "\n" {
                push_wrap_token(&mut tokens, &mut current, current_is_whitespace);
                current_is_whitespace = None;
                tokens.push(StyledWrapToken::Break);
                continue;
            }

            let is_whitespace = grapheme.chars().all(char::is_whitespace);
            if current_is_whitespace.is_some_and(|current| current != is_whitespace) {
                push_wrap_token(&mut tokens, &mut current, current_is_whitespace);
            }
            current_is_whitespace = Some(is_whitespace);
            push_fragment(&mut current, grapheme.to_owned(), fragment.style);
        }
    }
    push_wrap_token(&mut tokens, &mut current, current_is_whitespace);
    tokens
}

fn push_wrap_token(
    tokens: &mut Vec<StyledWrapToken>,
    current: &mut Vec<StyledFragment>,
    is_whitespace: Option<bool>,
) {
    if current.is_empty() {
        return;
    }
    let fragments = std::mem::take(current);
    if is_whitespace.unwrap_or(false) {
        tokens.push(StyledWrapToken::Whitespace(fragments));
    } else {
        tokens.push(StyledWrapToken::Word(fragments));
    }
}

fn append_fragments(target: &mut Vec<StyledFragment>, fragments: &[StyledFragment]) {
    for fragment in fragments {
        push_fragment(target, fragment.content.clone(), fragment.style);
    }
}

fn render_markdown_table(
    alignments: &[Alignment],
    header: &[Vec<StyledFragment>],
    rows: &[Vec<Vec<StyledFragment>>],
    width: usize,
    context: MarkdownContext,
    marker: Option<&str>,
    lines: &mut Vec<Line<'static>>,
) {
    let column_count = alignments
        .len()
        .max(header.len())
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if column_count == 0 {
        return;
    }

    let (first_prefix, continuation_prefix) = markdown_prefixes(context, marker, None);
    let prefix_width = fragments_width(&first_prefix);
    let separator_width = column_count.saturating_sub(1) * 3;
    let available = width
        .saturating_sub(prefix_width)
        .saturating_sub(separator_width);
    let natural_column_widths = (0..column_count)
        .map(|column| {
            std::iter::once(header.get(column))
                .chain(rows.iter().map(|row| row.get(column)))
                .flatten()
                .map(|cell| display_width(&fragments_text(cell)))
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect::<Vec<_>>();
    let natural_grid_width = prefix_width
        .saturating_add(separator_width)
        .saturating_add(natural_column_widths.iter().sum::<usize>());
    if !rows.is_empty() && natural_grid_width > width {
        render_stacked_markdown_table(header, rows, width, context, marker, lines);
        return;
    }
    let mut column_widths = natural_column_widths;
    while column_widths.iter().sum::<usize>() > available {
        let Some((index, _)) = column_widths
            .iter()
            .enumerate()
            .filter(|(_, value)| **value > 1)
            .max_by_key(|(_, value)| **value)
        else {
            break;
        };
        column_widths[index] -= 1;
    }

    if !header.is_empty() {
        lines.push(table_row_line(
            header,
            alignments,
            &column_widths,
            &first_prefix,
            true,
        ));
        let mut separator = continuation_prefix.clone();
        for (index, column_width) in column_widths.iter().enumerate() {
            if index > 0 {
                push_fragment(&mut separator, "─┼─".into(), Style::default().fg(BORDER));
            }
            push_fragment(
                &mut separator,
                "─".repeat(*column_width),
                Style::default().fg(BORDER),
            );
        }
        lines.push(Line::from(fragments_into_spans(separator)));
    }
    for row in rows {
        lines.push(table_row_line(
            row,
            alignments,
            &column_widths,
            &continuation_prefix,
            false,
        ));
    }
}

fn render_stacked_markdown_table(
    header: &[Vec<StyledFragment>],
    rows: &[Vec<Vec<StyledFragment>>],
    width: usize,
    context: MarkdownContext,
    marker: Option<&str>,
    lines: &mut Vec<Line<'static>>,
) {
    let column_count = header
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    let record_context = MarkdownContext {
        indent: context.indent + marker.map(display_width).unwrap_or(0),
        ..context
    };
    for (row_index, row) in rows.iter().enumerate() {
        for column in 0..column_count {
            let first_field = row_index == 0 && column == 0;
            let field_context = if first_field { context } else { record_context };
            let field_marker = first_field.then_some(marker).flatten();
            let (base_first_prefix, base_continuation_prefix) =
                markdown_prefixes(field_context, field_marker, None);
            let label = stacked_table_label(header, column);
            let label_width = fragments_width(&label);
            let value = row.get(column).map(Vec::as_slice).unwrap_or(&[]);

            if fragments_width(&base_first_prefix)
                .saturating_add(label_width)
                .saturating_add(2)
                < width
            {
                let mut first_prefix = base_first_prefix;
                append_fragments(&mut first_prefix, &label);
                push_fragment(
                    &mut first_prefix,
                    ": ".into(),
                    Style::default().fg(DIM).add_modifier(Modifier::BOLD),
                );
                let mut continuation_prefix = base_continuation_prefix;
                push_fragment(
                    &mut continuation_prefix,
                    " ".repeat(label_width + 2),
                    text_style(),
                );
                lines.extend(wrap_styled_fragments(
                    value,
                    width,
                    &first_prefix,
                    &continuation_prefix,
                ));
            } else {
                let mut label_line = label;
                push_fragment(
                    &mut label_line,
                    ":".into(),
                    Style::default().fg(DIM).add_modifier(Modifier::BOLD),
                );
                lines.extend(wrap_styled_fragments(
                    &label_line,
                    width,
                    &base_first_prefix,
                    &base_continuation_prefix,
                ));
                let (mut value_first_prefix, mut value_continuation_prefix) =
                    markdown_prefixes(record_context, None, None);
                push_fragment(&mut value_first_prefix, "  ".into(), text_style());
                push_fragment(&mut value_continuation_prefix, "  ".into(), text_style());
                lines.extend(wrap_styled_fragments(
                    value,
                    width,
                    &value_first_prefix,
                    &value_continuation_prefix,
                ));
            }
        }
        if row_index + 1 < rows.len() {
            push_markdown_blank(lines, record_context);
        }
    }
}

fn stacked_table_label(header: &[Vec<StyledFragment>], column: usize) -> Vec<StyledFragment> {
    let label = header.get(column).filter(|fragments| {
        fragments
            .iter()
            .any(|fragment| !fragment.content.trim().is_empty())
    });
    label.map_or_else(
        || {
            vec![StyledFragment {
                content: format!("Column {}", column + 1),
                style: Style::default().fg(DIM).add_modifier(Modifier::BOLD),
            }]
        },
        |fragments| {
            fragments
                .iter()
                .cloned()
                .map(|fragment| StyledFragment {
                    content: fragment.content,
                    style: fragment.style.add_modifier(Modifier::BOLD),
                })
                .collect()
        },
    )
}

fn table_row_line(
    row: &[Vec<StyledFragment>],
    alignments: &[Alignment],
    column_widths: &[usize],
    prefix: &[StyledFragment],
    header: bool,
) -> Line<'static> {
    let mut fragments = prefix.to_vec();
    for (column, column_width) in column_widths.iter().enumerate() {
        if column > 0 {
            push_fragment(&mut fragments, " │ ".into(), Style::default().fg(BORDER));
        }
        let content = row.get(column).map_or_else(Vec::new, |cell| {
            fit_table_cell_fragments(cell, *column_width, header)
        });
        let content_width = fragments_width(&content);
        let padding = column_width.saturating_sub(content_width);
        let alignment = alignments.get(column).copied().unwrap_or(Alignment::None);
        let (left_padding, right_padding) = match alignment {
            Alignment::Right => (padding, 0),
            Alignment::Center => (padding / 2, padding - padding / 2),
            Alignment::None | Alignment::Left => (0, padding),
        };
        push_fragment(&mut fragments, " ".repeat(left_padding), text_style());
        fragments.extend(content);
        push_fragment(&mut fragments, " ".repeat(right_padding), text_style());
    }
    Line::from(fragments_into_spans(fragments))
}

fn fit_table_cell_fragments(
    fragments: &[StyledFragment],
    width: usize,
    header: bool,
) -> Vec<StyledFragment> {
    let fragments = fragments
        .iter()
        .cloned()
        .map(|fragment| StyledFragment {
            content: fragment.content,
            style: if header {
                fragment.style.add_modifier(Modifier::BOLD)
            } else {
                fragment.style
            },
        })
        .collect::<Vec<_>>();
    if fragments_width(&fragments) <= width {
        return fragments;
    }
    truncate_styled_fragments(&fragments, width)
}

fn truncate_styled_fragments(fragments: &[StyledFragment], width: usize) -> Vec<StyledFragment> {
    if width == 0 {
        return Vec::new();
    }
    let fallback_style = fragments
        .first()
        .map_or_else(text_style, |fragment| fragment.style);
    if width == 1 {
        return vec![StyledFragment {
            content: "…".into(),
            style: fallback_style,
        }];
    }

    let content_width = width - 1;
    let mut truncated = Vec::new();
    let mut current_width = 0_usize;
    'fragments: for fragment in fragments {
        for grapheme in fragment.content.graphemes(true) {
            let grapheme_width = display_width(grapheme);
            if current_width.saturating_add(grapheme_width) > content_width {
                break 'fragments;
            }
            push_fragment(&mut truncated, grapheme.to_owned(), fragment.style);
            current_width = current_width.saturating_add(grapheme_width);
        }
    }
    let ellipsis_style = truncated
        .last()
        .map_or(fallback_style, |fragment| fragment.style);
    push_fragment(&mut truncated, "…".into(), ellipsis_style);
    truncated
}

fn push_markdown_blank(lines: &mut Vec<Line<'static>>, context: MarkdownContext) {
    if lines.last().is_some_and(|line| line.width() == 0) {
        return;
    }
    if context.quote_depth == 0 && context.indent == 0 {
        lines.push(Line::from(""));
        return;
    }
    let (prefix, _) = markdown_prefixes(context, None, None);
    lines.push(Line::from(fragments_into_spans(prefix)));
}

fn push_fragment(fragments: &mut Vec<StyledFragment>, content: String, style: Style) {
    if content.is_empty() {
        return;
    }
    if let Some(last) = fragments.last_mut()
        && last.style == style
    {
        last.content.push_str(&content);
        return;
    }
    fragments.push(StyledFragment { content, style });
}

fn fragments_into_spans(fragments: Vec<StyledFragment>) -> Vec<Span<'static>> {
    fragments
        .into_iter()
        .map(|fragment| Span::styled(fragment.content, fragment.style))
        .collect()
}

fn fragments_text(fragments: &[StyledFragment]) -> String {
    fragments
        .iter()
        .map(|fragment| fragment.content.as_str())
        .collect()
}

fn fragments_width(fragments: &[StyledFragment]) -> usize {
    fragments
        .iter()
        .map(|fragment| display_width(&fragment.content))
        .sum()
}

fn display_width(value: &str) -> usize {
    Line::from(value).width()
}

fn truncate_display(value: &str, width: usize) -> String {
    if display_width(value) <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".into();
    }
    let mut truncated = String::new();
    let mut current_width = 0_usize;
    for grapheme in value.graphemes(true) {
        let grapheme_width = display_width(grapheme);
        if current_width.saturating_add(grapheme_width) >= width {
            break;
        }
        truncated.push_str(grapheme);
        current_width = current_width.saturating_add(grapheme_width);
    }
    truncated.push('…');
    truncated
}

fn text_style() -> Style {
    Style::default().fg(TEXT)
}

fn surface_style() -> Style {
    Style::default().bg(SURFACE)
}

fn code_style() -> Style {
    Style::default().fg(AMBER)
}

fn inline_code_style() -> Style {
    text_style().add_modifier(Modifier::BOLD)
}

pub(crate) fn transcript_lines(entries: &[TranscriptEntry], width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in entries {
        match entry {
            TranscriptEntry::UserTurn { body } => push_prefixed_lines(
                &mut lines,
                body,
                "› ",
                "  ",
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
                Style::default().fg(TEXT),
                width,
            ),
            TranscriptEntry::AssistantMessage { body } => {
                lines.extend(markdown_lines(body, width));
            }
            TranscriptEntry::ToolCall(tool) => {
                push_prefixed_lines(
                    &mut lines,
                    &format!("Ran {}", tool_name(&tool.name)),
                    "• ",
                    "  ",
                    Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                    width,
                );
                let output_width = width.saturating_sub(4).max(1);
                for (index, line) in hard_wrap(&tool.output, output_width)
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
            TranscriptEntry::Error { body } => push_prefixed_lines(
                &mut lines,
                &format!("ERROR · {body}"),
                "× ",
                "  ",
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
                Style::default().fg(RED),
                width,
            ),
            TranscriptEntry::Notice { label, body } => {
                push_prefixed_lines(
                    &mut lines,
                    &format!("{} · {body}", label.as_deref().unwrap_or("NOTICE")),
                    "• ",
                    "  ",
                    Style::default().fg(DIM),
                    Style::default().fg(DIM),
                    width,
                );
            }
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
        for grapheme in source_line.graphemes(true) {
            let grapheme_width = display_width(grapheme);
            if line_width > 0 && line_width.saturating_add(grapheme_width) > width {
                wrapped.push(std::mem::take(&mut line));
                line_width = 0;
            }
            line.push_str(grapheme);
            line_width = line_width.saturating_add(grapheme_width);
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
    truncate_display(value, width)
}

fn inset(area: Rect, horizontal: u16, vertical: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(horizontal),
        y: area.y.saturating_add(vertical),
        width: area.width.saturating_sub(horizontal.saturating_mul(2)),
        height: area.height.saturating_sub(vertical.saturating_mul(2)),
    }
}
