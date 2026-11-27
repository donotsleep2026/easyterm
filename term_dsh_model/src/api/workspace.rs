//! Shared workspace data for daemon-delivered DSH state.

use serde::{Deserialize, Serialize};
use crate::api::session::SessionId;

/// Workspace id brand (opaque string on the wire).
pub type WorkspaceId = String;

/// One workspace row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceView {
    #[serde(rename = "workspaceId")]
    pub workspace_id: WorkspaceId,
    /// Canonical directory path (host-side realpath canon).
    pub path: String,
    /// Display title (defaults to the path basename at create).
    pub title: String,
    /// Sessions accounted under this workspace, in manually owned order.
    #[serde(rename = "sessionIds")]
    pub session_ids: Vec<SessionId>,
    /// ISO-8601 creation instant.
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// ISO-8601 last-mutation instant.
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

/// `workspace/create` result: the adopted workspace and whether it is new.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCreated {
    pub workspace: WorkspaceView,
    pub created: bool,
}
