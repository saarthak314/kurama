use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    Frame,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Padding, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;

use super::{ToolLifecycle, TranscriptEntry, TuiState};

const DIM: Color = Color::Rgb(126, 132, 146);
const TEXT: Color = Color::Reset;
const BORDER: Color = Color::Rgb(48, 53, 64);
const RED: Color = Color::Rgb(255, 92, 82);
const GREEN: Color = Color::Rgb(111, 207, 151);
const BLUE: Color = Color::Rgb(116, 177, 255);

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
            let frame_label = language
                .as_ref()
                .map_or_else(|| "┌".to_owned(), |language| format!("┌ {language}"));
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
            render_hard_fragments(
                &[StyledFragment {
                    content: content.clone(),
                    style: code_style(),
                }],
                width,
                context,
                None,
                Some(("│ ", "│ ", Style::default().fg(DIM))),
                lines,
            );
            render_fragments(
                &[StyledFragment {
                    content: "└".to_owned(),
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

pub(crate) fn truncate_display(value: &str, width: usize) -> String {
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

fn code_style() -> Style {
    text_style()
}

fn inline_code_style() -> Style {
    text_style().add_modifier(Modifier::BOLD)
}

pub fn transcript_lines(
    entries: &[TranscriptEntry],
    width: usize,
    _detail: TranscriptDetail,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in entries {
        match entry {
            TranscriptEntry::UserTurn { body } => {
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                push_prefixed_lines(
                    &mut lines,
                    body,
                    "› ",
                    "  ",
                    Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
                    Style::default().fg(TEXT),
                    width,
                );
            }
            TranscriptEntry::AssistantMessage { body } => {
                push_markdown_lines(&mut lines, body, width);
            }
            TranscriptEntry::ToolCall(tool) => {
                let name = tool_name(&tool.name);
                let summary = match tool.lifecycle {
                    ToolLifecycle::Running => format!("Running {name}"),
                    ToolLifecycle::Completed => format!("Ran {name}"),
                    ToolLifecycle::Failed => format!("{name} failed"),
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
                let output_width = width.saturating_sub(4).max(1);
                let output = tool.output.trim_end_matches(['\r', '\n']);
                let output = if output.is_empty() {
                    vec!["(no output)".to_owned()]
                } else {
                    word_wrap(output, output_width)
                };
                let output_len = output.len();
                for (index, line) in output.into_iter().enumerate() {
                    lines.push(Line::from(vec![
                        Span::styled(
                            if index + 1 == output_len {
                                "  └ "
                            } else {
                                "  │ "
                            },
                            Style::default().fg(DIM),
                        ),
                        Span::styled(line, Style::default().fg(DIM)),
                    ]));
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
                "",
                "",
                Style::default().fg(DIM),
                Style::default().fg(DIM),
                width,
            ),
        }
    }
    lines
}

pub(crate) fn render_transcript_view(frame: &mut Frame<'_>, state: &TuiState) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }

    let block = Block::default().padding(Padding::new(2, 2, 1, 1));
    let transcript_area = block.inner(area);
    if transcript_area.is_empty() {
        frame.render_widget(block, area);
        return;
    }

    let transcript = transcript_lines(
        &state.transcript,
        transcript_area.width as usize,
        TranscriptDetail::Expanded,
    );
    let viewport_height = transcript_area.height as usize;
    let scroll = state
        .scroll
        .min(transcript.len().saturating_sub(viewport_height));
    let start = transcript
        .len()
        .saturating_sub(viewport_height.saturating_add(scroll));
    let visible = transcript
        .into_iter()
        .skip(start)
        .take(viewport_height)
        .collect::<Vec<_>>();

    frame.render_widget(Paragraph::new(Text::from(visible)).block(block), area);
}

fn push_markdown_lines(lines: &mut Vec<Line<'static>>, body: &str, width: usize) {
    let prefix_width = display_width("• ");
    let mut markdown = markdown_lines(body, width.saturating_sub(prefix_width).max(1));
    if markdown.is_empty() {
        markdown.push(Line::default());
    }
    if markdown.first().is_some_and(line_starts_with_list_marker) {
        lines.extend(markdown);
        return;
    }
    for (index, mut line) in markdown.into_iter().enumerate() {
        line.spans.insert(
            0,
            Span::styled(
                if index == 0 { "• " } else { "  " },
                Style::default().fg(DIM),
            ),
        );
        lines.push(line);
    }
}

fn line_starts_with_list_marker(line: &Line<'_>) -> bool {
    let text = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let text = text.trim_start();
    text.starts_with("• ")
        || text.split_once(". ").is_some_and(|(number, _)| {
            !number.is_empty() && number.chars().all(|character| character.is_ascii_digit())
        })
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
    for (index, line) in word_wrap(body, width.saturating_sub(prefix_width).max(1))
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

pub(crate) fn hard_wrap(value: &str, width: usize) -> Vec<String> {
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
                if let Some(previous) =
                    split_before_trailing_punctuation(&mut line, &mut line_width, grapheme, width)
                {
                    wrapped.push(previous);
                } else {
                    wrapped.push(std::mem::take(&mut line));
                    line_width = 0;
                }
            }
            line.push_str(grapheme);
            line_width = line_width.saturating_add(grapheme_width);
        }
        wrapped.push(line);
    }
    wrapped
}

fn split_before_trailing_punctuation(
    line: &mut String,
    line_width: &mut usize,
    grapheme: &str,
    width: usize,
) -> Option<String> {
    if !matches!(grapheme, "," | "." | ";" | ":" | "!" | "?") {
        return None;
    }

    let punctuation_width = display_width(grapheme);
    let split_at = line
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map(|(index, character)| index + character.len_utf8())
        .or_else(|| {
            line.grapheme_indices(true)
                .next_back()
                .map(|(index, _)| index)
                .filter(|index| *index > 0)
        })?;
    let previous = line[..split_at].trim_end().to_owned();
    let continuation = line[split_at..].trim_start();
    if previous.is_empty()
        || continuation.is_empty()
        || display_width(continuation).saturating_add(punctuation_width) > width
    {
        return None;
    }

    *line = continuation.to_owned();
    *line_width = display_width(line);
    Some(previous)
}

pub(crate) fn word_wrap(value: &str, width: usize) -> Vec<String> {
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
