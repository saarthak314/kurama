use ratatui::style::{Color, Modifier, Style};

const KEYWORD: Color = Color::Rgb(198, 120, 221);
const FUNCTION: Color = Color::Rgb(116, 177, 255);
const TYPE: Color = Color::Rgb(137, 220, 235);
const STRING: Color = Color::Rgb(166, 227, 161);
const NUMBER: Color = Color::Rgb(249, 226, 175);
const COMMENT: Color = Color::Rgb(126, 132, 146);
const CONSTANT: Color = Color::Rgb(255, 184, 108);
const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 10_000;
const MAX_HIGHLIGHT_LINE_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
    JavaScript,
    TypeScript,
    Python,
    Shell,
    Json,
    Toml,
    Yaml,
    Go,
    C,
    Cpp,
    Markdown,
}

pub(crate) struct HighlightedSpan {
    pub(crate) content: String,
    pub(crate) style: Style,
}

pub(crate) fn highlight_code(code: &str, language: &str) -> Option<Vec<HighlightedSpan>> {
    let language = parse_language(language)?;
    if exceeds_limits(code) {
        return None;
    }

    let mut spans = Vec::new();
    let mut offset = 0;
    while offset < code.len() {
        let rest = &code[offset..];

        if language == Language::Markdown && at_line_start(code, offset) && rest.starts_with('#') {
            let length = take_line(rest);
            push_span(
                &mut spans,
                &rest[..length],
                Style::default().fg(FUNCTION).add_modifier(Modifier::BOLD),
            );
            offset += length;
            continue;
        }
        if language == Language::Markdown && rest.starts_with("<!--") {
            let length = take_until_inclusive(rest, "-->");
            push_span(&mut spans, &rest[..length], comment_style());
            offset += length;
            continue;
        }
        if supports_block_comments(language) && rest.starts_with("/*") {
            let length = take_until_inclusive(rest, "*/");
            push_span(&mut spans, &rest[..length], comment_style());
            offset += length;
            continue;
        }
        if let Some(marker) = line_comment_marker(language)
            && rest.starts_with(marker)
        {
            let length = take_line(rest);
            push_span(&mut spans, &rest[..length], comment_style());
            offset += length;
            continue;
        }
        if language == Language::Rust
            && let Some(length) = rust_lifetime_length(rest)
        {
            push_span(&mut spans, &rest[..length], Style::default().fg(TYPE));
            offset += length;
            continue;
        }
        if let Some(quote) = string_quote(language, rest) {
            let length = take_quoted(rest, quote);
            let style =
                if language == Language::Json && rest[length..].trim_start().starts_with(':') {
                    Style::default().fg(FUNCTION)
                } else {
                    Style::default().fg(STRING)
                };
            push_span(&mut spans, &rest[..length], style);
            offset += length;
            continue;
        }

        let first = rest.as_bytes()[0];
        if first.is_ascii_digit() {
            let length = take_number(rest);
            push_span(&mut spans, &rest[..length], Style::default().fg(NUMBER));
            offset += length;
            continue;
        }
        if is_identifier_start(first) {
            let length = take_identifier(rest);
            let word = &rest[..length];
            let tail = rest[length..].trim_start();
            let style = if is_keyword(language, word) {
                Style::default().fg(KEYWORD)
            } else if is_type(language, word) {
                Style::default().fg(TYPE)
            } else if is_constant(word) {
                Style::default().fg(CONSTANT)
            } else if is_key(language, tail) || tail.starts_with('(') || tail.starts_with('!') {
                Style::default().fg(FUNCTION)
            } else {
                Style::default().fg(Color::Reset)
            };
            push_span(&mut spans, word, style);
            offset += length;
            continue;
        }

        let length = rest.chars().next().map_or(1, char::len_utf8);
        push_span(
            &mut spans,
            &rest[..length],
            Style::default().fg(Color::Reset),
        );
        offset += length;
    }

    Some(spans)
}

fn parse_language(language: &str) -> Option<Language> {
    match language.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" => Some(Language::Rust),
        "javascript" | "js" | "jsx" | "mjs" | "cjs" => Some(Language::JavaScript),
        "typescript" | "ts" | "tsx" | "mts" | "cts" => Some(Language::TypeScript),
        "python" | "python3" | "py" => Some(Language::Python),
        "bash" | "shell" | "sh" | "zsh" | "fish" => Some(Language::Shell),
        "json" | "jsonc" => Some(Language::Json),
        "toml" => Some(Language::Toml),
        "yaml" | "yml" => Some(Language::Yaml),
        "go" | "golang" => Some(Language::Go),
        "c" | "h" => Some(Language::C),
        "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hxx" => Some(Language::Cpp),
        "markdown" | "md" => Some(Language::Markdown),
        _ => None,
    }
}

fn exceeds_limits(code: &str) -> bool {
    if code.len() > MAX_HIGHLIGHT_BYTES {
        return true;
    }
    let mut lines = 1;
    let mut line_bytes = 0;
    for byte in code.bytes() {
        if byte == b'\n' {
            lines += 1;
            line_bytes = 0;
            if lines > MAX_HIGHLIGHT_LINES {
                return true;
            }
        } else {
            line_bytes += 1;
            if line_bytes > MAX_HIGHLIGHT_LINE_BYTES {
                return true;
            }
        }
    }
    false
}

fn supports_block_comments(language: Language) -> bool {
    matches!(
        language,
        Language::Rust
            | Language::JavaScript
            | Language::TypeScript
            | Language::Json
            | Language::Go
            | Language::C
            | Language::Cpp
    )
}

fn line_comment_marker(language: Language) -> Option<&'static str> {
    match language {
        Language::Rust
        | Language::JavaScript
        | Language::TypeScript
        | Language::Json
        | Language::Go
        | Language::C
        | Language::Cpp => Some("//"),
        Language::Python | Language::Shell | Language::Toml | Language::Yaml => Some("#"),
        Language::Markdown => None,
    }
}

fn string_quote(language: Language, rest: &str) -> Option<u8> {
    let quote = rest.as_bytes()[0];
    match quote {
        b'"' => Some(quote),
        b'\'' if language != Language::Json => Some(quote),
        b'`' if matches!(
            language,
            Language::JavaScript | Language::TypeScript | Language::Shell
        ) =>
        {
            Some(quote)
        }
        _ => None,
    }
}

fn rust_lifetime_length(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    if bytes.first() != Some(&b'\'')
        || !bytes
            .get(1)
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
    {
        return None;
    }
    let identifier_bytes = bytes[1..]
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
        .count();
    let length = identifier_bytes + 1;
    (bytes.get(length) != Some(&b'\'')).then_some(length)
}

fn take_quoted(value: &str, quote: u8) -> usize {
    let bytes = value.as_bytes();
    let mut offset = 1;
    let mut escaped = false;
    while offset < bytes.len() {
        let byte = bytes[offset];
        offset += 1;
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == quote {
            break;
        }
    }
    offset
}

fn take_until_inclusive(value: &str, marker: &str) -> usize {
    value
        .find(marker)
        .map_or(value.len(), |offset| offset + marker.len())
}

fn take_line(value: &str) -> usize {
    value.find('\n').unwrap_or(value.len())
}

fn take_number(value: &str) -> usize {
    value
        .as_bytes()
        .iter()
        .take_while(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'+' | b'-')
        })
        .count()
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$')
}

fn take_identifier(value: &str) -> usize {
    value
        .as_bytes()
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'-'))
        .count()
}

fn is_keyword(language: Language, word: &str) -> bool {
    match language {
        Language::Rust => matches!(
            word,
            "as" | "async"
                | "await"
                | "break"
                | "const"
                | "continue"
                | "crate"
                | "dyn"
                | "else"
                | "enum"
                | "extern"
                | "false"
                | "fn"
                | "for"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pub"
                | "ref"
                | "return"
                | "self"
                | "Self"
                | "static"
                | "struct"
                | "super"
                | "trait"
                | "true"
                | "type"
                | "unsafe"
                | "use"
                | "where"
                | "while"
        ),
        Language::JavaScript => is_javascript_keyword(word),
        Language::TypeScript => {
            is_javascript_keyword(word)
                || matches!(
                    word,
                    "abstract"
                        | "declare"
                        | "enum"
                        | "implements"
                        | "interface"
                        | "keyof"
                        | "namespace"
                        | "private"
                        | "protected"
                        | "public"
                        | "readonly"
                        | "satisfies"
                        | "type"
                )
        }
        Language::Python => matches!(
            word,
            "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "break"
                | "class"
                | "continue"
                | "def"
                | "del"
                | "elif"
                | "else"
                | "except"
                | "False"
                | "finally"
                | "for"
                | "from"
                | "global"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "None"
                | "nonlocal"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "True"
                | "try"
                | "while"
                | "with"
                | "yield"
        ),
        Language::Shell => matches!(
            word,
            "case"
                | "do"
                | "done"
                | "elif"
                | "else"
                | "esac"
                | "fi"
                | "for"
                | "function"
                | "if"
                | "in"
                | "select"
                | "then"
                | "time"
                | "until"
                | "while"
        ),
        Language::Go => matches!(
            word,
            "break"
                | "case"
                | "chan"
                | "const"
                | "continue"
                | "default"
                | "defer"
                | "else"
                | "fallthrough"
                | "for"
                | "func"
                | "go"
                | "goto"
                | "if"
                | "import"
                | "interface"
                | "map"
                | "package"
                | "range"
                | "return"
                | "select"
                | "struct"
                | "switch"
                | "type"
                | "var"
        ),
        Language::C | Language::Cpp => matches!(
            word,
            "alignas"
                | "alignof"
                | "auto"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "constexpr"
                | "continue"
                | "default"
                | "delete"
                | "do"
                | "else"
                | "enum"
                | "explicit"
                | "extern"
                | "false"
                | "for"
                | "friend"
                | "if"
                | "inline"
                | "namespace"
                | "new"
                | "noexcept"
                | "nullptr"
                | "operator"
                | "private"
                | "protected"
                | "public"
                | "return"
                | "sizeof"
                | "static"
                | "struct"
                | "switch"
                | "template"
                | "this"
                | "throw"
                | "true"
                | "try"
                | "typedef"
                | "typename"
                | "union"
                | "using"
                | "virtual"
                | "volatile"
                | "while"
        ),
        Language::Json | Language::Toml | Language::Yaml => {
            matches!(word, "false" | "null" | "true")
        }
        Language::Markdown => false,
    }
}

fn is_javascript_keyword(word: &str) -> bool {
    matches!(
        word,
        "async"
            | "await"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "from"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "let"
            | "new"
            | "null"
            | "of"
            | "return"
            | "static"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "undefined"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
    )
}

fn is_type(language: Language, word: &str) -> bool {
    let built_in = match language {
        Language::Rust => matches!(
            word,
            "bool"
                | "char"
                | "f32"
                | "f64"
                | "i8"
                | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "isize"
                | "str"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "usize"
        ),
        Language::JavaScript => false,
        Language::TypeScript => matches!(
            word,
            "any"
                | "bigint"
                | "boolean"
                | "never"
                | "number"
                | "object"
                | "string"
                | "symbol"
                | "unknown"
                | "void"
        ),
        Language::Python => matches!(
            word,
            "bool"
                | "bytes"
                | "dict"
                | "float"
                | "frozenset"
                | "int"
                | "list"
                | "object"
                | "set"
                | "str"
                | "tuple"
        ),
        Language::Go => matches!(
            word,
            "bool"
                | "byte"
                | "complex64"
                | "complex128"
                | "error"
                | "float32"
                | "float64"
                | "int"
                | "int8"
                | "int16"
                | "int32"
                | "int64"
                | "rune"
                | "string"
                | "uint"
                | "uint8"
                | "uint16"
                | "uint32"
                | "uint64"
                | "uintptr"
        ),
        Language::C | Language::Cpp => matches!(
            word,
            "bool"
                | "char"
                | "double"
                | "float"
                | "int"
                | "long"
                | "short"
                | "signed"
                | "size_t"
                | "unsigned"
                | "void"
                | "wchar_t"
        ),
        Language::Shell | Language::Json | Language::Toml | Language::Yaml | Language::Markdown => {
            false
        }
    };
    built_in
        || (!matches!(
            language,
            Language::Shell | Language::Json | Language::Toml | Language::Yaml | Language::Markdown
        ) && word.chars().next().is_some_and(char::is_uppercase))
}

fn is_key(language: Language, tail: &str) -> bool {
    match language {
        Language::Toml => tail.starts_with('='),
        Language::Yaml => tail.starts_with(':'),
        _ => false,
    }
}

fn is_constant(word: &str) -> bool {
    word.len() > 1
        && word.bytes().any(|byte| byte.is_ascii_alphabetic())
        && word
            .bytes()
            .all(|byte| !byte.is_ascii_alphabetic() || byte.is_ascii_uppercase())
}

fn at_line_start(code: &str, offset: usize) -> bool {
    offset == 0 || code.as_bytes().get(offset.wrapping_sub(1)) == Some(&b'\n')
}

fn comment_style() -> Style {
    Style::default().fg(COMMENT).add_modifier(Modifier::ITALIC)
}

fn push_span(spans: &mut Vec<HighlightedSpan>, content: &str, style: Style) {
    if content.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.push_str(content);
    } else {
        spans.push(HighlightedSpan {
            content: content.to_owned(),
            style,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_literal_strings_use_string_style() {
        let spans = highlight_code("name = 'kurama'", "toml").expect("known language");
        let literal = spans
            .iter()
            .find(|span| span.content == "'kurama'")
            .expect("literal string span");

        assert_eq!(literal.style.fg, Some(STRING));
    }

    #[test]
    fn aliases_preserve_source_text() {
        for language in ["rs", "tsx", "python3", "shell", "yml", "golang", "c++"] {
            let spans = highlight_code("value = 42\n", language).expect("known alias");
            let reconstructed = spans
                .iter()
                .map(|span| span.content.as_str())
                .collect::<String>();
            assert_eq!(reconstructed, "value = 42\n", "alias {language}");
        }
    }

    #[test]
    fn unknown_and_pathological_inputs_fall_back() {
        assert!(highlight_code("value", "unknown-language").is_none());
        assert!(highlight_code(&"x".repeat(MAX_HIGHLIGHT_LINE_BYTES + 1), "rust").is_none());
        assert!(highlight_code(&"x\n".repeat(MAX_HIGHLIGHT_LINES + 1), "rust").is_none());
    }
}
