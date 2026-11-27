//! Daemon-delivered DSH event/frame data and pure decoding helpers.

use crate::api::session::{HistoryEntry, SessionAddress, SessionEventView, SubagentMode};
use crate::api::workspace::WorkspaceView;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Everything the hub delivers, on one channel.
///
/// `Serialize`/`Deserialize` exist so a frontend can receive hub events the
/// daemon decoded (the daemon owns the connection and the streams; see
/// `term_core_server::dsh_hub`), instead of every frontend running its own
/// remote.mux socket and decoders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HubEvent {
    /// `$events` stream ready frame: this connection generation is live.
    /// `client_id` binds later `event_result` answers to this generation.
    Ready { client_id: String, home: String },
    /// `$events` `emit` frame: an allowlisted one-way Cordis event
    /// (`api-session/added|removed|status|activity|error`,
    /// `agent-preset/selected`, `goal/activation-changed`,
    /// `llm/adapters-updated`, `settings/document-updated`, `commands/change`,
    /// `credentials/reference-updated`,
    /// `permission-presets/catalog-changed`, `plugin-manager/changed`,
    /// `plugin-manager/install-log`, `plugin-manager/install-state`,
    /// `cordis/*`).
    Emit { event: String, args: Vec<Value> },
    /// `$events` answerable waterfall (`approval/request`,
    /// `user-questions/request`). Answer with
    /// the daemon's waterfall-answer endpoint.
    Waterfall {
        event: String,
        event_id: String,
        agent_id: String,
        request: Value,
    },
    /// `$events` cancel for a pending waterfall.
    WaterfallCancel { event_id: String },
    /// One `session/control` frame.
    Control(ControlFrame),
    /// One `workspace/follow` frame.
    Workspace(WorkspaceFrame),
    /// `session/follow` opening snapshot (per followed address key).
    FollowSnapshot { key: String, snapshot: FollowSnapshot },
    /// `session/follow` live event.
    FollowEvent { key: String, event: SessionEventView },
    /// `session/follow` assistant-stream frame (only when the follow was
    /// opened with `assistant_stream: true`).
    FollowAssistantStream { key: String, frame: Value },
    /// A `session/follow` stream ended or errored.
    FollowEnded { key: String, reason: String },
    /// The whole hub died (base stream ended/errored, or the socket died);
    /// the app rebuilds the hub.
    HubEnded(String),
}

/// `session/control` stream frame
/// (`{type:'baseline', value}` | `{type:'projection', …}` — exact wire
/// fields). The 0.1.7 control stream carries projections only: queued inbox
/// rows ride the `inbox` projection key and background jobs moved to the
/// separate `job/*` namespace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ControlFrame {
    /// Complete live-control baseline, exactly once per stream generation.
    Baseline { value: ControlBaseline },
    /// One projection unit changed; clients keep higher-`seq`-wins per key.
    Projection {
        #[serde(rename = "sessionId")]
        session_id: String,
        key: String,
        value: Value,
        seq: i64,
    },
}

/// The `value` of a `session/control` baseline frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlBaseline {
    /// `Record<SessionId, {asOfSeq, values}>`.
    pub projections: Value,
}

/// `workspace/follow` stream frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum WorkspaceFrame {
    /// Complete workspace registry snapshot, exactly once per generation.
    Baseline { value: WorkspaceBaseline },
    /// One workspace was created or mutated; the full row rides along.
    Upsert { workspace: WorkspaceView },
    /// A workspace registration was deleted (files and sessions stay).
    Remove {
        #[serde(rename = "workspaceId")]
        workspace_id: String,
    },
    /// The complete durable workspace registry order after a reorder.
    Order {
        #[serde(rename = "workspaceIds")]
        workspace_ids: Vec<String>,
    },
    /// The full archive-set snapshot after every durable change.
    Archived {
        #[serde(rename = "archivedSessionIds")]
        archived_session_ids: Vec<String>,
    },
    /// The full pin-set snapshot after every durable change (0.1.7;
    /// most-recently-pinned first).
    Pinned {
        #[serde(rename = "pinnedSessionIds")]
        pinned_session_ids: Vec<String>,
    },
    /// 0.2.1: the complete state that supersedes everything delivered earlier
    /// in this generation. Sent instead of `remove` when a non-owner's access
    /// to a delivered Workspace was revoked (a Client never resurrects a
    /// removed row), so it must be applied exactly like a `baseline`.
    Replace { value: WorkspaceBaseline },
}

/// The `value` of a `workspace/follow` baseline frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceBaseline {
    pub items: Vec<WorkspaceView>,
    #[serde(rename = "archivedSessionIds")]
    pub archived_session_ids: Vec<String>,
    /// Registry-global pin set (0.1.7), most-recently-pinned first.
    #[serde(default, rename = "pinnedSessionIds")]
    pub pinned_session_ids: Vec<String>,
}

/// `session/follow` opening snapshot (`{type:'snapshot', …}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowSnapshot {
    /// `SessionWireHeader` (`{version, id, createdAt, cwd?, parentSession?,
    /// isSeeded, origin?, delegationDepth?, agentPreset?}`).
    pub header: Value,
    /// Seq of the last event included in `records`; resume watermark.
    pub cursor: i64,
    /// Opening history page (`{type:'event', event}` entries).
    #[serde(default)]
    pub records: Vec<HistoryEntry>,
    pub has_more: bool,
    /// Whole projection map baseline (`{asOfSeq, values}`); always present on
    /// a follow snapshot, kept optional for decode resilience.
    #[serde(default)]
    pub projections: Option<Value>,
    /// Live assistant-stream baseline, present only when the follow was
    /// opened with `assistantStream: true`.
    #[serde(default)]
    pub assistant_stream: Option<Value>,
}

/// Translate one `$events` stream item into a hub event; `None` for unknown
/// frame types (forward-compatible).
pub fn hub_event_from_events_item(value: &Value) -> Option<HubEvent> {
    match value.get("type")?.as_str()? {
        "ready" => Some(HubEvent::Ready {
            client_id: value.get("clientId")?.as_str()?.to_string(),
            home: value
                .get("host")?
                .get("home")?
                .as_str()?
                .to_string(),
        }),
        "emit" => Some(HubEvent::Emit {
            event: value.get("event")?.as_str()?.to_string(),
            args: serde_json::from_value(value.get("args").cloned()?).ok()?,
        }),
        "waterfall" => Some(HubEvent::Waterfall {
            event: value.get("event")?.as_str()?.to_string(),
            event_id: value.get("eventId")?.as_str()?.to_string(),
            agent_id: value.get("agentId")?.as_str()?.to_string(),
            request: value.get("request").cloned().unwrap_or(Value::Null),
        }),
        "cancel" => Some(HubEvent::WaterfallCancel {
            event_id: value.get("eventId")?.as_str()?.to_string(),
        }),
        _ => None,
    }
}

/// Translate one `session/control` stream item; `None` when unparseable.
pub fn hub_event_from_control_item(value: Value) -> Option<HubEvent> {
    serde_json::from_value(value)
        .ok()
        .map(HubEvent::Control)
}

/// Translate one `workspace/follow` stream item; `None` when unparseable.
pub fn hub_event_from_workspace_item(value: Value) -> Option<HubEvent> {
    serde_json::from_value(value)
        .ok()
        .map(HubEvent::Workspace)
}

/// Translate one `session/follow` stream item; `None` for unknown frame types.
pub fn hub_event_from_follow_item(key: &str, value: &Value) -> Option<HubEvent> {
    match value.get("type")?.as_str()? {
        "snapshot" => serde_json::from_value(value.clone())
            .ok()
            .map(|snapshot| HubEvent::FollowSnapshot {
                key: key.to_string(),
                snapshot,
            }),
        "event" => serde_json::from_value(value.get("event")?.clone())
            .ok()
            .map(|event| HubEvent::FollowEvent {
                key: key.to_string(),
                event,
            }),
        "assistant-stream" => Some(HubEvent::FollowAssistantStream {
            key: key.to_string(),
            frame: value.get("frame").cloned().unwrap_or(Value::Null),
        }),
        _ => None,
    }
}

/// Stable dedup key for one followed address (`session:<id>` /
/// `subagent:<parent>:<child>:<mode>`).
pub fn follow_key(address: &SessionAddress) -> String {
    match address {
        SessionAddress::Session { session_id } => format!("session:{session_id}"),
        SessionAddress::Subagent {
            parent_session_id,
            child_session_id,
            mode,
        } => {
            let mode = match mode {
                SubagentMode::OneShot => "one-shot",
                SubagentMode::Continuable => "continuable",
                SubagentMode::Unknown => "unknown",
            };
            format!("subagent:{parent_session_id}:{child_session_id}:{mode}")
        }
    }
}
