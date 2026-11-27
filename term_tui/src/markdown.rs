//! Markdown for the transcript.
//!
//! The agent's answers are markdown, and a terminal that prints the source shows
//! the scaffolding instead of the answer: `**bold**` with its asterisks, a fence
//! language glued to the first line of a command, a table that is only readable
//! as source. This module renders the subset that matters in a conversation —
//! paragraphs, headings, lists, quotes, rules, inline emphasis and links, and
//! fenced or indented code with a light syntax highlight — into styled terminal
//! lines.
//!
//! It is deliberately a *renderer*, not a parser: it never fails, never reorders
//! text, and always produces lines that fit the width it was given. A model
//! streams half-finished markdown (an unclosed fence, a lone `**`), so the
//! interesting states are the incomplete ones — an open fence renders as code
//! that is visibly still arriving rather than as a paragraph full of backticks.
//!
//! Everything here is a pure function of the text and the width, so the whole
//! surface is unit-tested without a terminal.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// The colours a transcript uses. One place, so the page and the tests agree.
mod palette {
    use super::*;

    pub const TEXT: Style = Style::new();
    pub const CODE: Style = Style::new().fg(Color::LightCyan);
    pub const CODE_BG: Color = Color::Indexed(236);
    pub const CODE_GUTTER: Style = Style::new().fg(Color::Indexed(240));
    pub const CODE_LANG: Style = Style::new().fg(Color::Indexed(245));
    pub const HEADING: [Style; 3] = [
        Style::new().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
        Style::new().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
        Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
    ];
    pub const BULLET: Style = Style::new().fg(Color::DarkGray);
    pub const QUOTE: Style = Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM);
    pub const RULE: Style = Style::new().fg(Color::Indexed(238));
    pub const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
    pub const ITALIC: Style = Style::new().add_modifier(Modifier::ITALIC);
    pub const STRIKE: Style = Style::new().add_modifier(Modifier::CROSSED_OUT);
    pub const LINK: Style = Style::new().fg(Color::LightBlue).add_modifier(Modifier::UNDERLINED);
    pub const URL: Style = Style::new().fg(Color::Indexed(240));
    pub const KEYWORD: Style = Style::new().fg(Color::LightMagenta);
    pub const STRING: Style = Style::new().fg(Color::LightGreen);
    pub const COMMENT: Style = Style::new().fg(Color::Indexed(243)).add_modifier(Modifier::ITALIC);
    pub const NUMBER: Style = Style::new().fg(Color::LightYellow);
    pub const DIFF_ADD: Style = Style::new().fg(Color::LightGreen);
    pub const DIFF_DEL: Style = Style::new().fg(Color::LightRed);
    pub const DIFF_META: Style = Style::new().fg(Color::LightCyan);
}

/// Display width of one character.
fn width_of(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(1)
}

/// Total display width of a string.
pub fn text_width(s: &str) -> usize {
    s.chars().map(width_of).sum()
}

/// Render markdown `text` into lines that fit `width` display cells.
///
/// No line is wider than `width`, including the code gutter; a word that cannot
/// fit is split rather than dropped.
pub fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut out: Vec<Line<'static>> = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0usize;
    // Set while a paragraph is open, so consecutive source lines are re-flowed
    // together (markdown soft-wrap) instead of each becoming its own line.
    let mut paragraph: Vec<String> = Vec::new();

    while i < lines.len() {
        let raw = lines[i].trim_end_matches('\r');
        let trimmed = raw.trim_start();

        // ── fenced code ────────────────────────────────────────────────────
        if let Some(lang) = fence_language(trimmed) {
            flush_paragraph(&mut paragraph, width, &mut out);
            i += 1;
            let mut code: Vec<String> = Vec::new();
            let mut closed = false;
            while i < lines.len() {
                let line = lines[i].trim_end_matches('\r');
                if is_fence(line.trim_start()) {
                    closed = true;
                    i += 1;
                    break;
                }
                code.push(line.to_string());
                i += 1;
            }
            // An unclosed fence is a streaming answer mid-block: render it as
            // code anyway, and leave it visibly open.
            emit_code(&lang, &code, closed, width, &mut out);
            continue;
        }

        // ── indented (4-space) code ────────────────────────────────────────
        if raw.starts_with("    ") && !raw.trim().is_empty() && !looks_like_list(trimmed) {
            flush_paragraph(&mut paragraph, width, &mut out);
            let mut code: Vec<String> = Vec::new();
            while i < lines.len() {
                let line = lines[i].trim_end_matches('\r');
                if line.trim().is_empty() {
                    // A blank line inside the block only counts if more code
                    // follows it.
                    let next_is_code = lines
                        .get(i + 1)
                        .map(|n| n.starts_with("    ") && !n.trim().is_empty())
                        .unwrap_or(false);
                    if !next_is_code {
                        break;
                    }
                    code.push(String::new());
                    i += 1;
                    continue;
                }
                if !line.starts_with("    ") {
                    break;
                }
                code.push(line[4..].to_string());
                i += 1;
            }
            emit_code("", &code, true, width, &mut out);
            continue;
        }

        // ── blank ──────────────────────────────────────────────────────────
        if trimmed.is_empty() {
            flush_paragraph(&mut paragraph, width, &mut out);
            out.push(Line::from(""));
            i += 1;
            continue;
        }

        // ── rule ───────────────────────────────────────────────────────────
        if is_rule(trimmed) {
            flush_paragraph(&mut paragraph, width, &mut out);
            out.push(Line::from(Span::styled("─".repeat(width), palette::RULE)));
            i += 1;
            continue;
        }

        // ── heading ────────────────────────────────────────────────────────
        if let Some((level, body)) = heading(trimmed) {
            flush_paragraph(&mut paragraph, width, &mut out);
            let style = palette::HEADING[level.saturating_sub(1).min(2)];
            let marker = match level {
                1 => "▌ ",
                2 => "▎ ",
                _ => "",
            };
            let mut spans: Vec<(String, Style)> = Vec::new();
            if !marker.is_empty() {
                spans.push((marker.to_string(), style));
            }
            spans.extend(inline(body, style));
            out.extend(wrap_spans(spans, width, 0));
            i += 1;
            continue;
        }

        // ── list item ──────────────────────────────────────────────────────
        if let Some((indent, marker, body)) = list_item(raw) {
            flush_paragraph(&mut paragraph, width, &mut out);
            let pad = " ".repeat(indent.min(width.saturating_sub(4)));
            let mut spans: Vec<(String, Style)> = vec![
                (pad, palette::TEXT),
                (format!("{marker} "), palette::BULLET),
            ];
            spans.extend(inline(body, palette::TEXT));
            // A wrapped list item hangs under its text, not under the bullet.
            let hanging = text_width(&span_text(&spans[..2]));
            out.extend(wrap_spans(spans, width, hanging));
            i += 1;
            continue;
        }

        // ── quote ──────────────────────────────────────────────────────────
        if let Some(body) = trimmed.strip_prefix('>') {
            flush_paragraph(&mut paragraph, width, &mut out);
            let body = body.trim_start();
            let mut spans: Vec<(String, Style)> = vec![("│ ".to_string(), palette::QUOTE)];
            spans.extend(inline(body, palette::QUOTE));
            out.extend(wrap_spans(spans, width, 0));
            i += 1;
            continue;
        }

        // ── a table row: a fence-like block of pipes is unreadable when
        //    re-flowed, so its rows are kept as they are.
        if trimmed.starts_with('|') {
            flush_paragraph(&mut paragraph, width, &mut out);
            let body = if is_table_separator(trimmed) {
                "─".repeat(text_width(trimmed).min(width))
            } else {
                trimmed.to_string()
            };
            let style = if is_table_separator(trimmed) {
                palette::RULE
            } else {
                palette::TEXT
            };
            for chunk in hard_split(&body, width) {
                out.push(Line::from(Span::styled(chunk, style)));
            }
            i += 1;
            continue;
        }

        // ── paragraph text ─────────────────────────────────────────────────
        paragraph.push(trimmed.to_string());
        i += 1;
    }
    flush_paragraph(&mut paragraph, width, &mut out);
    out
}

/// The text of some spans, for measuring a hanging indent.
fn span_text(spans: &[(String, Style)]) -> String {
    spans.iter().map(|(t, _)| t.as_str()).collect()
}

/// Emit the accumulated paragraph, re-flowed.
fn flush_paragraph(paragraph: &mut Vec<String>, width: usize, out: &mut Vec<Line<'static>>) {
    if paragraph.is_empty() {
        return;
    }
    let joined = paragraph.join(" ");
    paragraph.clear();
    out.extend(wrap_spans(inline(&joined, palette::TEXT), width, 0));
}

/// Emit a code block: a header naming the language, a gutter per line, and a
/// closing rule when the fence was closed.
fn emit_code(
    lang: &str,
    code: &[String],
    closed: bool,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let inner = width.saturating_sub(4).max(4);
    let label = lang.trim();
    let mut header: Vec<Span<'static>> = vec![Span::styled("┌─", palette::CODE_GUTTER)];
    if !label.is_empty() {
        header.push(Span::styled(format!(" {label} "), palette::CODE_LANG));
    }
    if !closed {
        // Still arriving: say so, so a half-written block does not look final.
        header.push(Span::styled("…", palette::CODE_LANG));
    }
    out.push(Line::from(header));

    let diff = matches!(label, "diff" | "patch" | "udiff");
    for line in code {
        // Code keeps its own layout: long lines are split, never re-flowed.
        for chunk in hard_split(line, inner) {
            let mut spans = vec![Span::styled("│ ", palette::CODE_GUTTER)];
            spans.extend(
                highlight(&chunk, label, diff)
                    .into_iter()
                    .map(|(text, style)| {
                        Span::styled(text, style.bg(palette::CODE_BG))
                    }),
            );
            out.push(Line::from(spans));
        }
    }
    if closed {
        out.push(Line::from(Span::styled("└─", palette::CODE_GUTTER)));
    }
}

/// A light syntax highlight: enough to make code read as code.
///
/// Deliberately not a real tokenizer — it recognises the categories a reader
/// scans for (comments, strings, numbers, keywords) and leaves everything else
/// alone, in whatever language it is given.
fn highlight(line: &str, lang: &str, diff: bool) -> Vec<(String, Style)> {
    if diff {
        let style = if line.starts_with('+') {
            palette::DIFF_ADD
        } else if line.starts_with('-') {
            palette::DIFF_DEL
        } else if line.starts_with("@@") {
            palette::DIFF_META
        } else {
            palette::TEXT
        };
        return vec![(line.to_string(), style)];
    }

    let comment_prefix: &[&str] = match lang {
        "rust" | "rs" | "c" | "cpp" | "c++" | "h" | "hpp" | "js" | "javascript" | "ts"
        | "typescript" | "java" | "go" | "json5" => &["//"],
        "python" | "py" | "sh" | "bash" | "shell" | "zsh" | "toml" | "yaml" | "yml" | "make"
        | "makefile" | "ruby" | "rb" | "perl" | "r" => &["#"],
        "sql" | "lua" | "haskell" | "hs" => &["--"],
        _ => &[],
    };
    let keywords: &[&str] = match lang {
        "rust" | "rs" => &[
            "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
            "enum", "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
            "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
            "trait", "type", "unsafe", "use", "where", "while",
        ],
        "python" | "py" => &[
            "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
            "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in",
            "is", "lambda", "None", "not", "or", "pass", "raise", "return", "True", "False",
            "try", "while", "with", "yield",
        ],
        "js" | "javascript" | "ts" | "typescript" => &[
            "async", "await", "break", "case", "catch", "class", "const", "continue", "default",
            "delete", "do", "else", "export", "extends", "finally", "for", "function", "if",
            "import", "in", "instanceof", "let", "new", "of", "return", "switch", "this",
            "throw", "try", "typeof", "var", "void", "while", "yield",
        ],
        "sh" | "bash" | "shell" | "zsh" => &[
            "case", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if", "in",
            "select", "then", "until", "while", "export", "local", "readonly", "return",
        ],
        "go" => &[
            "break", "case", "chan", "const", "continue", "defer", "else", "fallthrough", "for",
            "func", "go", "goto", "if", "import", "interface", "map", "package", "range",
            "return", "select", "struct", "switch", "type", "var",
        ],
        "sql" => &[
            "select", "from", "where", "insert", "update", "delete", "join", "left", "right",
            "inner", "group", "order", "by", "having", "limit", "create", "table", "index",
            "drop", "alter", "and", "or", "not", "null", "values", "into",
        ],
        "toml" | "yaml" | "yml" | "ini" | "conf" => &["true", "false", "null"],
        _ => &[],
    };
    let lower_keywords: bool = matches!(lang, "sql"); // SQL reads in any case

    let mut out: Vec<(String, Style)> = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    let mut plain = String::new();

    // A comment marker anywhere outside a string runs to the end of the line.
    let comment_at = comment_prefix.iter().find_map(|prefix| {
        let mut needle = prefix.chars();
        let first = needle.next()?;
        let mut state = 0usize;
        let mut in_string: Option<char> = None;
        for (idx, ch) in chars.iter().enumerate() {
            if let Some(quote) = in_string {
                if *ch == quote && chars.get(idx.wrapping_sub(1)) != Some(&'\\') {
                    in_string = None;
                }
                continue;
            }
            if *ch == '"' || *ch == '\'' || *ch == '`' {
                in_string = Some(*ch);
                continue;
            }
            if *ch == first {
                state = 1;
                continue;
            }
            if state > 0 {
                if prefix.chars().nth(state) == Some(*ch) {
                    state += 1;
                    if state == prefix.chars().count() {
                        return Some(idx + 1 - prefix.chars().count());
                    }
                } else {
                    state = if *ch == first { 1 } else { 0 };
                }
            }
        }
        None
    });

    while i < chars.len() {
        let ch = chars[i];
        if let Some(at) = comment_at {
            if i == at {
                if !plain.is_empty() {
                    out.push((std::mem::take(&mut plain), palette::TEXT));
                }
                out.push((chars[i..].iter().collect(), palette::COMMENT));
                break;
            }
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            let quote = ch;
            let start = i;
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 2;
                    continue;
                }
                if chars[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            if !plain.is_empty() {
                out.push((std::mem::take(&mut plain), palette::TEXT));
            }
            out.push((chars[start..i].iter().collect(), palette::STRING));
            continue;
        }
        if ch.is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                i += 1;
            }
            if !plain.is_empty() {
                out.push((std::mem::take(&mut plain), palette::TEXT));
            }
            out.push((chars[start..i].iter().collect(), palette::NUMBER));
            continue;
        }
        if ch.is_alphabetic() || ch == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let hit = if lower_keywords {
                keywords.contains(&word.to_lowercase().as_str())
            } else {
                keywords.contains(&word.as_str())
            };
            if !plain.is_empty() {
                out.push((std::mem::take(&mut plain), palette::TEXT));
            }
            out.push((word, if hit { palette::KEYWORD } else { palette::TEXT }));
            continue;
        }
        plain.push(ch);
        i += 1;
    }
    if !plain.is_empty() {
        out.push((plain, palette::TEXT));
    }
    out
}

/// Detect a fence line and return its language (`""` when it names none).
fn fence_language(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("```").or_else(|| trimmed.strip_prefix("~~~"))?;
    // A fence's info string may carry attributes; the first word is the language.
    let lang = rest.trim().split_whitespace().next().unwrap_or("").to_string();
    Some(lang)
}

/// Whether a line closes a fence.
fn is_fence(trimmed: &str) -> bool {
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// A markdown rule (`---`, `***`, `___`, and longer).
fn is_rule(trimmed: &str) -> bool {
    let trimmed = trimmed.trim_end();
    if trimmed.len() < 3 {
        return false;
    }
    for mark in ['-', '*', '_'] {
        if trimmed.chars().all(|c| c == mark) && trimmed.chars().count() >= 3 {
            return true;
        }
    }
    false
}

/// A heading, with its level (1-6).
fn heading(trimmed: &str) -> Option<(usize, &str)> {
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    // A closing run of hashes is decoration.
    let body = rest.trim().trim_end_matches('#').trim_end();
    Some((hashes, body))
}

/// A table separator row (`|---|---|`).
fn is_table_separator(trimmed: &str) -> bool {
    trimmed.starts_with('|')
        && trimmed
            .chars()
            .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '+'))
        && trimmed.contains('-')
}

fn looks_like_list(trimmed: &str) -> bool {
    list_item(trimmed).is_some()
}

/// A list item: `(indent columns, marker, body)`.
fn list_item(raw: &str) -> Option<(usize, String, &str)> {
    let indent_text: String = raw
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let indent = indent_text.replace('\t', "  ").chars().count();
    let body = &raw[indent_text.len()..];
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = body.strip_prefix(marker) {
            return Some((indent, "•".to_string(), rest.trim_start()));
        }
    }
    // Ordered: `1. ` / `1) `.
    let digits: String = body.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() && digits.len() <= 3 {
        let after = &body[digits.len()..];
        if let Some(rest) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
            let marker = if after.starts_with(')') {
                format!("{digits})")
            } else {
                format!("{digits}.")
            };
            return Some((indent, marker, rest.trim_start()));
        }
    }
    None
}

/// Split a line into display-width chunks (no re-flow: code and tables).
fn hard_split(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    for ch in line.chars() {
        let w = width_of(ch);
        if used + w > width && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push(ch);
        used += w;
    }
    out.push(current);
    out
}

/// Parse inline markdown into styled segments.
fn inline(text: &str, base: Style) -> Vec<(String, Style)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut plain = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        // `code`
        if chars[i] == '`' {
            if let Some(end) = find(&chars, i + 1, '`') {
                if !plain.is_empty() {
                    out.push((std::mem::take(&mut plain), base));
                }
                out.push((chars[i + 1..end].iter().collect(), base.patch(palette::CODE)));
                i = end + 1;
                continue;
            }
        }
        // **bold**
        if chars[i] == '*' && chars.get(i + 1) == Some(&'*') {
            if let Some(end) = find_seq(&chars, i + 2, "**") {
                if !plain.is_empty() {
                    out.push((std::mem::take(&mut plain), base));
                }
                let inner: String = chars[i + 2..end].iter().collect();
                out.extend(inline(&inner, base.patch(palette::BOLD)));
                i = end + 2;
                continue;
            }
        }
        // ~~strike~~
        if chars[i] == '~' && chars.get(i + 1) == Some(&'~') {
            if let Some(end) = find_seq(&chars, i + 2, "~~") {
                if !plain.is_empty() {
                    out.push((std::mem::take(&mut plain), base));
                }
                let inner: String = chars[i + 2..end].iter().collect();
                out.extend(inline(&inner, base.patch(palette::STRIKE)));
                i = end + 2;
                continue;
            }
        }
        // *italic* / _italic_
        if (chars[i] == '*' || chars[i] == '_') && i + 1 < chars.len() && chars[i + 1] != ' ' {
            let mark = chars[i];
            if let Some(end) = find(&chars, i + 1, mark) {
                if end > i + 1 {
                    if !plain.is_empty() {
                        out.push((std::mem::take(&mut plain), base));
                    }
                    let inner: String = chars[i + 1..end].iter().collect();
                    out.extend(inline(&inner, base.patch(palette::ITALIC)));
                    i = end + 1;
                    continue;
                }
            }
        }
        // [text](url)
        if chars[i] == '[' {
            if let Some(close) = find(&chars, i + 1, ']') {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(end) = find(&chars, close + 2, ')') {
                        if !plain.is_empty() {
                            out.push((std::mem::take(&mut plain), base));
                        }
                        let label: String = chars[i + 1..close].iter().collect();
                        let url: String = chars[close + 2..end].iter().collect();
                        out.push((label, base.patch(palette::LINK)));
                        if url != chars[i + 1..close].iter().collect::<String>() {
                            out.push((format!(" {url}"), base.patch(palette::URL)));
                        }
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        plain.push(chars[i]);
        i += 1;
    }
    if !plain.is_empty() {
        out.push((plain, base));
    }
    if out.is_empty() {
        out.push((String::new(), base));
    }
    out
}

/// The next index of `needle` at or after `from`.
fn find(chars: &[char], from: usize, needle: char) -> Option<usize> {
    chars[from.min(chars.len())..]
        .iter()
        .position(|c| *c == needle)
        .map(|p| p + from)
}

/// The next index of the multi-character `needle` at or after `from`.
fn find_seq(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() {
        return None;
    }
    let mut i = from;
    while i + needle.len() <= chars.len() {
        if chars[i..i + needle.len()] == needle[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

// `inline` merges styles with ratatui's own `Style::patch` (the right-hand
// style's colours win, modifiers accumulate), which is exactly the rule this
// renderer wants: a keyword inside a quote keeps the quote's dim.
/// Wrap styled segments to `width`, with `indent` cells of hanging indent on
/// every line but the first (so a list item's text lines up under its text).
fn wrap_spans(spans: Vec<(String, Style)>, width: usize, indent: usize) -> Vec<Line<'static>> {
    let width = width.max(4);
    // Flatten into (word, style) tokens, keeping spaces attached to the word
    // that follows them so a wrap never starts with a space.
    let mut tokens: Vec<(String, Style)> = Vec::new();
    for (text, style) in spans {
        let mut current = String::new();
        for ch in text.chars() {
            if ch == ' ' || ch == '\t' {
                if current.is_empty() {
                    tokens.push((" ".to_string(), style));
                } else {
                    current.push(' ');
                }
            } else {
                current.push(ch);
            }
        }
        if !current.is_empty() {
            tokens.push((current, style));
        }
    }

    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let first_budget = width;
    let rest_budget = width.saturating_sub(indent).max(4);
    let mut budget = first_budget;

    for (token, style) in tokens {
        let is_space = token.chars().all(|c| c == ' ');
        let mut token = token;
        loop {
            let token_width = text_width(&token);
            if used + token_width <= budget || (used == 0 && !is_space) {
                // Fits, or it is a fresh line: a word longer than the line is
                // split below rather than overflowing.
                if used + token_width > budget {
                    let (head, rest) = split_width(&token, budget);
                    current.push(Span::styled(head, style));
                    lines.push(std::mem::take(&mut current));
                    budget = rest_budget;
                    used = 0;
                    token = rest;
                    continue;
                }
                if !(is_space && used == 0) {
                    current.push(Span::styled(token.clone(), style));
                    used += token_width;
                }
                break;
            }
            lines.push(std::mem::take(&mut current));
            budget = rest_budget;
            used = 0;
            if is_space {
                break;
            }
        }
    }
    lines.push(current);

    lines
        .into_iter()
        .enumerate()
        .map(|(i, spans)| {
            let mut spans = spans;
            if i > 0 && indent > 0 {
                spans.insert(0, Span::raw(" ".repeat(indent)));
            }
            Line::from(spans)
        })
        .collect()
}

/// Split a token into (head that fits, rest).
fn split_width(token: &str, width: usize) -> (String, String) {
    let mut head = String::new();
    let mut used = 0usize;
    let mut chars = token.chars();
    while let Some(ch) = chars.next() {
        let w = width_of(ch);
        if used + w > width {
            let mut rest = String::new();
            rest.push(ch);
            rest.extend(chars);
            return (head, rest);
        }
        head.push(ch);
        used += w;
    }
    (head, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flatten lines to text, for assertions.
    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn styles_of(lines: &[Line<'static>]) -> Vec<Style> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.style))
            .collect()
    }

    #[test]
    fn a_paragraph_is_re_flowed_to_the_width() {
        let lines = render("the quick brown fox jumps over the lazy dog", 20);
        assert!(lines.len() > 1, "it wraps");
        for l in &lines {
            assert!(l.width() <= 20, "line too wide: {:?}", text_of(&[l.clone()]));
        }
        assert!(text_of(&lines).contains("the quick brown fox"));
    }

    #[test]
    fn inline_markdown_loses_its_scaffolding_and_keeps_its_emphasis() {
        let lines = render("a **bold** and `code` and *ital* and ~~gone~~ end", 80);
        let text = text_of(&lines);
        assert!(!text.contains('*'), "no asterisks left: {text}");
        assert!(!text.contains('`'), "no backticks left: {text}");
        assert!(!text.contains("~~"), "{text}");
        assert!(text.contains("bold") && text.contains("code") && text.contains("ital"));
        let styles = styles_of(&lines);
        assert!(
            styles.iter().any(|s| s.add_modifier.contains(Modifier::BOLD)),
            "bold survived"
        );
        assert!(styles.iter().any(|s| s.fg == Some(Color::LightCyan)), "inline code is styled");
        assert!(styles.iter().any(|s| s.add_modifier.contains(Modifier::CROSSED_OUT)));
    }

    #[test]
    fn a_link_shows_its_text_and_its_target() {
        let lines = render("see [the docs](https://example.com/x) for more", 80);
        let text = text_of(&lines);
        assert!(text.contains("the docs"), "{text}");
        assert!(text.contains("https://example.com/x"), "{text}");
        assert!(!text.contains("]("), "no markdown syntax left: {text}");
        assert!(styles_of(&lines).iter().any(|s| s.add_modifier.contains(Modifier::UNDERLINED)));
    }

    #[test]
    fn a_fenced_block_is_kept_as_written_and_named() {
        let source = "before\n\n```rust\nfn main() {\n    let x = 1;   // note\n}\n```\n\nafter";
        let lines = render(source, 60);
        let text = text_of(&lines);
        assert!(text.contains("┌─ rust"), "the language is named: {text}");
        assert!(text.contains("└─"), "the block is closed: {text}");
        assert!(
            text.contains("    let x = 1;   // note"),
            "inner indentation and spacing survive: {text}"
        );
        assert!(text.contains("before") && text.contains("after"), "{text}");
        assert!(!text.contains("```"), "the fence itself is gone: {text}");
        // The keyword and the comment are highlighted, the code sits on a
        // background so the block reads as a block.
        let styles = styles_of(&lines);
        assert!(styles.iter().any(|s| s.fg == Some(Color::LightMagenta)), "fn is a keyword");
        assert!(
            styles.iter().any(|s| s.bg == Some(palette::CODE_BG)),
            "code has a background"
        );
    }

    #[test]
    fn an_unclosed_fence_is_still_code_and_says_it_is_arriving() {
        // This is the streaming case: the model has not finished the block.
        let lines = render("here:\n```sh\npnpm test", 40);
        let text = text_of(&lines);
        assert!(text.contains("┌─ sh"), "{text}");
        assert!(text.contains('…'), "marked as still arriving: {text}");
        assert!(text.contains("pnpm test"), "{text}");
        assert!(!text.contains("└─"), "no closing rule while it is open: {text}");
    }

    #[test]
    fn a_long_code_line_is_split_not_re_flowed() {
        let line = format!("```\n{}\n```", "x".repeat(120));
        let lines = render(&line, 30);
        for l in &lines {
            assert!(l.width() <= 30, "code line too wide: {:?}", text_of(&[l.clone()]));
        }
        // Nothing was dropped or re-ordered.
        let code: String = text_of(&lines)
            .lines()
            .filter(|l| l.starts_with("│ "))
            .map(|l| l.trim_start_matches("│ ").to_string())
            .collect();
        assert_eq!(code, "x".repeat(120));
    }

    #[test]
    fn a_diff_block_is_coloured_by_line() {
        let source = "```diff\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n```";
        let lines = render(source, 60);
        let add = lines
            .iter()
            .find(|l| text_of(&[(*l).clone()]).contains("+new"))
            .expect("the added line");
        let del = lines
            .iter()
            .find(|l| text_of(&[(*l).clone()]).contains("-old"))
            .expect("the removed line");
        assert!(add.spans.iter().any(|s| s.style.fg == Some(Color::LightGreen)));
        assert!(del.spans.iter().any(|s| s.style.fg == Some(Color::LightRed)));
    }

    #[test]
    fn an_unknown_language_is_left_alone_rather_than_guessed_at() {
        let lines = render("```whatever\nfn not_rust_like\n```", 40);
        let text = text_of(&lines);
        assert!(text.contains("fn not_rust_like"), "{text}");
        assert!(
            !styles_of(&lines).iter().any(|s| s.fg == Some(Color::LightMagenta)),
            "no keyword colouring without a known language"
        );
    }

    #[test]
    fn headings_lists_quotes_and_rules_are_structured() {
        let source = "# Title\n\n## Section\n\n- one\n- two\n\n1. first\n2. second\n\n> quoted\n\n---";
        let lines = render(source, 40);
        let text = text_of(&lines);
        assert!(text.contains("▌ Title"), "{text}");
        assert!(text.contains("▎ Section"), "{text}");
        assert!(text.contains("• one") && text.contains("• two"), "{text}");
        assert!(text.contains("1. first") && text.contains("2. second"), "{text}");
        assert!(text.contains("│ quoted"), "{text}");
        assert!(lines.iter().any(|l| text_of(&[l.clone()]).starts_with('─')), "a rule: {text}");
        assert!(!text.contains("# "), "no hash scaffolding: {text}");
    }

    #[test]
    fn a_wrapped_list_item_hangs_under_its_text() {
        let source = "- a list item long enough that it has to wrap somewhere in here";
        let lines = render(source, 30);
        assert!(lines.len() > 1);
        let continuation = text_of(&[lines[1].clone()]);
        assert!(
            continuation.starts_with("  "),
            "the continuation is indented under the text: {continuation:?}"
        );
        assert!(continuation.trim_start().len() > 0);
    }

    #[test]
    fn indented_code_is_a_block_too() {
        let source = "text:\n\n    line one\n    line two\n\nafter";
        let lines = render(source, 40);
        let text = text_of(&lines);
        assert!(text.contains("│ line one") && text.contains("│ line two"), "{text}");
        assert!(text.contains("after"), "{text}");
    }

    #[test]
    fn a_table_row_is_not_re_flowed() {
        let source = "| a | b |\n|---|---|\n| 1 | 2 |";
        let lines = render(source, 40);
        let text = text_of(&lines);
        assert!(text.contains("| a | b |"), "{text}");
        assert!(text.contains("| 1 | 2 |"), "{text}");
    }

    #[test]
    fn cjk_text_wraps_by_display_width() {
        let source = "中文测试内容中文测试内容中文测试内容";
        let lines = render(source, 20);
        for l in &lines {
            assert!(l.width() <= 20, "line too wide: {:?}", text_of(&[l.clone()]));
        }
        assert_eq!(lines.iter().map(|l| l.width()).sum::<usize>(), 36);
    }

    #[test]
    fn nothing_ever_exceeds_the_width() {
        let nasty = [
            "a".repeat(200),
            "**".to_string() + &"b ".repeat(100),
            "```\n".to_string() + &"c".repeat(300) + "\n```",
            "| x | ".repeat(50),
            "> ".to_string() + &"quote ".repeat(60),
            "- ".to_string() + &"item ".repeat(60),
        ];
        for source in nasty {
            for width in [8usize, 20, 41] {
                let lines = render(&source, width);
                for l in &lines {
                    assert!(
                        l.width() <= width,
                        "width {width}: {:?} is {} wide",
                        text_of(&[l.clone()]),
                        l.width()
                    );
                }
            }
        }
    }

    #[test]
    fn an_empty_message_produces_nothing_rather_than_panicking() {
        assert!(render("", 40).iter().all(|l| l.width() == 0));
        assert!(render("\n\n", 40).len() <= 3);
    }

    #[test]
    fn a_lone_marker_is_left_visible_while_it_streams() {
        // A half-typed `**` must not swallow the text that follows it.
        let lines = render("**still typing", 40);
        assert!(text_of(&lines).contains("still typing"), "{}", text_of(&lines));
    }
}
