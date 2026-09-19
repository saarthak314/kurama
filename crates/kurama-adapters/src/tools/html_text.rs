const DROPPED_TAGS: &[&str] = &["script", "style", "svg", "noscript"];
const BLOCK_TAGS: &[&str] = &[
    "p", "div", "section", "article", "h1", "h2", "h3", "h4", "h5", "h6", "li", "pre", "tr",
];

pub fn html_to_text(html: &str) -> String {
    let mut output = TextOutput::default();
    let mut index = 0;
    let bytes = html.as_bytes();

    while index < bytes.len() {
        if starts_with_ascii_case_insensitive(&html[index..], "<!--") {
            index = html[index + 4..]
                .find("-->")
                .map(|offset| index + 4 + offset + 3)
                .unwrap_or(bytes.len());
            continue;
        }

        if bytes[index] == b'<' {
            let Some(tag_end) = find_tag_end(html, index + 1) else {
                output.push_char('<');
                index += 1;
                continue;
            };
            let raw = html[index + 1..tag_end].trim();
            let closing = raw.starts_with('/');
            let tag = tag_name(raw);
            let self_closing = raw.ends_with('/');

            if !closing
                && DROPPED_TAGS
                    .iter()
                    .any(|dropped| tag.eq_ignore_ascii_case(dropped))
            {
                index = skip_dropped_element(html, tag_end + 1, tag);
                continue;
            }
            if tag.eq_ignore_ascii_case("br") {
                output.newline();
            } else if tag.eq_ignore_ascii_case("pre") {
                output.newline();
                output.preformatted = !closing;
                if closing {
                    output.newline();
                }
            } else if BLOCK_TAGS
                .iter()
                .any(|block| tag.eq_ignore_ascii_case(block))
            {
                output.newline();
                if closing || self_closing {
                    output.newline();
                }
            }
            index = tag_end + 1;
            continue;
        }

        if bytes[index] == b'&'
            && let Some((character, consumed)) = decode_entity(&html[index..])
        {
            output.push_char(character);
            index += consumed;
            continue;
        }

        let character = html[index..]
            .chars()
            .next()
            .expect("index remains on a character boundary");
        output.push_char(character);
        index += character.len_utf8();
    }

    output.finish()
}

#[derive(Default)]
struct TextOutput {
    value: String,
    pending_space: bool,
    preformatted: bool,
}

impl TextOutput {
    fn push_char(&mut self, character: char) {
        if self.preformatted {
            self.value.push(character);
            return;
        }
        if character.is_whitespace() {
            self.pending_space = !self.value.is_empty() && !self.value.ends_with('\n');
            return;
        }
        if self.pending_space && !self.value.ends_with([' ', '\n']) {
            self.value.push(' ');
        }
        self.pending_space = false;
        self.value.push(character);
    }

    fn newline(&mut self) {
        self.pending_space = false;
        while self.value.ends_with(' ') {
            self.value.pop();
        }
        if !self.value.is_empty() && !self.value.ends_with('\n') {
            self.value.push('\n');
        }
    }

    fn finish(mut self) -> String {
        self.value.truncate(self.value.trim_end().len());
        let leading = self.value.len() - self.value.trim_start().len();
        if leading != 0 {
            drop(self.value.drain(..leading));
        }
        self.value
    }
}

fn find_tag_end(html: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, character) in html[start..].char_indices() {
        match (quote, character) {
            (Some(expected), current) if expected == current => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None, '>') => return Some(start + offset),
            _ => {}
        }
    }
    None
}

fn tag_name(raw: &str) -> &str {
    let name = raw.trim_start_matches(['/', '!', '?']);
    let end = name
        .bytes()
        .position(|byte| !byte.is_ascii_alphanumeric() && byte != b'-')
        .unwrap_or(name.len());
    &name[..end]
}

fn skip_dropped_element(html: &str, start: usize, tag: &str) -> usize {
    let name_end = tag.len() + 2;
    let closing = html.as_bytes()[start..]
        .windows(name_end + 1)
        .position(|candidate| {
            candidate.starts_with(b"</")
                && candidate[2..name_end].eq_ignore_ascii_case(tag.as_bytes())
                && (candidate[name_end].is_ascii_whitespace()
                    || matches!(candidate[name_end], b'>' | b'/'))
        });
    closing
        .and_then(|offset| find_tag_end(html, start + offset + name_end))
        .map(|end| end + 1)
        .unwrap_or(html.len())
}

fn decode_entity(input: &str) -> Option<(char, usize)> {
    let end = input.get(1..)?.find(';')? + 1;
    if end > 16 {
        return None;
    }
    let entity = &input[1..end];
    let character = match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        value if value.starts_with("#x") || value.starts_with("#X") => {
            char::from_u32(u32::from_str_radix(&value[2..], 16).ok()?)?
        }
        value if value.starts_with('#') => char::from_u32(value[1..].parse().ok()?)?,
        _ => return None,
    };
    Some((character, end + 1))
}

fn starts_with_ascii_case_insensitive(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}
