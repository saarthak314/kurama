use std::{borrow::Cow, collections::VecDeque, ops::Range, sync::Arc};

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
    Overlay, ToolLifecycle, TranscriptEntry, TuiState,
    syntax::highlight_code,
    theme::{ACCENT, BLUE, BORDER, DIM, GREEN, RED, TEXT},
};

const MAX_LINK_TARGET_BYTES: usize = 4096;

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
    target: Option<Arc<str>>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TranscriptLine {
    pub text: Line<'static>,
    hyperlinks: Vec<HyperlinkRun>,
}

#[derive(Clone, Debug)]
struct HyperlinkRun {
    column: usize,
    width: usize,
    target: Arc<str>,
}

impl TranscriptLine {
    fn from_fragments(fragments: Vec<StyledFragment>) -> Self {
        let mut line = Self::default();
        let mut column = 0;
        for fragment in fragments {
            let width = display_width(&fragment.content);
            if let Some(target) = fragment.target
                && width > 0
            {
                line.hyperlinks.push(HyperlinkRun {
                    column,
                    width,
                    target,
                });
            }
            column += width;
            line.text
                .spans
                .push(Span::styled(fragment.content, fragment.style));
        }
        line
    }

    fn prepend(&mut self, prefix: &[StyledFragment]) {
        let width = fragments_width(prefix);
        for link in &mut self.hyperlinks {
            link.column += width;
        }
        self.text.spans.splice(
            0..0,
            prefix
                .iter()
                .map(|fragment| Span::styled(fragment.content.clone(), fragment.style)),
        );
    }

    pub(crate) fn link_at(&self, column: usize) -> Option<Arc<str>> {
        self.hyperlinks
            .iter()
            .find(|link| column >= link.column && column - link.column < link.width)
            .map(|link| Arc::clone(&link.target))
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(&self.text, area);
    }
}

impl From<Line<'static>> for TranscriptLine {
    fn from(text: Line<'static>) -> Self {
        Self {
            text,
            hyperlinks: Vec::new(),
        }
    }
}

fn safe_link_target(value: &str) -> Option<Arc<str>> {
    // Targets repeat across wrapped hit regions; bound their retained size.
    if value.len() > MAX_LINK_TARGET_BYTES {
        return None;
    }
    if value
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return None;
    }
    let (scheme, destination) = value.split_once(':')?;
    let supported = if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        destination.strip_prefix("//").is_some_and(|rest| {
            !rest
                .split(['/', '?', '#'])
                .next()
                .unwrap_or_default()
                .is_empty()
        })
    } else {
        scheme.eq_ignore_ascii_case("mailto") && !destination.is_empty()
    };
    supported.then(|| Arc::from(value))
}

fn bare_links(value: &str) -> impl Iterator<Item = (Range<usize>, Arc<str>)> + '_ {
    let mut offset = 0;
    std::iter::from_fn(move || {
        while offset < value.len() {
            let colon = offset + value[offset..].find("://")?;
            offset = colon + 3;
            let start = if colon >= 5
                && value
                    .get(colon - 5..colon)
                    .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"))
            {
                colon - 5
            } else if colon >= 4
                && value
                    .get(colon - 4..colon)
                    .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http"))
            {
                colon - 4
            } else {
                continue;
            };
            if value[..start]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
            {
                continue;
            }
            let candidate = &value[start..];
            let end = candidate
                .find(|ch: char| {
                    ch.is_whitespace()
                        || ch.is_control()
                        || matches!(ch, '<' | '>' | '"' | '\'' | '`')
                })
                .unwrap_or(candidate.len());
            offset = start + end;
            if end > MAX_LINK_TARGET_BYTES {
                continue;
            }
            let mut target = &candidate[..end];
            loop {
                let trimmed = target.trim_end_matches(['.', ',', ';', ':', '!', '?']);
                if trimmed.len() != target.len() {
                    target = trimmed;
                    continue;
                }
                let unmatched =
                    [('(', ')'), ('[', ']'), ('{', '}')]
                        .iter()
                        .any(|&(open, close)| {
                            target.ends_with(close)
                                && target.chars().filter(|&ch| ch == close).count()
                                    > target.chars().filter(|&ch| ch == open).count()
                        });
                if unmatched {
                    target = &target[..target.len() - 1];
                } else {
                    break;
                }
            }
            if let Some(link) = safe_link_target(target) {
                return Some((start..start + target.len(), link));
            }
        }
        None
    })
}

fn linkify_fragments(fragments: Vec<StyledFragment>) -> Vec<StyledFragment> {
    let mut result = Vec::new();
    for fragment in fragments {
        if fragment.target.is_some() {
            result.push(fragment);
            continue;
        }
        let mut cursor = 0;
        for (range, target) in bare_links(&fragment.content) {
            push_fragment_str(
                &mut result,
                &fragment.content[cursor..range.start],
                fragment.style,
                None,
            );
            push_fragment_str(
                &mut result,
                &fragment.content[range.clone()],
                fragment.style,
                Some(&target),
            );
            cursor = range.end;
        }
        if cursor == 0 {
            result.push(fragment);
        } else {
            push_fragment_str(
                &mut result,
                &fragment.content[cursor..],
                fragment.style,
                None,
            );
        }
    }
    result
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

fn markdown_lines(markdown: &str, width: usize) -> Vec<TranscriptLine> {
    if width == 0 {
        return Vec::new();
    }
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events = Parser::new_ext(markdown, options).collect::<Vec<_>>();
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
                target: None,
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
                    target: None,
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
                let mut text_fragments = Vec::new();
                push_fragment(&mut text_fragments, text.into_string(), style);
                fragments.extend(linkify_fragments(text_fragments));
            }
            Event::Code(code) => {
                let mut code_fragments = Vec::new();
                push_fragment(
                    &mut code_fragments,
                    code.into_string(),
                    style.patch(inline_code_style()),
                );
                fragments.extend(linkify_fragments(code_fragments));
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
                let target = safe_link_target(&destination);
                let mut linked = parse_inline_fragments(
                    events,
                    index,
                    Some(TagEnd::Link),
                    style.fg(BLUE).add_modifier(Modifier::UNDERLINED),
                );
                let label = fragments_text(&linked);
                for fragment in &mut linked {
                    fragment.target = target.clone();
                }
                fragments.extend(linked);
                if !destination.is_empty() && label != destination {
                    let dim = Style::default().fg(DIM);
                    push_fragment(&mut fragments, " (".into(), dim);
                    let visible = sanitize_terminal_text(&destination);
                    push_fragment_str(&mut fragments, &visible, dim, target.as_ref());
                    push_fragment(&mut fragments, ")".into(), dim);
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
    match sanitize_terminal_text(&content) {
        Cow::Borrowed(_) => content,
        Cow::Owned(clean) => clean,
    }
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
    lines: &mut Vec<TranscriptLine>,
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
    lines: &mut Vec<TranscriptLine>,
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
            line.prepend(prefix);
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
                target: None,
            }];
            heading.extend(fragments.iter().map(|fragment| StyledFragment {
                content: fragment.content.clone(),
                style: fragment.style.patch(heading_style),
                target: fragment.target.clone(),
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
            let highlighted = language
                .as_deref()
                .and_then(|language| highlight_code(content, language))
                .map(|spans| {
                    spans
                        .into_iter()
                        .map(|span| StyledFragment {
                            content: span.content,
                            style: span.style,
                            target: None,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| {
                    vec![StyledFragment {
                        content: content.clone(),
                        style: code_style(),
                        target: None,
                    }]
                });
            render_hard_fragments(&highlighted, width, context, marker, None, lines);
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
                "─".repeat(remaining),
                Style::default().fg(BORDER),
            ));
            lines.push(Line::from(spans).into());
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
    lines: &mut Vec<TranscriptLine>,
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
    lines: &mut Vec<TranscriptLine>,
) {
    let (mut first_prefix, mut continuation_prefix) =
        markdown_prefixes(context, marker, decoration);
    if let Some((first, continuation, style)) = decoration {
        first_prefix.push(StyledFragment {
            content: first.to_owned(),
            style,
            target: None,
        });
        continuation_prefix.push(StyledFragment {
            content: continuation.to_owned(),
            style,
            target: None,
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
    lines: &mut Vec<TranscriptLine>,
) {
    let (mut first_prefix, mut continuation_prefix) =
        markdown_prefixes(context, marker, decoration);
    if let Some((first, continuation, style)) = decoration {
        first_prefix.push(StyledFragment {
            content: first.to_owned(),
            style,
            target: None,
        });
        continuation_prefix.push(StyledFragment {
            content: continuation.to_owned(),
            style,
            target: None,
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
            target: None,
        });
        continuation.push(StyledFragment {
            content: quote,
            style,
            target: None,
        });
    }
    if context.indent > 0 {
        let indent = " ".repeat(context.indent);
        first.push(StyledFragment {
            content: indent.clone(),
            style: text_style(),
            target: None,
        });
        continuation.push(StyledFragment {
            content: indent,
            style: text_style(),
            target: None,
        });
    }
    if let Some(marker) = marker {
        first.push(StyledFragment {
            content: marker.to_owned(),
            style: Style::default().fg(DIM),
            target: None,
        });
        continuation.push(StyledFragment {
            content: " ".repeat(display_width(marker)),
            style: text_style(),
            target: None,
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
) -> Vec<TranscriptLine> {
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
                lines.push(TranscriptLine::from_fragments(current));
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
                    lines.push(TranscriptLine::from_fragments(current));
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
                            lines.push(TranscriptLine::from_fragments(current));
                            current = continuation_prefix.clone();
                            current_width = fragments_width(&current);
                            content_width = 0;
                        }
                        push_fragment_str(
                            &mut current,
                            grapheme,
                            fragment.style,
                            fragment.target.as_ref(),
                        );
                        current_width = current_width.saturating_add(grapheme_width);
                        content_width = content_width.saturating_add(grapheme_width);
                    }
                }
            }
        }
    }

    if content_width > 0 || lines.is_empty() {
        lines.push(TranscriptLine::from_fragments(current));
    }
    lines
}

fn hard_wrap_styled_fragments(
    fragments: &[StyledFragment],
    width: usize,
    first_prefix: &[StyledFragment],
    continuation_prefix: &[StyledFragment],
) -> Vec<TranscriptLine> {
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
                lines.push(TranscriptLine::from_fragments(current));
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
                    lines.push(TranscriptLine::from_fragments(current));
                    current = continuation_prefix.clone();
                    current_width = fragments_width(&current);
                    content_width = 0;
                }
                push_fragment_str(
                    &mut current,
                    grapheme,
                    fragment.style,
                    fragment.target.as_ref(),
                );
                current_width = current_width.saturating_add(grapheme_width);
                content_width = content_width.saturating_add(grapheme_width);
            }
        }
    }
    lines.push(TranscriptLine::from_fragments(current));
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
                fragment.target.as_ref(),
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
        push_fragment_str(
            target,
            &fragment.content,
            fragment.style,
            fragment.target.as_ref(),
        );
    }
}

fn render_markdown_table(
    alignments: &[Alignment],
    header: &[Vec<StyledFragment>],
    rows: &[Vec<Vec<StyledFragment>>],
    width: usize,
    context: MarkdownContext,
    marker: Option<&str>,
    lines: &mut Vec<TranscriptLine>,
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
        lines.push(TranscriptLine::from_fragments(separator));
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
    lines: &mut Vec<TranscriptLine>,
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
                target: None,
            }]
        },
        |fragments| {
            fragments
                .iter()
                .cloned()
                .map(|fragment| StyledFragment {
                    content: fragment.content,
                    style: fragment.style.add_modifier(Modifier::BOLD),
                    target: fragment.target,
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
) -> TranscriptLine {
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
                fragment.target.as_ref(),
            );
        }
        push_fragment(&mut fragments, " ".repeat(right_padding), text_style());
    }
    TranscriptLine::from_fragments(fragments)
}

fn push_markdown_blank(lines: &mut Vec<TranscriptLine>, context: MarkdownContext, width: usize) {
    if lines.last().is_some_and(|line| line.text.width() == 0) {
        return;
    }
    if context.quote_depth == 0 && context.indent == 0 {
        lines.push(TranscriptLine::default());
        return;
    }
    let (prefix, _) = markdown_prefixes(context, None, None);
    lines.push(TranscriptLine::from_fragments(bounded_prefix(
        &prefix, width,
    )));
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
        && last.target.is_none()
    {
        last.content.push_str(&content);
        return;
    }
    fragments.push(StyledFragment {
        content,
        style,
        target: None,
    });
}

fn push_fragment_str(
    fragments: &mut Vec<StyledFragment>,
    content: &str,
    style: Style,
    target: Option<&Arc<str>>,
) {
    if content.is_empty() {
        return;
    }
    if let Some(last) = fragments.last_mut()
        && last.style == style
        && last.target.as_ref() == target
    {
        last.content.push_str(content);
    } else {
        fragments.push(StyledFragment {
            content: content.to_owned(),
            style,
            target: target.cloned(),
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
    let dim = Style::default().fg(DIM);
    let accent = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let project = single_line_text(project);
    let model = single_line_text(model);
    let mode = mode_label(mode);
    let mut lines = Vec::with_capacity(4);
    let mut heading = Line::from(Span::styled(truncate_display("kurama", width), accent));
    // Stack the workspace only when the masthead would leave too little useful path.
    let inline_project = width >= 9 + display_width(&project).min(12);
    if inline_project {
        heading.spans.push(Span::styled(" / ", dim));
        heading.spans.push(Span::styled(
            truncate_display_left(&project, width - 9),
            text_style(),
        ));
        // Version is optional: it must never steal space from the workspace.
        if width >= 72 {
            let version = single_line_text(version);
            let version_width = display_width(&version).saturating_add(1);
            if !version.is_empty()
                && heading
                    .width()
                    .saturating_add(version_width)
                    .saturating_add(4)
                    <= width
            {
                heading.spans.push(Span::raw(
                    " ".repeat(width - heading.width() - version_width),
                ));
                heading.spans.push(Span::styled(format!("v{version}"), dim));
            }
        }
    }
    lines.push(heading);
    if !inline_project {
        lines.push(Line::from(Span::styled(
            truncate_display_left(&project, width),
            text_style(),
        )));
    }
    let mode_width = display_width(mode);
    if width > mode_width + 3 {
        lines.push(Line::from(vec![
            Span::styled(truncate_display(&model, width - mode_width - 3), dim),
            Span::styled(" · ", dim),
            Span::styled(mode, accent),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            truncate_display(&model, width),
            dim,
        )));
        // Even on tiny terminals the full safety mode remains readable across rows.
        lines.extend(
            hard_wrap(mode, width)
                .into_iter()
                .map(|row| Line::from(Span::styled(row, accent))),
        );
    }
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
    prepared_transcript_lines(entries, width, detail)
        .into_iter()
        .map(|line| line.text)
        .collect()
}

pub(crate) fn prepared_transcript_lines(
    entries: &[TranscriptEntry],
    width: usize,
    detail: TranscriptDetail,
) -> Vec<TranscriptLine> {
    render_transcript_entries(entries, width, detail, None)
}

pub(crate) fn transcript_lines_with_entry_starts(
    entries: &[TranscriptEntry],
    width: usize,
    detail: TranscriptDetail,
) -> (Vec<TranscriptLine>, Vec<usize>) {
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
) -> Vec<TranscriptLine> {
    #[cfg(test)]
    TRANSCRIPT_RENDER_CALLS.with(|calls| calls.set(calls.get() + 1));
    if width == 0 {
        return Vec::new();
    }

    let mut lines = Vec::new();
    let latest_assistant = entries
        .iter()
        .rposition(|entry| matches!(entry, TranscriptEntry::AssistantMessage { .. }));
    for (entry_index, entry) in entries.iter().enumerate() {
        if let Some(starts) = &mut entry_starts {
            starts.push(lines.len());
        }
        match entry {
            TranscriptEntry::Startup {
                version,
                model,
                project,
                mode,
            } => lines.extend(
                startup_lines(version, model, project, *mode, width)
                    .into_iter()
                    .map(TranscriptLine::from),
            ),
            TranscriptEntry::UserTurn { body } => {
                push_prefixed_lines(
                    &mut lines,
                    body,
                    "> ",
                    "  ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    text_style(),
                    width,
                );
            }
            TranscriptEntry::AssistantMessage { body } => {
                let start = lines.len();
                lines.extend(markdown_lines(body, width));
                if Some(entry_index) != latest_assistant {
                    while lines.len() > start
                        && lines.last().is_some_and(|line| {
                            line.text
                                .spans
                                .iter()
                                .all(|span| span.content.trim().is_empty())
                        })
                    {
                        lines.pop();
                    }
                    lines.push(TranscriptLine::default());
                    lines.push(
                        Line::from(Span::styled("─".repeat(width), Style::default().fg(BORDER)))
                            .into(),
                    );
                }
            }
            TranscriptEntry::ToolCall(tool) => {
                push_tool_summary(&mut lines, tool, width, detail);
                push_tool_output_lines(&mut lines, &tool.output, width, detail);
            }
            TranscriptEntry::Todos { items } => {
                if items.is_empty() {
                    continue;
                }
                push_prefixed_lines(
                    &mut lines,
                    "todo",
                    "",
                    "",
                    Style::default().fg(DIM),
                    Style::default().fg(DIM).add_modifier(Modifier::BOLD),
                    width,
                );
                for item in items {
                    let (marker, continuation, marker_style, body_style) = match item.status {
                        TodoStatus::Completed => (
                            "[x] ",
                            "    ",
                            Style::default().fg(DIM),
                            Style::default().fg(DIM),
                        ),
                        TodoStatus::InProgress => (
                            "[>] ",
                            "    ",
                            Style::default().fg(ACCENT),
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                        ),
                        TodoStatus::Pending => (
                            "[ ] ",
                            "    ",
                            Style::default().fg(TEXT),
                            Style::default().fg(TEXT),
                        ),
                        TodoStatus::Cancelled => (
                            "[-] ",
                            "    ",
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
                "",
                "",
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
        lines.push(TranscriptLine::default());
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

fn push_tool_summary(
    lines: &mut Vec<TranscriptLine>,
    tool: &super::ToolTranscript,
    width: usize,
    detail: TranscriptDetail,
) {
    let name = tool_name(&tool.name);
    let name_width = display_width(&name);
    let name_style = text_style().add_modifier(Modifier::BOLD);
    let context = tool
        .context
        .as_deref()
        .map(single_line_text)
        .filter(|context| !context.trim().is_empty());
    let (status, color) = match tool.lifecycle {
        ToolLifecycle::Running => ("running", ACCENT),
        ToolLifecycle::Completed => ("done", GREEN),
        ToolLifecycle::Failed => ("failed", RED),
    };
    let status_style = Style::default().fg(color);
    let status_width = display_width(status);
    let path_context = matches!(name.as_str(), "read" | "write");
    let truncate_context = |value: &str, available| {
        if path_context {
            truncate_display_left(value, available)
        } else {
            truncate_display(value, available)
        }
    };
    let mut inline_context = false;
    if name_width.saturating_add(status_width).saturating_add(2) <= width {
        let mut heading = Line::from(Span::styled(name, name_style));
        let available = width.saturating_sub(name_width + status_width + 4);
        if let Some(context) = &context
            && available > 0
            && (detail == TranscriptDetail::Compact || display_width(context) <= available)
        {
            heading.spans.push(Span::raw("  "));
            heading.spans.push(Span::styled(
                truncate_context(context, available),
                text_style(),
            ));
            inline_context = true;
        }
        heading.spans.push(Span::raw(
            " ".repeat(width - heading.width() - status_width),
        ));
        heading.spans.push(Span::styled(status, status_style));
        lines.push(TranscriptLine::from_fragments(
            heading
                .spans
                .into_iter()
                .enumerate()
                .map(|(index, span)| {
                    let target = if inline_context && index == 2 {
                        context.as_deref().and_then(safe_link_target)
                    } else {
                        None
                    };
                    StyledFragment {
                        content: span.content.into_owned(),
                        style: span.style,
                        target,
                    }
                })
                .collect(),
        ));
    } else {
        push_prefixed_lines(lines, &name, "", "", name_style, name_style, width);
        push_prefixed_lines(lines, status, "", "", status_style, status_style, width);
    }
    if !inline_context && let Some(context) = context {
        let context = match detail {
            TranscriptDetail::Compact => truncate_context(&context, width),
            TranscriptDetail::Expanded => context.into_owned(),
        };
        push_prefixed_lines(lines, &context, "", "", text_style(), text_style(), width);
    }
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

fn push_tool_output_lines(
    lines: &mut Vec<TranscriptLine>,
    output: &str,
    width: usize,
    detail: TranscriptDetail,
) {
    let gutter = tool_output_gutter(width);
    let output_width = width.saturating_sub(gutter);
    let prefix = [StyledFragment {
        content: " ".repeat(gutter),
        style: Style::default(),
        target: None,
    }];
    // Keep the allocation-free ASCII/streaming sanitizer path for output without links.
    if bare_links(output).next().is_none() {
        let output = match detail {
            TranscriptDetail::Compact => compact_tool_output(output, output_width),
            TranscriptDetail::Expanded => expanded_tool_output(output, output_width),
        };
        for row in output {
            lines.push(
                Line::from(vec![
                    Span::raw(prefix[0].content.clone()),
                    Span::styled(row, Style::default().fg(DIM)),
                ])
                .into(),
            );
        }
        return;
    }
    let mut visible = VecDeque::with_capacity(2);
    let mut total = 0_usize;
    let mut pending_blank = 0_usize;
    for_each_linked_tool_row(output, output_width, |row| {
        if detail == TranscriptDetail::Expanded {
            let mut line = TranscriptLine::from_fragments(row);
            line.prepend(&prefix);
            lines.push(line);
            return;
        }
        if row
            .iter()
            .all(|fragment| fragment.content.trim().is_empty())
        {
            pending_blank += 1;
            return;
        }
        total = total.saturating_add(pending_blank).saturating_add(1);
        for _ in 0..pending_blank.min(2) {
            if visible.len() == 2 {
                visible.pop_front();
            }
            visible.push_back(Vec::new());
        }
        pending_blank = 0;
        if visible.len() == 2 {
            visible.pop_front();
        }
        visible.push_back(row);
    });
    let omitted = total.saturating_sub(visible.len());
    if omitted > 0 {
        let mut line = TranscriptLine::from(Line::from(Span::styled(
            truncate_display(
                &format!("… {omitted} earlier {}", pluralize(omitted, "line")),
                output_width,
            ),
            Style::default().fg(DIM),
        )));
        line.prepend(&prefix);
        lines.push(line);
    }
    for row in visible {
        let mut line = TranscriptLine::from_fragments(row);
        line.prepend(&prefix);
        lines.push(line);
    }
}

fn for_each_linked_tool_row(
    output: &str,
    width: usize,
    mut visit: impl FnMut(Vec<StyledFragment>),
) {
    if width == 0 || output.is_empty() {
        return;
    }
    let clean = sanitize_terminal_text(output);
    let mut targets = bare_links(&clean).peekable();
    let mut row = Vec::new();
    let mut row_width = 0_usize;
    let mut source_width = 0_usize;
    for (offset, grapheme) in clean.grapheme_indices(true) {
        while targets.peek().is_some_and(|(range, _)| offset >= range.end) {
            targets.next();
        }
        let target = targets
            .peek()
            .filter(|(range, _)| range.contains(&offset))
            .map(|(_, target)| target);
        if grapheme == "\n" {
            visit(std::mem::take(&mut row));
            row_width = 0;
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
            if row_width > 0 && row_width.saturating_add(grapheme_width) > width {
                visit(std::mem::take(&mut row));
                row_width = 0;
            }
            push_fragment_str(&mut row, grapheme, Style::default().fg(DIM), target);
            row_width = row_width.saturating_add(grapheme_width);
        }
    }
    visit(row);
}

fn tool_output_gutter(width: usize) -> usize {
    if width >= 4 { 2 } else { 0 }
}

pub(crate) fn render_transcript_view(
    frame: &mut Frame<'_>,
    state: &TuiState,
    prepared_transcript: Option<&[TranscriptLine]>,
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
        owned = prepared_transcript_lines(
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
        line.render(
            frame,
            Rect::new(
                transcript_area.x,
                transcript_area.y + row as u16,
                transcript_area.width,
                1,
            ),
        );
    }
    if state.overlay == Overlay::None
        && let Some(selection) = &state.transcript_selection
    {
        selection.render(frame, transcript_area, start);
    }
    if hint_height > 0 {
        let hint = if state.overlay == Overlay::None && state.selection_copied {
            "Copied selection · Esc clear"
        } else if state.overlay == Overlay::None
            && state
                .transcript_selection
                .as_ref()
                .is_some_and(|selection| selection.dragging && selection.dragged)
        {
            "Release to copy selection · Esc clear"
        } else {
            "esc close · ↑↓ scroll · { } prompts"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(DIM)))),
            Rect::new(
                transcript_area.x,
                transcript_area.bottom(),
                transcript_area.width,
                1,
            ),
        );
    }
}

fn push_prefixed_lines(
    lines: &mut Vec<TranscriptLine>,
    body: &str,
    first_prefix: &'static str,
    continuation_prefix: &'static str,
    prefix_style: Style,
    body_style: Style,
    width: usize,
) {
    let body = sanitize_terminal_text(body);
    lines.extend(wrap_styled_fragments(
        &linkify_fragments(vec![StyledFragment {
            content: body.into_owned(),
            style: body_style,
            target: None,
        }]),
        width,
        &[StyledFragment {
            content: first_prefix.into(),
            style: prefix_style,
            target: None,
        }],
        &[StyledFragment {
            content: continuation_prefix.into(),
            style: prefix_style,
            target: None,
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
    let label = single_line_text(label);
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

    fn rich_plain(lines: &[TranscriptLine]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.text
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn render_rows(lines: &[TranscriptLine], width: u16) -> ratatui::buffer::Buffer {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
            width,
            lines.len().max(1) as u16,
        ))
        .unwrap();
        terminal
            .draw(|frame| {
                for (row, line) in lines.iter().enumerate() {
                    line.render(frame, Rect::new(0, row as u16, width, 1));
                }
            })
            .unwrap()
            .buffer
            .clone()
    }

    fn linked_text(rows: &[TranscriptLine], target: &str) -> String {
        let mut linked = String::new();
        for row in rows {
            let text = row
                .text
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            let mut column = 0;
            for grapheme in text.graphemes(true) {
                let width = display_width(grapheme);
                let hit = row.link_at(column);
                for offset in 1..width {
                    assert_eq!(row.link_at(column + offset), hit);
                }
                if hit.as_deref() == Some(target) {
                    linked.push_str(grapheme);
                }
                column += width;
            }
            assert!(row.link_at(column).is_none());
        }
        linked
    }

    #[test]
    fn links_hit_every_wrapped_unicode_cell_without_changing_plain_rendering() {
        let target = "https://example.test/界";
        let label = "界ébold👨‍👩‍👧‍👦abcdef";
        let rows = markdown_lines(&format!("- [界é**bold**👨‍👩‍👧‍👦abcdef]({target})"), 10);
        assert_eq!(linked_text(&rows, target), format!("{label}{target}"));
        for row in &rows {
            assert!(row.link_at(0).is_none());
            assert!(row.link_at(1).is_none());
        }
        let plain_rows = rows
            .iter()
            .map(|row| TranscriptLine::from(row.text.clone()))
            .collect::<Vec<_>>();
        for width in [7, 10] {
            let buffer = render_rows(&rows, width);
            assert_eq!(buffer, render_rows(&plain_rows, width));
            assert!(
                buffer
                    .content
                    .iter()
                    .all(|cell| !cell.symbol().contains('\x1b'))
            );
            assert!(
                buffer
                    .content
                    .iter()
                    .any(|cell| { cell.symbol() == "b" && cell.modifier.contains(Modifier::BOLD) })
            );
        }
    }

    #[test]
    fn changing_or_removing_link_targets_updates_hits_without_changing_text() {
        let row = |target| {
            TranscriptLine::from_fragments(vec![StyledFragment {
                content: "a b界".into(),
                style: Style::default(),
                target,
            }])
        };
        let original = row(safe_link_target("https://one.test"));
        let changed = row(safe_link_target("https://two.test"));
        let plain = row(None);
        for column in 0..5 {
            assert_eq!(
                original.link_at(column).as_deref(),
                Some("https://one.test")
            );
            assert_eq!(changed.link_at(column).as_deref(), Some("https://two.test"));
            assert!(plain.link_at(column).is_none());
        }
        assert!(original.link_at(5).is_none());
        assert!(changed.link_at(usize::MAX).is_none());
        let original = render_rows(&[original], 8);
        assert_eq!(original, render_rows(&[changed], 8));
        assert_eq!(original, render_rows(&[plain], 8));
    }

    #[test]
    fn unsafe_link_targets_remain_plain_and_never_emit_terminal_instructions() {
        for target in [
            "javascript:alert(1)",
            "file:///tmp/private",
            "https://host/\x1b]52;c;secret\x07",
            "https://host/\nnext",
            "https://",
            "mailto:",
        ] {
            assert!(safe_link_target(target).is_none(), "{target:?}");
        }
        for target in [
            "https://host/path?q=1&x=2",
            "HTTP://host",
            "mailto:user@example.test",
        ] {
            assert!(safe_link_target(target).is_some());
        }
        let rows = markdown_lines(
            "[script](javascript:alert) [file](file:///tmp/a) [bad](https://example.test/&#27;)",
            100,
        );
        assert!(rows.iter().all(|row| row.hyperlinks.is_empty()));
        assert!(
            rich_plain(&rows)
                .concat()
                .contains("script (javascript:alert)")
        );
        assert!(
            render_rows(&rows, 100)
                .content
                .iter()
                .all(|cell| !cell.symbol().contains('\x1b'))
        );
    }

    #[test]
    fn oversized_link_targets_stay_readable_without_becoming_clickable() {
        let target = format!("https://example.test/{}", "a".repeat(4096));
        let rows = markdown_lines(&format!("[reference]({target})"), 32);
        assert!(rows.iter().all(|row| row.hyperlinks.is_empty()));
        assert!(rich_plain(&rows).concat().contains(&target));
    }

    #[test]
    fn bare_tool_links_respect_word_boundaries_and_balanced_punctuation() {
        let entries = [tool(
            "éHTTP://ignored.test (HTTPS://example.test/a_(b)). —http://second.test/q?x=1#part!",
        )];
        let rows = prepared_transcript_lines(&entries, 160, TranscriptDetail::Expanded);
        assert!(linked_text(&rows, "HTTP://ignored.test").is_empty());
        for target in [
            "HTTPS://example.test/a_(b)",
            "http://second.test/q?x=1#part",
        ] {
            assert_eq!(linked_text(&rows, target), target);
        }
        let oversized = format!("https://example.test/{}", ")".repeat(5000));
        let rows = prepared_transcript_lines(&[tool(&oversized)], 40, TranscriptDetail::Compact);
        assert!(
            rows.iter()
                .all(|row| (0..row.text.width()).all(|column| row.link_at(column).is_none()))
        );
    }

    #[test]
    fn wrapped_tool_links_keep_the_complete_target_in_bounded_previews() {
        let target = format!("https://example.test/{}", "界é".repeat(20));
        let source = format!("{}Result {target}.", "old\n".repeat(1000));
        let entries = [tool(&source)];
        for detail in [TranscriptDetail::Compact, TranscriptDetail::Expanded] {
            let rows = prepared_transcript_lines(&entries, 18, detail);
            let linked = linked_text(&rows, &target);
            if detail == TranscriptDetail::Compact {
                assert!(rows.len() <= 5);
                assert!(linked.ends_with("界é"));
                assert!(target.ends_with(&linked));
            } else {
                assert_eq!(linked, target);
            }
        }
    }

    #[test]
    fn table_links_survive_grid_cells_and_stacked_header_prefixes() {
        let source = "| [Site](https://header.test) |\n|---|\n| [界value](https://value.test) |";
        for width in [12, 100] {
            let rows = markdown_lines(source, width);
            assert!(linked_text(&rows, "https://header.test").contains("Site"));
            assert!(linked_text(&rows, "https://value.test").contains("界value"));
            assert!(rows.iter().all(|row| row.text.width() <= width));
        }
    }

    #[test]
    fn copying_a_link_label_excludes_its_target_and_terminal_controls() {
        use super::super::{TranscriptPoint, TranscriptSelection};

        let rows = markdown_lines("[**reference**](https://example.test/hidden)\x1b[31m", 80);
        let mut selection = TranscriptSelection::new(TranscriptPoint { row: 0, column: 0 }, None);
        let end = TranscriptPoint { row: 0, column: 8 };
        selection.update(end);
        selection.finish(end);
        assert_eq!(selection.text(&rows), "reference");
    }

    #[test]
    fn fenced_code_hides_chrome_without_losing_nested_source_or_highlighting() {
        let source = "> ```rust\n> fn main() {\n> \n>     let answer = 42;\n> }\n> ```";
        let rows = markdown_lines(source, 40);
        assert_eq!(
            rich_plain(&rows),
            ["│ fn main() {", "│ ", "│     let answer = 42;", "│ }"]
        );
        assert!(rows.iter().flat_map(|row| &row.text.spans).any(|span| {
            span.content.contains("let")
                && span
                    .style
                    .fg
                    .is_some_and(|color| color != TEXT && color != DIM)
        }));
    }

    #[test]
    fn startup_version_never_displaces_workspace_model_or_mode() {
        let project = "~/src/界-workspace";
        let model = "model-α";
        let normal = startup_lines("1.2.3", model, project, ExecutionMode::Supervised, 96);
        let rows = plain(&normal);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains(project));
        assert!(rows[0].ends_with("v1.2.3"));
        assert!(rows[1].contains(model));
        assert!(rows[1].ends_with("supervised"));

        let long_version = "界\x1b[31m\n".repeat(100);
        assert_eq!(
            startup_lines(&long_version, model, project, ExecutionMode::Supervised, 96),
            startup_lines("", model, project, ExecutionMode::Supervised, 96),
        );
        let long_project = format!("{}/workspace", "界/e\u{301}".repeat(100));
        let long_model = "model-α".repeat(100);
        for width in [24, 48, 72, 96] {
            let lines = startup_lines(
                "1.2.3",
                &long_model,
                &long_project,
                ExecutionMode::Supervised,
                width,
            );
            let rows = plain(&lines);
            assert!(lines.iter().all(|line| line.width() <= width));
            assert_eq!(rows.len(), 2);
            assert!(rows[0].ends_with("/workspace"));
            assert!(rows[1].ends_with("supervised"));
            assert!(!rows.concat().contains("v1.2.3"));
        }
    }

    #[test]
    fn tiny_startup_preserves_the_complete_safety_mode() {
        for mode in [
            ExecutionMode::Supervised,
            ExecutionMode::Auto,
            ExecutionMode::Yolo,
        ] {
            for width in 1..24 {
                let lines = startup_lines("9.9.9", "long-model-name", "~/workspace", mode, width);
                assert!(lines.iter().all(|line| line.width() <= width));
                assert!(plain(&lines).concat().ends_with(mode_label(mode)));
            }
        }
    }

    #[test]
    fn narrow_tool_summaries_keep_lifecycle_and_expanded_context() {
        for (lifecycle, status) in [
            (ToolLifecycle::Running, "running"),
            (ToolLifecycle::Completed, "done"),
            (ToolLifecycle::Failed, "failed"),
        ] {
            let context = format!("{}/end.rs", "long/path/".repeat(8));
            let entry = TranscriptEntry::ToolCall(super::super::ToolTranscript {
                call_id: None,
                name: "read".into(),
                context: Some(context.clone()),
                output: String::new(),
                lifecycle,
            });
            for width in 1..32 {
                for detail in [TranscriptDetail::Compact, TranscriptDetail::Expanded] {
                    let lines = transcript_lines(std::slice::from_ref(&entry), width, detail);
                    let text = plain(&lines).concat();
                    assert!(lines.iter().all(|line| line.width() <= width));
                    assert!(text.contains(status), "width {width}: {text}");
                    if detail == TranscriptDetail::Expanded {
                        assert!(text.contains(&context));
                    }
                }
            }
        }
    }

    #[test]
    fn failed_reads_and_display_load_diagnostics_keep_compact_output() {
        let entries = [
            TranscriptEntry::ToolCall(super::super::ToolTranscript {
                call_id: None,
                name: "read".into(),
                context: Some("private.txt".into()),
                output: "initial detail\nread attempt\npermission denied\nno content read".into(),
                lifecycle: ToolLifecycle::Failed,
            }),
            TranscriptEntry::ToolCall(super::super::ToolTranscript {
                call_id: None,
                name: "read".into(),
                context: Some("large.txt".into()),
                output: "fallback detail\n[display output unavailable: invalid checksum]\n".into(),
                lifecycle: ToolLifecycle::Completed,
            }),
        ];
        let compact = plain(&transcript_lines(&entries, 80, TranscriptDetail::Compact)).join("\n");
        assert!(compact.contains("permission denied"));
        assert!(compact.contains("no content read"));
        assert!(compact.contains("[display output unavailable: invalid checksum]"));
        let expanded =
            plain(&transcript_lines(&entries, 80, TranscriptDetail::Expanded)).join("\n");
        assert!(expanded.contains("initial detail"));
        assert!(expanded.contains("read attempt"));
        assert!(expanded.contains("fallback detail"));
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
        let rows = rich_plain(&markdown_lines("```text\n alpha  \n\n\n \n```", 80));
        assert_eq!(rows, [" alpha  ", "", "", " "]);
        let source = "  one  \n\n\n two\n \n";
        let rows = plain(&transcript_lines(
            &[tool(source)],
            80,
            TranscriptDetail::Expanded,
        ));
        let output = rows[1..rows.len() - 1]
            .iter()
            .map(|row| row.strip_prefix("  ").unwrap())
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
    fn only_older_assistant_outputs_have_full_width_muted_rules() {
        let mut entries = vec![
            TranscriptEntry::UserTurn {
                body: "first".into(),
            },
            TranscriptEntry::AssistantMessage {
                body: "older".into(),
            },
            tool("result"),
            TranscriptEntry::UserTurn {
                body: "second".into(),
            },
        ];
        for detail in [TranscriptDetail::Compact, TranscriptDetail::Expanded] {
            let rows = transcript_lines(&entries, 40, detail);
            assert!(!plain(&rows).iter().any(|row| row.contains('─')));
        }
        entries.push(TranscriptEntry::AssistantMessage {
            body: "latest".into(),
        });
        for detail in [TranscriptDetail::Compact, TranscriptDetail::Expanded] {
            let rows = transcript_lines(&entries, 40, detail);
            let text = plain(&rows);
            let older = text.iter().position(|row| row == "older").unwrap();
            let latest = text.iter().position(|row| row == "latest").unwrap();
            assert!(text[older + 1].is_empty());
            assert_eq!(text[older + 2], "─".repeat(40));
            assert!(text[older + 3].is_empty());
            assert_eq!(rows[older + 2].spans[0].style.fg, Some(BORDER));
            assert_eq!(text.iter().filter(|row| row.contains('─')).count(), 1);
            assert!(text[latest + 1..].iter().all(|row| row.trim().is_empty()));
        }
    }

    #[test]
    fn separator_padding_is_independent_of_markdown_trailing_blank_rows() {
        for body in ["older\n\n", "```rust\nolder\n\n\n```"] {
            for width in [16, 40, 100] {
                let entries = [
                    TranscriptEntry::AssistantMessage { body: body.into() },
                    TranscriptEntry::AssistantMessage {
                        body: "latest".into(),
                    },
                ];
                let rows = transcript_lines(&entries, width, TranscriptDetail::Compact);
                assert_eq!(
                    plain(&rows),
                    ["older", "", &"─".repeat(width), "", "latest", ""]
                );
                let markdown_rule = markdown_lines("before\n\n---\n\nafter", width);
                assert!(rich_plain(&markdown_rule).contains(&"─".repeat(width)));
            }
        }
    }

    #[test]
    fn transcript_entries_fit_tiny_widths_and_never_emit_controls() {
        let hostile = "界👨‍👩‍👧‍👦e\u{301}\x1b[31m red\x07";
        let entries = [
            TranscriptEntry::Startup {
                version: hostile.into(),
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
            TranscriptEntry::ToolCall(super::super::ToolTranscript {
                call_id: None,
                name: format!("tool/{hostile}"),
                context: Some(hostile.into()),
                output: hostile.into(),
                lifecycle: ToolLifecycle::Running,
            }),
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
        let rows = rich_plain(&markdown_lines(
            "-\n  - child\n\n-\n  > quoted\n\n-\n  ```\n  code\n  ```",
            80,
        ));
        assert!(!rows.iter().any(|row| row.trim() == "•"));
        assert!(rows.iter().any(|row| row == "• • child"));
        assert!(rows.iter().any(|row| row == "• │ quoted"));
        assert!(rows.iter().any(|row| row == "• code"));
    }

    #[test]
    fn partial_markdown_can_reinterpret_earlier_text_without_losing_code() {
        assert_eq!(rich_plain(&markdown_lines("Title", 80)), ["Title"]);
        let heading = markdown_lines("Title\n===", 80);
        assert_eq!(rich_plain(&heading), ["# Title"]);
        assert!(
            heading[0]
                .text
                .spans
                .iter()
                .all(|span| span.style.add_modifier.contains(Modifier::BOLD))
        );
        for source in ["```rust\nlet x = 1;", "```rust\nlet x = 1;\n```"] {
            assert_eq!(rich_plain(&markdown_lines(source, 80)), ["let x = 1;"]);
        }
        let decoded = markdown_lines("| heading |\n|---|\n| &#27;[31mvisible&#7; |", 80);
        assert!(
            rich_plain(&decoded)
                .iter()
                .flat_map(|line| line.chars())
                .all(|ch| !ch.is_control())
        );
    }

    #[test]
    fn header_only_tables_and_deep_nesting_keep_source_available() {
        let rows = rich_plain(&markdown_lines("| alphabet | second |\n|---|---|", 3));
        assert_eq!(rows.concat(), "alphabetsecond");
        let source = format!("{}leaf", "> ".repeat(200));
        assert_eq!(rich_plain(&markdown_lines(&source, 20)).concat(), source);
        let mut lines = Vec::new();
        let items = vec![
            vec![MarkdownBlock::Paragraph(vec![StyledFragment {
                content: "one".into(),
                style: text_style(),
                target: None,
            }])],
            vec![MarkdownBlock::Paragraph(vec![StyledFragment {
                content: "two".into(),
                style: text_style(),
                target: None,
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
            rich_plain(&lines),
            [format!("{}. one", u64::MAX), format!("{}. two", u64::MAX)]
        );
    }
}
