//! Two-message RPC envelope types, mirroring the Typert Remote carrier
//! contract of `@deepseek-ai/dsh-client-connection` (`rpc.ts` /
//! `rpc-schema.ts`) at `dsh-v0.2.1-rc.2`.
//!
//! Wire shapes (exact field names matter):
//! - `client-request`: `{ type, rpcId, method, payload: { args: { … } } }`
//!   → `POST /api/<namespace>/<method>` body. `method` is the slash form
//!   (`session/list`); `args` carries one key per Host method parameter.
//! - `server-response`: `{ type, rpcId, result }` ← POST response body.
//!
//! `result` is `{ ok: true, value? } | { ok: false, error }`; a void or
//! `undefined` result omits `value`. `error` is the free-form
//! `{ code: "<domain>/<reason>", message, details: object }` — there is no
//! closed code enum on the wire anymore.
//!
//! The old `server-request` / `client-response` quadrants are gone:
//! host-initiated traffic now rides the `/api/remote.mux` stream carrier
//! and answerable events settle through the daemon-owned event-result call.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// Discriminant of the two wire messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RpcMessageKind {
    ClientRequest,
    ServerResponse,
}

/// Call initiated by the client (`POST /api/<namespace>/<method>` body).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientRequest {
    #[serde(rename = "type")]
    pub kind: RpcMessageKind,
    #[serde(rename = "rpcId")]
    pub rpc_id: String,
    pub method: String,
    pub payload: Value,
}

impl ClientRequest {
    /// Build a request with a freshly minted rpcId, wrapping `args` into the
    /// Remote `payload.args` carrier (`args` holds one key per Host method
    /// parameter; an `agent: Agent` parameter is `agentId`, and a trailing
    /// `signal: AbortSignal` is never on the wire).
    pub fn new(method: impl Into<String>, args: Value) -> Self {
        Self {
            kind: RpcMessageKind::ClientRequest,
            rpc_id: mint_rpc_id(),
            method: method.into(),
            payload: json!({ "args": args }),
        }
    }
}

/// Response to a `ClientRequest`; `rpc_id` always echoes the request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerResponse {
    #[serde(rename = "type")]
    pub kind: RpcMessageKind,
    #[serde(rename = "rpcId")]
    pub rpc_id: String,
    pub result: RpcResult,
}

/// Business success/failure slot: `{ ok: true, value? } | { ok: false, error }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcResult {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResult {
    pub fn ok(value: Value) -> Self {
        Self {
            ok: true,
            value: Some(value),
            error: None,
        }
    }

    pub fn err(error: RpcError) -> Self {
        Self {
            ok: false,
            value: None,
            error: Some(error),
        }
    }

    /// Split the result into a business value or a business error.
    ///
    /// A success without a `value` slot (void methods, or a value that was
    /// `undefined` on the host — e.g. `goals/get` with no current goal) maps
    /// to `Value::Null`.
    pub fn into_value(self) -> Result<Value, RpcError> {
        if self.ok {
            Ok(self.value.unwrap_or(Value::Null))
        } else {
            Err(self
                .error
                .unwrap_or_else(|| internal_error("error result missing error")))
        }
    }
}

/// Business error payload; `code` is a free-form `<domain>/<reason>` string
/// and `details` an opaque JSON object keyed by that code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub details: Value,
}

fn internal_error(message: &str) -> RpcError {
    RpcError {
        code: "gateway/internal".to_string(),
        message: message.to_string(),
        details: Value::Null,
    }
}

static RPC_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Mint a process-unique correlation id: monotonic counter mixed with time
/// and pid. The host treats the id as an opaque echo token.
fn mint_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = RPC_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{pid:x}-{counter:x}", pid = std::process::id())
}

/// Mint a process-unique rpcId for one unary RPC envelope.
pub fn mint_rpc_id() -> String {
    mint_id()
}

/// Mint a process-unique prompt identity (`SessionRequestId`) for
/// `session/prompt` and `subagents/prompt`; the host persists it on the
/// accepted user message and echoes it on queue items as `rpcId` so clients
/// can retire their optimistic submission echo.
pub fn mint_request_id() -> String {
    mint_id()
}
