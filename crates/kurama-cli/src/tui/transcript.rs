use std::{borrow::Cow, collections::VecDeque};

use kurama_protocol::policy::ExecutionMode;
use kurama_protocol::session::TodoStatus;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_segmentation::UnicodeSegmentation;

#[cfg(test)]
use std::cell::Cell;

use super::{
    ToolLifecycle, TranscriptEntry, TuiState,
    syntax::highlight_code,
    theme::{ACCENT, BLUE, BORDER, DIM, GREEN, RED, TEXT},
};

/// Removes terminal instructions from display text without changing stored content.
pub(crate) fn sanitize_terminal_text(value: &str) -> Cow<'_, str> {
    if !value
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
    {
        return Cow::Borrowed(value);
    }
    let mut clean = String::with_capacity(value.len());
    for_each_terminal_character(value, |ch| clean.push(ch));
    Cow::Owned(clean)
}

fn for_each_terminal_character(value: &str, mut emit: impl FnMut(char)) {
    #[derive(Clone, Copy)]
    enum Escape {
        Text,
        Start,
        Intermediate,
        Csi,
        String { bell: bool },
        Terminator { bell: bool },
    }
    let mut state = Escape::Text;
    for ch in value.chars() {
        state = match state {
            Escape::Text => match ch {
                '\x1b' => Escape::Start,
                '\u{009b}' => Escape::Csi,
                '\u{009d}' => Escape::String { bell: true },
                '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => Escape::String { bell: false },
                _ => {
                    if !ch.is_control() || matches!(ch, '\n' | '\t') {
                        emit(ch);
                    }
                    Escape::Text
                }
            },
            Escape::Start => match ch {
                '[' => Escape::Csi,
                ']' => Escape::String { bell: true },
                'P' | 'X' | '^' | '_' => Escape::String { bell: false },
                '\x20'..='\x2f' => Escape::Intermediate,
                '\x1b' => Escape::Start,
                _ => Escape::Text,
            },
            Escape::Intermediate => match ch {
                '\x30'..='\x7e' => Escape::Text,
                '\x1b' => Escape::Start,
                _ => Escape::Intermediate,
            },
            Escape::Csi => match ch {
                '\x40'..='\x7e' => Escape::Text,
                '\x1b' => Escape::Start,
                _ => Escape::Csi,
            },
            Escape::String { bell } => match ch {
                '\u{009c}' => Escape::Text,
                '\x07' if bell => Escape::Text,
                '\x1b' => Escape::Terminator { bell },
                _ => Escape::String { bell },
            },
            Escape::Terminator { bell } => match ch {
                '\\' | '\u{009c}' => Escape::Text,
                '\x07' if bell => Escape::Text,
                '\x1b' => Escape::Terminator { bell },
                _ => Escape::String { bell },
            },
        };
    }
}

#[cfg(test)]
thread_local! {
    static TRANSCRIPT_RENDER_CALLS: Cell<usize> = const { Cell::new(0) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptDetail {
    Compact,
    Expanded,
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
    if width == 0 {
        return Vec::new();
    }
    let markdown = sanitize_terminal_text(markdown);
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events = Parser::new_ext(&markdown, options).collect::<Vec<_>>();
    // Bound recursive rendering of adversarial nesting; the literal fallback keeps all source.
    let mut depth = 0_usize;
    if events.iter().any(|event| {
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
        depth > 128
    }) {
        return hard_wrap_styled_fragments(
            &[StyledFragment {
                content: markdown.to_string(),
                style: text_style(),
            }],
            width,
            &[],
            &[],
        );
    }
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
        if is_implicit_inline_event(&event) {
            *index -= 1;
            blocks.push(MarkdownBlock::Paragraph(parse_inline_fragments(
                events,
                index,
                None,
                text_style(),
            )));
            continue;
        }
        match event {
            Event::End(tag) if Some(tag) == end => break,
            Event::Start(Tag::Paragraph) => blocks.push(MarkdownBlock::Paragraph(
                parse_inline_fragments(events, index, Some(TagEnd::Paragraph), text_style()),
            )),
            Event::Start(Tag::Heading { level, .. }) => {
                blocks.push(MarkdownBlock::Heading(
                    level,
                    parse_inline_fragments(
                        events,
                        index,
                        Some(TagEnd::Heading(level)),
                        text_style(),
                    ),
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
            Event::Start(_)
            | Event::End(_)
            | Event::Text(_)
            | Event::Code(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_) => {}
        }
    }
    blocks
}

fn is_implicit_inline_event(event: &Event<'_>) -> bool {
    matches!(
        event,
        Event::Text(_)
            | Event::Code(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::Start(Tag::Emphasis)
            | Event::Start(Tag::Strong)
            | Event::Start(Tag::Strikethrough)
            | Event::Start(Tag::Link { .. })
            | Event::Start(Tag::Image { .. })
    )
}

fn parse_inline_fragments<'a>(
    events: &[Event<'a>],
    index: &mut usize,
    end: Option<TagEnd>,
    style: Style,
) -> Vec<StyledFragment> {
    let mut fragments = Vec::new();
    while *index < events.len() {
        let event = events[*index].clone();
        *index += 1;
        match event {
            Event::End(tag) if Some(tag) == end => break,
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
                Some(TagEnd::Emphasis),
                style.add_modifier(Modifier::ITALIC),
            )),
            Event::Start(Tag::Strong) => fragments.extend(parse_inline_fragments(
                events,
                index,
                Some(TagEnd::Strong),
                style.add_modifier(Modifier::BOLD),
            )),
            Event::Start(Tag::Strikethrough) => fragments.extend(parse_inline_fragments(
                events,
                index,
                Some(TagEnd::Strikethrough),
                style.add_modifier(Modifier::CROSSED_OUT),
            )),
            Event::Start(Tag::Link { dest_url, .. }) => {
                let destination = dest_url.into_string();
                let linked = parse_inline_fragments(
                    events,
                    index,
                    Some(TagEnd::Link),
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
                    Some(TagEnd::Image),
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
            Event::Start(tag) if end.is_some() => {
                fragments.extend(parse_inline_fragments(
                    events,
                    index,
                    Some(tag.to_end()),
                    style,
                ));
            }
            Event::Start(_) | Event::Rule | Event::End(_) => {
                *index -= 1;
                break;
            }
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
    // The final newline terminates the last source row; earlier blank rows are literal code.
    if content.ends_with('\n') {
        content.pop();
    }
    content
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
                Some(TagEnd::TableCell),
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
            push_markdown_blank(lines, context, width);
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
    if marker.is_some()
        && matches!(
            block,
            MarkdownBlock::Quote(_) | MarkdownBlock::Code { .. } | MarkdownBlock::List { .. }
        )
    {
        let (first, continuation) = markdown_prefixes(context, marker, None);
        let first = bounded_prefix(&first, width);
        let continuation = bounded_prefix(&continuation, width);
        let inner_width =
            width.saturating_sub(fragments_width(&first).max(fragments_width(&continuation)));
        let start = lines.len();
        render_markdown_block(block, inner_width, MarkdownContext::default(), None, lines);
        for (index, line) in lines[start..].iter_mut().enumerate() {
            let prefix = if index == 0 { &first } else { &continuation };
            line.spans.splice(
                0..0,
                prefix
                    .iter()
                    .map(|fragment| Span::styled(fragment.content.clone(), fragment.style)),
            );
        }
        return;
    }
    match block {
        MarkdownBlock::Paragraph(fragments) => {
            render_fragments(fragments, width, context, marker, None, lines);
        }
        MarkdownBlock::Heading(level, fragments) => {
            let heading_style = text_style().add_modifier(Modifier::BOLD);
            let mut heading = vec![StyledFragment {
                content: format!("{} ", "#".repeat(*level as usize)),
                style: heading_style,
            }];
            heading.extend(fragments.iter().map(|fragment| StyledFragment {
                content: fragment.content.clone(),
                style: fragment.style.patch(heading_style),
            }));
            render_fragments(&heading, width, context, marker, None, lines);
        }
        MarkdownBlock::Quote(blocks) => {
            let nested = MarkdownContext {
                quote_depth: context.quote_depth + 1,
                ..context
            };
            render_markdown_blocks(blocks, width, nested, true, lines);
        }
        MarkdownBlock::Code { language, content } => {
            let frame_label = language
                .as_ref()
                .map_or_else(|| "```".to_owned(), |language| format!("```{language}"));
            render_fragments(
                &[StyledFragment {
                    content: frame_label,
                    style: Style::default().fg(DIM).add_modifier(Modifier::BOLD),
                }],
                width,
                context,
                marker,
                None,
                lines,
            );
            let highlighted = language
                .as_deref()
                .and_then(|language| highlight_code(content, language))
                .map(|spans| {
                    spans
                        .into_iter()
                        .map(|span| StyledFragment {
                            content: span.content,
                            style: span.style,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| {
                    vec![StyledFragment {
                        content: content.clone(),
                        style: code_style(),
                    }]
                });
            render_hard_fragments(&highlighted, width, context, None, None, lines);
            render_fragments(
                &[StyledFragment {
                    content: "```".to_owned(),
                    style: Style::default().fg(DIM),
                }],
                width,
                context,
                None,
                None,
                lines,
            );
        }
        MarkdownBlock::List { start, items } => {
            render_markdown_list(*start, items, width, context, lines);
        }
        MarkdownBlock::Rule => {
            let (prefix, _) = markdown_prefixes(context, marker, None);
            let prefix = bounded_prefix(&prefix, width);
            let remaining = width.saturating_sub(fragments_width(&prefix));
            let mut spans = fragments_into_spans(prefix);
            spans.push(Span::styled(
                "─".repeat(remaining.min(32)),
                Style::default().fg(BORDER),
            ));
            lines.push(Line::from(spans));
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
    lines: &mut Vec<Line<'static>>,
) {
    for (item_index, item) in items.iter().enumerate() {
        let marker = start.map_or_else(
            || "• ".to_owned(),
            |number| format!("{}. ", number.saturating_add(item_index as u64)),
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
            style: Style::default().fg(DIM),
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

fn bounded_prefix(prefix: &[StyledFragment], width: usize) -> Vec<StyledFragment> {
    // Leave room for a wide grapheme; discard cosmetic indentation on tiny surfaces.
    if fragments_width(prefix) > width.saturating_sub(2) {
        Vec::new()
    } else {
        prefix.to_vec()
    }
}

fn wrap_styled_fragments(
    fragments: &[StyledFragment],
    width: usize,
    first_prefix: &[StyledFragment],
    continuation_prefix: &[StyledFragment],
) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let first_prefix = bounded_prefix(first_prefix, width);
    let continuation_prefix = bounded_prefix(continuation_prefix, width);
    let content_capacity = width
        .saturating_sub(fragments_width(&first_prefix).max(fragments_width(&continuation_prefix)));
    let mut lines = Vec::new();
    let mut current = first_prefix;
    let mut current_width = fragments_width(&current);
    let mut content_width = 0_usize;
    let mut pending_whitespace = Vec::new();

    for token in styled_wrap_tokens(fragments) {
        match token {
            StyledWrapToken::Whitespace(whitespace) => pending_whitespace = whitespace,
            StyledWrapToken::Break => {
                lines.push(Line::from(fragments_into_spans(current)));
                current = continuation_prefix.clone();
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
                    current = continuation_prefix.clone();
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
                        let original_width = display_width(grapheme);
                        let (grapheme, grapheme_width) = if original_width > content_capacity {
                            ("�", 1)
                        } else {
                            (grapheme, original_width)
                        };
                        if content_width > 0 && current_width.saturating_add(grapheme_width) > width
                        {
                            lines.push(Line::from(fragments_into_spans(current)));
                            current = continuation_prefix.clone();
                            current_width = fragments_width(&current);
                            content_width = 0;
                        }
                        push_fragment_str(&mut current, grapheme, fragment.style);
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
    if width == 0 {
        return Vec::new();
    }
    let mut current = bounded_prefix(first_prefix, width);
    let continuation_prefix = bounded_prefix(continuation_prefix, width);
    let content_capacity =
        width.saturating_sub(fragments_width(&current).max(fragments_width(&continuation_prefix)));
    let mut current_width = fragments_width(&current);
    let mut content_width = 0_usize;
    let mut source_width = 0_usize;
    let mut lines = Vec::new();
    for fragment in fragments {
        let content = sanitize_terminal_text(&fragment.content);
        for grapheme in content.graphemes(true) {
            if grapheme == "\n" {
                lines.push(Line::from(fragments_into_spans(current)));
                current = continuation_prefix.clone();
                current_width = fragments_width(&current);
                content_width = 0;
                source_width = 0;
                continue;
            }
            let spaces = if grapheme == "\t" {
                4 - source_width % 4
            } else {
                0
            };
            for _ in 0..spaces.max(1) {
                let grapheme = if spaces > 0 { " " } else { grapheme };
                let original_width = display_width(grapheme);
                source_width = source_width.saturating_add(original_width);
                let (grapheme, grapheme_width) = if original_width > content_capacity {
                    ("�", 1)
                } else {
                    (grapheme, original_width)
                };
                if content_width > 0 && current_width.saturating_add(grapheme_width) > width {
                    lines.push(Line::from(fragments_into_spans(current)));
                    current = continuation_prefix.clone();
                    current_width = fragments_width(&current);
                    content_width = 0;
                }
                push_fragment_str(&mut current, grapheme, fragment.style);
                current_width = current_width.saturating_add(grapheme_width);
                content_width = content_width.saturating_add(grapheme_width);
            }
        }
    }
    lines.push(Line::from(fragments_into_spans(current)));
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
        let content = sanitize_terminal_text(&fragment.content);
        for grapheme in content.graphemes(true) {
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
            push_fragment_str(
                &mut current,
                if grapheme == "\t" { " " } else { grapheme },
                fragment.style,
            );
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
        push_fragment_str(target, &fragment.content, fragment.style);
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
    let natural_column_widths = (0..column_count)
        .map(|column| {
            std::iter::once(header.get(column))
                .chain(rows.iter().map(|row| row.get(column)))
                .flatten()
                .map(|cell| fragments_width(cell))
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect::<Vec<_>>();
    let natural_grid_width = prefix_width
        .saturating_add(separator_width)
        .saturating_add(natural_column_widths.iter().sum::<usize>());
    if natural_grid_width > width
        || header.iter().chain(rows.iter().flatten()).any(|cell| {
            cell.iter()
                .any(|fragment| fragment.content.contains(['\n', '\t']))
        })
    {
        if rows.is_empty() {
            for (column, cell) in header.iter().enumerate() {
                render_fragments(
                    cell,
                    width,
                    context,
                    if column == 0 { marker } else { None },
                    None,
                    lines,
                );
            }
        } else {
            render_stacked_markdown_table(header, rows, width, context, marker, lines);
        }
        return;
    }
    let column_widths = natural_column_widths;

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
                .saturating_add(4)
                <= width
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
                if fragments_width(&value_first_prefix).saturating_add(2) < width {
                    push_fragment(&mut value_first_prefix, "  ".into(), text_style());
                    push_fragment(&mut value_continuation_prefix, "  ".into(), text_style());
                }
                lines.extend(wrap_styled_fragments(
                    value,
                    width,
                    &value_first_prefix,
                    &value_continuation_prefix,
                ));
            }
        }
        if row_index + 1 < rows.len() {
            push_markdown_blank(lines, record_context, width);
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
        let content = row.get(column).map(Vec::as_slice).unwrap_or(&[]);
        let content_width = fragments_width(content);
        let padding = column_width.saturating_sub(content_width);
        let alignment = alignments.get(column).copied().unwrap_or(Alignment::None);
        let (left_padding, right_padding) = match alignment {
            Alignment::Right => (padding, 0),
            Alignment::Center => (padding / 2, padding - padding / 2),
            Alignment::None | Alignment::Left => (0, padding),
        };
        push_fragment(&mut fragments, " ".repeat(left_padding), text_style());
        for fragment in content {
            push_fragment_str(
                &mut fragments,
                &fragment.content,
                if header {
                    fragment.style.add_modifier(Modifier::BOLD)
                } else {
                    fragment.style
                },
            );
        }
        push_fragment(&mut fragments, " ".repeat(right_padding), text_style());
    }
    Line::from(fragments_into_spans(fragments))
}

fn push_markdown_blank(lines: &mut Vec<Line<'static>>, context: MarkdownContext, width: usize) {
    if lines.last().is_some_and(|line| line.width() == 0) {
        return;
    }
    if context.quote_depth == 0 && context.indent == 0 {
        lines.push(Line::from(""));
        return;
    }
    let (prefix, _) = markdown_prefixes(context, None, None);
    lines.push(Line::from(fragments_into_spans(bounded_prefix(
        &prefix, width,
    ))));
}

fn push_fragment(fragments: &mut Vec<StyledFragment>, content: String, style: Style) {
    let content = match sanitize_terminal_text(&content) {
        Cow::Borrowed(_) => content,
        Cow::Owned(clean) => clean,
    };
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

fn push_fragment_str(fragments: &mut Vec<StyledFragment>, content: &str, style: Style) {
    if content.is_empty() {
        return;
    }
    if let Some(last) = fragments.last_mut()
        && last.style == style
    {
        last.content.push_str(content);
    } else {
        fragments.push(StyledFragment {
            content: content.to_owned(),
            style,
        });
    }
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
    Span::raw(value).width()
}

pub(crate) fn truncate_display(value: &str, width: usize) -> String {
    let value = single_line_text(value);
    if display_width(&value) <= width {
        return value.into_owned();
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

fn truncate_display_left(value: &str, width: usize) -> String {
    let value = single_line_text(value);
    if display_width(&value) <= width {
        return value.into_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".into();
    }
    let mut graphemes = Vec::new();
    let mut current_width = 0_usize;
    for grapheme in value.graphemes(true).rev() {
        let grapheme_width = display_width(grapheme);
        if current_width.saturating_add(grapheme_width) >= width {
            break;
        }
        graphemes.push(grapheme);
        current_width = current_width.saturating_add(grapheme_width);
    }
    graphemes.reverse();
    format!("…{}", graphemes.concat())
}

fn single_line_text(value: &str) -> Cow<'_, str> {
    let value = sanitize_terminal_text(value);
    if value.contains(['\n', '\t']) {
        Cow::Owned(value.replace(['\n', '\t'], " "))
    } else {
        value
    }
}

pub(crate) fn startup_lines(
    version: &str,
    model: &str,
    project: &str,
    mode: ExecutionMode,
    width: usize,
) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let title = format!(">_ kurama  v{}", sanitize_terminal_text(version));
    let model = sanitize_terminal_text(model);
    let project = sanitize_terminal_text(project);
    let metadata = [
        ("model", model.as_ref()),
        ("directory", project.as_ref()),
        ("approval", mode_label(mode)),
    ];
    // A bounded card stays quiet on ultrawide terminals; tiny terminals get plain rows.
    let card_width = width.min(64);
    let framed = card_width >= 24;
    let inner_width = card_width.saturating_sub(if framed { 4 } else { 0 });
    let mut contents = vec![Line::from(Span::styled(
        truncate_display(&title, inner_width),
        text_style().add_modifier(Modifier::BOLD),
    ))];
    for (label, value) in metadata {
        let label = format!("{label}: ");
        if display_width(&label) < inner_width {
            let available = inner_width - display_width(&label);
            let value = if label.starts_with("directory") {
                truncate_display_left(value, available)
            } else {
                truncate_display(value, available)
            };
            contents.push(Line::from(vec![
                Span::styled(label, Style::default().fg(DIM)),
                Span::styled(value, text_style()),
            ]));
        } else {
            contents.push(Line::from(Span::styled(
                truncate_display(value, inner_width),
                text_style(),
            )));
        }
    }
    if !framed {
        return contents;
    }
    let border = Style::default().fg(BORDER);
    let mut lines = Vec::with_capacity(contents.len() + 2);
    lines.push(Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(card_width - 2)),
        border,
    )));
    for mut line in contents {
        let padding = inner_width.saturating_sub(line.width());
        line.spans.insert(0, Span::styled("│ ", border));
        line.spans
            .push(Span::styled(format!("{} │", " ".repeat(padding)), border));
        lines.push(line);
    }
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(card_width - 2)),
        border,
    )));
    lines
}

fn mode_label(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Supervised => "supervised",
        ExecutionMode::Auto => "auto",
        ExecutionMode::Yolo => "yolo",
    }
}

fn text_style() -> Style {
    Style::default().fg(TEXT)
}

fn code_style() -> Style {
    text_style()
}

fn inline_code_style() -> Style {
    Style::default().fg(Color::Green)
}

pub fn transcript_lines(
    entries: &[TranscriptEntry],
    width: usize,
    detail: TranscriptDetail,
) -> Vec<Line<'static>> {
    render_transcript_entries(entries, width, detail, None)
}

pub(crate) fn transcript_lines_with_entry_starts(
    entries: &[TranscriptEntry],
    width: usize,
    detail: TranscriptDetail,
) -> (Vec<Line<'static>>, Vec<usize>) {
    if width == 0 {
        return (Vec::new(), Vec::new());
    }
    let mut starts = Vec::with_capacity(entries.len());
    let lines = render_transcript_entries(entries, width, detail, Some(&mut starts));
    (lines, starts)
}

fn render_transcript_entries(
    entries: &[TranscriptEntry],
    width: usize,
    detail: TranscriptDetail,
    mut entry_starts: Option<&mut Vec<usize>>,
) -> Vec<Line<'static>> {
    #[cfg(test)]
    TRANSCRIPT_RENDER_CALLS.with(|calls| calls.set(calls.get() + 1));
    if width == 0 {
        return Vec::new();
    }

    let mut lines = Vec::new();
    for entry in entries {
        if let Some(starts) = &mut entry_starts {
            starts.push(lines.len());
        }
        match entry {
            TranscriptEntry::Startup {
                version,
                model,
                project,
                mode,
            } => lines.extend(startup_lines(version, model, project, *mode, width)),
            TranscriptEntry::UserTurn { body } => {
                push_prefixed_lines(
                    &mut lines,
                    body,
                    "› ",
                    "  ",
                    Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
                    text_style(),
                    width,
                );
            }
            TranscriptEntry::AssistantMessage { body } => {
                push_markdown_lines(&mut lines, body, width);
            }
            TranscriptEntry::ToolCall(tool) => {
                let name = tool_name(&tool.name);
                let action = match tool.lifecycle {
                    ToolLifecycle::Running => format!("Running {name}"),
                    ToolLifecycle::Completed => format!("Ran {name}"),
                    ToolLifecycle::Failed => format!("{name} failed"),
                };
                let summary = tool
                    .context
                    .as_deref()
                    .filter(|context| !context.trim().is_empty())
                    .map_or(action.clone(), |context| format!("{action} · {context}"));
                let summary = match detail {
                    TranscriptDetail::Compact => {
                        truncate_display(&summary, width.saturating_sub(2).max(1))
                    }
                    TranscriptDetail::Expanded => summary,
                };
                let failed = tool.lifecycle == ToolLifecycle::Failed;
                push_prefixed_lines(
                    &mut lines,
                    &summary,
                    if failed { "× " } else { "• " },
                    "  ",
                    Style::default().fg(if failed { RED } else { DIM }),
                    Style::default()
                        .fg(if failed { RED } else { TEXT })
                        .add_modifier(Modifier::BOLD),
                    width,
                );
                let output_width = width.saturating_sub(tool_output_gutter(width));
                let output = match detail {
                    TranscriptDetail::Compact => compact_tool_output(&tool.output, output_width),
                    TranscriptDetail::Expanded => expanded_tool_output(&tool.output, output_width),
                };
                push_tool_output_lines(&mut lines, output, width);
            }
            TranscriptEntry::Todos { items } => {
                if items.is_empty() {
                    continue;
                }
                push_prefixed_lines(
                    &mut lines,
                    "todo",
                    "• ",
                    "  ",
                    Style::default().fg(DIM),
                    Style::default().fg(DIM).add_modifier(Modifier::BOLD),
                    width,
                );
                for item in items {
                    let (marker, continuation, marker_style, body_style) = match item.status {
                        TodoStatus::Completed => (
                            "  [x] ",
                            "      ",
                            Style::default().fg(DIM),
                            Style::default().fg(DIM),
                        ),
                        TodoStatus::InProgress => (
                            "  [>] ",
                            "      ",
                            Style::default().fg(ACCENT),
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                        ),
                        TodoStatus::Pending => (
                            "  [ ] ",
                            "      ",
                            Style::default().fg(TEXT),
                            Style::default().fg(TEXT),
                        ),
                        TodoStatus::Cancelled => (
                            "  [-] ",
                            "      ",
                            Style::default().fg(DIM),
                            Style::default().fg(DIM),
                        ),
                    };
                    push_prefixed_lines(
                        &mut lines,
                        &item.content,
                        marker,
                        continuation,
                        marker_style,
                        body_style,
                        width,
                    );
                }
            }
            TranscriptEntry::Error { body } => push_prefixed_lines(
                &mut lines,
                &format!("Error · {body}"),
                "× ",
                "  ",
                Style::default().fg(RED),
                Style::default().fg(RED),
                width,
            ),
            TranscriptEntry::Notice {
                label: Some(label),
                body,
            } => push_prefixed_lines(
                &mut lines,
                &format!("{label} · {body}"),
                "• ",
                "  ",
                Style::default().fg(DIM),
                Style::default().fg(DIM),
                width,
            ),
            TranscriptEntry::Notice { label: None, body } => push_prefixed_lines(
                &mut lines,
                body,
                "• ",
                "  ",
                Style::default().fg(DIM),
                Style::default().fg(DIM),
                width,
            ),
        }
        // Each entry owns its separator, making separately committed batches composable.
        lines.push(Line::default());
    }
    lines
}

#[cfg(test)]
pub(crate) fn reset_transcript_render_calls() {
    TRANSCRIPT_RENDER_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
pub(crate) fn transcript_render_calls() -> usize {
    TRANSCRIPT_RENDER_CALLS.with(Cell::get)
}

fn compact_tool_output(output: &str, width: usize) -> Vec<String> {
    if output.is_empty() || width == 0 {
        return Vec::new();
    }
    compact_wrapped_tail(output, width, 2)
}

fn compact_wrapped_tail(output: &str, width: usize, tail: usize) -> Vec<String> {
    let mut visible = VecDeque::<String>::with_capacity(tail);
    let mut retain_row = |line: &str| {
        if tail == 0 {
            return;
        }
        let mut row = if visible.len() == tail {
            visible.pop_front().unwrap_or_default()
        } else {
            String::new()
        };
        row.clear();
        row.push_str(line);
        visible.push_back(row);
    };
    let mut total = 0_usize;
    let mut pending_blank = 0_usize;
    for_each_wrapped_line(output, width, |line| {
        if line.trim().is_empty() {
            pending_blank = pending_blank.saturating_add(1);
            return;
        }
        total = total.saturating_add(pending_blank).saturating_add(1);
        for _ in 0..pending_blank.min(tail) {
            retain_row("");
        }
        pending_blank = 0;
        retain_row(line);
    });
    let omitted = total.saturating_sub(visible.len());
    let mut lines = Vec::with_capacity(visible.len() + usize::from(omitted > 0));
    if omitted > 0 {
        lines.push(truncate_display(
            &format!("… {omitted} earlier {}", pluralize(omitted, "line")),
            width,
        ));
    }
    lines.extend(visible);
    lines
}

fn expanded_tool_output(output: &str, width: usize) -> Vec<String> {
    if output.is_empty() {
        Vec::new()
    } else {
        hard_wrap(output, width)
    }
}

fn pluralize(count: usize, singular: &'static str) -> &'static str {
    if count == 1 { singular } else { "lines" }
}

fn push_tool_output_lines(lines: &mut Vec<Line<'static>>, output: Vec<String>, width: usize) {
    let prefix = " ".repeat(tool_output_gutter(width));
    for line in output {
        lines.push(Line::from(vec![
            Span::raw(prefix.clone()),
            Span::styled(line, Style::default().fg(DIM)),
        ]));
    }
}

fn tool_output_gutter(width: usize) -> usize {
    if width >= 6 { 4 } else { 0 }
}

pub(crate) fn render_transcript_view(
    frame: &mut Frame<'_>,
    state: &TuiState,
    prepared_transcript: Option<&[Line<'static>]>,
) {
    let area = frame.area();
    let mut transcript_area = super::layout::main_area(area);
    let hint_height = u16::from(transcript_area.height > 1);
    transcript_area.height = transcript_area.height.saturating_sub(hint_height);
    state.transcript_width.set(transcript_area.width);
    state.viewport_height.set(transcript_area.height);
    if transcript_area.is_empty() {
        return;
    }
    let owned;
    let transcript = if let Some(prepared) = prepared_transcript {
        prepared
    } else {
        owned = transcript_lines(
            &state.transcript,
            transcript_area.width as usize,
            TranscriptDetail::Expanded,
        );
        &owned
    };
    let viewport_height = transcript_area.height as usize;
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
                transcript_area.x,
                transcript_area.y + row as u16,
                transcript_area.width,
                1,
            ),
        );
    }
    if hint_height > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "esc close · ↑↓ scroll · { } prompts",
                Style::default().fg(DIM),
            ))),
            Rect::new(
                transcript_area.x,
                transcript_area.bottom(),
                transcript_area.width,
                1,
            ),
        );
    }
}

fn push_markdown_lines(lines: &mut Vec<Line<'static>>, body: &str, width: usize) {
    if width == 0 {
        return;
    }
    let gutter = if width >= 4 { 2 } else { 0 };
    let mut markdown = markdown_lines(body, width - gutter);
    if markdown.is_empty() {
        markdown.push(Line::default());
    }
    for (index, mut line) in markdown.into_iter().enumerate() {
        if gutter > 0 {
            line.spans.insert(
                0,
                Span::styled(
                    if index == 0 { "• " } else { "  " },
                    Style::default().fg(DIM),
                ),
            );
        }
        lines.push(line);
    }
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
    let body = sanitize_terminal_text(body);
    lines.extend(wrap_styled_fragments(
        &[StyledFragment {
            content: body.into_owned(),
            style: body_style,
        }],
        width,
        &[StyledFragment {
            content: first_prefix.into(),
            style: prefix_style,
        }],
        &[StyledFragment {
            content: continuation_prefix.into(),
            style: prefix_style,
        }],
    ));
}

pub(crate) fn hard_wrap(value: &str, width: usize) -> Vec<String> {
    let mut wrapped = Vec::new();
    for_each_wrapped_line(value, width, |line| wrapped.push(line.to_owned()));
    wrapped
}

// Visits rows with one reusable buffer, so compact previews retain only their visible tail.
pub(crate) fn for_each_wrapped_line(value: &str, width: usize, mut visit: impl FnMut(&str)) {
    if width == 0 {
        return;
    }
    if value
        .bytes()
        .all(|byte| byte == b'\n' || byte == b' ' || byte.is_ascii_graphic())
    {
        for line in value.split('\n') {
            if line.is_empty() {
                visit("");
            } else {
                for start in (0..line.len()).step_by(width) {
                    visit(&line[start..line.len().min(start.saturating_add(width))]);
                }
            }
        }
        return;
    }
    let mut line = String::new();
    let mut line_width = 0_usize;
    let mut source_width = 0_usize;
    let mut render = |safe: &str| {
        for grapheme in safe.graphemes(true) {
            if grapheme == "\n" {
                visit(&line);
                line.clear();
                line_width = 0;
                source_width = 0;
                continue;
            }
            let spaces = if grapheme == "\t" {
                4 - source_width % 4
            } else {
                0
            };
            for _ in 0..spaces.max(1) {
                let grapheme = if spaces > 0 { " " } else { grapheme };
                let original_width = display_width(grapheme);
                source_width = source_width.saturating_add(original_width);
                let (grapheme, grapheme_width) = if original_width > width {
                    ("�", 1)
                } else {
                    (grapheme, original_width)
                };
                if line_width > 0 && line_width.saturating_add(grapheme_width) > width {
                    visit(&line);
                    line.clear();
                    line_width = 0;
                }
                line.push_str(grapheme);
                line_width = line_width.saturating_add(grapheme_width);
            }
        }
    };
    if value
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
    {
        // Carry the last grapheme across sanitized chunks so even escapes inside emoji
        // cannot split a cluster. Memory is bounded by the chunk and the largest grapheme.
        let mut chunk = String::with_capacity(4096);
        let mut flush_at = 4096_usize;
        for_each_terminal_character(value, |ch| {
            chunk.push(ch);
            if chunk.len() >= flush_at {
                let boundary = chunk
                    .grapheme_indices(true)
                    .next_back()
                    .map_or(0, |(index, _)| index);
                if boundary > 0 {
                    render(&chunk[..boundary]);
                    drop(chunk.drain(..boundary));
                    flush_at = chunk.len().saturating_add(4096);
                } else {
                    flush_at = flush_at.saturating_mul(2);
                }
            }
        });
        render(&chunk);
    } else {
        render(value);
    }
    visit(&line);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn compact_preview_does_not_hide_text_behind_terminal_newlines() {
        let rows = plain(&transcript_lines(
            &[tool("first\nlast\n\n\n")],
            80,
            TranscriptDetail::Compact,
        ));
        assert!(rows.iter().any(|row| row.contains("last")));
    }

    fn tool(output: &str) -> TranscriptEntry {
        TranscriptEntry::ToolCall(super::super::ToolTranscript {
            call_id: None,
            name: "bash".into(),
            context: None,
            output: output.into(),
            lifecycle: ToolLifecycle::Completed,
        })
    }

    #[test]
    fn terminal_sequences_are_removed_without_copying_clean_text() {
        let clean = "界 e\u{301}\tvalue\nnext";
        assert!(matches!(sanitize_terminal_text(clean), Cow::Borrowed(value) if value == clean));
        let unsafe_text = concat!(
            "a\x1b[2Jb\x1b]8;;https://example.test\x1b\\link\x1b]8;;\x07",
            "\x1bPignored\x1b\\c\x1b(0d\u{009b}31me\u{009d}title\u{009c}",
            "\x00\x07\x08\r\x7f\t\nend\x1b[31"
        );
        assert_eq!(sanitize_terminal_text(unsafe_text), "ablinkcde\t\nend");
    }

    #[test]
    fn literal_code_and_tool_blank_rows_survive_rendering() {
        let rows = plain(&markdown_lines("```text\n alpha  \n\n\n \n```", 80));
        assert_eq!(&rows[1..5], &[" alpha  ", "", "", " "]);
        let source = "  one  \n\n\n two\n \n";
        let rows = plain(&transcript_lines(
            &[tool(source)],
            80,
            TranscriptDetail::Expanded,
        ));
        let output = rows[1..rows.len() - 1]
            .iter()
            .map(|row| row.strip_prefix("    ").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(output, source.split('\n').collect::<Vec<_>>());
        assert_eq!(hard_wrap("a bcd.", 5).concat(), "a bcd.");
        assert_eq!(hard_wrap("a\tb", 80), ["a   b"]);
    }

    #[test]
    fn escapes_split_across_stream_chunks_do_not_change_visible_graphemes() {
        let source = format!(
            "{}e\x1b[31m\u{301}👨\x1b[0m‍👩‍👧‍👦\n\x1b]title spanning\nrows\x1b\\tail",
            "x".repeat(4094)
        );
        let clean = format!("{}e\u{301}👨‍👩‍👧‍👦\ntail", "x".repeat(4094));
        assert_eq!(hard_wrap(&source, 40), hard_wrap(&clean, 40));
        assert_eq!(
            compact_tool_output(&source, 40),
            compact_tool_output(&clean, 40)
        );
        assert_eq!(hard_wrap("12345\tx", 3).concat(), "12345   x");
    }

    #[test]
    fn compact_tail_counts_blank_and_wrapped_rows_without_discarding_the_tail() {
        assert_eq!(
            compact_tool_output("a\n\n\nz", 40),
            ["… 2 earlier lines", "", "z"]
        );
        let source = format!("{}{}", "old\n".repeat(10_000), "x".repeat(80));
        let preview = compact_tool_output(&source, 40);
        assert_eq!(
            preview,
            [
                "… 10000 earlier lines".to_owned(),
                "x".repeat(40),
                "x".repeat(40)
            ]
        );
        assert_eq!(
            expanded_tool_output(&source, 40).last(),
            Some(&"x".repeat(40))
        );
    }

    #[test]
    fn independent_entry_batches_keep_identical_separators() {
        let entries = [
            TranscriptEntry::UserTurn {
                body: "first".into(),
            },
            TranscriptEntry::AssistantMessage {
                body: "answer\n\n```\na\n\n```".into(),
            },
            tool("result\n\n"),
            TranscriptEntry::UserTurn {
                body: "second".into(),
            },
        ];
        let together = transcript_lines(&entries, 40, TranscriptDetail::Expanded);
        let separate = entries
            .iter()
            .flat_map(|entry| {
                transcript_lines(std::slice::from_ref(entry), 40, TranscriptDetail::Expanded)
            })
            .collect::<Vec<_>>();
        assert_eq!(together, separate);
    }

    #[test]
    fn transcript_entries_fit_tiny_widths_and_never_emit_controls() {
        let hostile = "界👨‍👩‍👧‍👦e\u{301}\x1b[31m red\x07";
        let entries = [
            TranscriptEntry::Startup {
                version: "1.0".into(),
                model: hostile.into(),
                project: hostile.into(),
                mode: ExecutionMode::Supervised,
            },
            TranscriptEntry::UserTurn {
                body: hostile.into(),
            },
            TranscriptEntry::AssistantMessage {
                body: format!(
                    "# {hostile}\n\n> quoted\n>\n> next\n\n- list\n  - nested\n\n---\n\n```rust\n{hostile}\tvalue\n\n```\n\n| head | second |\n|---|---|\n|{hostile}|long value|\n\n| header only | other |\n|---|---|"
                ),
            },
            tool(hostile),
            TranscriptEntry::Error {
                body: hostile.into(),
            },
            TranscriptEntry::Notice {
                label: Some(hostile.into()),
                body: hostile.into(),
            },
        ];
        for width in 0..40 {
            for detail in [TranscriptDetail::Compact, TranscriptDetail::Expanded] {
                let lines = transcript_lines(&entries, width, detail);
                assert!(
                    lines.iter().all(|line| line.width() <= width),
                    "width {width}: {:?}",
                    plain(&lines)
                );
                assert!(
                    plain(&lines)
                        .iter()
                        .flat_map(|line| line.chars())
                        .all(|ch| !ch.is_control())
                );
            }
        }
        assert_eq!(hard_wrap("界", 1), ["�"]);
        assert_eq!(hard_wrap("界👨‍👩‍👧‍👦e\u{301}", 2).concat(), "界👨‍👩‍👧‍👦e\u{301}");
    }

    #[test]
    fn nested_blocks_keep_their_parent_marker_and_continuation_indent() {
        let rows = plain(&markdown_lines(
            "-\n  - child\n\n-\n  > quoted\n\n-\n  ```\n  code\n  ```",
            80,
        ));
        assert!(!rows.iter().any(|row| row.trim() == "•"));
        assert!(rows.iter().any(|row| row == "• • child"));
        assert!(rows.iter().any(|row| row == "• │ quoted"));
        assert!(rows.iter().any(|row| row == "  code"));
    }

    #[test]
    fn partial_markdown_can_reinterpret_earlier_text_without_losing_code() {
        assert_eq!(plain(&markdown_lines("Title", 80)), ["Title"]);
        let heading = markdown_lines("Title\n===", 80);
        assert_eq!(plain(&heading), ["# Title"]);
        assert!(
            heading[0]
                .spans
                .iter()
                .all(|span| span.style.add_modifier.contains(Modifier::BOLD))
        );
        for source in ["```rust\nlet x = 1;", "```rust\nlet x = 1;\n```"] {
            assert_eq!(plain(&markdown_lines(source, 80))[1], "let x = 1;");
        }
        let decoded = markdown_lines("| heading |\n|---|\n| &#27;[31mvisible&#7; |", 80);
        assert!(
            plain(&decoded)
                .iter()
                .flat_map(|line| line.chars())
                .all(|ch| !ch.is_control())
        );
    }

    #[test]
    fn header_only_tables_and_deep_nesting_keep_source_available() {
        let rows = plain(&markdown_lines("| alphabet | second |\n|---|---|", 3));
        assert_eq!(rows.concat(), "alphabetsecond");
        let source = format!("{}leaf", "> ".repeat(200));
        assert_eq!(plain(&markdown_lines(&source, 20)).concat(), source);
        let mut lines = Vec::new();
        let items = vec![
            vec![MarkdownBlock::Paragraph(vec![StyledFragment {
                content: "one".into(),
                style: text_style(),
            }])],
            vec![MarkdownBlock::Paragraph(vec![StyledFragment {
                content: "two".into(),
                style: text_style(),
            }])],
        ];
        render_markdown_list(
            Some(u64::MAX),
            &items,
            80,
            MarkdownContext::default(),
            &mut lines,
        );
        assert_eq!(
            plain(&lines),
            [format!("{}. one", u64::MAX), format!("{}. two", u64::MAX)]
        );
    }
}
