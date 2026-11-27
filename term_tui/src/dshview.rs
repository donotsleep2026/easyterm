//! Drawing the DSH page: the daemon's rows as a terminal transcript.
//!
//! The rows come from `term_dsh_model::chat` — the same ones the GUI renders —
//! so this module only decides how a terminal *shows* them: one row per line
//! group, dim prefixes for the things a person reads at a glance (tool calls,
//! contexts, retries), and a hard wrap by display width.
//!
//! Everything here is either a pure function of the row model (transcript
//! lines, wrapped text) or a `Frame` painter, so the whole page can be asserted
//! on a `TestBackend` buffer without a terminal.

use term_dsh_model::api::session::SessionSummary;
use term_dsh_model::chat::{
    log_message_bytes, log_message_seq, log_message_turn, now_millis, ChatMessage, ChatLog,
    QueuedItem,
};
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

use crate::dsh::{short_id, DshFeed, Link, Pending, Questions};
use crate::markdown;

/// The page's own state: what the user typed and where they are looking.
#[derive(Debug, Default)]
pub struct DshView {
    /// The message being composed.
    pub input: String,
    /// Where the next character goes, in characters (not bytes): a message in
    /// Chinese must not put the cursor in the middle of one.
    pub cursor: usize,
    /// Lines scrolled up from the newest (`0` = pinned to the live edge).
    pub scroll: usize,
    /// The session list is open.
    pub sessions: bool,
    /// Cursor in the session list, as an index into the **filtered** rows.
    pub session_cursor: usize,
    /// The session list's `/` filter (the host lists hundreds of sessions).
    pub session_filter: String,
    /// Whether the filter has the keyboard: then plain letters are search text.
    pub session_filtering: bool,
    /// The rendered transcript, so a streaming turn redraws at frame rate
    /// without re-parsing five hundred rows of markdown every frame.
    transcript: TranscriptCache,
    /// When the page's own animation last advanced.
    animation: Option<std::time::Instant>,
    /// The width the transcript was last rendered at, so a page that arrives
    /// between frames can measure what it added.
    last_width: usize,
    /// How far the transcript can scroll, as of the last frame.
    max_scroll: usize,
}

/// How often the page's own animation advances (the spinner and the clock).
///
/// The streamed content forces a redraw when it changes; the spinner only has to
/// move often enough to look alive, and ten times a second it does.
const ANIMATION_TICK: std::time::Duration = std::time::Duration::from_millis(100);

impl DshView {
    /// Whether the animation should advance now.
    ///
    /// Called every frame while a turn runs, and true at most every
    /// [`ANIMATION_TICK`] — a spinner does not need sixty frames a second.
    pub fn animation_due(&mut self, now: std::time::Instant) -> bool {
        match self.animation {
            Some(last) if now.saturating_duration_since(last) < ANIMATION_TICK => false,
            _ => {
                self.animation = Some(now);
                true
            }
        }
    }

    /// The transcript for the current rows and width, rendered once per change.
    pub fn transcript(&mut self, feed: &DshFeed, width: usize) -> &[Line<'static>] {
        self.last_width = width.max(8);
        self.transcript.get(feed, width)
    }

    /// Refresh the transcript at the width it was last drawn at.
    ///
    /// Used when older history is prepended between frames: the cache has to
    /// re-render before the page can measure what it added.
    pub fn refresh_transcript(&mut self, feed: &DshFeed) {
        let width = if self.last_width == 0 { 80 } else { self.last_width };
        self.transcript.get(feed, width);
    }

    /// Whether the view is scrolled to the oldest line it holds (which is what
    /// asks the daemon for the page before it).
    pub fn at_top(&self) -> bool {
        self.max_scroll > 0 && self.scroll >= self.max_scroll
    }

    /// How many rendered lines the first `rows` rows occupy.
    ///
    /// Prepending older rows pushes the visible window down by exactly these
    /// lines, so the caller adds them to the scroll offset to leave the reader
    /// on the line they were reading.
    pub fn lines_in_first(&self, rows: usize) -> usize {
        if rows == 0 {
            return 0;
        }
        self.transcript
            .spans
            .get(rows - 1)
            .map(|range| range.end)
            .unwrap_or(0)
    }

    /// The session-list rows matching the filter, as indices into the host's
    /// list, in the order they are drawn.
    pub fn filtered_sessions<'a>(&self, sessions: &'a [SessionSummary]) -> Vec<&'a SessionSummary> {
        let needle = self.session_filter.trim().to_lowercase();
        sessions
            .iter()
            .filter(|s| {
                if needle.is_empty() {
                    return true;
                }
                s.session_id.to_lowercase().contains(&needle)
                    || s.cwd
                        .as_deref()
                        .map(|c| c.to_lowercase().contains(&needle))
                        .unwrap_or(false)
            })
            .collect()
    }
}

/// The rendered transcript, kept row by row.
///
/// A streaming answer rewrites one row and redraws every frame, and a full
/// render of a long conversation is a whole frame's budget (measured: ~16 ms for
/// 500 rows, ~2000 lines). Each row's lines are therefore cached under a key of
/// everything they depend on, and only the rows whose key changed are rendered
/// again — the cost of a streaming delta becomes one row, not the conversation.
#[derive(Debug, Default)]
struct TranscriptCache {
    /// The width everything was rendered for; a different one starts over.
    width: usize,
    /// Row keys, in row order.
    keys: Vec<RowKey>,
    /// Each row's lines, as a range into `lines`.
    spans: Vec<std::ops::Range<usize>>,
    /// The flattened lines the renderer reads.
    lines: Vec<Line<'static>>,
}

impl TranscriptCache {
    /// The rendered lines for `feed` at `width`, rendering only what changed.
    fn get(&mut self, feed: &DshFeed, width: usize) -> &[Line<'static>] {
        let width = width.max(8);
        let rows = feed.rows();
        if self.width != width || self.keys.is_empty() {
            // Every row wraps differently (or nothing is cached yet): start over.
            self.keys.clear();
            self.spans.clear();
            self.lines.clear();
            self.width = width;
        }
        if rows.len() < self.keys.len() {
            // Rows went away (the log was trimmed): their lines are at the end.
            let keep = self.spans[rows.len()].start;
            self.lines.truncate(keep);
            self.spans.truncate(rows.len());
            self.keys.truncate(rows.len());
        }
        // Rows that appeared since last time: append them. Nothing follows them,
        // so no range moves.
        for i in self.keys.len()..rows.len() {
            let (key, rendered) = {
                let ctx = RowContext {
                    prev: if i == 0 { None } else { rows.get(i - 1) },
                    next: rows.get(i + 1),
                    log: &feed.log,
                };
                (row_key(&rows[i], &ctx), render_row(&rows[i], &ctx, width))
            };
            let at = self.lines.len();
            let inserted = rendered.len();
            self.lines.extend(rendered);
            self.spans.push(at..at + inserted);
            self.keys.push(key);
        }
        // Rows whose key changed: rendered again, backwards, so replacing one
        // only moves the ranges after it — which have been dealt with already.
        // A streaming delta touches exactly one row this way.
        for i in (0..self.keys.len()).rev() {
            let (key, rendered) = {
                let ctx = RowContext {
                    prev: if i == 0 { None } else { rows.get(i - 1) },
                    next: rows.get(i + 1),
                    log: &feed.log,
                };
                let key = row_key(&rows[i], &ctx);
                if self.keys[i] == key {
                    continue;
                }
                (key, render_row(&rows[i], &ctx, width))
            };
            let (start, end) = (self.spans[i].start, self.spans[i].end);
            let inserted = rendered.len();
            self.lines.splice(start..end, rendered);
            self.spans[i] = start..start + inserted;
            self.keys[i] = key;
            let delta = inserted as isize - end.saturating_sub(start) as isize;
            for span in self.spans[i + 1..].iter_mut() {
                span.start = (span.start as isize + delta) as usize;
                span.end = (span.end as isize + delta) as usize;
            }
        }
        debug_assert_eq!(self.spans.len(), rows.len());
        debug_assert_eq!(
            self.lines.len(),
            self.spans.last().map(|r| r.end).unwrap_or(0)
        );
        &self.lines
    }
}
impl DshView {
    /// The input line's cursor column, in display cells.
    pub fn input_cursor(&self) -> u16 {
        self.input
            .chars()
            .take(self.cursor)
            .map(width_of)
            .sum::<usize>() as u16
    }

    /// Put a character at the cursor.
    pub fn insert(&mut self, ch: char) {
        let at = self.cursor.min(self.input.chars().count());
        let byte = self
            .input
            .char_indices()
            .nth(at)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len());
        self.input.insert(byte, ch);
        self.cursor = at + 1;
    }

    /// Remove the character before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let at = self.cursor.min(self.input.chars().count());
        if let Some((byte, _)) = self.input.char_indices().nth(at - 1) {
            self.input.remove(byte);
        }
        self.cursor = at - 1;
    }

    /// Remove the character under the cursor.
    pub fn delete(&mut self) {
        let at = self.cursor.min(self.input.chars().count());
        if let Some((byte, _)) = self.input.char_indices().nth(at) {
            self.input.remove(byte);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.input.chars().count();
    }

    /// Clear the line (`Ctrl-U`).
    pub fn clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    /// Delete the word before the cursor (`Ctrl-W`), keeping everything after
    /// it — the caret may be in the middle of the line.
    pub fn delete_word(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let end = self.cursor.min(chars.len());
        let mut start = end;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let mut out: String = chars[..start].iter().collect();
        out.extend(chars[end..].iter());
        self.input = out;
        self.cursor = start;
    }

    /// Take the line out (to send it, or to hand it back on failure).
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.input)
    }
}

/// Display width of one character (1 for anything unprintable/zero-width).
fn width_of(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(1).max(if ch == '\t' { 1 } else { 0 })
}

/// Wrap `text` to `width` display cells, honouring its own line breaks.
///
/// Words are kept whole when they fit; a word longer than the line is split
/// (a path or a URL has no spaces to break at, and dropping it would be worse).
/// Runs of spaces survive *inside* a line, so an indented code block keeps its
/// shape, but a wrapped line never starts with the space it broke at.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_w = 0usize;
        for token in split_tokens(line) {
            let token_w: usize = token.chars().map(width_of).sum();
            if token.chars().all(|c| c == ' ') {
                // Spaces stay between words, but never lead a wrapped line.
                if current_w > 0 && current_w + token_w <= width {
                    current.push_str(&token);
                    current_w += token_w;
                }
                continue;
            }
            if current_w > 0 && current_w + token_w > width {
                out.push(current.trim_end().to_string());
                current.clear();
                current_w = 0;
            }
            if current_w == 0 && token_w > width {
                let mut rest = token;
                while rest.chars().map(width_of).sum::<usize>() > width {
                    let (head, tail) = split_at_width(&rest, width);
                    out.push(head);
                    rest = tail;
                }
                current_w = rest.chars().map(width_of).sum();
                current = rest;
                continue;
            }
            current.push_str(&token);
            current_w += token_w;
        }
        out.push(current.trim_end().to_string());
    }
    out
}

/// Split a line into alternating tokens: runs of spaces, and runs of anything
/// else (so a `  ` indent inside a message survives the wrap).
fn split_tokens(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_space = false;
    for ch in line.chars() {
        let is_space = ch == ' ' || ch == '\t';
        if is_space != in_space && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        in_space = is_space;
        current.push(if ch == '\t' { ' ' } else { ch });
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Split a word into (head that fits, rest).
fn split_at_width(word: &str, width: usize) -> (String, String) {
    let mut head = String::new();
    let mut used = 0usize;
    let mut chars = word.chars();
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

/// One dim, one-line summary of a long body: first line plus how much is left.
fn summarize(text: &str) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let lines = text.lines().count();
    let mut s: String = first.chars().take(160).collect();
    if first.chars().count() > 160 {
        s.push('…');
    }
    if lines > 1 {
        s.push_str(&format!(" (+{} lines)", lines - 1));
    }
    s
}

/// The gutter a row is drawn with: the first line's label and its style.
///
/// The assistant's own text has none (it is the page's content, and markdown
/// wants the width); everything else is annotated the way a person scanning a
/// transcript expects — who spoke, what tool ran, what it answered.
fn row_gutter(msg: &ChatMessage) -> (&'static str, Style) {
    match msg {
        ChatMessage::User { .. } => (
            "you › ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        ChatMessage::Assistant { .. } => ("", Style::default().fg(Color::White)),
        // `⏺ name(...)`, the shape Claude Code uses for tool use.
        ChatMessage::ToolCall { .. } => ("⏺ ", Style::default().fg(Color::Yellow)),
        ChatMessage::ToolResult { ok, .. } => (
            "⎿ ",
            Style::default().fg(if *ok { Color::DarkGray } else { Color::Red }),
        ),
        ChatMessage::Context { .. } | ChatMessage::SystemPrompt { .. } => (
            "· ",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        ),
        ChatMessage::Retry { .. } => ("⟳ ", Style::default().fg(Color::Magenta)),
    }
}

/// The handle a tool call was given: the one argument a reader wants to see.
fn tool_handle(name: &str, args: &str) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(args).ok();
    let pick = |keys: &[&str]| -> Option<String> {
        let value = parsed.as_ref()?;
        for key in keys {
            if let Some(v) = value.get(*key) {
                if let Some(s) = v.as_str() {
                    return Some(s.to_string());
                }
            }
        }
        None
    };
    let detail = pick(&[
        "command",
        "cmd",
        "path",
        "file_path",
        "filePath",
        "pattern",
        "query",
        "url",
        "prompt",
    ])
    .or_else(|| {
        parsed.as_ref().and_then(|v| {
            v.as_object().map(|o| {
                o.iter()
                    .take(2)
                    .map(|(k, val)| {
                        let text = match val {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        format!("{k}={text}")
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        })
    });
    match detail {
        Some(detail) if !detail.trim().is_empty() => {
            format!("{name}({})", summarize(&detail))
        }
        _ => format!("{name}()"),
    }
}

/// Whether a blank line goes between two rows.
///
/// A transcript is read in blocks: a tool call and its result belong together
/// (no gap), while a change of speaker or of kind gets one. Without this every
/// row runs into the next and the eye has nothing to land on.
fn needs_blank_before(prev: Option<&ChatMessage>, msg: &ChatMessage) -> bool {
    let Some(prev) = prev else {
        return false;
    };
    // A result belongs to the call above it.
    if matches!(prev, ChatMessage::ToolCall { .. })
        && matches!(msg, ChatMessage::ToolResult { .. })
    {
        return false;
    }
    // Bookkeeping rows of the same kind are one block.
    if matches!(
        (prev, msg),
        (ChatMessage::Context { .. }, ChatMessage::Context { .. })
            | (ChatMessage::SystemPrompt { .. }, ChatMessage::SystemPrompt { .. })
            | (ChatMessage::Retry { .. }, ChatMessage::Retry { .. })
    ) {
        return false;
    }
    true
}

/// Whether a row's text is *output* rather than prose: its lines are kept as
/// lines (markdown re-flows a paragraph, which is exactly wrong for a compiler
/// error, a test summary or a diff hunk).
fn row_is_verbatim(msg: &ChatMessage) -> bool {
    matches!(msg, ChatMessage::ToolResult { .. })
}

/// The style a row's *body* is drawn in. The marker says what happened (see
/// [`row_gutter`]); the body has to stay readable, which for tool output means
/// the normal foreground rather than the marker's dim grey.
fn row_body_style(msg: &ChatMessage) -> Style {
    match msg {
        ChatMessage::ToolCall { .. } => Style::default().fg(Color::Yellow),
        ChatMessage::Context { .. } | ChatMessage::SystemPrompt { .. } => {
            Style::default().fg(Color::Indexed(245)).add_modifier(Modifier::DIM)
        }
        ChatMessage::Retry { .. } => Style::default().fg(Color::Magenta),
        _ => Style::default().fg(Color::White),
    }
}

/// The body text of a row, as the transcript shows it.
fn row_body(msg: &ChatMessage) -> String {
    match msg {
        ChatMessage::User { text, images, .. } => {
            let mut s = text.clone();
            if !images.is_empty() {
                s.push_str(&format!(" [{} image(s)]", images.len()));
            }
            s
        }
        ChatMessage::Assistant { text, .. } => text.clone(),
        ChatMessage::ToolCall { name, args, .. } => {
            // The name is bold (markdown does the styling), the handle is the
            // one argument a reader looks for.
            format!("**{}**", tool_handle(name, args))
        }
        ChatMessage::ToolResult { output, ok, .. } => {
            let verdict = if *ok { "ok" } else { "failed" };
            if output.trim().is_empty() {
                return format!("{verdict} (no output)");
            }
            // A result is worth a few lines, not one: a compiler error, a test
            // summary or a diff hunk is unreadable collapsed to its first line.
            // Long ones stay collapsed (`summarize` adds the count).
            let mut parts: Vec<String> = vec![format!("{verdict}:")];
            let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
            for line in lines.iter().take(3) {
                parts.push(summarize(line));
            }
            if lines.len() > 3 {
                parts.push(format!("… (+{} lines)", lines.len() - 3));
            }
            // Line by line: `transcript` renders this row verbatim (see
            // `row_is_verbatim`), so a compiler error or a diff hunk keeps its
            // shape instead of being re-flowed into one paragraph.
            parts.join("\n")
        }
        ChatMessage::Context {
            text,
            label,
            role,
            form,
            ..
        } => {
            let what = label
                .clone()
                .or_else(|| form.clone())
                .unwrap_or_else(|| role.clone());
            format!(
                "context {what}, {} chars) {}",
                text.chars().count(),
                summarize(text)
            )
        }
        ChatMessage::SystemPrompt { text, .. } => format!(
            "system prompt, {} chars) {}",
            text.chars().count(),
            summarize(text)
        ),
        ChatMessage::Retry {
            retry,
            delay_ms,
            message,
            mode,
            started,
            ..
        } => {
            let when = if *started {
                "running".to_string()
            } else {
                format!("in {delay_ms}ms")
            };
            format!(
                "retry #{} ({mode}) {when}: {}",
                retry + 1,
                summarize(message)
            )
        }
    }
}

/// A dim line for a turn that did not simply finish.
fn turn_notice(log: &ChatLog, turn: i64) -> Option<String> {
    let reason = log.turn_end.get(&turn)?;
    let kind = reason.kind.as_str();
    if kind == "completed" || kind.is_empty() {
        return None;
    }
    let detail = if reason.message.trim().is_empty() {
        String::new()
    } else {
        format!(": {}", summarize(&reason.message))
    };
    let code = reason
        .code
        .as_deref()
        .map(|c| format!(" [{c}]"))
        .unwrap_or_default();
    Some(format!("(turn ended: {kind}{code}{detail})"))
}

/// The files a turn produced (mutation tool calls), if any.
fn produced_line(log: &ChatLog, turn: i64) -> Option<String> {
    let files = log.produced.get(&turn)?;
    if files.is_empty() {
        return None;
    }
    let paths: Vec<&str> = files.iter().map(|(_, p)| p.as_str()).take(6).collect();
    let more = files.len().saturating_sub(paths.len());
    let mut s = format!("(wrote {})", paths.join(", "));
    if more > 0 {
        s.push_str(&format!(" +{more} more"));
    }
    s.push(')');
    Some(s)
}

/// The whole transcript as rendered lines, newest last.
///
/// The page uses the cached [`TranscriptCache`] instead; this renders everything
/// at once (tests, and the cache's own rebuild path).
pub fn transcript(feed: &DshFeed, width: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    let rows = feed.rows();
    let mut out: Vec<Line<'static>> = Vec::new();
    for (i, msg) in rows.iter().enumerate() {
        let ctx = RowContext {
            prev: if i == 0 { None } else { rows.get(i - 1) },
            next: rows.get(i + 1),
            log: &feed.log,
        };
        out.extend(render_row(msg, &ctx, width));
    }
    out
}

/// Everything outside a row that its rendered lines depend on.
struct RowContext<'a> {
    /// The row above: decides the blank line between blocks.
    prev: Option<&'a ChatMessage>,
    /// The row below: decides whether this row ends its turn (its notices).
    next: Option<&'a ChatMessage>,
    /// The conversation's per-turn tables (turn-end reasons, produced files).
    log: &'a ChatLog,
}

/// What a cached row's lines depend on.
///
/// Everything here is cheap to compute, which is the point: comparing keys is
/// how the cache decides which rows to render again after a streaming delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowKey {
    seq: i64,
    bytes: usize,
    turn: Option<i64>,
    /// A live draft draws the trailing block, and may have no prose at all.
    done: bool,
    /// Whether the row ends its turn (the notices hang off that).
    last_of_turn: bool,
    /// Whether its turn has an end reason to report.
    turn_end: bool,
    /// How many files its turn wrote.
    produced: usize,
    /// Whether a blank line goes above it.
    blank_before: bool,
}

fn row_key(msg: &ChatMessage, ctx: &RowContext<'_>) -> RowKey {
    let turn = log_message_turn(msg);
    let last_of_turn = match (turn, ctx.next) {
        (Some(t), Some(next)) => log_message_turn(next) != Some(t),
        (Some(_), None) => true,
        _ => false,
    };
    RowKey {
        seq: log_message_seq(msg),
        bytes: log_message_bytes(msg),
        turn,
        done: !matches!(msg, ChatMessage::Assistant { done: false, .. }),
        last_of_turn,
        turn_end: turn
            .map(|t| ctx.log.turn_end.contains_key(&t))
            .unwrap_or(false),
        produced: turn
            .and_then(|t| ctx.log.produced.get(&t))
            .map(|files| files.len())
            .unwrap_or(0),
        blank_before: needs_blank_before(ctx.prev, msg),
    }
}

/// Render one row: its block separator, its thinking line, its markdown body,
/// its live marker, and the notices that belong after it.
fn render_row(msg: &ChatMessage, ctx: &RowContext<'_>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut out: Vec<Line<'static>> = Vec::new();
    // Air between blocks, none inside one.
    if needs_blank_before(ctx.prev, msg) {
        out.push(Line::from(""));
    }
    let (gutter, style) = row_gutter(msg);
    let body_style = row_body_style(msg);
    let gutter_width = markdown::text_width(gutter);
    let body = row_body(msg);
    let body_width = width.saturating_sub(gutter_width).max(8);
    // The model's thinking gets one compact dim line of its own: it is not part
    // of the answer, and putting it through markdown would let it re-flow into
    // the answer's paragraph.
    if let ChatMessage::Assistant { thinking, .. } = msg {
        if !thinking.trim().is_empty() {
            out.push(Line::from(vec![
                Span::styled("✻ ", Style::default().fg(Color::Magenta)),
                Span::styled(
                    summarize(thinking),
                    Style::default()
                        .fg(Color::Indexed(243))
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
    }
    // Markdown, with the row's own colour as the base so a tool row stays dim and
    // an answer reads as an answer. A row whose text is *output* (a tool result)
    // is rendered line by line instead: markdown re-flows a paragraph, which
    // turns a compiler error or a diff hunk into prose.
    let source: Vec<&str> = if row_is_verbatim(msg) {
        body.split('\n').collect()
    } else {
        vec![body.as_str()]
    };
    let mut rendered: Vec<Line<'static>> = Vec::new();
    // A row with no prose (a thinking-only draft) draws nothing but the thinking
    // line above it, and no line gets a bare gutter: the leading and trailing
    // blanks of a body are the block separator's business.
    if !body.trim().is_empty() {
        for chunk in source {
            for line in markdown::render(chunk, body_width) {
                let mut spans = line.spans;
                for span in spans.iter_mut() {
                    if span.style.fg.is_none() {
                        span.style = span.style.patch(body_style);
                    }
                }
                rendered.push(Line::from(spans));
            }
        }
        while rendered.first().map(|l| l.width() == 0).unwrap_or(false) {
            rendered.remove(0);
        }
        while rendered.last().map(|l| l.width() == 0).unwrap_or(false) {
            rendered.pop();
        }
    }
    let mut first = true;
    for line in rendered {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if first {
            if !gutter.is_empty() {
                spans.push(Span::styled(gutter, style));
            }
            first = false;
        } else if gutter_width > 0 {
            spans.push(Span::raw(" ".repeat(gutter_width)));
        }
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
    // A live draft ends with a block, so a running answer looks running.
    if let ChatMessage::Assistant { done: false, .. } = msg {
        if let Some(last) = out.last_mut() {
            last.spans.push(Span::styled(
                "\u{258c}",
                Style::default().fg(Color::LightGreen).add_modifier(Modifier::SLOW_BLINK),
            ));
        }
    }
    // Per-turn notices go after the turn's last row, where a reader expects them:
    // the turn-end reason, then what it wrote.
    let turn = log_message_turn(msg);
    let is_last_of_turn = match (turn, ctx.next) {
        (Some(t), Some(next)) => log_message_turn(next) != Some(t),
        (Some(_), None) => true,
        _ => false,
    };
    if let Some(t) = turn.filter(|_| is_last_of_turn) {
        if let Some(notice) = turn_notice(ctx.log, t) {
            out.push(Line::from(Span::styled(
                format!("  {notice}"),
                Style::default().fg(Color::Red),
            )));
        }
        if let Some(files) = produced_line(ctx.log, t) {
            out.push(Line::from(Span::styled(
                format!("  {files}"),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    out
}
/// The queue, as one dim line per pending item (most a screen will show).
pub fn queue_lines(queue: &[QueuedItem]) -> Vec<Line<'static>> {
    queue
        .iter()
        .take(3)
        .map(|item| {
            Line::from(vec![
                Span::styled(
                    format!("  queue[{}] ", item.placement),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(item.preview.clone(), Style::default().fg(Color::Gray)),
            ])
        })
        .collect()
}

/// The frames of the "working" spinner. A turn that is running has to *look*
/// alive; a still screen reads as a hung one.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The spinner frame for now (a new one every 100 ms).
pub fn spinner(now_ms: u64) -> &'static str {
    SPINNER[((now_ms / 100) % SPINNER.len() as u64) as usize]
}

/// How long the newest turn has been running, as `12.4s` / `1m04s`.
///
/// `None` when no turn is open (or its clock is unknown), so the header says
/// nothing rather than counting something it cannot see.
fn running_for(feed: &DshFeed) -> Option<String> {
    let (turn, times) = feed
        .log
        .turn_times
        .iter()
        .filter(|(_, t)| t.start_ms.is_some() && t.end_ms.is_none())
        .max_by_key(|(turn, _)| **turn)?;
    let _ = turn;
    let start = times.start_ms?;
    let elapsed = (now_millis() as i64 - start).max(0) as f64 / 1000.0;
    Some(if elapsed < 60.0 {
        format!("{elapsed:.1}s")
    } else {
        format!("{}m{:02}s", (elapsed / 60.0) as i64, (elapsed % 60.0) as i64)
    })
}

/// The header: where the connection stands and what is on screen.
fn header_line(feed: &DshFeed) -> Line<'static> {
    let (state_style, state) = match &feed.link {
        Link::Connected => (Style::default().fg(Color::Green), feed.link.label()),
        Link::Connecting => (Style::default().fg(Color::Yellow), feed.link.label()),
        Link::NeedsLicense => (Style::default().fg(Color::Red), feed.link.label()),
        Link::Failed(_) => (Style::default().fg(Color::Red), feed.link.label()),
        Link::Idle => (Style::default().fg(Color::DarkGray), feed.link.label()),
    };
    let mut spans = vec![
        Span::styled("dsh ", Style::default().fg(Color::DarkGray)),
        Span::styled(state, state_style),
    ];
    match &feed.session_id {
        Some(id) => {
            spans.push(Span::styled(
                "  ".to_string(),
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(
                feed.title(),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!("  ({})", short_id(id)),
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(
                format!("  {} rows", feed.rows().len()),
                Style::default().fg(Color::DarkGray),
            ));
        }
        // No session yet: say so, with the way out, instead of a bare
        // "0 rows" glued to the state word.
        None => spans.push(Span::styled(
            "  no session  [Tab] to pick one",
            Style::default().fg(Color::LightYellow),
        )),
    }
    if feed.running {
        // Animated: the frame comes from the clock, so a redraw shows a moving
        // spinner and an elapsed time that counts.
        let now = now_millis();
        spans.push(Span::styled(
            format!("  {} working", spinner(now)),
            Style::default().fg(Color::LightGreen),
        ));
        if let Some(elapsed) = running_for(feed) {
            spans.push(Span::styled(
                format!(" {elapsed}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    if !feed.queue.is_empty() {
        spans.push(Span::styled(
            format!("  queue {}", feed.queue.len()),
            Style::default().fg(Color::Yellow),
        ));
    }
    Line::from(spans)
}

/// Paint the page: header, transcript, queue, input, footer (or an overlay).
///
/// Returns where the terminal cursor belongs (the input line), for the caller
/// to place after the frame.
pub fn draw_dsh(f: &mut Frame<'_>, feed: &DshFeed, view: &mut DshView) -> Option<(u16, u16)> {
    let area = f.area();
    let queue_extra = if feed.queue.is_empty() { 0 } else { feed.queue.len().min(3) as u16 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),            // header
            Constraint::Min(1),               // transcript
            Constraint::Length(queue_extra),  // queue
            Constraint::Length(1),            // input
            Constraint::Length(1),            // footer
        ])
        .split(area);

    f.render_widget(Paragraph::new(header_line(feed)), chunks[0]);

    // The transcript: the tail, less whatever the user scrolled up by. The
    // cache is borrowed only long enough to copy the visible window, so the
    // scroll can be clamped afterwards.
    let body = chunks[1];
    let height = body.height as usize;
    let wanted_scroll = view.scroll;
    let (visible, scroll, max_scroll) = {
        let lines = view.transcript(feed, body.width as usize);
        let max_scroll = lines.len().saturating_sub(height);
        let scroll = wanted_scroll.min(max_scroll);
        let end = lines.len() - scroll;
        let start = end.saturating_sub(height);
        (lines[start..end].to_vec(), scroll, max_scroll)
    };
    view.scroll = scroll;
    view.max_scroll = max_scroll;
    let mut transcript_area = body;
    if view.scroll > 0 {
        // Scrolled back: say so, where in the log the view is, and whether there
        // is more history to fetch — the host pages it, the frontend does not
        // hold the whole conversation.
        let at_top = scroll >= max_scroll;
        let tail = if !at_top {
            "[Shift-End] back to live".to_string()
        } else if feed.loading_older {
            "loading older…".to_string()
        } else if feed.has_more {
            "[Shift-Home] again for older turns".to_string()
        } else {
            "the conversation starts here".to_string()
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  scrolled up {} line(s)  {tail}", view.scroll),
                Style::default().fg(Color::LightYellow),
            ))),
            Rect::new(body.x, body.y, body.width, 1),
        );
        transcript_area = Rect::new(body.x, body.y + 1, body.width, body.height.saturating_sub(1));
    }
    f.render_widget(
        Paragraph::new(visible).wrap(Wrap { trim: false }),
        transcript_area,
    );

    if queue_extra > 0 {
        f.render_widget(Paragraph::new(queue_lines(&feed.queue)), chunks[2]);
    }

    // The input line: `> ` plus what has been typed.
    let input = Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Green)),
        Span::styled(view.input.clone(), Style::default().fg(Color::White)),
    ]);
    f.render_widget(Paragraph::new(input), chunks[3]);

    let footer = match (&feed.pending, &feed.status) {
        (Some(Pending::Approval { tool, .. }), _) => Line::from(Span::styled(
            format!(" allow {tool}?   [y] once   [n] reject "),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        (Some(Pending::Question(_)), _) => Line::from(Span::styled(
            " the agent asked a question   [1-9] pick   [t] type   [Enter] submit   [Esc] cancel ",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        (None, Some(status)) => Line::from(Span::styled(
            format!(" {status}"),
            Style::default().fg(Color::Yellow),
        )),
        (None, None) => Line::from(Span::styled(
            " [Enter] send   [Ctrl-S] steer   [Ctrl-C] stop   [Tab] sessions   [Shift-PgUp] scroll   [Esc] back ",
            Style::default().fg(Color::DarkGray),
        )),
    };
    f.render_widget(Paragraph::new(footer), chunks[4]);

    // Overlays last: they own the screen while an answer is needed.
    if let Some(Pending::Question(q)) = &feed.pending {
        draw_question(q, f.buffer_mut(), area);
        return None;
    }
    if view.sessions {
        let cursor = draw_sessions(feed, view, f.buffer_mut(), area);
        view.session_cursor = cursor;
        return None;
    }

    // The cursor sits where the next character will land, when nothing else
    // owns the keyboard.
    let cursor = chunks[3];
    Some((cursor.x + 2 + view.input_cursor(), cursor.y))
}

/// The session list overlay; returns the cursor it clamped to.
///
/// The host on a working machine lists hundreds of sessions (one install here
/// had 319), so the list filters: `/` takes the keyboard, and what is typed is
/// matched against the session's cwd and its id.
fn draw_sessions(feed: &DshFeed, view: &DshView, buf: &mut Buffer, area: Rect) -> usize {
    let h = (area.height.saturating_sub(5)).min(18).max(3);
    let w = (area.width * 4) / 5;
    let box_area = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    let shown = view.filtered_sessions(&feed.sessions);
    let cursor = view.session_cursor.min(shown.len().saturating_sub(1));
    let mut lines: Vec<Line<'static>> = Vec::new();
    if feed.sessions.is_empty() {
        lines.push(Line::from(Span::styled(
            "The host listed no sessions.",
            Style::default().fg(Color::Yellow),
        )));
    } else if shown.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("No session matches \"{}\".", view.session_filter),
            Style::default().fg(Color::Yellow),
        )));
    }
    for (i, s) in shown.iter().enumerate() {
        let title = feed
            .titles
            .get(&s.session_id)
            .cloned()
            .unwrap_or_else(|| short_id(&s.session_id));
        let mark = if i == cursor { "> " } else { "  " };
        let mut spans = vec![
            Span::styled(mark.to_string(), Style::default().fg(Color::LightGreen)),
            Span::styled(
                title,
                Style::default().add_modifier(if i == cursor {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
        ];
        if s.running {
            spans.push(Span::styled(
                "  running",
                Style::default().fg(Color::LightGreen),
            ));
        }
        if s.blank {
            spans.push(Span::styled("  blank", Style::default().fg(Color::DarkGray)));
        }
        // The row a person recognises: where the session ran.
        if let Some(cwd) = &s.cwd {
            spans.push(Span::styled(
                format!("  {cwd}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        let id = short_id(&s.session_id);
        if !id.is_empty() {
            spans.push(Span::styled(
                format!("  {id}"),
                Style::default().fg(Color::Indexed(240)),
            ));
        }
        lines.push(Line::from(spans));
    }
    let title = if view.session_filter.trim().is_empty() {
        format!(" dsh sessions — {} (Enter opens, / filters) ", feed.sessions.len())
    } else {
        format!(" dsh sessions — {} of {} ", shown.len(), feed.sessions.len())
    };
    let mut inner = box_area;
    if view.session_filtering || !view.session_filter.is_empty() {
        // A filter line under the list, so what is being searched for is visible.
        let split = ratatui::layout::Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(box_area);
        inner = split[0];
        f_render_filter(view, split[1], buf);
    }
    Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .style(Style::default().fg(Color::White))
        .render(inner, buf);
    cursor
}

/// The filter line under the session list.
fn f_render_filter(view: &DshView, area: Rect, buf: &mut Buffer) {
    let line = Line::from(vec![
        Span::styled(" /", Style::default().fg(Color::Yellow)),
        Span::styled(view.session_filter.clone(), Style::default().fg(Color::White)),
        Span::styled(
            if view.session_filtering {
                "  [Esc] clear  [Enter] open"
            } else {
                "  [/] filter"
            },
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    Paragraph::new(line).render(area, buf);
}

/// The question overlay: the agent is blocked until this is answered.
fn draw_question(q: &Questions, buf: &mut Buffer, area: Rect) {
    let h = (area.height.saturating_sub(4)).min(16).max(4);
    let w = (area.width * 4) / 5;
    let box_area = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    let mut lines: Vec<Line<'static>> = Vec::new();
    let total = q.items.len();
    if let Some(item) = q.current() {
        if total > 1 {
            lines.push(Line::from(Span::styled(
                format!("question {} of {total}", q.index + 1),
                Style::default().fg(Color::DarkGray),
            )));
        }
        if !item.header.trim().is_empty() {
            lines.push(Line::from(Span::styled(
                item.header.clone(),
                Style::default().fg(Color::LightYellow),
            )));
        }
        lines.push(Line::from(Span::styled(
            item.text.clone(),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        for (i, label) in item.options.iter().enumerate() {
            let picked = q
                .picked
                .get(q.index)
                .map(|p| p.iter().any(|l| l == label))
                .unwrap_or(false);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  [{}] ", i + 1),
                    Style::default().fg(if picked { Color::LightGreen } else { Color::DarkGray }),
                ),
                Span::styled(
                    label.clone(),
                    Style::default().fg(if picked { Color::LightGreen } else { Color::White }),
                ),
            ]));
        }
        if item.multi {
            lines.push(Line::from(Span::styled(
                "  (several may be picked)",
                Style::default().fg(Color::DarkGray),
            )));
        }
        if q.typing {
            lines.push(Line::from(vec![
                Span::styled("  answer: ", Style::default().fg(Color::Green)),
                Span::styled(q.input.clone(), Style::default().fg(Color::White)),
            ]));
        } else if let Some(custom) = q.custom.get(q.index).filter(|c| !c.trim().is_empty()) {
            lines.push(Line::from(Span::styled(
                format!("  answer: {custom}"),
                Style::default().fg(Color::LightGreen),
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "the host asked a question with no content",
            Style::default().fg(Color::Yellow),
        )));
    }
    Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" dsh question — [Enter] answer  [Esc] cancel ")
                .style(Style::default().fg(Color::Yellow)),
        )
        .wrap(Wrap { trim: false })
        .alignment(Alignment::Left)
        .render(box_area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use crate::dsh::DshFeed;
    use term_dsh_model::chat::{ChatMessage, ImageRef, TurnEndReason, TurnTimes};

    fn user(text: &str, seq: i64, turn: i64) -> ChatMessage {
        ChatMessage::User {
            text: text.to_string(),
            images: Vec::new(),
            seq,
            turn: Some(turn),
            rpc_id: None,
            time: None,
        }
    }

    fn assistant(text: &str, seq: i64, turn: i64, done: bool) -> ChatMessage {
        ChatMessage::Assistant {
            text: text.to_string(),
            thinking: String::new(),
            images: Vec::new(),
            seq,
            turn: Some(turn),
            step: Some(1),
            done,
        }
    }

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn summary_row(id: &str, cwd: &str) -> SessionSummary {
        SessionSummary {
            session_id: id.to_string(),
            updated_at: 0,
            running: false,
            blank: false,
            agent_available: false,
            parent_session_id: None,
            projections: None,
            origin: None,
            cwd: Some(cwd.to_string()),
            format_status: None,
        }
    }

    fn feed_with(rows: Vec<ChatMessage>) -> DshFeed {
        let mut feed = DshFeed::new();
        feed.log.messages = rows;
        feed
    }

    #[test]
    fn wrapping_breaks_on_words_and_never_exceeds_the_width() {
        let wrapped = wrap_text("the quick brown fox jumps over the lazy dog", 12);
        assert!(wrapped.iter().all(|l| l.chars().count() <= 12), "{wrapped:?}");
        assert_eq!(wrapped[0], "the quick");
        assert!(wrapped.len() > 2);
    }

    #[test]
    fn a_word_with_no_spaces_is_split_not_dropped() {
        let long = "a".repeat(30);
        let wrapped = wrap_text(&long, 10);
        assert_eq!(wrapped.len(), 3);
        assert!(wrapped.iter().all(|l| l.len() == 10));
        let path = "/very/long/path/that/never/ends/anywhere/file.rs";
        let wrapped = wrap_text(path, 12);
        assert_eq!(wrapped.concat(), path, "no character is lost");
        assert!(wrapped.iter().all(|l| l.chars().count() <= 12));
    }

    #[test]
    fn wrapping_counts_a_cjk_character_as_two_cells() {
        // Six full-width characters do not fit in ten cells; four do.
        let wrapped = wrap_text("中文测试内容", 10);
        assert_eq!(wrapped.len(), 2, "{wrapped:?}");
        assert_eq!(wrapped[0], "中文测试内", "five full-width characters fill ten cells");
        assert_eq!(wrapped[1], "容");
        // …and at 9 cells the fifth one no longer fits.
        let wrapped = wrap_text("中文测试内容", 9);
        assert_eq!(wrapped[0], "中文测试");
    }

    #[test]
    fn wrapping_keeps_the_message_s_own_line_breaks() {
        let wrapped = wrap_text("first\n\nsecond", 20);
        assert_eq!(wrapped, vec!["first".to_string(), String::new(), "second".to_string()]);
    }

    #[test]
    fn every_kind_of_row_renders_something_readable() {
        let feed = feed_with(vec![
            user("hello there", 1, 1),
            ChatMessage::Assistant {
                text: "answer".into(),
                thinking: "weighing options\nmore thinking".into(),
                images: Vec::new(),
                seq: 2,
                turn: Some(1),
                step: Some(1),
                done: true,
            },
            ChatMessage::ToolCall {
                name: "shell".into(),
                args: "{\"cmd\":\"ls\"}".into(),
                call_id: "c1".into(),
                seq: 3,
                turn: Some(1),
            },
            ChatMessage::ToolResult {
                call_id: "c1".into(),
                output: "a.rs\nb.rs".into(),
                ok: true,
                seq: 4,
                turn: Some(1),
            },
            ChatMessage::Context {
                text: "system-ish".into(),
                source: serde_json::json!({}),
                label: Some("agent".into()),
                role: "system".into(),
                form: None,
                seq: 5,
                turn: Some(2),
            },
            ChatMessage::SystemPrompt {
                text: "you are a helper".into(),
                seq: 6,
                turn: Some(2),
            },
            ChatMessage::Retry {
                retry_id: "r1".into(),
                retry: 1,
                max_retries: Some(3),
                mode: "normal".into(),
                delay_ms: 250,
                message: "rate limited".into(),
                code: None,
                started: false,
                seq: 7,
                turn: Some(2),
                step: Some(1),
            },
        ]);
        let text = text_of(&transcript(&feed, 80));
        for needle in [
            // The user's own row is labelled…
            "you › hello there",
            // …the thinking is its own compact line, the answer follows as
            // markdown…
            "✻ weighing options (+1 lines)",
            "answer",
            // …a tool call shows its name and its handle, and its output keeps
            // its own lines…
            "⏺ shell(ls)",
            "⎿ ok:",
            "a.rs",
            "b.rs",
            // …and the bookkeeping rows stay out of the way.
            "context agent",
            "system prompt",
            "⟳ retry #2 (normal) in 250ms: rate limited",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from:\n{text}");
        }
    }

    #[test]
    fn an_image_row_says_how_many_images_it_carries() {
        let feed = feed_with(vec![ChatMessage::User {
            text: "look".into(),
            images: vec![ImageRef {
                attachment_id: "att-1".into(),
                media_type: "image/png".into(),
                width: 64,
                height: 32,
            }],
            seq: 1,
            turn: Some(1),
            rpc_id: None,
            time: None,
        }]);
        let text = text_of(&transcript(&feed, 80));
        assert!(text.contains("[1 image(s)]"), "{text}");
    }

    #[test]
    fn an_answer_renders_as_markdown() {
        let answer = "Here:\n\n```rust\nlet x = 1; // note\n```\n\nand **bold** text";
        let feed = feed_with(vec![assistant(answer, 1, 1, true)]);
        let lines = transcript(&feed, 60);
        let text = text_of(&lines);
        assert!(text.contains("┌─ rust"), "the code block is drawn: {text}");
        assert!(text.contains("│ let x = 1; // note"), "{text}");
        assert!(text.contains("└─"), "{text}");
        assert!(!text.contains("```"), "the fence is gone: {text}");
        assert!(!text.contains("**"), "the emphasis markers are gone: {text}");
        assert!(text.contains("bold"));
        // The block's code is highlighted, so the block does not read as prose.
        assert!(lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.fg == Some(Color::LightMagenta)));
    }

    #[test]
    fn a_tool_call_shows_the_argument_a_reader_needs() {
        assert_eq!(
            tool_handle("shell", r#"{"command":"cargo test --offline"}"#),
            "shell(cargo test --offline)"
        );
        assert_eq!(
            tool_handle("read", r#"{"file_path":"src/main.rs"}"#),
            "read(src/main.rs)"
        );
        assert_eq!(tool_handle("shell", "{}"), "shell()");
        assert_eq!(tool_handle("shell", "not json"), "shell()");
        // An unknown shape still says something rather than nothing.
        assert_eq!(tool_handle("x", r#"{"a":"1","b":2}"#), "x(a=1 b=2)");
    }

    #[test]
    fn tool_output_keeps_its_lines_and_says_how_much_it_hid() {
        let feed = feed_with(vec![ChatMessage::ToolResult {
            call_id: "c".into(),
            output: (1..=7).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"),
            ok: true,
            seq: 2,
            turn: Some(1),
        }]);
        let text = text_of(&transcript(&feed, 60));
        assert!(text.contains("line 1") && text.contains("line 3"), "{text}");
        assert!(!text.contains("line 4"), "only the first three are shown: {text}");
        assert!(text.contains("… (+4 lines)"), "{text}");
        // Line by line, not re-flowed into one paragraph.
        assert!(text.contains("line 1\n"), "{text}");
        assert!(!text.contains("line 1 line 2"), "{text}");
    }

    #[test]
    fn the_session_list_filters_on_what_a_person_recognises() {
        let mut a = summary_row("session-aaaaaaaa-1111", "/home/me/work");
        a.blank = false;
        let mut b = summary_row("session-bbbbbbbb-2222", "/srv/www");
        b.blank = false;
        let sessions = vec![a, b];
        let mut view = DshView::default();
        assert_eq!(view.filtered_sessions(&sessions).len(), 2, "no filter, every row");
        view.session_filter = "www".to_string();
        let shown = view.filtered_sessions(&sessions);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].session_id, "session-bbbbbbbb-2222");
        view.session_filter = "AAAA".to_string();
        assert_eq!(view.filtered_sessions(&sessions).len(), 1, "the id matches too, case-insensitively");
        view.session_filter = "nothing-like-this".to_string();
        assert!(view.filtered_sessions(&sessions).is_empty());
    }

    #[test]
    fn the_cache_never_differs_from_a_full_render() {
        // The cache is an optimization, so its one hard rule is that it can
        // never disagree with rendering everything: every change a conversation
        // goes through is checked against a from-scratch render.
        let mut feed = feed_with(vec![
            user("hello", 1, 1),
            assistant("```rust\nlet a = 1;\n```", 2, 1, false),
        ]);
        let mut view = DshView::default();
        let check = |view: &mut DshView, feed: &DshFeed, width: usize, what: &str| {
            let cached = text_of(view.transcript(feed, width));
            let full = text_of(&transcript(feed, width));
            assert_eq!(cached, full, "after {what} (width {width})");
        };

        check(&mut view, &feed, 60, "the first render");
        check(&mut view, &feed, 60, "the same thing again");
        // A different width re-wraps everything.
        check(&mut view, &feed, 32, "a narrower terminal");
        check(&mut view, &feed, 60, "and back");

        // A new row.
        feed.log.messages.push(assistant("second answer", 3, 1, true));
        check(&mut view, &feed, 60, "a new row");

        // A growing tail: the streaming case, where the row's seq stays put.
        for _ in 0..5 {
            if let Some(ChatMessage::Assistant { text, .. }) = feed.log.messages.last_mut() {
                text.push_str(" and more words");
            }
            check(&mut view, &feed, 60, "a streaming tail");
        }

        // A turn-end notice appearing: the tables change, the rows do not.
        feed.log.turn_end.insert(
            1,
            TurnEndReason {
                kind: "aborted".into(),
                message: "stopped".into(),
                code: None,
                seq: 9,
            },
        );
        check(&mut view, &feed, 60, "a turn notice");

        // Files a turn wrote, on the same row.
        feed.log.produced.insert(1, vec![(9, "src/main.rs".to_string())]);
        check(&mut view, &feed, 60, "produced files");

        // Rows dropped from the front (a trim): every row shifts.
        feed.log.messages.remove(0);
        check(&mut view, &feed, 60, "a trim");

        // And the conversation going away.
        feed.log.messages.clear();
        check(&mut view, &feed, 60, "an empty log");
    }

    #[test]
    fn the_view_can_measure_what_a_prepended_page_added() {
        // The anchor: older rows are prepended, so the window moves down by
        // exactly the lines they render to, and the reader stays put.
        let feed = feed_with(vec![user("one", 1, 1), assistant("two", 2, 1, true)]);
        let mut view = DshView::default();
        let all = view.transcript(&feed, 60).len();
        assert_eq!(view.lines_in_first(0), 0);
        assert!(view.lines_in_first(1) > 0 && view.lines_in_first(1) <= all);
        assert_eq!(view.lines_in_first(2), all, "every row accounted for");
        assert!(!view.at_top(), "an unscrolled view is not at the top");
    }

    #[test]
    fn the_animation_advances_at_its_own_pace() {
        // A spinner does not need sixty frames a second, and an idle page does
        // not need any.
        let mut view = DshView::default();
        let t0 = std::time::Instant::now();
        assert!(view.animation_due(t0), "the first frame draws");
        assert!(!view.animation_due(t0 + Duration::from_millis(50)), "not yet");
        assert!(
            view.animation_due(t0 + Duration::from_millis(100)),
            "a tick has passed"
        );
    }

    #[test]
    fn blocks_are_separated_and_a_tool_call_stays_with_its_result() {
        let feed = feed_with(vec![
            user("run it", 1, 1),
            ChatMessage::ToolCall {
                name: "shell".into(),
                args: r#"{"command":"ls"}"#.into(),
                call_id: "c1".into(),
                seq: 2,
                turn: Some(1),
            },
            ChatMessage::ToolResult {
                call_id: "c1".into(),
                output: "a.rs".into(),
                ok: true,
                seq: 3,
                turn: Some(1),
            },
            assistant("done", 4, 1, true),
        ]);
        let lines = transcript(&feed, 60);
        let text = text_of(&lines);
        let pos = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("{needle} in {text}"));
        assert!(pos("you › run it") < pos("⏺ shell(ls)"), "{text}");
        assert!(pos("⏺ shell(ls)") < pos("⎿ ok:"), "{text}");
        // The call and its result are one block: no blank line between them.
        let call_at = lines
            .iter()
            .position(|l| text_of(&[l.clone()]).contains("⏺ shell(ls)"))
            .unwrap();
        assert!(
            text_of(&[lines[call_at + 1].clone()]).trim_start().starts_with("⎿"),
            "the result follows immediately: {text}"
        );
        // …and the answer after them is a new block.
        assert!(text.contains("\n\n"), "blocks are separated: {text}");
    }

    #[test]
    fn a_thinking_only_draft_draws_its_thinking_and_nothing_else() {
        let feed = feed_with(vec![ChatMessage::Assistant {
            text: String::new(),
            thinking: "weighing the options".into(),
            images: Vec::new(),
            seq: 1,
            turn: Some(1),
            step: Some(1),
            done: false,
        }]);
        let text = text_of(&transcript(&feed, 60));
        assert!(text.contains("✻ weighing the options"), "{text}");
        assert_eq!(
            text.trim().lines().count(),
            1,
            "no empty gutter line under it: {text:?}"
        );
    }

    #[test]
    fn the_working_indicator_moves_with_the_clock() {
        // A still screen reads as a hung one: the spinner has to advance, and it
        // is derived from the clock so a redraw shows the next frame.
        let frames: Vec<&str> = (0..10).map(|i| spinner(i * 100)).collect();
        assert_eq!(frames[0], "⠋");
        assert_ne!(frames[0], frames[1], "the next frame differs");
        assert_eq!(spinner(0), spinner(1000), "and it wraps after ten frames");
    }

    #[test]
    fn a_running_answer_is_marked_and_a_finished_one_is_not() {
        let running = feed_with(vec![assistant("half", 1, 1, false)]);
        let lines = transcript(&running, 80);
        assert!(lines[0].spans.iter().any(|s| s.content.contains('\u{258c}')));
        let finished = feed_with(vec![assistant("done", 1, 1, true)]);
        let lines = transcript(&finished, 80);
        assert!(!lines[0].spans.iter().any(|s| s.content.contains('\u{258c}')));
    }

    #[test]
    fn a_failed_turn_reports_why_after_its_last_row() {
        let mut feed = feed_with(vec![user("do it", 1, 4), assistant("nope", 2, 4, true)]);
        feed.log.turn_end.insert(
            4,
            TurnEndReason {
                kind: "error".into(),
                message: "provider said no".into(),
                code: Some("AUTH".into()),
                seq: 3,
            },
        );
        let text = text_of(&transcript(&feed, 80));
        let notice = text.find("(turn ended: error [AUTH]: provider said no)");
        assert!(notice.is_some(), "{text}");
        // …and it follows the row it belongs to, not the whole transcript.
        assert!(notice.unwrap() > text.find("nope").unwrap(), "{text}");
    }

    #[test]
    fn a_completed_turn_says_nothing() {
        let mut feed = feed_with(vec![assistant("fine", 1, 2, true)]);
        feed.log.turn_end.insert(
            2,
            TurnEndReason {
                kind: "completed".into(),
                message: String::new(),
                code: None,
                seq: 2,
            },
        );
        let text = text_of(&transcript(&feed, 80));
        assert!(!text.contains("turn ended"), "{text}");
    }

    #[test]
    fn turn_clocks_are_available_to_the_renderer() {
        // The tables ride the same rows; the page keeps them on the log.
        let mut feed = feed_with(vec![assistant("timed", 1, 3, true)]);
        feed.log.turn_times.insert(
            3,
            TurnTimes {
                start_ms: Some(1_000),
                end_ms: Some(3_500),
                first_chunk_ms: Some(1_200),
            },
        );
        assert!(feed.log.turn_times.contains_key(&3));
    }

    #[test]
    fn files_a_turn_wrote_are_named_after_its_last_row() {
        let mut feed = feed_with(vec![assistant("wrote it", 9, 7, true)]);
        feed.log.produced.insert(7, vec![(9, "src/main.rs".to_string())]);
        let text = text_of(&transcript(&feed, 80));
        assert!(text.contains("(wrote src/main.rs)"), "{text}");
    }

    #[test]
    fn the_queue_is_shown_as_previews() {
        let queue = vec![
            QueuedItem {
                id: "1".into(),
                placement: "queued".into(),
                preview: "first pending".into(),
                text: None,
                has_image: false,
            },
            QueuedItem {
                id: "2".into(),
                placement: "steering".into(),
                preview: "second pending".into(),
                text: None,
                has_image: false,
            },
        ];
        let text = text_of(&queue_lines(&queue));
        assert!(text.contains("queue[queued] first pending"), "{text}");
        assert!(text.contains("queue[steering] second pending"), "{text}");
    }

    #[test]
    fn the_header_names_the_link_the_session_and_the_queue() {
        let mut feed = feed_with(vec![user("hi", 1, 1)]);
        feed.link = Link::Connected;
        feed.session_id = Some("0123456789abcdef".into());
        feed.running = true;
        feed.queue = vec![QueuedItem {
            id: "1".into(),
            placement: "queued".into(),
            preview: "later".into(),
            text: None,
            has_image: false,
        }];
        let text = text_of(&[header_line(&feed)]);
        for needle in ["dsh", "connected", "01234567", "1 rows", "working", "queue 1"] {
            assert!(text.contains(needle), "{needle:?} missing from {text}");
        }
    }

    #[test]
    fn editing_works_in_characters_not_bytes() {
        let mut view = DshView::default();
        for ch in "中文ab".chars() {
            view.insert(ch);
        }
        assert_eq!(view.input, "中文ab");
        assert_eq!(view.cursor, 4);
        view.left();
        view.left();
        view.insert('X');
        assert_eq!(view.input, "中文Xab", "inserted where the cursor is");
        view.backspace();
        assert_eq!(view.input, "中文ab");
        view.home();
        view.delete();
        assert_eq!(view.input, "文ab");
        view.end();
        view.backspace();
        assert_eq!(view.input, "文a");
        view.insert('中');
        assert_eq!(view.input, "文a中");
        assert_eq!(
            view.input_cursor(),
            5,
            "three characters, five cells: the caret is measured in cells"
        );
    }

    #[test]
    fn the_input_line_edits_like_a_shell() {
        let mut view = DshView::default();
        for ch in "one two three".chars() {
            view.insert(ch);
        }
        view.delete_word();
        assert_eq!(view.input, "one two ", "Ctrl-W eats the word before the cursor");
        view.delete_word();
        assert_eq!(view.input, "one ", "and the space it was sitting behind");
        // In the middle of a line the text after the caret survives.
        view.clear();
        for ch in "alpha beta gamma".chars() {
            view.insert(ch);
        }
        view.home();
        for _ in 0.."alpha beta".len() {
            view.right();
        }
        view.delete_word();
        assert_eq!(view.input, "alpha  gamma");
        assert_eq!(view.cursor, 6);
        view.clear();
        assert_eq!(view.input, "");
        assert_eq!(view.cursor, 0);
    }

    #[test]
    fn the_header_separates_what_it_shows_and_names_a_missing_session() {
        // "connected0 rows" was the bug: with no session followed there was
        // nothing between the state word and the row count.
        let mut feed = DshFeed::new();
        feed.link = Link::Connected;
        let text = text_of(&[header_line(&feed)]);
        assert!(text.contains("connected  no session"), "{text}");
        assert!(text.contains("[Tab] to pick one"), "{text}");
        assert!(!text.contains("connected0"), "{text}");

        feed.session_id = Some("0123456789abcdef".into());
        feed.log.messages = vec![user("hi", 1, 1)];
        let text = text_of(&[header_line(&feed)]);
        assert!(text.contains("01234567"), "{text}");
        assert!(text.contains("1 rows"), "{text}");
        assert!(!text.contains("no session"), "the session is shown now: {text}");
    }

    #[test]
    fn summaries_keep_the_first_line_and_count_the_rest() {
        assert_eq!(summarize("one line"), "one line");
        assert_eq!(summarize("a\nb\nc"), "a (+2 lines)");
        assert_eq!(summarize(""), "");
    }
}
