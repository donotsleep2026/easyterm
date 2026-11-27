//! The DSH conversation model: conversation rows assembled from `session/follow`
//! stream items (snapshot records + live events + live assistant-stream frames),
//! the projections that ride them, and the parsers for queue items, tool calls,
//! token usage and permissions.
//!
//! Deliberately free of egui, localization and session-id types: the daemon owns
//! a `ChatLog` per followed session (so any frontend renders the same rows), and
//! the frontend keeps only the localized strings (`relative_time`,
//! `permission_label`, `session_stats_line`).
//! Pure chat-log model: assembles conversation messages from dsh
//! `session/follow` stream items (snapshot records + live events + live
//! assistant-stream frames) so the UI renders history and live streaming
//! through one testable code path.

use crate::api::session::SessionEventView;
use serde_json::Value;

/// A durable image reference from a `content` block of type `image`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImageRef {
    pub attachment_id: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
}

/// One assembled conversation item.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChatMessage {
    User {
        text: String,
        images: Vec<ImageRef>,
        seq: i64,
        turn: Option<i64>,
        /// Client prompt id (`source.rpcId`); the vision relay re-injects the
        /// same prompt with the same id, so duplicates share it and are
        /// hidden (web behavior).
        rpc_id: Option<String>,
        /// Wall-clock time of the event (epoch millis), when available.
        time: Option<i64>,
    },
    Assistant {
        text: String,
        thinking: String,
        images: Vec<ImageRef>,
        seq: i64,
        turn: Option<i64>,
        step: Option<i64>,
        /// False while the assistant message is still streaming.
        done: bool,
    },
    ToolCall {
        name: String,
        args: String,
        call_id: String,
        seq: i64,
        turn: Option<i64>,
    },
    ToolResult {
        call_id: String,
        output: String,
        ok: bool,
        seq: i64,
        turn: Option<i64>,
    },
    /// Injected context (system prompt / agent instructions / recalled session
    /// snapshots / etc.) surfaced as a `user/message` event whose `source.kind`
    /// is not `"user"` or whose `source.baseline` is true. Rendered as a
    /// collapsible context row rather than a human message (web parity).
    Context {
        text: String,
        source: Value,
        label: Option<String>,
        role: String,
        form: Option<String>,
        seq: i64,
        turn: Option<i64>,
    },
    /// The rendered system prompt (`system/message` event, protocol 0.1.5:
    /// the old `request/header`'s `header.system` moved here). Collapsed by
    /// default; the body shows the exact model-visible text (web
    /// `SystemPromptRow` parity, 20k display cap).
    SystemPrompt {
        text: String,
        seq: i64,
        turn: Option<i64>,
    },
    /// A provider-routed model-request retry (`llm/retry`), rendered as a
    /// collapsed disclosure row; consecutive retries of one chain collapse
    /// into the same row (`retry_id` is stable per chain, web parity).
    Retry {
        retry_id: String,
        /// Zero-based attempt number this scheduled retry will run as.
        retry: i64,
        /// `maxRetries` for `normal` mode; absent for `always` (∞).
        max_retries: Option<i64>,
        /// `normal` or `always`.
        mode: String,
        delay_ms: i64,
        /// Failure reason that triggered the retry.
        message: String,
        code: Option<String>,
        /// True once `llm/retry-started` fired (the wait is over).
        started: bool,
        seq: i64,
        turn: Option<i64>,
        step: Option<i64>,
    },
}

/// Conversation buffer for one dsh session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatLog {
    pub messages: Vec<ChatMessage>,
    /// Streaming block state by (turn, step, index): the block kind from
    /// `block-start` (or inferred from the delta type) plus the accumulated
    /// text. `block-end` replaces the text with the server's full block text,
    /// so deltas + end never duplicate.
    chunk_blocks: std::collections::HashMap<(Option<i64>, Option<i64>, u64), (String, String)>,
    /// rpcIds of user messages already shown, to skip vision-relay
    /// re-injections (they resend the same prompt with the same id).
    seen_user_rpc: std::collections::HashSet<String>,
    /// Mutation-tool call index (`call_id -> (name, args)`), so a later
    /// successful `tool/result` can derive the turn's produced files (web
    /// `mutationPath` parity: `write`/`edit`/`str_replace_editor` paths come
    /// from the validated raw call arguments — protocol 0.1.5 removed the
    /// host-computed `view` render intent from the wire).
    call_index: std::collections::HashMap<String, (String, String)>,
    /// Live assistant-stream attempts (`attemptId -> (turn, step)`), opened by
    /// `assistant-stream` `start` frames and retired by `end` frames.
    /// Per-attempt step bookkeeping (keyed by attempt id); the row model's
    /// own state, exposed so a frontend can inspect/restore it.
    pub attempt_steps: std::collections::HashMap<String, (i64, i64)>,
    /// Next synthetic seq for stream-driven drafts. Live `assistant-stream`
    /// frames carry NO seq (only durable events have one), so drafts are
    /// numbered from [`SYNTHETIC_SEQ_BASE`] upward — far above every real
    /// seq, so they never collide with dense durable seqs arriving mid-turn.
    /// The committed `assistant/message` reseqs its draft onto the durable
    /// seq (see `apply_assistant_message`); consumers that compare seqs
    /// against the wire (unread counters, seen-watermarks) must skip
    /// synthetic seqs via [`is_synthetic_seq`].
    next_synthetic_seq: i64,
    /// Files produced by successful mutation tool calls, per turn
    /// (`turn -> [(result seq, path)]` in first-seen order, deduplicated).
    /// A mutation is recognized by render intent, not tool name: a `diff`
    /// card, or a `generic` card whose `kind` is `edit`.
    pub produced: std::collections::HashMap<i64, Vec<(i64, String)>>,
    /// How a finished turn ended (`turn/end` `reason`), for the turn-failure
    /// and max-tokens notices (web parity).
    pub turn_end: std::collections::HashMap<i64, TurnEndReason>,
    /// Wall-clock timings per turn (`turn/start` → `turn/end` plus the first
    /// assistant chunk), for the web-style "用时 / 首 token" clocks.
    pub turn_times: std::collections::HashMap<i64, TurnTimes>,
    /// Compaction summaries by compaction id (`compaction/summary`), for the
    /// web-style compaction checkpoint marker row.
    pub compaction_summaries: std::collections::HashMap<String, CompactionSummary>,
    /// Wall-clock millis of the last streaming flush. Rebuilding the message
    /// from the block buffers on EVERY delta is O(buffered bytes) per delta —
    /// O(len²) per long streamed message — so `apply_assistant_chunk`
    /// throttles the flush to [`STREAM_FLUSH_MIN_MS`] (block-end and
    /// turn/end always flush).
    /// When the last streamed assistant frame was folded in (ms since epoch).
    pub last_stream_flush_ms: u64,
    /// A throttled delta is buffered but not yet rebuilt into the message;
    /// the UI frame loop drains these via `flush_streaming_tails` so the
    /// visible tail is one frame stale, not stuck until the next delta.
    stream_flush_pending: bool,
    /// Seq of the latest streamed delta (tail-flush bookkeeping).
    last_stream_seq: i64,
    /// Latest event seq seen per `(turn, step)` block. A frame drain or a
    /// `turn/end` force-flush may cover several pending steps at once, but it
    /// must never reuse ONE seq for all of them: the drained messages push
    /// with that shared seq, so two assistant messages (each with its own
    /// thinking fold) end up with the same seq. Duplicate seqs break the
    /// seq-ordered log and make every per-row widget id that is salted by
    /// `seq` collide — egui paints "First/Second use of widget ID" over each
    /// thinking row. Storing each block's own last seq keeps the drained
    /// messages on their real, distinct wire seqs.
    last_seq_by_step: std::collections::HashMap<(Option<i64>, Option<i64>), i64>,
    /// Per-seq occurrence counts over ALL currently-held messages (every
    /// variant). Backs O(1) [`Self::seq_present`] / duplicate detection
    /// instead of a full-scan per push / per frame.
    seq_counts: std::collections::HashMap<i64, usize>,
    /// Per-seq counts restricted to still-streaming (`done: false`) assistant
    /// drafts. `seq_present` must never treat a live draft as a finished
    /// duplicate (its own later events still update it), so the "finished"
    /// count for a seq is `seq_counts − streaming_seq_counts`.
    streaming_seq_counts: std::collections::HashMap<i64, usize>,
    /// Total number of streaming (`done: false`) assistant messages. This is
    /// the O(1) source for [`Self::has_streaming`] — the per-frame working
    /// query used to `iter().any(done: false)` across the whole (possibly
    /// hundreds-of-thousands-entry) log every call.
    streaming_total: usize,
    /// Number of distinct seqs occurring more than once. Zero ⇒ the renderer
    /// can skip its per-frame occurrence-ordinal map entirely (duplicate seqs
    /// are the only case that needs ordinal-disambiguated egui row ids).
    dup_seqs: usize,
}

/// Minimum interval between streaming-message rebuilds (see ChatLog docs).
const STREAM_FLUSH_MIN_MS: u64 = 50;

/// First seq of the synthetic range numbering live stream drafts
/// (`assistant-stream` frames carry no seq). Far above any real session-log
/// seq, so drafts never collide with dense durable seqs arriving mid-turn.
pub const SYNTHETIC_SEQ_BASE: i64 = i64::MAX / 4;

/// True when `seq` belongs to a live stream draft, not a durable event.
/// Consumers comparing row seqs against wire watermarks (unread counters,
/// seen markers, history cursors) must skip synthetic seqs.
pub fn is_synthetic_seq(seq: i64) -> bool {
    seq >= SYNTHETIC_SEQ_BASE
}

/// Current wall clock in milliseconds (throttle bookkeeping only).
/// Milliseconds since the Unix epoch (the model's clock).
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `turn/end` reason of one finished turn (web turn-error / max-tokens rows).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct TurnEndReason {
    /// `error`, `max-tokens`, `aborted`, `completed`, ...
    pub kind: String,
    /// Display-safe failure message (for `kind == "error"`). Empty when the
    /// host error code is `AUTH` (web `displayFailure` blanks provider auth
    /// diagnostics; the row shows the localized auth copy instead).
    pub message: String,
    /// Optional host error code.
    pub code: Option<String>,
    /// Seq of the `turn/end` event itself. The failure / max-tokens notice is
    /// an element OUTSIDE the message rows, so it needs an anchor row: a turn
    /// that produced no row of its own (an LLM call failing before any content
    /// — e.g. `AUTH`) has no row carrying its turn number, and this seq lets
    /// the renderer fall back to the last row at or before it (see
    /// `dsh_turn_notice_anchor` in app_dsh.rs).
    pub seq: i64,
}

/// Per-turn wall-clock timings (epoch millis from the event envelope).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct TurnTimes {
    /// `turn/start` time.
    pub start_ms: Option<i64>,
    /// `turn/end` time.
    pub end_ms: Option<i64>,
    /// Time of the first assistant chunk after `turn/start` (TTFT).
    pub first_chunk_ms: Option<i64>,
}

/// One `compaction/summary` record (web compaction checkpoint marker).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct CompactionSummary {
    /// Number of shadowed (replaced) messages.
    pub item_count: usize,
    /// Estimated token count of the replaced range.
    pub token_count: i64,
    /// The summary text blocks joined.
    pub text: String,
}

/// Everything a conversation renders OUTSIDE its message rows: the produced
/// files of each turn (web "Files changed" / `本轮文件改动`), how each finished
/// turn ended (the turn-failure / max-tokens notice), its wall-clock times (the
/// `用时 / 首 token` clock) and the compaction summaries (checkpoint markers).
///
/// Rows cannot carry these — they are per-turn, not per-message — and the
/// daemon owns the rows, so it ships this alongside them (see
/// [`ChatSnapshotWire`] / [`ChatUpdateWire`]). A frontend merges what it gets
/// into its own [`ChatLog`] via [`ChatLog::merge_side_tables`] instead of
/// replaying the event stream itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChatSideTables {
    /// Files produced per turn (`turn -> [(result seq, path)]`, first-seen
    /// order, deduplicated by path).
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub produced: std::collections::HashMap<i64, Vec<(i64, String)>>,
    /// How each finished turn ended.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub turn_end: std::collections::HashMap<i64, TurnEndReason>,
    /// Wall-clock timings per turn.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub turn_times: std::collections::HashMap<i64, TurnTimes>,
    /// Compaction summaries by compaction id.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub compaction_summaries: std::collections::HashMap<String, CompactionSummary>,
}

impl ChatSideTables {
    /// True when there is nothing to ship or merge.
    pub fn is_empty(&self) -> bool {
        self.produced.is_empty()
            && self.turn_end.is_empty()
            && self.turn_times.is_empty()
            && self.compaction_summaries.is_empty()
    }
}

/// Parsed `tool/call` event fields.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolCallInfo {
    pub name: String,
    pub args: String,
    pub call_id: String,
}

/// Parsed `tool/result` event fields.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolResultInfo {
    pub call_id: String,
    pub output: String,
    pub ok: bool,
}

/// One transient inbox occurrence, projected for the queue dock (web
/// `QueuedMessage` parity: `preview` mirrors `previewOf`, `text` mirrors
/// `textOf`). Since 0.1.7 the rows come from the session's `inbox`
/// projection, not a control-stream queue frame.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueuedItem {
    /// Inbox occurrence id (the durable message id) accepted by
    /// `session.updateQueue` as `itemId`.
    pub id: String,
    /// Which inbox list carried the message: `queued` (`next-turn`) or
    /// `steering` (`next-step`).
    pub placement: String,
    /// Flattened one-line preview: text blocks joined, non-text blocks as
    /// `[type]`, whitespace collapsed, capped at 200 chars.
    pub preview: String,
    /// Full editable text; `None` when the message contains non-text blocks.
    pub text: Option<String>,
    /// Whether the message carries an image block (row shows a glyph).
    pub has_image: bool,
}

/// Preview cap for queued rows (web `QUEUE_PREVIEW_CHARS`).
const QUEUE_PREVIEW_CHARS: usize = 200;

/// Parse the `next-turn`/`next-step` arrays of one `inbox` session
/// projection (`InboxWireState`: each entry is the durable user message
/// `{id, content, …}`) into dock rows. Malformed entries are skipped so one
/// bad item never blanks the whole dock.
pub fn parse_inbox_projection(value: &Value) -> Vec<QueuedItem> {
    let mut out = Vec::new();
    for (target, placement) in [("next-turn", "queued"), ("next-step", "steering")] {
        let Some(array) = value.get(target).and_then(Value::as_array) else {
            continue;
        };
        for message in array {
            let Some(id) = message.get("id").and_then(Value::as_str) else {
                continue;
            };
            let content = message
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            out.push(QueuedItem {
                id: id.to_string(),
                placement: placement.to_string(),
                preview: queue_preview_of(&content),
                text: queue_text_of(&content),
                has_image: content
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("image")),
            });
        }
    }
    out
}

/// Flatten content blocks into a one-line preview (web `previewOf`).
fn queue_preview_of(content: &[Value]) -> String {
    let flat: String = content
        .iter()
        .map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") => block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            Some(other) => format!("[{other}]"),
            None => String::new(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    let collapsed: String = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = collapsed.chars().collect();
    if chars.len() > QUEUE_PREVIEW_CHARS {
        let mut head: String = chars.into_iter().take(QUEUE_PREVIEW_CHARS).collect();
        head.push('…');
        head
    } else {
        collapsed
    }
}

/// Full editable text when every block is text, else `None` (web `textOf`).
fn queue_text_of(content: &[Value]) -> Option<String> {
    if !content
        .iter()
        .all(|b| b.get("type").and_then(Value::as_str) == Some("text"))
    {
        return None;
    }
    Some(
        content
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect(),
    )
}

impl ChatLog {
    /// Fold one durable session-log event into the conversation.
    ///
    /// Protocol 0.1.5: events arrive from `session/follow` snapshot records
    /// and live `event` frames, or from `session/page` history pages. The old
    /// host-computed `view` render intent no longer exists on the wire;
    /// produced files derive from `deliverables/presented` events and from
    /// the raw arguments of successful mutation tool calls (web parity).
    pub fn apply_event(&mut self, event: &SessionEventView) {
        let seq = event.seq.unwrap_or(0);
        let turn = event.data.get("turn").and_then(Value::as_i64);
        let step = event.data.get("step").and_then(Value::as_i64);
        // v3 surface replacement: `{op:'replace', startSeq, endSeq}` retires
        // the inclusive surface-span before this event's own row lands (e.g.
        // a `system/message` replacing the previous system prompt). Applied
        // generically ahead of the per-kind arms.
        if let Some(op) = event.surface_op.as_ref().filter(|v| v.is_object()) {
            if op.get("op").and_then(Value::as_str) == Some("replace") {
                let start = op.get("startSeq").and_then(Value::as_i64);
                let end = op.get("endSeq").and_then(Value::as_i64);
                if let (Some(start), Some(end)) = (start, end) {
                    self.retire_surface_span(start, end);
                }
            }
        }
        match event.kind.as_str() {
            "user/message" => {
                // Non-user sources (system-prompt snapshots, AGENTS.md
                // instructions, recalled sessions, etc.) are model-facing
                // context injections rather than human messages. Render them as
                // a collapsible context row (web parity).
                if is_injected_context(&event.data) {
                    // Replay / history-live overlap: an already-applied
                    // context injection (same seq) must not append twice.
                    if self.seq_present(seq) {
                        return;
                    }
                    let provenance = context_provenance(event.data.get("source"));
                    let form = context_form(event.data.get("source"));
                    let text = extract_text(&event.data);
                    self.push_message(ChatMessage::Context {
                        text,
                        source: event.data.get("source").cloned().unwrap_or(Value::Null),
                        label: provenance.label,
                        role: provenance.role,
                        form,
                        seq,
                        turn,
                    });
                    return;
                }
                let rpc_id = event
                    .data
                    .get("source")
                    .and_then(|s| s.get("rpcId"))
                    .and_then(Value::as_str)
                    .map(String::from);
                // The vision relay resends the user's prompt (same rpcId)
                // with its analysis attached; web keeps only the first
                // occurrence, so drop the duplicate.
                if let Some(rpc_id) = &rpc_id {
                    if !self.seen_user_rpc.insert(rpc_id.clone()) {
                        return;
                    }
                }
                let text = extract_text(&event.data);
                let images = extract_images(&event.data);
                // Replay / history-live overlap: the same user message (same
                // seq) already applied as a finished row — drop the duplicate.
                if self.seq_present(seq) {
                    return;
                }
                self.push_message(ChatMessage::User {
                    text,
                    images,
                    seq,
                    turn,
                    rpc_id,
                    time: event.time,
                });
            }
            "assistant/message" => self.apply_assistant_message(&event.data, seq, turn, step),
            "turn/start" => {
                if let Some(turn) = turn {
                    let times = self.turn_times.entry(turn).or_default();
                    if times.start_ms.is_none() {
                        times.start_ms = event.time;
                    }
                }
            }
            "turn/end" => {
                // Force-flush any throttled tail deltas FIRST:
                // flush_assistant_step locates the in-flight (done=false)
                // message — once the messages below are marked done, a late
                // flush would push a phantom new message instead of updating
                // the existing one (and the phantom would pin the session's
                // working state forever).
                let pending: Vec<(Option<i64>, Option<i64>)> = self
                    .chunk_blocks
                    .keys()
                    .filter(|(t, _, _)| turn.is_none() || *t == turn)
                    .map(|(t, s, _)| (*t, *s))
                    .collect();
                for (t, s) in pending {
                    // Use each block's own last wire seq, not the turn/end
                    // seq for every step: flushing several steps with ONE seq
                    // pushes several assistant messages with the same seq, and
                    // every id salted by seq (thinking folds, scroll areas)
                    // collides — egui paints "First/Second use of widget ID".
                    let step_seq = self
                        .last_seq_by_step
                        .get(&(t, s))
                        .copied()
                        .unwrap_or(seq);
                    self.flush_assistant_step(step_seq, t, s);
                }
                // The turn is over: mark every still-streaming assistant
                // message done (the last log entry may be a tool result, not
                // an assistant message, so scanning only the tail is wrong).
                // Every draft below is finished by the scan (no turn filter),
                // so the streaming index resets in one go — keeping
                // `has_streaming()` O(1) without per-message bookkeeping.
                self.streaming_total = 0;
                self.streaming_seq_counts.clear();
                for message in &mut self.messages {
                    if let ChatMessage::Assistant { done, .. } = message {
                        *done = true;
                    }
                }
                // Record how the turn ended (error / max-tokens / aborted /
                // completed) for the turn-failure and max-tokens notices.
                if let Some(turn) = turn {
                    let mut reason = TurnEndReason::default();
                    reason.seq = seq;
                    if let Some(kind) = event
                        .data
                        .get("reason")
                        .and_then(|r| r.get("kind"))
                        .and_then(Value::as_str)
                    {
                        reason.kind = kind.to_string();
                    }
                    if let Some(err) = event.data.get("reason").and_then(|r| r.get("error")) {
                        if let Some(message) = err.get("message").and_then(Value::as_str) {
                            reason.message = message.to_string();
                        }
                        reason.code = err.get("code").and_then(Value::as_str).map(String::from);
                    }
                    // Web `displayFailure`: provider AUTH diagnostics may echo a
                    // masked or partially preserved credential, so the raw text
                    // is dropped and the row shows the localized auth copy
                    // instead ("API key is invalid" / "API 密钥无效").
                    if reason.code.as_deref() == Some("AUTH") {
                        reason.message.clear();
                    }
                    self.turn_end.insert(turn, reason);
                    if event.time.is_some() {
                        self.turn_times.entry(turn).or_default().end_ms = event.time;
                    }
                }
                // The turn's block buffers and attempt routing are no longer
                // needed.
                self.chunk_blocks.clear();
                self.attempt_steps.clear();
            }
            "llm/retry" => {
                // One scheduled provider-routed retry. Consecutive retries of
                // the same chain (retryId) collapse into the last row.
                let retry_id = event
                    .data
                    .get("retryId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if retry_id.is_empty() {
                    return;
                }
                // Consecutive retries of one chain collapse into the row.
                // Update in place (keeping the original seq) so the row's
                // widget id — salted by seq — stays stable across attempts.
                if let Some(ChatMessage::Retry {
                    retry_id: last,
                    retry,
                    max_retries,
                    mode,
                    delay_ms,
                    message,
                    code,
                    started,
                    turn: row_turn,
                    step: row_step,
                    ..
                }) = self.messages.last_mut()
                {
                    if *last == retry_id {
                        *retry = event.data.get("retry").and_then(Value::as_i64).unwrap_or(0);
                        *max_retries = event.data.get("maxRetries").and_then(Value::as_i64);
                        *mode = event
                            .data
                            .get("mode")
                            .and_then(Value::as_str)
                            .unwrap_or("normal")
                            .to_string();
                        *delay_ms = event.data.get("delayMs").and_then(Value::as_i64).unwrap_or(0);
                        *message = event
                            .data
                            .get("failure")
                            .and_then(|f| f.get("message"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        *code = event
                            .data
                            .get("failure")
                            .and_then(|f| f.get("code"))
                            .and_then(Value::as_str)
                            .map(String::from);
                        *started = false;
                        *row_turn = turn;
                        *row_step = step;
                        return;
                    }
                }
                // Replay / history-live overlap: the retry event already
                // applied (finished row with the same seq) — drop the
                // duplicate push. In-place chain updates above are untouched.
                if self.seq_present(seq) {
                    return;
                }
                self.push_message(ChatMessage::Retry {
                    retry_id,
                    retry: event.data.get("retry").and_then(Value::as_i64).unwrap_or(0),
                    max_retries: event
                        .data
                        .get("maxRetries")
                        .and_then(Value::as_i64),
                    mode: event
                        .data
                        .get("mode")
                        .and_then(Value::as_str)
                        .unwrap_or("normal")
                        .to_string(),
                    delay_ms: event.data.get("delayMs").and_then(Value::as_i64).unwrap_or(0),
                    message: event
                        .data
                        .get("failure")
                        .and_then(|f| f.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    code: event
                        .data
                        .get("failure")
                        .and_then(|f| f.get("code"))
                        .and_then(Value::as_str)
                        .map(String::from),
                    started: false,
                    seq,
                    turn,
                    step,
                });
            }
            "llm/retry-started" => {
                let retry_id = event
                    .data
                    .get("retryId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                for message in self.messages.iter_mut().rev() {
                    if let ChatMessage::Retry {
                        retry_id: id, started, ..
                    } = message
                    {
                        if id == retry_id {
                            *started = true;
                            break;
                        }
                    }
                }
            }
            "compaction/summary" => {
                let id = event
                    .data
                    .get("compactionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !id.is_empty() {
                    let item_count = event
                        .data
                        .get("shadowedSeqs")
                        .and_then(Value::as_array)
                        .map(|v| v.len())
                        .unwrap_or(0);
                    let token_count = event
                        .data
                        .get("shadowedTokenCount")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    let text = event
                        .data
                        .get("summary")
                        .and_then(Value::as_array)
                        .map(|blocks| {
                            blocks
                                .iter()
                                .filter_map(|b| b.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .unwrap_or_default();
                    self.compaction_summaries.insert(
                        id.to_string(),
                        CompactionSummary {
                            item_count,
                            token_count,
                            text,
                        },
                    );
                }
            }
            "tool/call" => {
                if let Some(info) = parse_tool_call(&event.data) {
                    // Replay / history-live overlap: this tool call already
                    // applied (finished row with the same seq) — drop it
                    // before touching the mutation index.
                    if self.seq_present(seq) {
                        return;
                    }
                    // Index the call so a later successful result can derive
                    // the turn's produced files from the raw arguments (web
                    // `mutationPath`: write/edit/str_replace_editor only).
                    if mutation_path(&info.name, &info.args).is_some() {
                        self.call_index
                            .insert(info.call_id.clone(), (info.name.clone(), info.args.clone()));
                    }
                    self.push_message(ChatMessage::ToolCall {
                        name: info.name,
                        args: info.args,
                        call_id: info.call_id,
                        seq,
                        turn,
                    });
                }
            }
            "tool/result" => {
                if let Some(info) = parse_tool_result(&event.data) {
                    // Replay / history-live overlap: this tool result already
                    // applied — drop it so the log never grows on repeated
                    // seqs (the produced-files list below dedups on its own,
                    // so skipping is consistent with a fresh apply).
                    if self.seq_present(seq) {
                        return;
                    }
                    // Successful mutation calls contribute their target path
                    // to the turn's produced-files list (web deliverables).
                    if info.ok {
                        if let Some((name, args)) = self.call_index.get(&info.call_id) {
                            if let Some(path) = mutation_path(name, args) {
                                if let Some(turn) = turn {
                                    let entry = self.produced.entry(turn).or_default();
                                    if !entry.iter().any(|(_, p)| *p == path) {
                                        entry.push((seq, path));
                                    }
                                }
                            }
                        }
                    }
                    self.push_message(ChatMessage::ToolResult {
                        call_id: info.call_id,
                        output: info.output,
                        ok: info.ok,
                        seq,
                        turn,
                    });
                }
            }
            "deliverables/presented" => {
                // Durable explicit delivery (`present` tool):
                // `{turn, callId, files:[{path, description?}]}`. Feeds the
                // same per-turn produced-files list as mutation folds; being
                // a persisted event, it survives history replay (the old
                // live-only `view` cards did not).
                if let Some(turn) = turn {
                    if let Some(files) = event.data.get("files").and_then(Value::as_array) {
                        let entry = self.produced.entry(turn).or_default();
                        for path in files
                            .iter()
                            .filter_map(|f| f.get("path").and_then(Value::as_str))
                        {
                            if !entry.iter().any(|(_, p)| p == path) {
                                entry.push((seq, path.to_string()));
                            }
                        }
                    }
                }
            }
            "system/message" => {
                // The rendered system prompt (v3: surface node 0, formerly
                // `request/header`'s `header.system`). Collapsed row; resume
                // replays dedupe on seq.
                if self.seq_present(seq) {
                    return;
                }
                let text = event
                    .data
                    .get("message")
                    .map(extract_text)
                    .unwrap_or_default();
                if text.trim().is_empty() {
                    return;
                }
                // Web SystemPromptRow display cap: 20k chars.
                let text: String = text.chars().take(20_000).collect();
                self.push_message(ChatMessage::SystemPrompt { text, seq, turn });
            }
            "assistant/attempt" => {
                // A model attempt that committed NO surface message (failed /
                // retried / cancelled). The retry rows and turn/end reason
                // already narrate the failure; the embedded raw stream is a
                // diagnostic, not a bubble — never rendered (web parity: the
                // conversation shows no row for it either).
            }
            _ => {} // step events, compaction, etc.
        }
    }

    /// Apply one live `assistant-stream` frame of the `session/follow` stream
    /// (protocol 0.1.5: replaces the old durable `assistant/chunk` events).
    /// `start` registers the attempt's (turn, step); `chunk` folds deltas
    /// into the block buffers; `end` retires the attempt — a committed
    /// stream's durable `assistant/message` arrives as an ordinary event and
    /// replaces the draft, an abandoned one drops the draft here.
    ///
    /// Returns whether a row actually changed. The host repeats frames
    /// (`block-start`, usage/finish markers, throttled deltas that did not
    /// reach a flush, re-delivered frames after a reconnect) and a broadcast
    /// per repetition makes the frontend re-render an identical tail, so
    /// callers must only publish when this says so.
    pub fn apply_assistant_stream_frame(&mut self, frame: &Value) -> bool {
        let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "start" => {
                let attempt_id = frame
                    .get("attemptId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let turn = frame.get("turn").and_then(Value::as_i64);
                let step = frame.get("step").and_then(Value::as_i64);
                if let (Some(turn), Some(step), false) = (turn, step, attempt_id.is_empty()) {
                    self.attempt_steps.insert(attempt_id, (turn, step));
                }
                false
            }
            "chunk" => {
                let attempt_id = frame.get("attemptId").and_then(Value::as_str);
                let Some((turn, step)) = attempt_id
                    .and_then(|id| self.attempt_steps.get(id).copied())
                else {
                    return false; // chunk for an unknown attempt (pre-start) — skip
                };
                let Some(chunk) = frame.get("chunk") else {
                    return false;
                };
                // TTFT clock: first chunk time of the turn.
                if let Some(time) = frame.get("time").and_then(Value::as_i64) {
                    let times = self.turn_times.entry(turn).or_default();
                    if times.first_chunk_ms.is_none() {
                        times.first_chunk_ms = Some(time);
                    }
                }
                let seq = self.alloc_stream_seq();
                self.apply_assistant_chunk(chunk, seq, Some(turn), Some(step))
            }
            "end" => {
                let Some(attempt_id) = frame.get("attemptId").and_then(Value::as_str) else {
                    return false;
                };
                let attempt = self.attempt_steps.remove(attempt_id);
                let committed = frame
                    .get("outcome")
                    .and_then(|o| o.get("kind"))
                    .and_then(Value::as_str)
                    == Some("committed");
                if !committed {
                    // Abandoned attempt: its partial output never becomes a
                    // durable message — drop the draft and the buffers.
                    if let Some((turn, step)) = attempt {
                        return self.drop_streaming_step(turn, step);
                    }
                }
                false
            }
            _ => false,
        }
    }

    /// Reconnect baseline: the follow snapshot's `assistantStream` carries the
    /// in-flight attempt's accumulated stream so a reopened session shows the
    /// live tail (`{revision, activeAttempt?: {attemptId, startedAfterSeq,
    /// turn, step, nextIndex, stream}}`).
    ///
    /// An attempt whose turn already ended is **never** revived. The host keeps
    /// advertising an attempt whose `end` frame never arrived — a stream cut
    /// mid-flight (the `STREAM_CLOSED` shape) leaves `activeAttempt` in every
    /// later baseline — but the durable `turn/end` has already passed on this
    /// side. Replaying it would rebuild a `done: false` draft that *nothing*
    /// can close (the turn is over, [`Self::apply_event`]'s `turn/end` cleared
    /// the attempts, and a rebaseline would only re-arm it), pinning
    /// [`Self::has_streaming`] — and with it the session's title spinner —
    /// forever while the host reports the session stopped (web, which spins on
    /// the host's status alone, looks fine).
    pub fn apply_assistant_stream_baseline(&mut self, baseline: &Value) {
        let Some(attempt) = baseline.get("activeAttempt") else {
            return;
        };
        let attempt_id = attempt
            .get("attemptId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let turn = attempt.get("turn").and_then(Value::as_i64);
        let step = attempt.get("step").and_then(Value::as_i64);
        if let (Some(turn), Some(step), false) = (turn, step, attempt_id.is_empty()) {
            if self.turn_end.contains_key(&turn) {
                return;
            }
            self.attempt_steps.insert(attempt_id, (turn, step));
            if let Some(stream) = attempt.get("stream").and_then(Value::as_array) {
                // The baseline stream is the COMPACT form
                // (`AssistantStreamRecord`): packed `text-chunks` /
                // `reasoning-chunks` runs (`{index, texts[]}`) interleaved
                // with raw `{type:'chunk', chunk}` records. Unpack the runs
                // straight into the block buffers; raw records ride the
                // ordinary chunk path.
                for record in stream {
                    match record.get("type").and_then(Value::as_str) {
                        Some("text-chunks") | Some("reasoning-chunks") => {
                            let kind = if record.get("type").and_then(Value::as_str)
                                == Some("reasoning-chunks")
                            {
                                "reasoning"
                            } else {
                                "text"
                            };
                            let index =
                                record.get("index").and_then(Value::as_u64).unwrap_or(0);
                            let text: String = record
                                .get("texts")
                                .and_then(Value::as_array)
                                .map(|texts| {
                                    texts
                                        .iter()
                                        .filter_map(|t| t.as_str())
                                        .collect::<Vec<_>>()
                                        .join("")
                                })
                                .unwrap_or_default();
                            if !text.is_empty() {
                                let entry = self
                                    .chunk_blocks
                                    .entry((Some(turn), Some(step), index))
                                    .or_insert_with(|| (kind.to_string(), String::new()));
                                entry.1.push_str(&text);
                            }
                        }
                        Some("chunk") => {
                            if let Some(chunk) = record.get("chunk") {
                                let seq = self.alloc_stream_seq();
                                self.apply_assistant_chunk(
                                    chunk,
                                    seq,
                                    Some(turn),
                                    Some(step),
                                );
                            }
                        }
                        _ => {} // tool-call-chunks carry no bubble text
                    }
                }
                // Rebuild the draft once for the whole replayed baseline.
                let seq = self.alloc_stream_seq();
                self.flush_assistant_step(seq, Some(turn), Some(step));
            }
        }
    }

    /// Stream frames carry no durable seq; number drafts from the synthetic
    /// range (never colliding with dense wire seqs). The committed
    /// `assistant/message` reseqs its draft onto the real seq.
    fn alloc_stream_seq(&mut self) -> i64 {
        if self.next_synthetic_seq < SYNTHETIC_SEQ_BASE {
            self.next_synthetic_seq = SYNTHETIC_SEQ_BASE;
        }
        let seq = self
            .next_synthetic_seq
            .max(self.messages.last().map(log_message_seq).unwrap_or(0) + 1);
        self.next_synthetic_seq = seq + 1;
        seq
    }

    /// Drop the still-streaming draft of `(turn, step)` and its block buffers
    /// (abandoned attempt). Committed attempts leave their draft in place —
    /// the durable `assistant/message` replaces it. Returns whether a row was
    /// actually removed.
    fn drop_streaming_step(&mut self, turn: i64, step: i64) -> bool {
        self.chunk_blocks
            .retain(|(t, s, _), _| *t != Some(turn) || *s != Some(step));
        self.last_seq_by_step.remove(&(Some(turn), Some(step)));
        if let Some(pos) = self.messages.iter().rposition(|m| {
            matches!(m, ChatMessage::Assistant { done: false, turn: t, step: s, .. }
                if *t == Some(turn) && *s == Some(step))
        }) {
            let message = self.messages.remove(pos);
            self.deindex_message(&message);
            return true;
        }
        false
    }

    /// Streamed assistant deltas: `text-delta`, `reasoning-delta` and
    /// `block-end` chunks update the block buffers and rebuild the current
    /// (turn, step) assistant message, routing thinking/reasoning blocks into
    /// the message's `thinking` field. `seq` is a synthetic draft seq (see
    /// [`Self::alloc_stream_seq`]); the committed message's durable seq
    /// replaces it via [`Self::apply_assistant_message`].
    ///
    /// Returns whether the visible log changed: a throttled delta only moves
    /// bytes into the buffers, and the periodic drain publishes them.
    fn apply_assistant_chunk(
        &mut self,
        chunk: &Value,
        seq: i64,
        turn: Option<i64>,
        step: Option<i64>,
    ) -> bool {
        let ctype = chunk.get("type").and_then(Value::as_str).unwrap_or("");
        let index = chunk.get("index").and_then(Value::as_u64).unwrap_or(0);
        let key = (turn, step, index);
        // Remember the latest event seq per block so a later drain / turn-end
        // force-flush can build each message on its own seq instead of
        // reusing one shared seq for every step (duplicate-seq bug).
        self.last_seq_by_step.insert((turn, step), seq);
        match ctype {
            "block-start" => {
                let kind = chunk
                    .get("blockType")
                    .and_then(Value::as_str)
                    .unwrap_or("text")
                    .to_string();
                self.chunk_blocks
                    .entry(key)
                    .or_insert_with(|| (kind, String::new()));
                false
            }
            "text-delta" | "reasoning-delta" | "block-end" => {
                let text = chunk
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        chunk
                            .get("block")
                            .and_then(|b| b.get("text"))
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("");
                if text.is_empty() {
                    return false;
                }
                let kind = self
                    .chunk_blocks
                    .get(&key)
                    .map(|(kind, _)| kind.clone())
                    .unwrap_or_else(|| {
                        if ctype == "reasoning-delta" {
                            "reasoning".to_string()
                        } else {
                            "text".to_string()
                        }
                    });
                let entry = self
                    .chunk_blocks
                    .entry(key)
                    .or_insert_with(|| (kind, String::new()));
                if ctype == "block-end" {
                    // The server repeats the block's full text here; the
                    // deltas already carry it, so replace to avoid dupes.
                    entry.1 = text.to_string();
                } else {
                    entry.1.push_str(text);
                }
                // Rebuilding the message from the block buffers is
                // O(buffered bytes); doing it per delta makes one long
                // streamed message cost O(len²) over a turn. Flush at most
                // every STREAM_FLUSH_MIN_MS — block-end always flushes (it
                // carries the authoritative full block text), and turn/end
                // force-flushes the tail before clearing the buffers.
                let now = now_millis();
                if ctype == "block-end"
                    || now.saturating_sub(self.last_stream_flush_ms) >= STREAM_FLUSH_MIN_MS
                {
                    let changed = self.flush_assistant_step(seq, turn, step);
                    self.last_stream_flush_ms = now;
                    changed
                } else {
                    self.stream_flush_pending = true;
                    self.last_stream_seq = seq;
                    false
                }
            }
            _ => false, // usage / finish markers
        }
    }

    /// Rebuild the streaming Assistant message for `(turn, step)` from the
    /// accumulated block buffers, so every delta keeps the bubble/fold live
    /// without duplicating `block-end` full text. Returns whether the row's
    /// rendered text/thinking actually changed (an identical rebuild — a
    /// repeated `block-end`, a drain with no new bytes — must not be
    /// published as an update).
    fn flush_assistant_step(&mut self, seq: i64, turn: Option<i64>, step: Option<i64>) -> bool {
        let mut text_parts: Vec<(&str, u64)> = Vec::new();
        let mut thinking_parts: Vec<(&str, u64)> = Vec::new();
        for ((block_turn, block_step, index), (kind, buf)) in &self.chunk_blocks {
            if *block_turn != turn || *block_step != step || buf.is_empty() {
                continue;
            }
            match kind.as_str() {
                "text" => text_parts.push((buf, *index)),
                "thinking" | "reasoning" => thinking_parts.push((buf, *index)),
                _ => {} // tool-call and other blocks carry no bubble text
            }
        }
        text_parts.sort_by_key(|(_, index)| *index);
        thinking_parts.sort_by_key(|(_, index)| *index);
        let text: String = text_parts.into_iter().map(|(t, _)| t).collect();
        let thinking: String = thinking_parts.into_iter().map(|(t, _)| t).collect();
        let pos = self.messages.iter().rposition(|m| match m {
            ChatMessage::Assistant {
                done,
                turn: message_turn,
                step: message_step,
                ..
            } => !*done && *message_turn == turn && *message_step == step,
            _ => false,
        });
        if let Some(pos) = pos {
            if let ChatMessage::Assistant {
                text: message_text,
                thinking: message_thinking,
                ..
            } = &mut self.messages[pos]
            {
                if *message_text == text && *message_thinking == thinking {
                    return false;
                }
                *message_text = text;
                *message_thinking = thinking;
                return true;
            }
            false
        } else if !text.is_empty() || !thinking.is_empty() {
            // Guarantee a strictly-increasing seq even if a drain / turn-end
            // flush could still hand us a stale seq: every new message must
            // sort after the tail so the log stays seq-ordered and per-row ids
            // salted by seq can never collide (defense in depth).
            let seq = seq.max(self.messages.last().map(log_message_seq).unwrap_or(0) + 1);
            self.push_message(ChatMessage::Assistant {
                text,
                thinking,
                images: Vec::new(),
                seq,
                turn,
                step,
                done: false,
            });
            true
        } else {
            false
        }
    }

    /// Final assembled assistant message: replaces the partial stream draft
    /// for the same (turn, step), otherwise appends a new message. The
    /// replaced draft also adopts the durable event's real seq — stream
    /// frames carry no seq, so the draft was numbered synthetically above
    /// the durable floor; the settlement is the row's true identity.
    fn apply_assistant_message(
        &mut self,
        data: &Value,
        seq: i64,
        turn: Option<i64>,
        step: Option<i64>,
    ) {
        let (text, thinking) = extract_message_content(data);
        if text.is_empty() && thinking.is_empty() {
            return;
        }
        let pos = self.messages.iter().rposition(|m| match m {
            ChatMessage::Assistant {
                done,
                turn: message_turn,
                step: message_step,
                ..
            } => !*done && *message_turn == turn && *message_step == step,
            _ => false,
        });
        if let Some(pos) = pos {
            // Reseq onto the durable event. The draft sits at the tail and
            // every durable event between its creation and this settlement
            // precedes the settlement on the wire, so the real seq keeps the
            // log seq-ordered.
            let old = self.messages[pos].clone();
            self.deindex_message(&old);
            if let ChatMessage::Assistant {
                text: last_text,
                thinking: last_thinking,
                images: last_images,
                seq: message_seq,
                ..
            } = &mut self.messages[pos]
            {
                *last_text = text;
                *last_thinking = thinking;
                *last_images = extract_images(data);
                *message_seq = seq;
            }
            let new = self.messages[pos].clone();
            self.index_message(&new);
            return;
        }
        // Replay / history-live overlap: the same message event (same seq) can
        // be applied twice. If a non-streaming message with this seq already
        // exists, the draft above was already finalized — drop the duplicate.
        if self.seq_present(seq) {
            return;
        }
        // Monotonic guard, measured against the last DURABLE row: a live
        // synthetic-seq draft at the tail must not inflate a real event's
        // identity into the synthetic range.
        let durable_tail = self
            .messages
            .iter()
            .rev()
            .map(log_message_seq)
            .find(|s| !is_synthetic_seq(*s))
            .unwrap_or(0);
        self.push_message(ChatMessage::Assistant {
            text,
            thinking,
            images: extract_images(data),
            seq: seq.max(durable_tail + 1),
            turn,
            step,
            done: false,
        });
    }

    /// v3 surface replacement: retire every message whose seq falls in the
    /// inclusive `[start_seq, end_seq]` span (the event carrying this
    /// `surfaceOp` supplies the replacement row right after).
    fn retire_surface_span(&mut self, start_seq: i64, end_seq: i64) {
        let mut i = 0;
        while i < self.messages.len() {
            let seq = log_message_seq(&self.messages[i]);
            if seq >= start_seq && seq <= end_seq {
                let message = self.messages.remove(i);
                self.deindex_message(&message);
            } else {
                i += 1;
            }
        }
    }

    /// Rough retained-size estimate of the whole log (sum of all text
    /// payloads; struct overheads and image refs ignored).
    pub fn approx_bytes(&self) -> usize {
        self.messages.iter().map(log_message_bytes).sum()
    }

    /// True if a finished (non-streaming-draft) message with `seq` already
    /// exists — i.e. this seq was applied before. Used to drop replay /
    /// history-live duplicates at the append sites. In-progress streaming
    /// drafts (`done: false`) never count, so their later events still update
    /// them. O(1): backed by the incrementally-maintained [`Self::seq_counts`]
    /// / [`Self::streaming_seq_counts`] maps instead of a full-log scan.
    pub fn seq_present(&self, seq: i64) -> bool {
        let all = self.seq_counts.get(&seq).copied().unwrap_or(0);
        let streaming = self.streaming_seq_counts.get(&seq).copied().unwrap_or(0);
        all > streaming
    }

    /// True while at least one assistant message is still streaming
    /// (`done: false`). O(1) — the per-frame "is this session working"
    /// query used to scan every message of the log each call.
    pub fn has_streaming(&self) -> bool {
        self.streaming_total > 0
    }

    /// True when any message seq occurs more than once in the log (wire
    /// replay / history-live overlap artifacts). The per-frame egui row-id
    /// occurrence ordinals that disambiguate duplicate seqs are only needed
    /// while this is true, so a clean log skips that whole pass.
    pub fn has_duplicate_seqs(&self) -> bool {
        self.dup_seqs > 0
    }

    /// Append a message while keeping the seq / streaming indexes exact.
    /// This is the ONLY append path that leaves [`Self::seq_present`],
    /// [`Self::has_streaming`] and [`Self::has_duplicate_seqs`] truthful —
    /// app-side code (history merges, tests) must go through it rather than
    /// poking `messages` directly.
    pub fn push_message(&mut self, message: ChatMessage) {
        self.index_message(&message);
        self.messages.push(message);
    }

    /// Register one message in the seq / streaming indexes (call before the
    /// actual push).
    fn index_message(&mut self, message: &ChatMessage) {
        let seq = log_message_seq(message);
        let count = self.seq_counts.entry(seq).or_insert(0);
        *count += 1;
        if *count == 2 {
            self.dup_seqs += 1;
        }
        if matches!(message, ChatMessage::Assistant { done: false, .. }) {
            let streaming = self.streaming_seq_counts.entry(seq).or_insert(0);
            *streaming += 1;
            self.streaming_total += 1;
        }
    }

    /// Unregister one message (front-trimmed by the retention window). The
    /// message may be absent from the indexes (raw-constructed logs in tests);
    /// the removal is then a no-op.
    fn deindex_message(&mut self, message: &ChatMessage) {
        let seq = log_message_seq(message);
        if let Some(count) = self.seq_counts.get_mut(&seq) {
            if *count == 2 {
                self.dup_seqs = self.dup_seqs.saturating_sub(1);
            }
            *count -= 1;
            if *count == 0 {
                self.seq_counts.remove(&seq);
            }
        }
        if matches!(message, ChatMessage::Assistant { done: false, .. }) {
            if let Some(streaming) = self.streaming_seq_counts.get_mut(&seq) {
                *streaming -= 1;
                if *streaming == 0 {
                    self.streaming_seq_counts.remove(&seq);
                }
            }
            self.streaming_total = self.streaming_total.saturating_sub(1);
        }
    }

    /// Recompute the seq / streaming indexes from `messages` from scratch.
    /// Bulk operations that rewrite the message list (older-page merges that
    /// sort + dedupe) call this once instead of trying to fold deltas through
    /// dedup — merges are rare, so a linear rebuild is fine there.
    fn rebuild_seq_index(&mut self) {
        self.seq_counts.clear();
        self.streaming_seq_counts.clear();
        self.streaming_total = 0;
        self.dup_seqs = 0;
        for message in &self.messages {
            let seq = log_message_seq(message);
            let count = self.seq_counts.entry(seq).or_insert(0);
            *count += 1;
            if *count == 2 {
                self.dup_seqs += 1;
            }
            if matches!(message, ChatMessage::Assistant { done: false, .. }) {
                let streaming = self.streaming_seq_counts.entry(seq).or_insert(0);
                *streaming += 1;
                self.streaming_total += 1;
            }
        }
    }

    /// Mark every assistant message finished (`done: true`) — history pages
    /// are final transcript state and never stream, so any draft left by
    /// event assembly must be closed. The streaming index resets in one go.
    /// Rebuild the streaming index (and the seq index) after a caller replaced
    /// [`ChatLog::messages`] wholesale.
    pub fn reindex(&mut self) {
        self.rebuild_seq_index();
    }

    pub fn mark_all_done(&mut self) {
        if self.streaming_total > 0 {
            self.streaming_total = 0;
            self.streaming_seq_counts.clear();
        }
        for message in &mut self.messages {
            if let ChatMessage::Assistant { done, .. } = message {
                *done = true;
            }
        }
    }

    /// Absorb an older (lower-seq) page's messages into this log: append,
    /// re-sort by seq, drop duplicate seqs and duplicated user rpcIds, then
    /// rebuild the seq index. Only `older.messages` is consumed — the caller
    /// still merges the page's side tables (`produced`, `turn_times`,
    /// `turn_end`, compaction summaries) from `older`.
    pub fn absorb_history_messages(&mut self, older: &mut ChatLog) {
        self.messages
            .append(&mut std::mem::take(&mut older.messages));
        self.messages.sort_by_key(log_message_seq);
        self.messages.dedup_by_key(|m| log_message_seq(m));
        dedupe_user_rpc(&mut self.messages);
        self.rebuild_seq_index();
    }

    /// This log's side tables, for shipping alongside its rows.
    pub fn side_tables(&self) -> ChatSideTables {
        ChatSideTables {
            produced: self.produced.clone(),
            turn_end: self.turn_end.clone(),
            turn_times: self.turn_times.clone(),
            compaction_summaries: self.compaction_summaries.clone(),
        }
    }

    /// True when this log's side tables equal `tables` (allocation-free).
    ///
    /// The daemon marks its tables dirty on every durable event, but most
    /// events change none of them; comparing against what was last shipped
    /// keeps an update from re-sending the whole table set (produced paths, and
    /// above all the compaction summaries) on every step.
    pub fn side_tables_match(&self, tables: &ChatSideTables) -> bool {
        self.produced == tables.produced
            && self.turn_end == tables.turn_end
            && self.turn_times == tables.turn_times
            && self.compaction_summaries == tables.compaction_summaries
    }

    /// Merge side tables into this log (a union, never a replacement).
    ///
    /// The tables have two producers: the daemon's rows/updates (authoritative,
    /// but only for the window its own log holds) and the older history pages
    /// this side folds in itself ([`Self::absorb_history_messages`]' caller).
    /// Replacing would drop whichever producer the other one did not know, so
    /// produced paths are unioned (deduplicated by path, seq-sorted) and the
    /// per-turn entries take the incoming value — both sides derive them from
    /// the same durable events.
    pub fn merge_side_tables(&mut self, tables: &ChatSideTables) {
        for (turn, entries) in &tables.produced {
            let list = self.produced.entry(*turn).or_default();
            for entry in entries {
                if !list.iter().any(|(_, path)| *path == entry.1) {
                    list.push(entry.clone());
                }
            }
            list.sort_by_key(|(seq, _)| *seq);
        }
        for (turn, reason) in &tables.turn_end {
            self.turn_end.insert(*turn, reason.clone());
        }
        for (turn, times) in &tables.turn_times {
            self.turn_times.insert(*turn, times.clone());
        }
        for (id, summary) in &tables.compaction_summaries {
            self.compaction_summaries.insert(id.clone(), summary.clone());
        }
    }

    /// Sliding-window retention: drop the oldest messages so at most
    /// `max_messages` / `max_bytes` remain. The full history stays on the
    /// server — the app re-pages it in via `history_earliest_seq` when the
    /// user scrolls up — so the in-memory log never grows without bound while
    /// a session streams (the "open over the weekend" freeze).
    ///
    /// The currently-streaming turn is never cut: the earliest message whose
    /// turn contains a `done: false` assistant message (and everything after
    /// it) is kept, so a live response can't be half-dropped.
    ///
    /// Returns the trim outcome (for `history_earliest_seq` bookkeeping), or
    /// `None` when already within the window.
    pub fn trim_to_window(&mut self, max_messages: usize, max_bytes: usize) -> Option<TrimOutcome> {
        let len = self.messages.len();
        if len <= max_messages && self.approx_bytes() <= max_bytes {
            return None;
        }

        // First index that must be kept: the start of the open (streaming)
        // turn, if any. Messages are seq-ordered, so a turn is contiguous.
        let protect_from = self.open_turn_start().unwrap_or(len);

        // Count-driven cut...
        let mut cut = len.saturating_sub(max_messages);
        // ...widened until the retained tail fits the byte cap.
        if cut < protect_from {
            let mut kept_bytes: usize = self.messages[cut..].iter().map(log_message_bytes).sum();
            while cut < protect_from && kept_bytes > max_bytes {
                kept_bytes -= log_message_bytes(&self.messages[cut]);
                cut += 1;
            }
        }
        // Never eat into the streaming turn.
        cut = cut.min(protect_from);
        if cut == 0 {
            return None;
        }

        let removed: Vec<ChatMessage> = self.messages.drain(..cut).collect();
        let new_first_seq = self.messages.first().map(log_message_seq);
        // Keep the seq / streaming indexes in step with the trimmed rows so
        // `seq_present` / `has_streaming` / `has_duplicate_seqs` stay exact.
        for message in &removed {
            self.deindex_message(message);
        }

        // Drop per-turn/per-call bookkeeping that only trimmed rows still
        // reference, so the cap applies to the whole log, not just `messages`.
        let mut turns = std::collections::HashSet::new();
        let mut call_ids = std::collections::HashSet::new();
        let mut rpc_ids = std::collections::HashSet::new();
        for m in &self.messages {
            if let Some(turn) = log_message_turn(m) {
                turns.insert(turn);
            }
            match m {
                ChatMessage::ToolCall { call_id, .. } | ChatMessage::ToolResult { call_id, .. } => {
                    call_ids.insert(call_id.clone());
                }
                ChatMessage::User {
                    rpc_id: Some(id), ..
                } => {
                    rpc_ids.insert(id.clone());
                }
                _ => {}
            }
        }
        self.turn_times.retain(|turn, _| turns.contains(turn));
        self.turn_end.retain(|turn, _| turns.contains(turn));
        self.produced.retain(|turn, _| turns.contains(turn));
        self.call_index.retain(|id, _| call_ids.contains(id));
        self.seen_user_rpc.retain(|id| rpc_ids.contains(id));
        // Trimmed compaction rows keep their summaries harmless; summaries
        // are tiny and the rows reference them only while retained.

        Some(TrimOutcome {
            removed,
            new_first_seq,
        })
    }

    /// Frame-loop drain for throttled streaming deltas: rebuild every
    /// buffered (turn, step) so the visible tail is at most one frame stale
    /// instead of stuck until the next delta arrives. No-op when nothing
    /// was throttled since the last drain, and returns whether any row
    /// actually changed so callers can skip publishing an identical tail.
    pub fn flush_streaming_tails(&mut self) -> bool {
        if !self.stream_flush_pending {
            return false;
        }
        self.stream_flush_pending = false;
        let steps: Vec<(Option<i64>, Option<i64>)> = self
            .chunk_blocks
            .keys()
            .map(|(t, s, _)| (*t, *s))
            .collect();
        let mut changed = false;
        for (t, s) in steps {
            // One step may flush into a message that already exists (in-place
            // update); for steps that still need a NEW message, use that
            // block's OWN last wire seq so two steps never share a seq (which
            // would make their widget ids salted by seq collide).
            let step_seq = self
                .last_seq_by_step
                .get(&(t, s))
                .copied()
                .unwrap_or(self.last_stream_seq);
            changed |= self.flush_assistant_step(step_seq, t, s);
        }
        self.last_stream_flush_ms = now_millis();
        changed
    }

    /// Index of the first message of the currently-streaming turn (the turn
    /// of the last `done: false` assistant message), or `None` when nothing
    /// is streaming.
    fn open_turn_start(&self) -> Option<usize> {
        let open = self.messages.iter().rposition(|m| {
            matches!(m, ChatMessage::Assistant { done: false, .. })
        })?;
        match log_message_turn(&self.messages[open]) {
            Some(turn) => {
                let mut start = open;
                while start > 0 && log_message_turn(&self.messages[start - 1]) == Some(turn) {
                    start -= 1;
                }
                Some(start)
            }
            None => Some(open),
        }
    }
}

/// Outcome of [`ChatLog::trim_to_window`]: how many rows were dropped from
/// the front and the seq of the new oldest retained message (the cursor the
/// "load earlier" paging continues from).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrimOutcome {
    /// Messages removed from the front, kept for callers that maintain
    /// seq-keyed side tables (row heights, unread state).
    pub removed: Vec<ChatMessage>,
    /// Seq of the first retained message; `None` only when the log is empty.
    pub new_first_seq: Option<i64>,
}

/// The log seq of a chat message (every variant carries one).
/// The `seq` a row carries (0 when it has none).
pub fn log_message_seq(message: &ChatMessage) -> i64 {
    match message {
        ChatMessage::User { seq, .. }
        | ChatMessage::Assistant { seq, .. }
        | ChatMessage::Context { seq, .. }
        | ChatMessage::SystemPrompt { seq, .. }
        | ChatMessage::ToolCall { seq, .. }
        | ChatMessage::ToolResult { seq, .. }
        | ChatMessage::Retry { seq, .. } => *seq,
    }
}

/// The turn a chat message belongs to (all variants carry one).
/// The turn a row belongs to, when it has one.
pub fn log_message_turn(message: &ChatMessage) -> Option<i64> {
    match message {
        ChatMessage::User { turn, .. }
        | ChatMessage::Assistant { turn, .. }
        | ChatMessage::Context { turn, .. }
        | ChatMessage::SystemPrompt { turn, .. }
        | ChatMessage::ToolCall { turn, .. }
        | ChatMessage::ToolResult { turn, .. }
        | ChatMessage::Retry { turn, .. } => *turn,
    }
}

/// Rough payload size of one message, for the retained-bytes cap.
/// A row's approximate in-memory size, for the trim budget.
pub fn log_message_bytes(message: &ChatMessage) -> usize {
    match message {
        ChatMessage::User { text, .. } => text.len(),
        ChatMessage::Assistant {
            text, thinking, ..
        } => text.len() + thinking.len(),
        ChatMessage::ToolCall { name, args, .. } => name.len() + args.len(),
        ChatMessage::ToolResult { output, .. } => output.len(),
        ChatMessage::Context { text, .. } => text.len(),
        ChatMessage::SystemPrompt { text, .. } => text.len(),
        ChatMessage::Retry { message, .. } => message.len(),
    }
}

/// Drop `user/message` duplicates that share the same `source.rpcId` (the
/// vision relay resends the prompt with its analysis; the web shows only the
/// first occurrence). Applied when an older history page is absorbed.
fn dedupe_user_rpc(messages: &mut Vec<ChatMessage>) {
    let mut seen = std::collections::HashSet::new();
    messages.retain(|m| match m {
        ChatMessage::User {
            rpc_id: Some(id), ..
        } => seen.insert(id.clone()),
        _ => true,
    });
}

/// True when a `user/message` payload is host-injected context rather than a
/// real human message: `source.kind` other than `"user"`, or `baseline: true`.
pub fn is_injected_context(data: &Value) -> bool {
    let Some(source) = data.get("source") else {
        return false;
    };
    if source.get("baseline").and_then(Value::as_bool).unwrap_or(false) {
        return true;
    }
    if let Some(kind) = source.get("kind").and_then(Value::as_str) {
        return kind != "user";
    }
    false
}

/// Context injection provenance: role and human-facing producer label read
/// from the durable `source` map (web `contextProvenance` parity).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextProvenance {
    pub role: String,
    pub label: Option<String>,
}

pub fn context_provenance(source: Option<&Value>) -> ContextProvenance {
    let source = match source {
        Some(Value::Object(m)) => m,
        _ => return ContextProvenance { role: "inject".to_string(), label: None },
    };
    let kind = source.get("kind").and_then(Value::as_str);
    let kind = match kind {
        Some(k) => k,
        None => return ContextProvenance { role: "inject".to_string(), label: None },
    };
    let label = match kind {
        "session-reference" => {
            // Collect distinct non-empty reference labels, if any.
            let labels: Vec<String> = source
                .get("references")
                .and_then(Value::as_array)
                .map(|arr| {
                    let mut seen = std::collections::HashSet::new();
                    let mut out = Vec::new();
                    for item in arr {
                        if let Some(label) = item.get("label").and_then(Value::as_str) {
                            if !label.is_empty() && seen.insert(label.to_string()) {
                                out.push(label.to_string());
                            }
                        }
                    }
                    out
                })
                .unwrap_or_default();
            if labels.is_empty() { Some(kind.to_string()) } else { Some(labels.join(", ")) }
        }
        "agent-instructions" => {
            let paths: Vec<String> = source
                .get("changes")
                .and_then(Value::as_array)
                .map(|arr| {
                    let mut seen = std::collections::HashSet::new();
                    let mut out = Vec::new();
                    for item in arr {
                        if let Some(path) = item.get("path").and_then(Value::as_str) {
                            if !path.is_empty() && seen.insert(path.to_string()) {
                                out.push(path.to_string());
                            }
                        }
                    }
                    out
                })
                .unwrap_or_default();
            if paths.is_empty() { Some(kind.to_string()) } else { Some(paths.join(", ")) }
        }
        "plugin" => source
            .get("plugin")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| Some(kind.to_string())),
        "skill-invocation" => source
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| Some(kind.to_string())),
        _ => Some(kind.to_string()),
    };
    let role = if kind == "session-reference" { "recall" } else { "inject" };
    ContextProvenance { role: role.to_string(), label }
}

/// Producer-declared information form (web `contextForm` parity). Unknown or
/// absent forms return `None` and render as opaque text.
const KNOWN_CONTEXT_FORMS: &[&str] = &["instructions", "catalog", "snapshot", "notice", "relay", "recall"];

pub fn context_form(source: Option<&Value>) -> Option<String> {
    let source = source?.as_object()?;
    let form = source.get("form").and_then(Value::as_str)?;
    if KNOWN_CONTEXT_FORMS.contains(&form) {
        Some(form.to_string())
    } else {
        None
    }
}

/// Collect durable image references from `content` / `message.content`
/// blocks of type `image` (`attachment.attachmentId/mediaType/width/height`).
pub fn extract_images(data: &Value) -> Vec<ImageRef> {
    let content = data
        .get("content")
        .or_else(|| data.get("message").and_then(|m| m.get("content")))
        .and_then(Value::as_array);
    let Some(blocks) = content else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("image") {
                return None;
            }
            let attachment = block.get("attachment")?;
            let attachment_id = attachment.get("attachmentId")?.as_str()?.to_string();
            let media_type = attachment
                .get("mediaType")
                .and_then(Value::as_str)
                .unwrap_or("image/png")
                .to_string();
            let width = attachment.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
            let height = attachment.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
            Some(ImageRef {
                attachment_id,
                media_type,
                width,
                height,
            })
        })
        .collect()
}

/// Display text of a user/assistant message payload: `data.text` or the text
/// blocks of `content` / `message.content`.
pub fn extract_text(data: &Value) -> String {
    if let Some(text) = data.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    extract_message_content(data).0
}

/// Split an assistant payload into (text, thinking) from content blocks of
/// types `text` and `thinking`/`reasoning`.
pub fn extract_message_content(data: &Value) -> (String, String) {
    let content = data
        .get("content")
        .or_else(|| data.get("message").and_then(|m| m.get("content")))
        .and_then(Value::as_array);
    if let Some(blocks) = content {
        let mut text = Vec::new();
        let mut thinking = Vec::new();
        for block in blocks {
            let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
            match (kind, block.get("text").and_then(Value::as_str)) {
                ("text", Some(t)) => text.push(t),
                ("thinking" | "reasoning", Some(t)) => thinking.push(t),
                _ => {}
            }
        }
        if !text.is_empty() || !thinking.is_empty() {
            return (text.join("\n"), thinking.join("\n"));
        }
    }
    if let Some(t) = data.get("text").and_then(Value::as_str) {
        return (t.to_string(), String::new());
    }
    (String::new(), String::new())
}

/// Parse a `tool/call` event payload (`{callId, name, arguments}`).
pub fn parse_tool_call(data: &Value) -> Option<ToolCallInfo> {
    let name = data.get("name")?.as_str()?.to_string();
    let call_id = data.get("callId")?.as_str()?.to_string();
    let args = match data.get("arguments") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(_)) => serde_json::to_string(data.get("arguments")?).ok()?,
        _ => String::new(),
    };
    Some(ToolCallInfo {
        name,
        args,
        call_id,
    })
}

/// Parse a `tool/result` event payload. The call id lives at
/// `message.callId` / `message.source.callId` / `data.callId`; output is the
/// text blocks of `message.content` (or `data.output`); `isError` flips ok.
pub fn parse_tool_result(data: &Value) -> Option<ToolResultInfo> {
    let message = data.get("message");
    let call_id = message
        .and_then(|m| m.get("callId"))
        .or_else(|| {
            message
                .and_then(|m| m.get("source"))
                .and_then(|s| s.get("callId"))
        })
        .or_else(|| data.get("callId"))?
        .as_str()?
        .to_string();
    // `isError` may sit on the message or on nested `tool-result` content
    // blocks (the common aborted/error shape).
    let is_error = message
        .and_then(|m| m.get("isError"))
        .and_then(Value::as_bool)
        .or_else(|| {
            message
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
                .and_then(|blocks| {
                    blocks.iter().find_map(|b| {
                        b.get("isError")
                            .and_then(Value::as_bool)
                            .or_else(|| {
                                b.get("tool-result")
                                    .and_then(|r| r.get("isError"))
                                    .and_then(Value::as_bool)
                            })
                    })
                })
        })
        .unwrap_or(false);
    let output = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| {
                    b.get("text")
                        .and_then(Value::as_str)
                        .map(String::from)
                        .or_else(|| {
                            // `tool-result` blocks carry their text nested in
                            // `content[]`.
                            b.get("content").and_then(Value::as_array).map(|inner| {
                                inner
                                    .iter()
                                    .filter_map(|c| c.get("text").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                        })
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|s| !s.is_empty())
        .or_else(|| data.get("output").and_then(Value::as_str).map(String::from))
        .unwrap_or_default();
    Some(ToolResultInfo {
        call_id,
        output,
        ok: !is_error,
    })
}

/// Extract the target path of a supported first-party mutation call from its
/// raw JSON arguments (port of web `mutationPath` in
/// `ui-deliverables/src/client/turn-deliverables.ts`, protocol 0.1.5 — the
/// old host-computed `view` cards are gone, so the "Files changed" fold
/// reads the call args directly). `write` needs a string `content`, `edit`
/// needs a non-empty `old_string` differing from `new_string`, and
/// `str_replace_editor` only counts its `create`/`str_replace`/`insert`
/// commands. Everything else (reads, deletes, malformed args) yields `None`.
pub fn mutation_path(name: &str, args_raw: &str) -> Option<String> {
    let args: Value = serde_json::from_str(args_raw).ok()?;
    let obj = args.as_object()?;
    let str_field = |key: &str| obj.get(key).and_then(Value::as_str);
    let path_value = |v: Option<&str>| v.map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    match name {
        "write" => {
            if str_field("content").is_some() {
                path_value(str_field("file_path"))
            } else {
                None
            }
        }
        "edit" => {
            let old = str_field("old_string");
            let new = str_field("new_string");
            let valid = matches!(old, Some(o) if !o.is_empty())
                && new.is_some()
                && old != new
                && obj
                    .get("replace_all")
                    .map(|v| v.is_boolean())
                    .unwrap_or(true);
            if valid {
                path_value(str_field("file_path"))
            } else {
                None
            }
        }
        "str_replace_editor" => {
            let path = path_value(str_field("path"))?;
            match str_field("command") {
                Some("create") => str_field("file_text").map(|_| path),
                Some("str_replace") => match str_field("old_str") {
                    Some(old) if !old.is_empty() => {
                        let new_ok = match obj.get("new_str") {
                            None => true,
                            Some(Value::String(_)) => true,
                            _ => false,
                        };
                        new_ok.then_some(path)
                    }
                    _ => None,
                },
                Some("insert") => {
                    let line_ok = obj
                        .get("insert_line")
                        .and_then(Value::as_i64)
                        .map(|l| l >= 0)
                        .unwrap_or(false);
                    if line_ok && str_field("new_str").is_some() {
                        Some(path)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Insert zero-width spaces into unbroken runs longer than `limit` so long
/// URLs / code tokens wrap inside chat bubbles.
pub fn insert_soft_breaks(input: &str, limit: usize) -> String {
    let mut out = String::with_capacity(input.len() + input.len() / limit + 1);
    let mut run = 0usize;
    for ch in input.chars() {
        if ch.is_whitespace() {
            run = 0;
        } else {
            run += 1;
            if run > limit {
                out.push('\u{200B}');
                run = 1;
            }
        }
        out.push(ch);
    }
    out
}

/// Per-session token meter from the `tokenUsage` projection.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    pub uncached_input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

impl TokenUsage {
    pub fn from_value(value: &Value) -> Self {
        Self {
            uncached_input: value["uncachedInputTokens"].as_i64().unwrap_or(0),
            output: value["outputTokens"].as_i64().unwrap_or(0),
            cache_read: value["cacheReadTokens"].as_i64().unwrap_or(0),
            cache_write: value["cacheWriteTokens"].as_i64().unwrap_or(0),
        }
    }
}

/// Whole-log conversation figures from the `sessionStats` projection
/// (mirrors `SessionStatsProjection` in dsh-session-stats).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionStats {
    pub turns: i64,
    pub steps: i64,
    pub llm_ms: i64,
    pub tool_ms: i64,
    pub ttft_ms: i64,
    pub ttft_steps: i64,
    pub decode_ms: i64,
    pub decode_tokens: i64,
}

impl SessionStats {
    pub fn from_value(value: &Value) -> Self {
        Self {
            turns: value["turns"].as_i64().unwrap_or(0),
            steps: value["steps"].as_i64().unwrap_or(0),
            llm_ms: value["llmMs"].as_i64().unwrap_or(0),
            tool_ms: value["toolMs"].as_i64().unwrap_or(0),
            ttft_ms: value["ttftMs"].as_i64().unwrap_or(0),
            ttft_steps: value["ttftSteps"].as_i64().unwrap_or(0),
            decode_ms: value["decodeMs"].as_i64().unwrap_or(0),
            decode_tokens: value["decodeTokens"].as_i64().unwrap_or(0),
        }
    }
}

/// Permission preset state from the `permissions` projection.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PermissionState {
    pub current: String,
    pub options: Vec<String>,
}

impl PermissionState {
    pub fn from_value(value: &Value) -> Self {
        let current = value["currentValue"].as_str().unwrap_or("").to_string();
        let options = value["options"]
            .as_array()
            .map(|options| {
                options
                    .iter()
                    .filter_map(|option| {
                        let value = option["value"].as_str().unwrap_or("");
                        if value == "custom" {
                            None
                        } else {
                            Some(value.to_string())
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { current, options }
    }
}

/// Compact token count (web `formatTokens`): 517 / 12.2K / 517K / 1.2M.
pub fn format_tokens(n: i64) -> String {
    let scaled = |v: f64| -> String {
        if v >= 100.0 {
            format!("{}", v.round() as i64)
        } else {
            format!("{}", (v * 10.0).round() / 10.0)
        }
    };
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{}K", scaled(n as f64 / 1_000.0))
    } else {
        format!("{}M", scaled(n as f64 / 1_000_000.0))
    }
}

/// Compact duration (web `formatDuration`): 45.2s under a minute, else 2m42s.
pub fn format_duration(ms: i64) -> String {
    let seconds = ms as f64 / 1_000.0;
    if seconds < 60.0 {
        format!("{}s", (seconds * 10.0).round() / 10.0)
    } else {
        let whole = seconds.round() as i64;
        format!("{}m{}s", whole / 60, whole % 60)
    }
}

/// Tokens per second (web `formatTokensPerSecond`): >= 10 rounds, else 1 dp.
pub fn format_tokens_per_second(tps: f64) -> String {
    let clamped = tps.max(0.0);
    if clamped >= 10.0 {
        format!("{}", clamped.round() as i64)
    } else {
        format!("{}", (clamped * 10.0).round() / 10.0)
    }
}

/// Sum the three disjoint prompt-side billing buckets (web `billedInputTokens`).
pub fn billed_input_tokens(usage: &TokenUsage) -> i64 {
    usage.uncached_input + usage.cache_read + usage.cache_write
}

/// Cache-hit share of prompt-side input (web `cacheHitPercent`).
pub fn cache_hit_percent(usage: &TokenUsage) -> Option<i64> {
    let denominator = billed_input_tokens(usage);
    if denominator == 0 {
        None
    } else {
        Some(((usage.cache_read as f64 / denominator as f64) * 100.0).round() as i64)
    }
}

/// One session's conversation state, as the daemon hands it to a frontend.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ChatSnapshotWire {
    /// The follow key this state belongs to (`session:<id>` / `subagent:…`).
    pub key: String,
    /// Every row, oldest first.
    pub rows: Vec<ChatMessage>,
    /// The session's projections (`todos`, `goal`, `stats`, …), by key.
    pub projections: std::collections::HashMap<String, Value>,
    /// Per-turn state the rows cannot carry (produced files, turn-end reasons,
    /// turn clocks, compaction summaries). Absent from a daemon that predates
    /// the field — an empty set simply merges nothing.
    #[serde(default, skip_serializing_if = "ChatSideTables::is_empty")]
    pub side_tables: ChatSideTables,
    /// The authoritative queued/steering inbox.
    pub queue: Vec<QueuedItem>,
    /// Cursor of the snapshot this state was built from (the `session/page`
    /// cut a frontend pages older history back from).
    pub cursor: i64,
    /// Whether the host has more (older) history than the snapshot carries.
    pub has_more: bool,
}

/// One incremental conversation update: the frontend replaces its rows from
/// `from_seq` onwards (or all of them, when `from_seq` is `None`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ChatUpdateWire {
    pub key: String,
    /// Lowest row seq that changed; `None` means "the whole log" (a rebuild,
    /// e.g. after a follow snapshot or a trim).
    pub from_seq: Option<i64>,
    /// Rows from `from_seq` onwards (or all of them).
    pub rows: Vec<ChatMessage>,
    /// Projections that changed in this update.
    pub projections: std::collections::HashMap<String, Value>,
    /// Per-turn side tables that changed with these rows (see
    /// [`ChatSideTables`]); empty when only rows/projections moved.
    #[serde(default, skip_serializing_if = "ChatSideTables::is_empty")]
    pub side_tables: ChatSideTables,
    /// The queue, when it changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<Vec<QueuedItem>>,
}
