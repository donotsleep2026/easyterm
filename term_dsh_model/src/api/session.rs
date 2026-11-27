//! Shared session data for daemon-delivered DSH state.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Session id brand (opaque string on the wire).
pub type SessionId = String;

/// Projection baseline riding **`session/list` rows**, follow snapshots and
/// control baselines: a watermark plus the current value per projection key
/// (title, list metadata, ...).
///
/// On **list rows** the block also carries `kind`: `sequenced` means the live
/// registry produced it (its `as_of_seq` is comparable with control frames and
/// baselines of the same connection); `cached` means a header-only listing
/// read it from the persisted cache (its `as_of_seq` is that record's own
/// watermark and must NOT be compared with a connected session's seqs — a
/// cached row may only fill keys no sequenced row holds). Follow-snapshot and
/// control-baseline blocks are `SessionProjectionBaseline` and have no `kind`,
/// hence the `Option`.
///
/// This block is **not** what `session/projections` answers since 0.2.1: that
/// RPC returns the tagged [`SessionProjectionsValue`] union.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProjectionsBlock {
    /// Seq of the last event the values reflect; -1 for an empty log.
    #[serde(rename = "asOfSeq")]
    pub as_of_seq: i64,
    /// Whole current value per registered projection key.
    pub values: std::collections::HashMap<String, Value>,
    /// List-row hint provenance (`cached` | `sequenced`); absent on
    /// follow/control baselines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// `session/projections` answer (0.2.1 `SessionProjectionsValue`): a tagged
/// union instead of the bare baseline block. `null` (the RPC's third arm) maps
/// to `None` at the Rust boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SessionProjectionsValue {
    /// Exact read of a live or migratable Session: the values plus the event
    /// cut they reflect (comparable with control frames of the same
    /// connection).
    Sequenced {
        /// Seq of the last event the values reflect; -1 for an empty log.
        #[serde(rename = "asOfSeq")]
        as_of_seq: i64,
        /// Whole current value per registered projection key.
        values: std::collections::HashMap<String, Value>,
    },
    /// The stored log must be opened once (migration) before exact projections
    /// exist; the host answers with sequence-free cache hints. The values are
    /// best-effort and their seq must not be compared with a live session's.
    MigrationRequired {
        /// Cached projection values, when the projection cache holds any.
        values: std::collections::HashMap<String, Value>,
    },
}

impl SessionProjectionsValue {
    /// The projection values, whichever kind the host answered with.
    pub fn values(&self) -> &std::collections::HashMap<String, Value> {
        match self {
            SessionProjectionsValue::Sequenced { values, .. } => values,
            SessionProjectionsValue::MigrationRequired { values } => values,
        }
    }

    /// The event cut the values reflect, or `None` for cached migration hints.
    pub fn as_of_seq(&self) -> Option<i64> {
        match self {
            SessionProjectionsValue::Sequenced { as_of_seq, .. } => Some(*as_of_seq),
            SessionProjectionsValue::MigrationRequired { .. } => None,
        }
    }

    /// Whether exact projections wait for the Session's log migration.
    pub fn is_migration_required(&self) -> bool {
        matches!(self, SessionProjectionsValue::MigrationRequired { .. })
    }
}

/// One row of `session/list`. Note: `agentPreset` is no longer a summary
/// field at this tag — it moved to the `agentPreset` projection key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    #[serde(rename = "sessionId")]
    pub session_id: SessionId,
    /// Later of creation and latest human-authored prompt (epoch millis).
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
    /// Status of the attached agent; always false for cold sessions.
    pub running: bool,
    /// Whether this session currently owns a live Agent (0.1.7). Independent
    /// of `running`: a live-but-idle agent is available but not running.
    #[serde(rename = "agentAvailable", default)]
    pub agent_available: bool,
    /// True while no turn has run; clients hide blank sessions from lists.
    pub blank: bool,
    /// fork/spawn lineage; absent for root sessions.
    #[serde(rename = "parentSessionId", default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
    /// Projection hints for this row (carry the `title` value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projections: Option<SessionProjectionsBlock>,
    /// Session origin: 'subagent' for child subagent sessions, otherwise absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// The session's working directory (authoritative root for resolving
    /// relative file paths).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Persistence-format classification added at 0.2.1: `current` for an
    /// attached session, `migration-required` when the stored log must be
    /// opened once before exact projections (or a fork) are available.
    /// Absent when the host could not classify the row.
    #[serde(rename = "formatStatus", default, skip_serializing_if = "Option::is_none")]
    pub format_status: Option<String>,
}

/// One row of `session/search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchItem {
    #[serde(rename = "sessionId")]
    pub session_id: SessionId,
    pub snippet: String,
}

/// `session/search` result (at most 20 rows; snippets ≤240 code points).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchResult {
    pub items: Vec<SessionSearchItem>,
    #[serde(rename = "hasMore")]
    pub has_more: bool,
}

/// `session/create` request; every field optional (a blank session is created).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionCreateRequest {
    #[serde(rename = "workspaceId", skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Optional preallocated session id.
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(rename = "agentPreset", skip_serializing_if = "Option::is_none")]
    pub agent_preset: Option<String>,
}

/// `session/create` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreated {
    #[serde(rename = "sessionId")]
    pub session_id: SessionId,
    #[serde(rename = "agentPreset", default, skip_serializing_if = "Option::is_none")]
    pub agent_preset: Option<String>,
}

/// Durable identity selecting an ordinary session or one direct subagent
/// child; the sole address vocabulary of `session/page` and `session/follow`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SessionAddress {
    Session {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    Subagent {
        #[serde(rename = "parentSessionId")]
        parent_session_id: String,
        #[serde(rename = "childSessionId")]
        child_session_id: String,
        mode: SubagentMode,
    },
}

impl SessionAddress {
    /// Address one ordinary session.
    pub fn session(session_id: impl Into<String>) -> Self {
        SessionAddress::Session {
            session_id: session_id.into(),
        }
    }

    /// Address one direct subagent child through its parent.
    pub fn subagent(
        parent_session_id: impl Into<String>,
        child_session_id: impl Into<String>,
        mode: SubagentMode,
    ) -> Self {
        SessionAddress::Subagent {
            parent_session_id: parent_session_id.into(),
            child_session_id: child_session_id.into(),
            mode,
        }
    }
}

/// Subagent lifetime mode, part of the durable subagent address.
/// `Unknown` is the 0.1.7 "host resolves it" discriminator: an address
/// carrying it skips the host's mode-match fence (history.ts
/// `address.mode !== 'unknown'`), which is what lets clients follow/page a
/// child before (or without) resolving its catalog mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubagentMode {
    #[serde(rename = "one-shot")]
    OneShot,
    #[serde(rename = "continuable")]
    Continuable,
    #[serde(rename = "unknown")]
    Unknown,
}

/// One history-page / follow-snapshot record. At this tag the wire wraps
/// every event in an entry object `{type:'event', event}`; the old
/// host-computed `view` companion is gone from the wire (render intent is
/// derived client-side from `event.data.meta` and `deliverables/presented`
/// events).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Always `"event"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub event: SessionEventView,
}

/// One history page of `session/page`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionPage {
    pub records: Vec<HistoryEntry>,
    #[serde(rename = "hasMore")]
    pub has_more: bool,
}

/// A session-log event (`SessionWireEvent`): `kind` is the event type
/// (`user/message`, `assistant/message`, `system/message`, ...), `data` the
/// event payload. The v3 envelope fields are kept opaque — the app decides
/// how to apply them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEventView {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub seq: Option<i64>,
    /// Wall-clock time of the event (epoch millis), when the host provides it.
    #[serde(default)]
    pub time: Option<i64>,
    pub data: Value,
    /// Canonical surface placement: `"append"` or
    /// `{op:'replace', startSeq, endSeq}`. Required on surface events
    /// (`system/message`, `user/message`, `assistant/message`, `tool/result`),
    /// absent elsewhere.
    #[serde(rename = "surfaceOp", default, skip_serializing_if = "Option::is_none")]
    pub surface_op: Option<Value>,
    /// Earlier source-event seqs cited by a surface event (compaction
    /// replacement sets); never present on `assistant/message`.
    #[serde(rename = "sourceEventSeqs", default, skip_serializing_if = "Option::is_none")]
    pub source_event_seqs: Option<Value>,
    /// Marks an event a reader may safely skip when it does not recognize
    /// `kind`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignorable: Option<bool>,
}

/// One prompt content part. `File` parts carry the opaque receipt returned by
/// a preceding upload on the same session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PromptContentPart {
    Text {
        text: String,
    },
    Image {
        #[serde(rename = "mediaType")]
        media_type: String,
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    File {
        #[serde(rename = "receiptId")]
        receipt_id: String,
    },
}

/// Prompt delivery mode: `queue` sends, `steer` steers the active turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptMode {
    Queue,
    Steer,
}

/// One `session/updateQueue` mutation verb (`kind` discriminator on the wire).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum QueueAction {
    /// Replace the pending occurrence's content with text blocks.
    Edit {
        content: Vec<PromptContentPart>,
    },
    /// Drop the pending occurrence from the inbox.
    Remove,
    /// Strictly steer this occurrence into the running turn (FIFO among peers).
    Steer,
}

/// `session/prompt` acceptance (`{accepted: true}`); the answer stream arrives
/// on the session/follow stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptAccepted {
    pub accepted: bool,
}

/// `session/rename` result: normalized title plus its event seq.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRenamed {
    pub title: String,
    pub seq: i64,
}

/// `session/attachment` result: the durable ref plus base64 image bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttachment {
    #[serde(default)]
    pub attachment: Value,
    /// Base64-encoded image bytes.
    pub data: String,
}

/// Complete model selection for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSelection {
    /// Registered provider route.
    pub provider: String,
    /// Provider-owned model id.
    pub model: String,
    /// Adapter-owned reasoning effort; absence preserves provider defaults.
    #[serde(
        rename = "reasoningEffort",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub reasoning_effort: Option<String>,
}

/// One provider and the models it advertised successfully.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProviderGroup {
    /// Provider route id used for requests.
    pub id: String,
    /// Provider display name.
    pub name: String,
    /// Models in provider-preferred order.
    pub models: Vec<ModelCatalogModel>,
}

/// One model displayed inside its provider group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogModel {
    /// Provider-owned model id.
    pub id: String,
    /// Provider-supplied display name.
    pub name: String,
    /// Optional user-facing distinction from otherwise similar models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Exact-route reasoning metadata when the adapter exposes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ModelReasoning>,
}

/// One adapter-owned reasoning effort displayed for an exact model route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelReasoningEffort {
    /// Opaque value submitted back to the owning adapter.
    pub id: String,
    /// Adapter-supplied display name.
    pub name: String,
    /// Optional adapter-supplied description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Selectable reasoning metadata for one exact model route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelReasoning {
    /// Efforts in adapter-preferred display order.
    pub efforts: Vec<ModelReasoningEffort>,
    /// Adapter-configured default; absence preserves the provider default.
    #[serde(rename = "defaultEffort", default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

/// One provider whose model catalog lookup failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogFailure {
    pub id: String,
    pub name: String,
    pub message: String,
}

/// `session/modelCatalog` snapshot: host-generation model catalog plus the
/// default used by unconfigured sessions. Replaces the old per-session
/// `session.models` RPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalog {
    /// Deployment default selection.
    pub default: ModelSelection,
    /// Provider routes currently able to serve a request (may have empty catalogs).
    #[serde(rename = "routableProviders")]
    pub routable_providers: Vec<String>,
    /// Successfully loaded non-empty provider groups.
    pub groups: Vec<ModelProviderGroup>,
    /// Provider-local catalog failures; successful groups remain usable.
    pub failures: Vec<ModelCatalogFailure>,
}
