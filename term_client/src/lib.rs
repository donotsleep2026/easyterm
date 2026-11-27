//! `term_client` 闁?UI-free protocol client for the terminal-core C/S split.
//!
//! Speaks [`term_proto`] frames to a [`term_core_server`] daemon over TCP
//! loopback. This is the shared base both frontends (the egui GUI and the
//! ratatui TUI) build on 闁?it never links `term_core` or egui.
//!
//! # Model
//!
//! - **One connection hosts many sessions** (each identified by `sid`).
//! - [`Client`] holds the whole connection: its background reader task pumps
//!   the socket and queues `ServerEvent`s (snapshots, title, 闁? and
//!   `ServerAsk`s (host questions) for the host. Call methods take `&self`.
//! - [`Client::writer`] returns a cheaply cloneable [`Writer`] so many tasks
//!   (one per GUI tab, for instance) can issue calls on the same connection
//!   while a single pump task owns the exclusive event/ask receivers.
//!
//! # Host-question contract
//!
//! A session-open for SSH may block server-side until authentication
//! questions are answered, so the host **must** drain `asks()` and answer
//! them (or cancel) while an open is in flight.

pub mod ask;
mod transport;
pub use transport::{configure_external_endpoint, daemon_trust_dir};

/// Use the current frontend's login token on its subsequent daemon sockets.
pub fn set_external_vault_token(token: Option<String>) {
    if let Some(endpoint) = transport::external_endpoint() {
        *endpoint.token.lock().unwrap() = token;
    }
}

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use log::{debug, error, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use term_proto::{
    ClientAnswer, ClientMethod, ClientRequest, CoreFrameType, OpenSessionParams, ServerAsk,
    ServerEvent, ServerReply, ServerResult,
};

/// How long the interactive share calls wait for the server.
///
/// An open request or a keystroke that takes longer than this means the server is
/// stuck inside another share operation; blocking the caller forever - a task that
/// forwards a pane's input, and therefore a thread the runtime waits for at
/// shutdown - only hides that.
const SHARE_CALL_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Handshake replies are bounded and fully consumed before starting any event
/// consumer. A spoofed daemon cannot allocate an arbitrary frame or push state.
async fn connection_exchange(
    sock: &mut impl transport::ProtocolStream,
    rpc_id: u64,
    method: ClientMethod,
) -> std::io::Result<ServerResult> {
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid daemon authentication reply",
        )
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let request = ClientRequest { rpc_id, method };
        let payload = rmp_serde::to_vec_named(&request).map_err(|_| invalid())?;
        sock.write_all(&term_proto::encode_frame(
            CoreFrameType::ClientRequest,
            &payload,
        ))
        .await?;
        sock.flush().await?;
        let len = sock.read_u32_le().await? as usize;
        if !(1..=16 * 1024).contains(&len) {
            return Err(invalid());
        }
        let mut frame = vec![0; len];
        sock.read_exact(&mut frame).await?;
        if frame[0] != CoreFrameType::ServerReply as u8 {
            return Err(invalid());
        }
        let reply: ServerReply = rmp_serde::from_slice(&frame[1..]).map_err(|_| invalid())?;
        if reply.rpc_id != rpc_id {
            return Err(invalid());
        }
        Ok(reply.result)
    })
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "daemon authentication timed out",
        )
    })?
}

/// Shared outbound half of one connection. Cheap to clone; issue calls from
/// any task (e.g. one writer per GUI tab).
#[derive(Clone)]
pub struct Writer {
    writer: Arc<tokio::sync::Mutex<WriteHalf<transport::BoxStream>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ServerResult>>>>,
    pending_changed: Arc<tokio::sync::Notify>,
    closed: Arc<AtomicBool>,
    next_rpc: Arc<AtomicU64>,
    ask_lifetimes: Arc<ask::AskLifetimes>,
}

/// A dropped/expired RPC must not keep an abandoned consumer's reader alive.
/// It also wakes a reader already waiting for the last in-flight reply.
struct PendingCall {
    rpc_id: u64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ServerResult>>>>,
    changed: Arc<tokio::sync::Notify>,
}

impl Drop for PendingCall {
    fn drop(&mut self) {
        let removed = self.pending.lock().unwrap().remove(&self.rpc_id).is_some();
        if removed { self.changed.notify_one(); }
    }
}

impl Writer {
    /// Enumerate serial devices on the daemon, including USB descriptions.
    pub async fn serial_ports(&self) -> Result<Vec<term_proto::SerialPortInfo>, String> {
        match self.call(ClientMethod::SerialPorts).await? {
            ServerResult::Ok(term_proto::ServerOk::SerialPorts { ports }) => Ok(ports),
            ServerResult::Err(error) => Err(format!("{}: {}", error.code, error.message)),
            _ => Err("unexpected reply to SerialPorts".into()),
        }
    }
    /// Execute on this connection's daemon-local shell session, using its
    /// working directory/environment. The daemon enforces the paid exec gate.
    pub async fn shell_exec(&self, sid: u64, cmd: &str, timeout_ms: u64) -> Result<(String, i32), String> {
        match self.call(ClientMethod::ShellExec { sid, params: term_proto::SshExecParams { cmd: cmd.into(), timeout_ms } }).await? {
            ServerResult::Ok(term_proto::ServerOk::SshExecStdout { stdout, exit_code }) => Ok((String::from_utf8_lossy(&stdout).into_owned(), exit_code)),
            ServerResult::Err(error) => Err(format!("{}: {}", error.code, error.message)),
            _ => Err("unexpected reply to ShellExec".into()),
        }
    }
    /// A named daemon-owned business operation. The backend validates its
    /// operation-specific payload and permissions; no local files are read.
    pub async fn daemon_call(&self, op: &str, payload: serde_json::Value) -> Result<serde_json::Value, String> {
        match self.call(ClientMethod::DaemonCall { op: op.into(), payload }).await? {
            ServerResult::Ok(term_proto::ServerOk::DaemonValue(value)) => Ok(value),
            ServerResult::Err(error) => Err(format!("{}: {}", error.code, error.message)),
            _ => Err("unexpected reply to DaemonCall".into()),
        }
    }
    /// Save a daemon-owned document without resolving paths on this machine.
    pub async fn save_declared_config(
        &self,
        document: term_proto::ConfigDocument,
        contents: &str,
    ) -> Result<(), String> {
        match self.call(ClientMethod::ConfigSaveDeclared { document, contents: contents.into() }).await? {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(error) => Err(format!("{}: {}", error.code, error.message)),
            _ => Err("unexpected reply to ConfigSaveDeclared".into()),
        }
    }
    /// List a daemon-local directory without accessing the frontend filesystem.
    pub async fn local_path_list(&self, path: Option<&str>) -> Result<term_proto::LocalPathListInfo, String> {
        match self.call(ClientMethod::LocalPathList { path: path.map(str::to_owned) }).await? {
            ServerResult::Ok(term_proto::ServerOk::LocalPathList(info)) => Ok(info),
            ServerResult::Err(error) => Err(format!("{}: {}", error.code, error.message)),
            _ => Err("unexpected reply to LocalPathList".into()),
        }
    }

    /// Turn one port forward's **debug feed** on/off (see
    /// [`Client::port_map_debug`]).
    ///
    /// While it is on, the daemon reports that forward's connections and traffic
    /// as `CoreEvent::PortMapDebug` on this connection.
    pub async fn port_map_debug(
        &self,
        sid: term_proto::Sid,
        kind: term_proto::PortMapKind,
        bind_host: &str,
        bind_port: u32,
        on: bool,
    ) -> Result<(), String> {
        match self
            .call(ClientMethod::PortMapDebug {
                sid,
                params: term_proto::PortMapDebugParams {
                    kind,
                    bind_host: bind_host.to_string(),
                    bind_port,
                    on,
                },
            })
            .await?
        {
            ServerResult::Ok(_) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Send a request and await the server's reply (matched by fresh rpc id).
    pub async fn call(&self, method: ClientMethod) -> Result<ServerResult, String> {
        self.call_inner(method, None).await
    }

    /// [`call`](Self::call) with a deadline on the reply.
    ///
    /// `call` waits for as long as the connection lives - and a connection stays
    /// alive while the server is wedged inside a handler that never returns (a
    /// share operation holding a lock the rest of them need, say). Then the
    /// caller blocks forever: a share op dispatched on a blocking thread never
    /// finishes, so it keeps the single-flight gate taken and, at shutdown, keeps
    /// the whole process from exiting.
    pub async fn call_within(
        &self,
        method: ClientMethod,
        wait: std::time::Duration,
    ) -> Result<ServerResult, String> {
        self.call_inner(method, Some(wait)).await
    }

    async fn call_inner(
        &self,
        method: ClientMethod,
        wait: Option<std::time::Duration>,
    ) -> Result<ServerResult, String> {
        let rpc_id = self.next_rpc.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if self.closed.load(Ordering::Acquire) {
                return Err("connection closed while awaiting reply".into());
            }
            pending.insert(rpc_id, tx);
        }
        let _pending_call = PendingCall {
            rpc_id, pending: self.pending.clone(), changed: self.pending_changed.clone(),
        };
        let req = ClientRequest { rpc_id, method };
        let payload = match rmp_serde::to_vec_named(&req) {
            Ok(p) => p,
            Err(e) => {
                return Err(format!("encode request: {e}"));
            }
        };
        let frame = term_proto::encode_frame(CoreFrameType::ClientRequest, &payload);
        let res = {
            let mut wh = self.writer.lock().await;
            async {
                wh.write_all(&frame).await?;
                // A TLS write can consume plaintext before the final encrypted
                // record reaches TCP. Flush while the frame lock is held.
                wh.flush().await
            }.await
        };
        if let Err(e) = res {
            return Err(format!("send request: {e}"));
        }
        match wait {
            None => rx
                .await
                .map_err(|_| "connection closed while awaiting reply".into()),
            Some(d) => match tokio::time::timeout(d, rx).await {
                Ok(result) => result.map_err(|_| "connection closed while awaiting reply".into()),
                Err(_) => {
                    // Drop the reply slot: a caller that polls on a timer must not
                    // grow the pending table one entry per timeout.
                    Err(format!(
                        "the core server did not answer within {}s",
                        d.as_secs_f64()
                    ))
                }
            },
        }
    }

    /// Read the daemon's current verified entitlements without presenting a JWT.
    pub async fn license_status(&self) -> Result<Option<term_proto::LicenseInfo>, String> {
        match self.call(ClientMethod::LicenseStatus).await? {
            ServerResult::Ok(term_proto::ServerOk::LicenseStatus(info)) => Ok(info),
            ServerResult::Ok(_) => Err("unexpected reply to LicenseStatus".into()),
            ServerResult::Err(e) => {
                Err(format!("daemon license status: {} ({})", e.message, e.code))
            }
        }
    }

    pub async fn offline_license_status(
        &self,
    ) -> Result<term_proto::OfflineLicenseStatusInfo, String> {
        match self
            .call_within(
                ClientMethod::OfflineLicenseStatus,
                std::time::Duration::from_secs(15),
            )
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::OfflineLicenseStatus(info)) => Ok(info),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            _ => Err("unexpected reply to OfflineLicenseStatus".into()),
        }
    }

    /// Open a session (shell or ssh) and return the assigned sid.
    pub async fn open_session(&self, spec: term_proto::OpenSpec) -> Result<u64, String> {
        match self
            .call(ClientMethod::SessionOpen(OpenSessionParams { spec }))
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Open { sid }) => Ok(sid),
            ServerResult::Err(e) => Err(format!("session/open: {} ({})", e.message, e.code)),
            other => Err(format!("unexpected reply to session/open: {other:?}")),
        }
    }

    // 閳光偓閳光偓 Direct sharing (same surface as `Client`, for a connection that is
    // held open by a long-lived task such as the GUI's share client) 閳光偓閳光偓

    /// Hand the daemon the share configuration it cannot know by itself: the
    /// server's iroh relay address, the force-relay switch and the account token
    /// used to attribute relayed bytes to the account's relay quota.
    ///
    /// Without this the daemon can only run a Pro share relay-free (pure P2P),
    /// because the relay address lives in the frontend's synced global config.
    /// Best-effort by design: an older daemon rejects the unknown method and the
    /// share still starts relay-free, so callers may ignore an error here.
    pub async fn share_configure_relay(
        &self,
        relay_url: Option<String>,
        force_relay: bool,
        token: Option<String>,
    ) -> Result<(), String> {
        match self
            .call(ClientMethod::ShareConfigureRelay {
                relay_url,
                force_relay,
                token,
            })
            .await?
        {
            ServerResult::Ok(_) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Start a LAN share; returns the code observers type and its address.
    pub async fn share_start_lan(
        &self,
        sid: u64,
        name: &str,
        permission: term_proto::SharePermission,
        sharer_name: &str,
        instance_id: &str,
    ) -> Result<ShareStart, String> {
        match self
            .call(ClientMethod::ShareStartLan {
                sid,
                name: name.to_string(),
                permission,
                sharer_name: sharer_name.to_string(),
                sharer_instance_id: instance_id.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::ShareStarted {
                code_or_ticket,
                addr,
            }) => Ok(ShareStart {
                code: code_or_ticket,
                addr,
            }),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to ShareStartLan: {other:?}")),
        }
    }

    /// Join a LAN share by code (multicast discovery).
    pub async fn join_lan_share(
        &self,
        code: &str,
        observer_name: &str,
        instance_id: &str,
    ) -> Result<term_proto::ShareStatusWire, String> {
        match self
            .call(ClientMethod::ShareJoinLan {
                code: code.to_string(),
                observer_name: observer_name.to_string(),
                observer_instance_id: instance_id.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::ShareStatus(s)) => Ok(s),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to ShareJoinLan: {other:?}")),
        }
    }

    /// Join a LAN share at a known address.
    pub async fn join_lan_direct_share(
        &self,
        code: &str,
        addr: &str,
        observer_name: &str,
        instance_id: &str,
    ) -> Result<term_proto::ShareStatusWire, String> {
        match self
            .call(ClientMethod::ShareJoinLanDirect {
                code: code.to_string(),
                addr: addr.to_string(),
                observer_name: observer_name.to_string(),
                observer_instance_id: instance_id.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::ShareStatus(s)) => Ok(s),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to ShareJoinLanDirect: {other:?}")),
        }
    }

    /// Share one more session in the running channel; returns its item id.
    pub async fn share_add_tab(
        &self,
        sid: u64,
        name: &str,
        permission: term_proto::SharePermission,
    ) -> Result<String, String> {
        match self
            .call(ClientMethod::ShareAddTab {
                sid,
                name: name.to_string(),
                permission,
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::ShareItemAdded { item_id }) => Ok(item_id),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to ShareAddTab: {other:?}")),
        }
    }

    /// Open a local listener for a shared port item and forward it through the
    /// channel (the server owns the tunnel and the listener).
    pub async fn open_shared_port(
        &self,
        item_id: &str,
        bind_addr: &str,
        udp: bool,
    ) -> Result<(), String> {
        match self
            .call(ClientMethod::ShareOpenPort {
                item_id: item_id.to_string(),
                bind_addr: bind_addr.to_string(),
                protocol: if udp { "udp".into() } else { "tcp".into() },
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to ShareOpenPort: {other:?}")),
        }
    }

    /// Stop forwarding a shared port item and close its listener.
    pub async fn close_shared_port(&self, item_id: &str) -> Result<(), String> {
        match self
            .call(ClientMethod::ShareClosePort {
                item_id: item_id.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to ShareClosePort: {other:?}")),
        }
    }
    /// Ask the core server to check for an update (it owns the manifest, the
    /// download and the staged files).
    pub async fn update_check(
        &self,
        base_url: &str,
        channel: &str,
        skipped_version: Option<&str>,
        manual: bool,
    ) -> Result<term_proto::UpdateStateWire, String> {
        // Deadline, same rationale as the share calls: the reply rides the same
        // single connection everything else uses, and a wedged handler must not
        // leave the caller (the settings page's "Checking..." state) waiting
        // forever. The daemon's own HTTP timeout is 15s, so 25s covers it.
        match self
            .call_within(
                ClientMethod::UpdateCheck {
                    base_url: base_url.to_string(),
                    channel: channel.to_string(),
                    skipped_version: skipped_version.map(|s| s.to_string()),
                    manual,
                },
                std::time::Duration::from_secs(25),
            )
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::UpdateState(s)) => Ok(s),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to UpdateCheck: {other:?}")),
        }
    }

    /// Start downloading the pending update server-side.
    pub async fn update_download(&self) -> Result<(), String> {
        match self.call(ClientMethod::UpdateDownload).await? {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to UpdateDownload: {other:?}")),
        }
    }

    /// Hand the staged build to the swap-in updater (the caller exits after).
    pub async fn update_apply(&self) -> Result<(), String> {
        match self.call(ClientMethod::UpdateApply).await? {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to UpdateApply: {other:?}")),
        }
    }

    /// Read the server's update state.
    pub async fn update_state(&self) -> Result<term_proto::UpdateStateWire, String> {
        match self.call(ClientMethod::UpdateState).await? {
            ServerResult::Ok(term_proto::ServerOk::UpdateState(s)) => Ok(s),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to UpdateState: {other:?}")),
        }
    }
    /// Open (idempotently) the daemon's DSH stream hub. Decoded hub
    /// events then arrive on this connection's event stream (`DSH_MUX_SID`).
    pub async fn dsh_hub_open(&self) -> Result<(), String> {
        match self.call(ClientMethod::DshHubOpen).await? {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(e.message),
            _ => Err("unexpected reply to DshHubOpen".into()),
        }
    }

    /// Follow one session/subagent address on the daemon's hub. `address` is a
    /// serialized `easyterm_dsh::api::session::SessionAddress`.
    pub async fn dsh_follow(
        &self,
        address: serde_json::Value,
        max_messages: Option<u32>,
        assistant_stream: bool,
    ) -> Result<(), String> {
        match self
            .call(ClientMethod::DshFollow {
                address,
                max_messages,
                assistant_stream,
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to DshFollow: {other:?}")),
        }
    }

    /// Stop following one key on the daemon's hub.
    pub async fn dsh_unfollow(&self, key: &str) -> Result<(), String> {
        match self
            .call(ClientMethod::DshUnfollow {
                key: key.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to DshUnfollow: {other:?}")),
        }
    }

    /// Answer one pending waterfall on the daemon's hub. `outcome` is a
    /// serialized `easyterm_dsh::client::EventOutcome`.
    pub async fn dsh_answer(
        &self,
        event_id: &str,
        outcome: serde_json::Value,
    ) -> Result<(), String> {
        match self
            .call(ClientMethod::DshAnswer {
                event_id: event_id.to_string(),
                outcome,
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to DshAnswer: {other:?}")),
        }
    }

    /// The daemon's conversation state for one followed session (rows,
    /// projections and queued inbox), as it holds them.
    pub async fn dsh_chat_open(&self, key: &str) -> Result<serde_json::Value, String> {
        match self
            .call(ClientMethod::DshChatOpen {
                key: key.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::DshChat(snapshot)) => Ok(snapshot),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to DshChatOpen: {other:?}")),
        }
    }

    /// Drop one session's conversation state (its follow was cancelled).
    pub async fn dsh_chat_close(&self, key: &str) -> Result<(), String> {
        match self
            .call(ClientMethod::DshChatClose {
                key: key.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to DshChatClose: {other:?}")),
        }
    }

    /// One UI pass of the daemon-hosted Lua runtime: send what the user did
    /// while drawing and get back the frame to draw.
    pub async fn lua_pass(
        &self,
        focused_chanid: u64,
        focused_sid: u64,
        inputs: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match self
            .call(ClientMethod::LuaPass {
                focused_chanid,
                focused_sid,
                inputs,
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::LuaPass(pass)) => Ok(pass),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to LuaPass: {other:?}")),
        }
    }

    /// Any other command for the daemon-hosted Lua runtime (a serialized
    /// `term_lua::LuaCommand`): hooks, catalog control, surface closes.
    pub async fn lua_command(&self, command: serde_json::Value) -> Result<(), String> {
        match self.call(ClientMethod::LuaCommand { command }).await? {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to LuaCommand: {other:?}")),
        }
    }

    /// The plugin catalog the daemon-hosted runtime holds.
    pub async fn lua_plugins(&self) -> Result<serde_json::Value, String> {
        match self.call(ClientMethod::LuaPlugins).await? {
            ServerResult::Ok(term_proto::ServerOk::LuaPlugins(plugins)) => Ok(plugins),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to LuaPlugins: {other:?}")),
        }
    }

    /// Drop the daemon's stream hub (every stream ends; the next
    /// `dsh_hub_open` rebuilds it).
    pub async fn dsh_hub_close(&self) -> Result<(), String> {
        match self.call(ClientMethod::DshHubClose).await? {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to DshHubClose: {other:?}")),
        }
    }

    /// Bring the daemon's DSH connection up: it probes the address,
    /// spawns the local sidecar when the parameters allow it, and keeps the
    /// auth-bootstrapped client. Returns the resulting state.
    pub async fn dsh_connect(
        &self,
        params: term_proto::DshConnectParams,
    ) -> Result<term_proto::DshStateWire, String> {
        match self.call(ClientMethod::DshConnect(params)).await? {
            ServerResult::Ok(term_proto::ServerOk::DshState(state)) => Ok(state),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to DshConnect: {other:?}")),
        }
    }

    /// The daemon's current DSH connection state.
    pub async fn dsh_status(&self) -> Result<term_proto::DshStateWire, String> {
        match self.call(ClientMethod::DshStatus).await? {
            ServerResult::Ok(term_proto::ServerOk::DshState(state)) => Ok(state),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to DshStatus: {other:?}")),
        }
    }

    /// Bring the daemon's DSH connection up from the endpoint it already knows
    /// (`ClientMethod::DshReconnect`): the caller has no DSH settings of its
    /// own, and the daemon owns the connection anyway.
    pub async fn dsh_reconnect(&self) -> Result<term_proto::DshStateWire, String> {
        match self.call(ClientMethod::DshReconnect).await? {
            ServerResult::Ok(term_proto::ServerOk::DshState(state)) => Ok(state),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to DshReconnect: {other:?}")),
        }
    }

    /// Stop the daemon's DSH connection and any sidecar it spawned.
    pub async fn dsh_stop(&self) -> Result<(), String> {
        match self.call(ClientMethod::DshStop).await? {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to DshStop: {other:?}")),
        }
    }

    /// Run one DSH RPC on the daemon's connection: the daemon
    /// holds the host credentials, so the frontend never sees them.
    pub async fn dsh_call(
        &self,
        method: &str,
        args: serde_json::Value,
    ) -> Result<term_proto::DshCallOutcomeWire, String> {
        match self
            .call(ClientMethod::DshCall {
                method: method.to_string(),
                args,
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::DshValue(outcome)) => Ok(outcome),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to DshCall: {other:?}")),
        }
    }

    /// The daemon's account/license state for this installation.
    ///
    /// The daemon owns the account manager (tokens, the stored auth record and
    /// the device identity), so this is the only copy a frontend needs: it does
    /// not hold or persist any of it itself.
    pub async fn account_state(&self) -> Result<term_proto::AccountState, String> {
        match self.call(ClientMethod::AccountState).await? {
            ServerResult::Ok(term_proto::ServerOk::AccountState(state)) => Ok(state),
            ServerResult::Ok(_) => Err("unexpected reply to AccountState".into()),
            ServerResult::Err(e) => Err(format!("account state: {}", e.message)),
        }
    }

    /// Run one named account operation on the daemon (see
    /// `ClientMethod::AccountCall`). The daemon owns the account manager, so
    /// this is how a frontend triggers its actions.
    pub async fn account_call(
        &self,
        op: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match self
            .call(ClientMethod::AccountCall {
                op: op.to_string(),
                payload,
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::AccountValue(value)) => Ok(value),
            ServerResult::Ok(_) => Err(format!("unexpected reply to account op {op:?}")),
            ServerResult::Err(e) => Err(format!("account op {op:?}: {}", e.message)),
        }
    }

    /// Run the daemon's post-auth sync (event queue, pending seat release,
    /// license verification, subscription refresh). `Err` carries the daemon's
    /// reason when it failed.
    pub async fn account_sync_now(&self) -> Result<(), String> {
        match self.call(ClientMethod::AccountSyncNow).await? {
            ServerResult::Ok(term_proto::ServerOk::AccountSynced { error: None }) => Ok(()),
            ServerResult::Ok(term_proto::ServerOk::AccountSynced { error: Some(e) }) => Err(e),
            ServerResult::Ok(_) => Err("unexpected reply to AccountSyncNow".into()),
            ServerResult::Err(e) => Err(format!("account sync: {}", e.message)),
        }
    }

    /// End the session the daemon holds (see `ClientMethod::AccountLogout`).
    pub async fn account_logout(&self) -> Result<(), String> {
        match self.call(ClientMethod::AccountLogout).await? {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Ok(_) => Err("unexpected reply to AccountLogout".into()),
            ServerResult::Err(e) => Err(format!("account logout: {}", e.message)),
        }
    }

    /// Forward a web request to the management server via the daemon, which
    /// gates the `feature` on the verified plan. Returns (status, body).
    pub async fn web_proxy(
        &self,
        feature: &str,
        base_url: &str,
        path: &str,
        method: &str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<(u16, Vec<u8>), String> {
        match self
            .call(ClientMethod::WebProxy(term_proto::WebProxyParams {
                feature: feature.to_string(),
                base_url: base_url.to_string(),
                path: path.to_string(),
                method: method.to_string(),
                headers,
                body,
            }))
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::WebProxy { status, body }) => Ok((status, body)),
            ServerResult::Ok(_) => Err("unexpected reply to WebProxy".into()),
            ServerResult::Err(e) => Err(format!("web proxy: {}", e.message)),
        }
    }

    /// Ask the sharer to (re)send an observed item's content.
    pub async fn open_shared_item(&self, item_id: &str) -> Result<(), String> {
        match self
            .call_within(
                ClientMethod::ShareOpenItem {
                    item_id: item_id.to_string(),
                },
                SHARE_CALL_WAIT,
            )
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to ShareOpenItem: {other:?}")),
        }
    }

    /// Send collaborative input for an observed item.
    pub async fn send_shared_input(&self, item_id: &str, data: Vec<u8>) -> Result<(), String> {
        match self
            .call_within(
                ClientMethod::ShareInput {
                    item_id: item_id.to_string(),
                    data,
                    mouse: None,
                },
                SHARE_CALL_WAIT,
            )
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to ShareInput: {other:?}")),
        }
    }

    /// Freeze or resume the session's local PTY child: the process belongs to
    /// whoever runs the backend, so this reaches the daemon's backend.
    pub async fn set_session_frozen(&self, sid: u64, frozen: bool) -> Result<(), String> {
        match self
            .call(ClientMethod::SessionFreeze { sid, frozen })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to SessionFreeze: {other:?}")),
        }
    }

    /// Kill the session's local PTY child tree.
    pub async fn kill_session_process(&self, sid: u64) -> Result<(), String> {
        match self.call(ClientMethod::SessionKillProcess { sid }).await? {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to SessionKillProcess: {other:?}")),
        }
    }

    /// Write bytes into a session's PTY.
    pub async fn write(&self, sid: u64, data: Vec<u8>) -> Result<(), String> {
        match self
            .call(ClientMethod::SessionWrite {
                sid,
                params: term_proto::SessionWriteParams { data },
            })
            .await?
        {
            ServerResult::Ok(_) => Ok(()),
            ServerResult::Err(e) => Err(format!("session/write: {} ({})", e.message, e.code)),
        }
    }

    /// Resize a session's grid.
    pub async fn resize(&self, sid: u64, cols: u16, rows: u16) -> Result<(), String> {
        match self
            .call(ClientMethod::SessionResize {
                sid,
                params: term_proto::SessionResizeParams { cols, rows },
            })
            .await?
        {
            ServerResult::Ok(_) => Ok(()),
            ServerResult::Err(e) => Err(format!("session/resize: {} ({})", e.message, e.code)),
        }
    }

    /// Close a session.
    pub async fn close(&self, sid: u64) -> Result<(), String> {
        // Closing is the last thing a session does, so it must never hang: a
        // daemon that is busy shutting the PTY down can take a while to answer,
        // and the caller (a GUI tab, a TUI session) would be stuck waiting. Give
        // it a moment, then treat the session as closed — dropping this
        // connection closes it daemon-side anyway.
        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.call(ClientMethod::SessionClose { sid }),
        )
        .await;
        match reply {
            Ok(Ok(ServerResult::Ok(_))) => Ok(()),
            Ok(Ok(ServerResult::Err(e))) => {
                Err(format!("session/close: {} ({})", e.message, e.code))
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(()),
        }
    }

    /// Answer a host question (kbdint/passphrase/host-key).
    pub async fn answer(
        &self,
        ask_id: u64,
        result: term_proto::AnswerResult,
    ) -> Result<(), String> {
        let payload =
            rmp_serde::to_vec_named(&ClientAnswer { ask_id, result }).map_err(|e| e.to_string())?;
        let frame = term_proto::encode_frame(CoreFrameType::ClientAnswer, &payload);
        let mut wh = self.writer.lock().await;
        let result = async {
            wh.write_all(&frame).await?;
            wh.flush().await
        }.await.map_err(|e: std::io::Error| e.to_string());
        self.ask_lifetimes.finish(ask_id);
        result
    }
}

/// A connected protocol client. Holds the exclusive read side (events/asks)
/// plus a shareable [`Writer`].
pub struct Client {
    requires_master_password: bool,
    peer_addr: Option<std::net::SocketAddr>,
    writer: Writer,
    /// Reader-side clones kept alive for the lifetime of the client so the
    /// unbounded channels never close while this handle exists.
    #[allow(dead_code)]
    events_tx: mpsc::UnboundedSender<ServerEvent>,
    events_rx: mpsc::UnboundedReceiver<ServerEvent>,
    /// Host questions (kbdint/passphrase/host-key) queued for the host.
    #[allow(dead_code)]
    asks_tx: mpsc::UnboundedSender<ServerAsk>,
    asks_rx: mpsc::UnboundedReceiver<ServerAsk>,
}

/// A started share: the code an observer types (LAN) or the ticket it pastes
/// (Pro), plus the address a LAN peer can dial directly when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareStart {
    pub code: String,
    pub addr: Option<String>,
}

impl Client {
    pub async fn serial_ports(&self) -> Result<Vec<term_proto::SerialPortInfo>, String> {
        self.writer.serial_ports().await
    }
    pub async fn shell_exec(&self, sid: u64, cmd: &str, timeout_ms: u64) -> Result<(String, i32), String> {
        self.writer.shell_exec(sid, cmd, timeout_ms).await
    }
    pub async fn daemon_call(&self, op: &str, payload: serde_json::Value) -> Result<serde_json::Value, String> {
        self.writer.daemon_call(op, payload).await
    }

    /// Connect to an explicit endpoint without reusing the frontend's vault token.
    pub async fn connect_external<A: tokio::net::ToSocketAddrs>(addr: A) -> Result<Self, std::io::Error> {
        Self::connect_inner(addr, None, true).await
    }

    /// The remote listener requires a vault login before business requests.
    pub fn requires_master_password(&self) -> bool {
        self.requires_master_password
    }
    pub async fn save_declared_config(
        &self,
        document: term_proto::ConfigDocument,
        contents: &str,
    ) -> Result<(), String> {
        self.writer.save_declared_config(document, contents).await
    }
    /// List paths on the daemon (see [`Writer::local_path_list`]).
    pub async fn local_path_list(&self, path: Option<&str>) -> Result<term_proto::LocalPathListInfo, String> {
        self.writer.local_path_list(path).await
    }

    /// The daemon's account/license state for this installation.
    ///
    /// The daemon owns the account manager (tokens, the stored auth record and
    /// the device identity), so this is the only copy a frontend needs: it does
    /// not hold or persist any of it itself.
    pub async fn account_state(&self) -> Result<term_proto::AccountState, String> {
        match self.call(ClientMethod::AccountState).await? {
            ServerResult::Ok(term_proto::ServerOk::AccountState(state)) => Ok(state),
            ServerResult::Ok(_) => Err("unexpected reply to AccountState".into()),
            ServerResult::Err(e) => Err(format!("account state: {}", e.message)),
        }
    }

    /// Run one named account operation on the daemon (see
    /// `ClientMethod::AccountCall`). The daemon owns the account manager, so
    /// this is how a frontend triggers its actions.
    pub async fn account_call(
        &self,
        op: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match self
            .call(ClientMethod::AccountCall {
                op: op.to_string(),
                payload,
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::AccountValue(value)) => Ok(value),
            ServerResult::Ok(_) => Err(format!("unexpected reply to account op {op:?}")),
            ServerResult::Err(e) => Err(format!("account op {op:?}: {}", e.message)),
        }
    }

    /// Run the daemon's post-auth sync (event queue, pending seat release,
    /// license verification, subscription refresh). `Err` carries the daemon's
    /// reason when it failed.
    pub async fn account_sync_now(&self) -> Result<(), String> {
        match self.call(ClientMethod::AccountSyncNow).await? {
            ServerResult::Ok(term_proto::ServerOk::AccountSynced { error: None }) => Ok(()),
            ServerResult::Ok(term_proto::ServerOk::AccountSynced { error: Some(e) }) => Err(e),
            ServerResult::Ok(_) => Err("unexpected reply to AccountSyncNow".into()),
            ServerResult::Err(e) => Err(format!("account sync: {}", e.message)),
        }
    }

    /// End the session the daemon holds (see `ClientMethod::AccountLogout`).
    pub async fn account_logout(&self) -> Result<(), String> {
        match self.call(ClientMethod::AccountLogout).await? {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Ok(_) => Err("unexpected reply to AccountLogout".into()),
            ServerResult::Err(e) => Err(format!("account logout: {}", e.message)),
        }
    }

    /// Connect to a daemon and complete the `Hello` version and mutual local
    /// authentication handshake before spawning the background reader. The OS
    /// restricts credential access to the current user; no vault unlock or
    /// server connection is needed. A daemon speaking a different protocol
    /// version is reported as an [`std::io::ErrorKind::InvalidData`] error
    /// rather than failing later on a message decode.
    pub async fn connect<A: tokio::net::ToSocketAddrs>(addr: A) -> Result<Self, std::io::Error> {
        Self::connect_inner(addr, None, false).await
    }

    /// Connect with a credential obtained directly from an in-process daemon
    /// handle. This authenticates the connection, not access to the vault.
    pub async fn connect_with_token<A: tokio::net::ToSocketAddrs>(
        addr: A,
        token: &str,
    ) -> Result<Self, std::io::Error> {
        Self::connect_inner(addr, Some(token), false).await
    }

    async fn connect_inner<A: tokio::net::ToSocketAddrs>(
        addr: A,
        token: Option<&str>,
        login_probe: bool,
    ) -> Result<Self, std::io::Error> {
        let mut sock = tokio::time::timeout(std::time::Duration::from_secs(10), TcpStream::connect(addr)).await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "daemon connection timed out"))??;
        let peer = sock.peer_addr()?;
        let endpoint = if token.is_none() { transport::external_endpoint() } else { None };
        if let Some(endpoint) = endpoint {
            let mut allowed = tokio::time::timeout(std::time::Duration::from_secs(10), tokio::net::lookup_host(endpoint.addr.as_str())).await
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "daemon address resolution timed out"))??;
            if !allowed.any(|addr| addr == peer) {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied,
                    "connection target differs from the selected daemon address"));
            }
        }
        let allow_network = login_probe || endpoint.is_some();
        if !allow_network && peer.ip() != std::net::Ipv4Addr::LOCALHOST {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "a daemon outside 127.0.0.1 requires an explicit endpoint",
            ));
        }
        let hello = Self::hello(&mut sock).await?;
        if allow_network && matches!(&hello, ServerResult::Err(e) if e.code == term_proto::TLS_REQUIRED) {
            let mut stream = transport::tls(sock, endpoint).await?;
            let hello = Self::hello(&mut stream).await?;
            if !matches!(hello, ServerResult::Ok(term_proto::ServerOk::Hello { proto_version: term_proto::PROTO_VERSION, features, .. })
                if features.iter().any(|f| f == term_proto::NETWORK_MASTER_PASSWORD))
            {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData,
                    "The daemon does not support secure master-password login. Update both builds."));
            }
            let mut client = Self::from_transport(stream, Some(peer));
            client.requires_master_password = true;
            if !login_probe {
                let token = endpoint.and_then(|endpoint| endpoint.token.lock().unwrap().clone());
                if let Some(token) = token {
                    client.auth(&token).await.map_err(|err| std::io::Error::new(std::io::ErrorKind::PermissionDenied, err))?;
                }
            }
            return Ok(client);
        }
        Self::authenticate_stream(Box::new(sock), peer, token, hello).await
    }

    async fn hello(sock: &mut impl transport::ProtocolStream) -> std::io::Result<ServerResult> {
        connection_exchange(
            sock,
            1,
            ClientMethod::Hello {
                proto_version: term_proto::PROTO_VERSION,
                client: concat!("term_client/", env!("CARGO_PKG_VERSION")).to_string(),
            },
        )
        .await.map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    async fn authenticate_stream(
        mut sock: transport::BoxStream,
        peer: std::net::SocketAddr,
        token: Option<&str>,
        hello: ServerResult,
    ) -> Result<Self, std::io::Error> {
        use term_discovery::connection_auth::{new_nonce, proof, verify_proof, ProofRole};
        match hello {
            ServerResult::Ok(term_proto::ServerOk::Hello {
                proto_version,
                features,
                auth_id,
                server_nonce,
            }) => {
                if proto_version != term_proto::PROTO_VERSION {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "daemon proto v{proto_version} != client proto v{}; \
                             rebuild both from the same tree",
                            term_proto::PROTO_VERSION
                        ),
                    ));
                }
                // The IPC credential is scoped to this IPv4 loopback and port.
                if peer.ip() != std::net::Ipv4Addr::LOCALHOST || !features.iter().any(|f| f == "connection_auth") {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "daemon does not support local connection authentication; upgrade it",
                    ));
                }
                let credential = match token {
                    Some(token) => token.to_owned(),
                    None => term_discovery::connection_auth::read(peer.port(), &auth_id).map_err(
                        |_| {
                            std::io::Error::new(
                                std::io::ErrorKind::PermissionDenied,
                                "cannot access this daemon's local connection credential",
                            )
                        },
                    )?,
                };
                let client_nonce = new_nonce()?;
                let client_proof = proof(
                    &credential,
                    &auth_id,
                    &server_nonce,
                    &client_nonce,
                    ProofRole::Client,
                )?;
                match connection_exchange(
                    &mut sock,
                    2,
                    ClientMethod::ConnectionAuth {
                        client_nonce: client_nonce.clone(),
                        proof: client_proof.into(),
                    },
                )
                .await
                {
                    Ok(ServerResult::Ok(term_proto::ServerOk::ConnectionAuthed {
                        proof: response,
                    })) if verify_proof(
                        &credential,
                        &auth_id,
                        &server_nonce,
                        &client_nonce,
                        ProofRole::Server,
                        response.as_str(),
                    ) => {}
                    _ => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "daemon connection authentication failed",
                        ))
                    }
                }
                debug!("daemon proto v{proto_version}, features {features:?}");
                Ok(Self::from_transport(sock, Some(peer)))
            }
            ServerResult::Err(e) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("daemon rejected handshake: {} ({})", e.message, e.code),
            )),
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unexpected reply to Hello: {other:?}"),
            )),
        }
    }

    /// Wrap an already-connected TCP stream without authenticating it (tests,
    /// custom transports). Only Hello/ConnectionAuth are usable until the
    /// caller explicitly completes the connection handshake.
    pub fn from_stream(sock: TcpStream) -> Self {
        let peer_addr = sock.peer_addr().ok();
        Self::from_transport(Box::new(sock), peer_addr)
    }

    fn from_transport(sock: transport::BoxStream, peer_addr: Option<std::net::SocketAddr>) -> Self {
        let (rh, wh) = tokio::io::split(sock);
        let writer = Arc::new(tokio::sync::Mutex::new(wh));
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<ServerResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_changed = Arc::new(tokio::sync::Notify::new());
        let closed = Arc::new(AtomicBool::new(false));
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (asks_tx, asks_rx) = mpsc::unbounded_channel();
        let next_rpc = Arc::new(AtomicU64::new(1));
        let ask_lifetimes = Arc::new(ask::AskLifetimes::default());

        let c = Self {
            requires_master_password: false,
            peer_addr,
            writer: Writer {
                writer: writer.clone(),
                pending: pending.clone(),
                pending_changed: pending_changed.clone(),
                closed: closed.clone(),
                next_rpc: next_rpc.clone(),
                ask_lifetimes: ask_lifetimes.clone(),
            },
            events_tx: events_tx.clone(),
            events_rx,
            asks_tx: asks_tx.clone(),
            asks_rx,
        };

        // The reader owns one half of a `tokio::io::split` socket, and the two
        // halves only close the socket once *both* are gone: dropping the
        // `Client` (which owns the write half) sends no FIN while this task is
        // still parked in `read`. So the reader has to end when the consumer
        // side disappears, or every short-lived client — the one-shot helpers
        // (`account_state`, `pull_config_files`, an update check, ...) create
        // one per call — leaks an ESTABLISHED connection, and with it a daemon
        // connection task whose 16 ms frame ticker keeps waking the runtime.
        // A live app was seen with ~2276 such connections and a full core of
        // CPU burnt on empty frame ticks.
        let consumer_gone = events_tx.clone();
        tokio::spawn(async move {
            let mut reader = FrameReader::new(rh);
            let mut consumers_closed = false;
            'reading: loop {
                // Keep this future across notification branches: it may have
                // consumed a partial reply before an event pump disappears.
                let next = reader.next_frame();
                tokio::pin!(next);
                let next = loop {
                    if consumers_closed {
                        let pending = pending.lock().unwrap();
                        if pending.is_empty() {
                            // Serialize retirement with call registration. A
                            // writer cannot add a request after this decision.
                            closed.store(true, Ordering::Release);
                            break 'reading;
                        }
                    }
                    tokio::select! {
                        biased;
                        // Once ready, this branch stays disabled while pending
                        // RPCs drain; polling closed() repeatedly would spin.
                        _ = consumer_gone.closed(), if !consumers_closed => { consumers_closed = true; }
                        _ = pending_changed.notified(), if consumers_closed => {}
                        next = &mut next => break next,
                    }
                };
                match next {
                    Ok(Some((CoreFrameType::ServerReply, payload))) => {
                        match rmp_serde::from_slice::<ServerReply>(&payload) {
                            Ok(reply) => {
                                let tx = pending.lock().unwrap().remove(&reply.rpc_id);
                                if let Some(tx) = tx {
                                    let _ = tx.send(reply.result);
                                } else {
                                    warn!("reply for unknown rpc {}", reply.rpc_id);
                                }
                            }
                            Err(e) => error!("bad reply payload: {e}"),
                        }
                    }
                    Ok(Some((CoreFrameType::ServerEvent, payload))) => {
                        if let Ok(ev) = rmp_serde::from_slice::<ServerEvent>(&payload) {
                            if let term_proto::CoreEvent::Zmodem(
                                term_proto::ZmodemEventWire::Cancelled { sid } | term_proto::ZmodemEventWire::Ended { sid }
                            ) = &ev.event {
                                ask_lifetimes.cancel_session(*sid);
                            }
                            let _ = events_tx.send(ev);
                        }
                    }
                    Ok(Some((CoreFrameType::ServerAsk, payload))) => {
                        if let Ok(ask) = rmp_serde::from_slice::<ServerAsk>(&payload) {
                            ask_lifetimes.register(&ask);
                            let _ = asks_tx.send(ask);
                        }
                    }
                    Ok(Some((_other, _))) => {
                        debug!("ignoring frame {_other:?}");
                    }
                    Ok(None) => {
                        debug!("server closed connection");
                        break;
                    }
                    Err(e) => {
                        error!("frame read error: {e}");
                        break;
                    }
                }
            }
            // Writer clones outlive this reader. Dropping its Arc alone leaves
            // their pending senders alive, so calls would wait forever after EOF,
            // a read error, or the last event consumer disappearing. Close under
            // the same lock used to register calls, so a later call cannot add a
            // reply slot after this final drain.
            let mut pending = pending.lock().unwrap();
            closed.store(true, Ordering::Release);
            pending.clear();
            ask_lifetimes.close();
        });

        c
    }

    /// A cheaply cloneable handle for issuing calls on this connection.
    pub fn writer(&self) -> Writer {
        self.writer.clone()
    }

    /// The connected daemon endpoint, useful for diagnostics and test fixtures.
    pub fn peer_addr(&self) -> Option<std::net::SocketAddr> {
        self.peer_addr
    }

    /// Split into (writer, event stream, ask stream). The writer may be
    /// shared (cloneable); the event/ask streams are exclusive 闁?hand them to
    /// a single pump task (e.g. the AutoMirror background loop).
    pub fn into_parts(
        self,
    ) -> (
        Writer,
        mpsc::UnboundedReceiver<ServerEvent>,
        mpsc::UnboundedReceiver<ServerAsk>,
    ) {
        (self.writer, self.events_rx, self.asks_rx)
    }

    /// Open a session (convenience on the shared writer).
    pub async fn open_session(&self, spec: term_proto::OpenSpec) -> Result<u64, String> {
        self.writer.open_session(spec).await
    }

    /// Send an arbitrary request and await its reply.
    pub async fn call(&self, method: ClientMethod) -> Result<ServerResult, String> {
        self.writer.call(method).await
    }

    /// Read the daemon's current verified entitlements without presenting a JWT.
    pub async fn license_status(&self) -> Result<Option<term_proto::LicenseInfo>, String> {
        self.writer.license_status().await
    }

    pub async fn offline_license_status(
        &self,
    ) -> Result<term_proto::OfflineLicenseStatusInfo, String> {
        self.writer.offline_license_status().await
    }

    /// Unlock the server's vault with the master password. Returns the per-run
    /// socket token, which also authenticates this connection.
    pub async fn unlock(&self, password: &str) -> Result<String, String> {
        match self
            .call(ClientMethod::Unlock {
                password: password.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Unlocked { token }) => Ok(token),
            ServerResult::Ok(_) => Err("unexpected reply to Unlock".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Authenticate this connection with a token derived from the master
    /// password (see `term_vault::derive_socket_token`).
    pub async fn auth(&self, token: &str) -> Result<(), String> {
        match self
            .call(ClientMethod::Auth {
                token: token.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Authed) => Ok(()),
            ServerResult::Ok(_) => Err("unexpected reply to Auth".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Turn one port forward's debug feed on/off.
    ///
    /// While it is on the daemon reports that forward's connections and traffic
    /// as `CoreEvent::PortMapDebug` on this connection; turning it off stops the
    /// reporting and forgets what it counted.
    pub async fn port_map_debug(
        &self,
        sid: term_proto::Sid,
        kind: term_proto::PortMapKind,
        bind_host: &str,
        bind_port: u32,
        on: bool,
    ) -> Result<(), String> {
        match self
            .call(ClientMethod::PortMapDebug {
                sid,
                params: term_proto::PortMapDebugParams {
                    kind,
                    bind_host: bind_host.to_string(),
                    bind_port,
                    on,
                },
            })
            .await?
        {
            ServerResult::Ok(_) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// The port forwards the session's **host config declares** (`-L`/`-R`/`-D`).
    ///
    /// The daemon owns the host document, so it answers this: the frontend renders
    /// the list in its port-mapping panel and opens the ones the user picks (see
    /// [`ClientMethod::SshForwardPresets`]). Nothing is started by asking.
    pub async fn ssh_forward_presets(
        &self,
        sid: term_proto::Sid,
    ) -> Result<Vec<term_proto::PortForwardInfo>, String> {
        match self.call(ClientMethod::SshForwardPresets { sid }).await? {
            ServerResult::Ok(term_proto::ServerOk::SshForwardPresets { forwards }) => Ok(forwards),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
            other => Err(format!("unexpected reply to SshForwardPresets: {other:?}")),
        }
    }

    /// `(unlocked, has_password, nonce)` 闁?the nonce is public and lets a
    /// frontend that already holds the master key derive the socket token
    /// without sending the password.
    pub async fn vault_status(&self) -> Result<(bool, bool, String), String> {
        match self.call(ClientMethod::VaultStatus).await? {
            ServerResult::Ok(term_proto::ServerOk::VaultStatus {
                unlocked,
                has_password,
                nonce,
            }) => Ok((unlocked, has_password, nonce)),
            ServerResult::Ok(_) => Err("unexpected reply to VaultStatus".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Change one field of one saved host (`close_on_quit`). Returns whether a
    /// host with that id exists.
    ///
    /// A field write, not a whole-file save: the daemon edits its own document,
    /// so the caller does not have to hold a current copy of every other host.
    pub async fn set_close_on_quit(
        &self,
        config_id: u32,
        close_on_quit: bool,
    ) -> Result<bool, String> {
        match self
            .call(ClientMethod::SessionSetCloseOnQuit {
                config_id,
                close_on_quit,
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::SessionUpdated { found }) => Ok(found),
            ServerResult::Ok(_) => Err("unexpected reply to SessionSetCloseOnQuit".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// The daemon's config files, read at the paths the daemon itself writes
    /// them with.
    ///
    /// A frontend should take its configuration from here rather than resolving
    /// `settings.toml`'s declared paths itself: the declaration is relative to
    /// the app root, and a frontend that picked a different base would read a
    /// different file from the one its saves land in.
    pub async fn config_snapshot(&self) -> Result<term_proto::ConfigSnapshot, String> {
        match self.call(ClientMethod::ConfigGetAll).await? {
            ServerResult::Ok(term_proto::ServerOk::Config(snap)) => Ok(snap),
            ServerResult::Ok(_) => Err("unexpected reply to ConfigGetAll".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Publish this connection's volatile host document (see
    /// [`ClientMethod::SshHostsOverlay`]): the frontend's in-memory hosts, so
    /// `SshFromConfig` expands *them* instead of the file. `None` clears it.
    ///
    /// Call it on a connection **before** the opens it should apply to; a
    /// document the daemon cannot parse is an error, not a silent fallback to a
    /// different host.
    pub async fn ssh_hosts_overlay(&self, document: Option<String>) -> Result<(), String> {
        match self
            .call(ClientMethod::SshHostsOverlay { document })
            .await?
        {
            ServerResult::Ok(_) => Ok(()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// The daemon's saved SSH hosts, as a picker renders them. Display fields
    /// only: opening one sends its `config_id` back, and the daemon expands the
    /// stored entry (and any credential) itself. Not vault-gated, so a frontend
    /// can render the list before unlocking.
    pub async fn session_list(&self) -> Result<Vec<term_proto::SessionEntryWire>, String> {
        match self.call(ClientMethod::SessionList).await? {
            ServerResult::Ok(term_proto::ServerOk::Sessions(entries)) => Ok(entries),
            ServerResult::Ok(_) => Err("unexpected reply to SessionList".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// The share state (outgoing share and/or observed share).
    pub async fn share_status(&self) -> Result<term_proto::ShareStatusWire, String> {
        match self.call(ClientMethod::ShareStatus).await? {
            ServerResult::Ok(term_proto::ServerOk::ShareStatus(s)) => Ok(s),
            ServerResult::Ok(_) => Err("unexpected reply to ShareStatus".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Start a LAN share of session `sid`; returns the code observers type.
    pub async fn share_start_lan(
        &self,
        sid: u64,
        name: &str,
        permission: term_proto::SharePermission,
        sharer_name: &str,
        instance_id: &str,
    ) -> Result<ShareStart, String> {
        match self
            .call(ClientMethod::ShareStartLan {
                sid,
                name: name.to_string(),
                permission,
                sharer_name: sharer_name.to_string(),
                sharer_instance_id: instance_id.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::ShareStarted {
                code_or_ticket,
                addr,
            }) => Ok(ShareStart {
                code: code_or_ticket,
                addr,
            }),
            ServerResult::Ok(_) => Err("unexpected reply to ShareStartLan".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Stop the outgoing share (channel and every item).
    pub async fn share_stop(&self) -> Result<(), String> {
        match self.call(ClientMethod::ShareStop).await? {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Ok(_) => Err("unexpected reply to ShareStop".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Share one more session in the running channel; returns its item id.
    pub async fn share_add_tab(
        &self,
        sid: u64,
        name: &str,
        permission: term_proto::SharePermission,
    ) -> Result<String, String> {
        match self
            .call(ClientMethod::ShareAddTab {
                sid,
                name: name.to_string(),
                permission,
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::ShareItemAdded { item_id }) => Ok(item_id),
            ServerResult::Ok(_) => Err("unexpected reply to ShareAddTab".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Join a LAN share at a known address (the code is still the proof).
    pub async fn join_lan_direct_share(
        &self,
        code: &str,
        addr: &str,
        observer_name: &str,
        instance_id: &str,
    ) -> Result<term_proto::ShareStatusWire, String> {
        match self
            .call(ClientMethod::ShareJoinLanDirect {
                code: code.to_string(),
                addr: addr.to_string(),
                observer_name: observer_name.to_string(),
                observer_instance_id: instance_id.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::ShareStatus(s)) => Ok(s),
            ServerResult::Ok(_) => Err("unexpected reply to ShareJoinLanDirect".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Join a LAN share by code (multicast discovery).
    pub async fn join_lan_share(
        &self,
        code: &str,
        observer_name: &str,
        instance_id: &str,
    ) -> Result<term_proto::ShareStatusWire, String> {
        match self
            .call(ClientMethod::ShareJoinLan {
                code: code.to_string(),
                observer_name: observer_name.to_string(),
                observer_instance_id: instance_id.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::ShareStatus(s)) => Ok(s),
            ServerResult::Ok(_) => Err("unexpected reply to ShareJoinLan".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Ask the sharer to (re)send an item's content.
    pub async fn open_shared_item(&self, item_id: &str) -> Result<(), String> {
        match self
            .call(ClientMethod::ShareOpenItem {
                item_id: item_id.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Ok(_) => Err("unexpected reply to ShareOpenItem".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Send collaborative input for a shared item (raw terminal bytes).
    pub async fn send_shared_input(&self, item_id: &str, data: Vec<u8>) -> Result<(), String> {
        match self
            .call(ClientMethod::ShareInput {
                item_id: item_id.to_string(),
                data,
                mouse: None,
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Ok(_) => Err("unexpected reply to ShareInput".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Stop observing a share.
    pub async fn leave_share(&self) -> Result<(), String> {
        match self.call(ClientMethod::ShareLeave).await? {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Ok(_) => Err("unexpected reply to ShareLeave".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }
    /// Post a chat message into the channel's chat room.
    pub async fn share_chat_send(&self, item_id: &str, text: &str) -> Result<(), String> {
        match self
            .call(ClientMethod::ShareChatSend {
                item_id: item_id.to_string(),
                text: text.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Ok(_) => Err("unexpected reply to ShareChatSend".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }
    /// List the vault's stored credentials: `(id, service)` rows plus key ids.
    pub async fn vault_list(&self) -> Result<(Vec<(String, String)>, Vec<String>), String> {
        match self.call(ClientMethod::VaultList).await? {
            ServerResult::Ok(term_proto::ServerOk::VaultList { passwords, keys }) => {
                Ok((passwords, keys))
            }
            ServerResult::Ok(_) => Err("unexpected reply to VaultList".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Read one stored password: `(service, password)`, or `None` if unknown.
    pub async fn vault_get_password(&self, id: &str) -> Result<Option<(String, String)>, String> {
        match self
            .call(ClientMethod::VaultGetPassword { id: id.to_string() })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::VaultPassword { service, password }) => {
                Ok(service.zip(password))
            }
            ServerResult::Ok(_) => Err("unexpected reply to VaultGetPassword".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Read one stored private key: `(pem, passphrase)`, or `None` if unknown.
    pub async fn vault_get_key(
        &self,
        id: &str,
    ) -> Result<Option<(String, Option<String>)>, String> {
        match self
            .call(ClientMethod::VaultGetKey { id: id.to_string() })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::VaultKey {
                key_content,
                passphrase,
            }) => Ok(key_content.map(|c| (c, passphrase))),
            ServerResult::Ok(_) => Err("unexpected reply to VaultGetKey".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Create or replace a stored password.
    pub async fn vault_set_password(
        &self,
        id: &str,
        service: &str,
        password: &str,
    ) -> Result<(), String> {
        match self
            .call(ClientMethod::VaultSetPassword {
                id: id.to_string(),
                service: service.to_string(),
                password: password.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Ok(_) => Err("unexpected reply to VaultSetPassword".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Delete a stored password.
    pub async fn vault_remove_password(&self, id: &str) -> Result<(), String> {
        match self
            .call(ClientMethod::VaultRemovePassword { id: id.to_string() })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Ok(_) => Err("unexpected reply to VaultRemovePassword".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Create or replace a stored private key.
    pub async fn vault_set_key(
        &self,
        id: &str,
        key_content: &str,
        passphrase: Option<&str>,
    ) -> Result<(), String> {
        match self
            .call(ClientMethod::VaultSetKey {
                id: id.to_string(),
                key_content: key_content.to_string(),
                passphrase: passphrase.map(|p| p.to_string()),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Ok(_) => Err("unexpected reply to VaultSetKey".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Delete a stored private key.
    pub async fn vault_remove_key(&self, id: &str) -> Result<(), String> {
        match self
            .call(ClientMethod::VaultRemoveKey { id: id.to_string() })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::Ack) => Ok(()),
            ServerResult::Ok(_) => Err("unexpected reply to VaultRemoveKey".into()),
            ServerResult::Err(e) => Err(format!("{}: {}", e.code, e.message)),
        }
    }

    /// Start recording session `sid` daemon-side (Pro-gated by the daemon).
    pub async fn start_recording(
        &self,
        sid: u64,
        path: &str,
        font_size: f32,
    ) -> Result<String, String> {
        match self
            .call(ClientMethod::RecordingStart {
                sid,
                params: term_proto::RecordingStartParams {
                    path: path.to_string(),
                    font_size,
                },
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::RecordingStarted { path }) => Ok(path),
            ServerResult::Ok(_) => Err("unexpected reply to RecordingStart".into()),
            ServerResult::Err(e) => Err(format!("recording start: {}", e.message)),
        }
    }

    /// Stop recording session `sid`; returns the `.cast` path.
    pub async fn stop_recording(&self, sid: u64) -> Result<String, String> {
        match self.call(ClientMethod::RecordingStop { sid }).await? {
            ServerResult::Ok(term_proto::ServerOk::RecordingStopped { path }) => Ok(path),
            ServerResult::Ok(_) => Err("unexpected reply to RecordingStop".into()),
            ServerResult::Err(e) => Err(format!("recording stop: {}", e.message)),
        }
    }

    /// Ask the daemon whether this connection is allowed a gated `feature`
    /// (the daemon is the authorization authority).
    pub async fn feature_allowed(&self, feature: &str) -> Result<bool, String> {
        match self
            .call(ClientMethod::FeatureAllowed {
                feature: feature.to_string(),
            })
            .await?
        {
            ServerResult::Ok(term_proto::ServerOk::FeatureAllowed { allowed }) => Ok(allowed),
            ServerResult::Err(e) => Err(e.message),
            _ => Err("unexpected reply to FeatureAllowed".into()),
        }
    }

    /// Bring the daemon's DSH connection up (see [`Writer::dsh_connect`]).
    pub async fn dsh_connect(
        &self,
        params: term_proto::DshConnectParams,
    ) -> Result<term_proto::DshStateWire, String> {
        self.writer.dsh_connect(params).await
    }

    /// The daemon's current DSH connection state.
    pub async fn dsh_status(&self) -> Result<term_proto::DshStateWire, String> {
        self.writer.dsh_status().await
    }

    /// Bring the daemon's DSH connection up from its remembered endpoint
    /// (see [`Writer::dsh_reconnect`]).
    pub async fn dsh_reconnect(&self) -> Result<term_proto::DshStateWire, String> {
        self.writer.dsh_reconnect().await
    }

    /// Stop the daemon's DSH connection and any sidecar it spawned.
    pub async fn dsh_stop(&self) -> Result<(), String> {
        self.writer.dsh_stop().await
    }

    /// Run one DSH RPC on the daemon's connection (see [`Writer::dsh_call`]).
    pub async fn dsh_call(
        &self,
        method: &str,
        args: serde_json::Value,
    ) -> Result<term_proto::DshCallOutcomeWire, String> {
        self.writer.dsh_call(method, args).await
    }

    /// Open (idempotently) the daemon's DSH stream hub (see
    /// [`Writer::dsh_hub_open`]).
    pub async fn dsh_hub_open(&self) -> Result<(), String> {
        self.writer.dsh_hub_open().await
    }

    /// Follow one DSH address on the daemon's hub (see [`Writer::dsh_follow`]).
    pub async fn dsh_follow(
        &self,
        address: serde_json::Value,
        max_messages: Option<u32>,
        assistant_stream: bool,
    ) -> Result<(), String> {
        self.writer
            .dsh_follow(address, max_messages, assistant_stream)
            .await
    }

    /// Stop following one DSH key (see [`Writer::dsh_unfollow`]).
    pub async fn dsh_unfollow(&self, key: &str) -> Result<(), String> {
        self.writer.dsh_unfollow(key).await
    }

    /// Answer one DSH waterfall (see [`Writer::dsh_answer`]).
    pub async fn dsh_answer(
        &self,
        event_id: &str,
        outcome: serde_json::Value,
    ) -> Result<(), String> {
        self.writer.dsh_answer(event_id, outcome).await
    }

    /// Drop the daemon's DSH stream hub (see [`Writer::dsh_hub_close`]).
    pub async fn dsh_hub_close(&self) -> Result<(), String> {
        self.writer.dsh_hub_close().await
    }

    /// Forward a web request to the management server via the daemon, which
    /// gates the `feature` on the verified plan. Returns (status, body).
    pub async fn web_proxy(
        &self,
        feature: &str,
        base_url: &str,
        path: &str,
        method: &str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<(u16, Vec<u8>), String> {
        self.writer
            .web_proxy(feature, base_url, path, method, headers, body)
            .await
    }

    /// Freeze or resume a session's local PTY child (daemon-side process).
    pub async fn set_session_frozen(&self, sid: u64, frozen: bool) -> Result<(), String> {
        self.writer.set_session_frozen(sid, frozen).await
    }

    /// Kill a session's local PTY child tree (daemon-side process).
    pub async fn kill_session_process(&self, sid: u64) -> Result<(), String> {
        self.writer.kill_session_process(sid).await
    }

    /// Write bytes into a session's PTY (convenience on the shared writer).
    pub async fn write(&self, sid: u64, data: Vec<u8>) -> Result<(), String> {
        self.writer.write(sid, data).await
    }

    /// Resize a session's grid (convenience on the shared writer).
    pub async fn resize(&self, sid: u64, cols: u16, rows: u16) -> Result<(), String> {
        self.writer.resize(sid, cols, rows).await
    }

    /// Close a session (convenience on the shared writer).
    pub async fn close(&self, sid: u64) -> Result<(), String> {
        self.writer.close(sid).await
    }

    /// Answer a host question (convenience on the shared writer).
    pub async fn answer(
        &self,
        ask_id: u64,
        result: term_proto::AnswerResult,
    ) -> Result<(), String> {
        self.writer.answer(ask_id, result).await
    }

    /// Receiver of server events (snapshots/title/wake/ssh-state/闁?.
    /// Exclusive: hand it to the single pump task.
    pub fn events(&mut self) -> &mut mpsc::UnboundedReceiver<ServerEvent> {
        &mut self.events_rx
    }

    /// Receiver of host questions that must be answered while a session is
    /// being opened (SSH auth). Drain and call [`Client::answer`].
    pub fn asks(&mut self) -> &mut mpsc::UnboundedReceiver<ServerAsk> {
        &mut self.asks_rx
    }
}

// 闁冲厜鍋撻柍鍏夊亾 Framing reader (mirrors the server's FrameReader) 闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾闁冲厜鍋撻柍鍏夊亾

struct FrameReader {
    rh: ReadHalf<transport::BoxStream>,
}

impl FrameReader {
    fn new(rh: ReadHalf<transport::BoxStream>) -> Self {
        Self { rh }
    }

    async fn next_frame(&mut self) -> std::io::Result<Option<(CoreFrameType, Vec<u8>)>> {
        let mut head = [0u8; 5];
        let mut filled = 0usize;
        while filled < 5 {
            let n = self.rh.read(&mut head[filled..]).await?;
            if n == 0 {
                if filled == 0 {
                    return Ok(None);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "EOF in frame header",
                ));
            }
            filled += n;
        }
        let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if len < 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame length < 1",
            ));
        }
        let mut body = vec![0u8; len];
        body[0] = head[4];
        self.rh.read_exact(&mut body[1..]).await?;
        let ftype = CoreFrameType::try_from(body[0]).map_err(|v| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad type {v}"))
        })?;
        Ok(Some((ftype, body[1..].to_vec())))
    }
}

/// Plain text of a frame's damaged rows: the cells the frame carries, per row,
/// with trailing blanks trimmed and rows joined by newlines.
///
/// A real frontend paints the cells; a tool (or a test) that only needs to look
/// for a string on screen can use this instead of running an emulator.
pub fn frame_text(frame: &term_proto::FramePayload) -> String {
    let mut out = String::new();
    for row in &frame.damage {
        if !out.is_empty() {
            out.push('\n');
        }
        let line: String = row.cells.iter().map(|cell| cell.ch).collect();
        out.push_str(line.trim_end());
    }
    out
}
