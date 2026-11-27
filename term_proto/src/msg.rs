//! Typed four-quadrant messages for the terminal-core protocol
//! (design doc 閹?.2 envelope layer + 閹?.4 session methods).
//!
//! Frame type 闁?quadrant mapping (see [`crate::frame::CoreFrameType`]):
//!
//! | frame type    | quadrant          | payload type      |
//! |---------------|-------------------|-------------------|
//! | ClientRequest | client 闁?server   | [`ClientRequest`] |
//! | ServerReply   | server 闁?client   | [`ServerReply`]   |
//! | ServerEvent   | server 闁?client   | [`ServerEvent`]   |
//! | ServerAsk     | server 闁?client   | [`ServerAsk`]     |
//! | ClientAnswer  | client 闁?server   | [`ClientAnswer`]  |
//! | ClientAck     | client 闁?server   | [`ClientAck`]     |
//!
//! Like the share protocol, the payload is msgpack-encoded; unlike share,
//! messages are strongly typed (not a `u8` kind + JSON blob), so both ends
//! get compile-time checking of the session methods in 閹?.4.
//!
//! Session routing: a connection may host many sessions; every
//! session-scoped call/event carries `sid` (assigned by the server on
//! `session/open`). Credentials cross the wire only as **references**
//! (`PassId`/`KeyId`, design doc 閹? risk 3) 闁?plaintext password/key
//! material is asked back through the `ServerAsk`/`ClientAnswer` quadrants
//! and never embedded in `session/open`.

use serde::{Deserialize, Serialize};

/// Synthetic session-id used by the daemon to relay DSH host frames
/// (`CoreEvent::DshFrame`) to the client after an `open_dsh_mux` request. It is
/// far above any real session id, so it never collides with a session.
pub const DSH_MUX_SID: u64 = u64::MAX - 1000;

/// Synthetic session-id used by the daemon to push Lua runtime events
/// (`CoreEvent::LuaEvent`) that are not the answer to a request: script effects
/// produced by a timer or a hook, and the plugin catalog.
pub const LUA_MUX_SID: u64 = u64::MAX - 1001;

/// Synthetic session-id for daemon-wide config notifications
/// (`CoreEvent::ConfigChanged`): they belong to no session, and every frontend
/// sees them.
pub const CONFIG_MUX_SID: u64 = u64::MAX - 1002;

/// Protocol version exchanged by [`ClientMethod::Hello`]. Bump this on any
/// breaking change to a message shape; the daemon refuses mismatched clients
/// with a `proto_version_mismatch` error instead of failing later inside a
/// msgpack decode.
/// 3: `ClientMethod::DshReconnect` (a daemon that re-connects DSH from its own
/// remembered endpoint, so a frontend that has no DSH settings can use it).
/// 4: `ClientMethod::SshHostsOverlay` (a frontend's unsaved hosts live in the
/// daemon, so **every** SSH open is `SshFromConfig` — a daemon that ignored the
/// overlay would silently open the file's version of an edited host).
/// 5: port forwards carry a direction (`-L` / `-R` / `-D`) and name their
/// listener by bind address + port, so a remote or SOCKS forward can be added,
/// listed and removed like a local one.
/// 6: `LicenseStatus` reads the daemon's current verified entitlements without
/// presenting or re-verifying a JWT when a frontend opens another session.
/// 7: `LuaSshExec` redeems a command issued by the daemon's Lua runtime, so a
/// Free plugin can execute it without opening the general paid exec endpoint.
/// 8: every local connection authenticates independently of vault unlock.
/// 9: offline-only local licensing; legacy JWT presentation is removed.
/// 10: activation and delivery require online account authentication; full-term
/// JWTs remain locally verifiable. File import and cancellation RPCs are removed.
/// 11: account snapshots contain current certificates and cloud metadata only;
/// old cache, device-secret, release and legacy clock fields are removed.
/// 12: server-defined products, checkout orders and device-proved voluntary void.
/// 13: content-anchored selection reads from the daemon's full scrollback.
/// 14: account credentials stay daemon-side, relay authentication is attached
/// by the daemon, and local path picking / explicit history scrolling are RPCs.
/// 15: session lifecycle settings come from the daemon's resolved host config.
/// 16: isolated Lua runtimes with one-use exec tickets; daemon-owned document
/// saves and retained-key configuration crypto for standalone GUI frontends.
/// 17: independent TLS frontends, named backend business operations, staged byte
/// transfers and session-bound daemon ShellExec.
pub const PROTO_VERSION: u32 = 17;

/// Capability tags the daemon advertises in its `Hello` reply. Frontends use
/// them to gate UI (e.g. hide a share panel against an older daemon) without
/// probing every method. Add a tag when a phase lands.
pub const DAEMON_FEATURES: &[&str] = &[
    "serial",         // daemon-local serial devices and saved serial profiles
    "connection_auth", // current-user connection credential, independent of the vault
    "ssh_caps",       // SshExec / Sftp* / PortMap* / SshPing / SshCwd
    "shell_exec",     // paid out-of-band commands run on the daemon's shell host
    "sftp_progress",  // CoreEvent::SftpProgress
    "recording",      // RecordingStart/Stop
    "web_proxy",      // WebProxy(ai/history/dsh/team)
    "dsh_mux",        // DshMuxOpen
    "online_activation_v1", // online issuance, full-term JWT, offline device verification
    "license_pools_v2", // one purchase, configurable device seats, explicit activation and device-scoped disable
    "vault",          // Unlock/Auth/VaultStatus: master-password-rooted socket auth
    "vault_config_text", // encrypt/decrypt configuration values without exporting the vault key
    "config_open",    // OpenSpec::SshFromConfig: server-side config expansion
    "config_save_declared", // save settings/SSH/shell documents by daemon-owned identity
    "hosts_overlay",  // SshHostsOverlay: the frontend's unsaved hosts live here too
    "session_list",   // SessionList: the daemon's saved-host list
    "lua_host",       // LuaPass/LuaCommand: the plugin runtime lives in the daemon
    "lua_exec",       // LuaSshExec: redeem a runtime-issued SSH exec request
    "lua_per_connection", // isolated plugin state and events, opaque one-use exec tickets
    "dsh_reconnect",  // DshReconnect: connect DSH from the daemon's own endpoint
    "license_inherit",// a license this daemon verified is offered to later local clients
    "license_status", // query current shared entitlements without presenting a JWT
    "local_path_list", // list daemon-local paths for transfer pickers
    "history_scroll",  // explicit keyboard scrollback, independent of mouse reporting
    "session_settings", // CoreEvent::SessionSettings from the resolved session config
];


/// Unique request id; replies echo it so a client can correlate async results.
pub type RpcId = u64;

/// Session id, assigned by the server on `session/open`.
pub type Sid = u64;

/// A client 闁?server call. Mirrors `client-request` in the dsh wire envelope
/// (design doc 閹?.2): the "method" is now a typed enum instead of a string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientRequest {
    pub rpc_id: RpcId,
    pub method: ClientMethod,
}

/// A connection proof. Debug output must never expose authentication material.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectionProof(String);

impl ConnectionProof {
    pub fn as_str(&self) -> &str { &self.0 }
}

impl From<String> for ConnectionProof {
    fn from(value: String) -> Self { Self(value) }
}

impl std::fmt::Debug for ConnectionProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Session-level methods (subset of 閹?.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMethod {
    /// Handshake: announce the client's protocol version and get the daemon's.
    /// A mismatched daemon answers `proto_version_mismatch` and closes the
    /// connection instead of failing later inside a message decode. Result:
    /// [`ServerOk::Hello`].
    Hello { proto_version: u32, client: String },
    /// Authenticate a local connection using the daemon's per-run credential.
    /// Mutual HMAC proof binds both fresh challenges and never sends the token.
    /// This grants no vault access. Result: [`ServerOk::ConnectionAuthed`].
    ConnectionAuth { client_nonce: String, proof: ConnectionProof },
    /// Handshake: unlock the vault with the master password. Success stores the
    /// master key in the server and returns the per-run socket token, which
    /// authenticates later connections (see [`ClientMethod::Auth`]). Result:
    /// [`ServerOk::Unlocked`].
    Unlock { password: String },
    /// Handshake: authenticate this connection with a socket token derived from
    /// the master password. Result: [`ServerOk::Authed`].
    Auth { token: String },
    /// Ask whether the vault is unlocked and whether the key file is
    /// password-protected (drives the frontend's master-password prompt).
    /// Result: [`ServerOk::VaultStatus`].
    VaultStatus,
    /// Persist one of the frontend's config files. `path` is relative to the
    /// server's config dir (absolute paths and `..` are rejected): the server
    /// owns the files on disk, the frontend owns the editor. Result:
    /// [`ServerOk::Ack`].
    ConfigSave { path: String, contents: String },
    /// Save one daemon-owned document without interpreting a frontend-local path.
    ConfigSaveDeclared { document: ConfigDocument, contents: String },
    /// Read the daemon's own config files, resolved against the same base the
    /// daemon writes them with. Frontends pull their configuration from here
    /// instead of resolving paths themselves — a frontend that joins
    /// `settings.toml`'s `ssh_setting_path` onto a different base reads a
    /// different file from the one the daemon writes. Result:
    /// [`ServerOk::Config`].
    ConfigGetAll,
    /// Change **one field** of one saved host: `close_on_quit`, which a frontend
    /// toggles on a running tab.
    ///
    /// A field write rather than a whole-file save, so recording a boolean does
    /// not depend on the frontend holding a current copy of every other host —
    /// the daemon edits the document itself. Result:
    /// [`ServerOk::SessionUpdated`] (`false` when no host has that id).
    SessionSetCloseOnQuit { config_id: u32, close_on_quit: bool },
    /// List the saved SSH hosts the daemon can open (`ssh.json` /
    /// `settings.toml`'s `ssh_setting_path`, resolved against its own config
    /// dir — the same file `SessionOpen`'s `SshFromConfig` expands). Display
    /// fields only: no credential ever crosses this call, so it is **not**
    /// vault-gated and works on a locked daemon. Result:
    /// [`ServerOk::Sessions`].
    SessionList,
    /// Enumerate serial devices on the daemon's machine.
    SerialPorts,
    /// List a daemon-local directory for a transfer picker. `None` / empty
    /// selects its home; relative paths use its cwd, and `~` expands there.
    /// Connection authentication is required; neither a paid plan nor vault
    /// unlock is needed. Result: [`ServerOk::LocalPathList`].
    LocalPathList { path: Option<String> },
    /// List the vault's stored credentials, ids only. Result:
    /// [`ServerOk::VaultList`]. Secrets never travel unless one is asked for by
    /// id, so a frontend can render the vault without holding every secret.
    VaultList,
    /// Protect a configuration value with the daemon's unlocked vault key.
    /// Requires vault authentication; the encryption key stays in the daemon.
    VaultEncrypt { plaintext: String },
    /// Read a configuration value protected by the daemon's unlocked vault key.
    VaultDecrypt { ciphertext: String },
    /// Daemon-owned GUI business operations. The daemon accepts only named
    /// operations with domain-specific validation and authorization.
    DaemonCall { op: String, payload: serde_json::Value },
    /// Read one stored password by id. Result: [`ServerOk::VaultPassword`]
    /// (`None` when the id is unknown).
    VaultGetPassword { id: String },
    /// Create or replace a stored password. Result: [`ServerOk::Ack`].
    VaultSetPassword { id: String, service: String, password: String },
    /// Delete a stored password. Result: [`ServerOk::Ack`].
    VaultRemovePassword { id: String },
    /// Rename a stored password's id (the entry itself is unchanged). Result:
    /// [`ServerOk::Ack`].
    VaultRenamePassword { from: String, to: String },
    /// Read one stored private key (PEM content + optional passphrase). Result:
    /// [`ServerOk::VaultKey`].
    VaultGetKey { id: String },
    /// Scroll a session's viewport by `delta` lines (positive = toward the
    /// output, matching the wheel path). A frame-fed frontend holds only the
    /// viewport, so the history it scrolls through lives in the daemon: the next
    /// [`CoreEvent::Frame`] is `full`, because the display offset moved.
    /// `column`/`line`/`modifiers` describe the pointer cell the wheel was over
    /// (0-based grid coordinates, modifier bits: shift=1, ctrl=2, alt=4,
    /// logo=8), so an application that reports the mouse scrolls the split under
    /// the pointer instead of the top-left one. They are defaulted so a frontend
    /// that only sends the delta still works.
    /// Result: [`ServerOk::Ack`].
    SessionScroll {
        sid: Sid,
        delta: i32,
        #[serde(default)]
        column: u32,
        #[serde(default)]
        line: i32,
        #[serde(default)]
        modifiers: u8,
    },
    /// Keyboard navigation of terminal history. Unlike `SessionScroll`, this
    /// never becomes remote mouse-wheel or application-cursor input.
    SessionHistoryScroll { sid: Sid, step: HistoryScrollStep },
    /// Run a search step on the session's daemon-side grid.
    ///
    /// A frame-fed frontend has no history, so searching scrolled-back output can
    /// only happen where the grid is: the daemon matches, moves the display
    /// offset to the hit, and sends the matches back in the next
    /// [`CoreEvent::Frame`] (`highlight`). Result: [`ServerOk::Ack`].
    SessionSearch { sid: Sid, step: SearchStep },
    /// Read a selection from the full session grid, independently of the
    /// current viewport. Result: [`ServerOk::SelectionText`].
    SessionSelectionText { sid: Sid, selection: SelectionSpec },
    /// Toggle L2 collapse state for a structured command block. Result: [`ServerOk::Ack`].
    SessionToggleBlockFold { sid: Sid, block_id: u32 },
    /// Enable or disable L2 block folding on the session. Result: [`ServerOk::Ack`].
    SessionSetBlockFolding { sid: Sid, enabled: bool },
    /// Jump viewport to previous (`prev = true`) or next (`prev = false`) command block.
    /// Result: [`ServerOk::Ack`].
    SessionJumpBlock { sid: Sid, prev: bool },
    /// Read a structured command block's full output text from history.
    /// Result: [`ServerOk::SelectionText`].
    SessionBlockOutput { sid: Sid, block_id: u32 },
    /// Create or replace a stored private key. Result: [`ServerOk::Ack`].
    VaultSetKey { id: String, key_content: String, passphrase: Option<String> },
    /// Change only the passphrase of a stored key, leaving the key content
    /// alone. Result: [`ServerOk::Ack`].
    VaultSetKeyPassphrase { id: String, passphrase: Option<String> },
    /// Delete a stored private key. Result: [`ServerOk::Ack`].
    VaultRemoveKey { id: String },
    /// Rename a stored private key's id. Result: [`ServerOk::Ack`].
    VaultRenameKey { from: String, to: String },
    /// Read the share state (outgoing share and/or observed share). Result:
    /// [`ServerOk::ShareStatus`].
    ShareStatus,
    /// Select the share relay and whether relay transport is mandatory. The
    /// daemon supplies its own account credential for its trusted relay.
    ///
    /// The daemon owns the share engine, so without this call a Pro share can
    /// only ever run relay-free (pure P2P): the relay address lives in the
    /// *frontend's* synced global config, and no other method carries it.
    /// Best-effort: a daemon that predates this method rejects it and the share
    /// still starts relay-free, exactly as before. Result: [`ServerOk::Ack`].
    ShareConfigureRelay {
        /// `None` = run relay-free (never fall back to a public relay).
        relay_url: Option<String>,
        /// Server switch: restrict the endpoint to the relay transport.
        force_relay: bool,
        /// Optional caller-owned credential, retained for older clients and
        /// explicitly configured relays. Frontends need not read the daemon's
        /// account token: it attaches that only to its server-configured relay.
        #[serde(default)]
        token: Option<String>,
    },
    /// Start a LAN share of session `sid`; the server binds the listener,
    /// announces the code over multicast and returns the code observers type.
    /// Result: [`ServerOk::ShareStarted`].
    ShareStartLan {
        sid: Sid,
        name: String,
        permission: SharePermission,
        sharer_name: String,
        sharer_instance_id: String,
    },
    /// Start a Pro share of session `sid` (iroh endpoint through the relay).
    /// Result: [`ServerOk::ShareStarted`] carrying the ticket.
    ShareStartPro {
        sid: Sid,
        name: String,
        permission: SharePermission,
        sharer_name: String,
        sharer_instance_id: String,
    },
    /// Share one more session in the running channel. Result:
    /// [`ServerOk::ShareItemAdded`].
    ShareAddTab {
        sid: Sid,
        name: String,
        permission: SharePermission,
    },
    /// Share a local file. Result: [`ServerOk::ShareItemAdded`].
    ShareAddFile { path: String },
    /// Open the channel's chat room (one per channel). Result:
    /// [`ServerOk::ShareItemAdded`].
    ShareAddChat { name: String },
    /// Share a local TCP/UDP port (`protocol`: "tcp" | "udp"). Result:
    /// [`ServerOk::ShareItemAdded`].
    ShareAddPort {
        ip: String,
        port: u16,
        protocol: String,
    },
    /// Stop sharing one item. Result: [`ServerOk::Ack`].
    ShareRemoveItem { item_id: String },
    /// Observer side: stop watching one item (its pane was closed). Releases this
    /// viewer's seat on the sharer (so its panel's "N watching" falls at once) and
    /// cancels any in-flight download of it. Result: [`ServerOk::Ack`].
    ShareCloseItem { item_id: String },
    /// Change one item's permission (propagated to every observer). Result:
    /// [`ServerOk::Ack`].
    ShareSetPermission {
        item_id: String,
        permission: SharePermission,
    },
    /// Post a chat message into the channel's chat room. Result:
    /// [`ServerOk::Ack`].
    ShareChatSend { item_id: String, text: String },
    /// Kick one observer (Pro: by login session; LAN: by observer id). Result:
    /// [`ServerOk::Ack`].
    ShareKick { observer_id: String },
    /// End the outgoing share (channel and every item). Result:
    /// [`ServerOk::Ack`].
    ShareStop,
    /// Join a LAN share by its 6-digit code (multicast discovery, then TCP).
    /// Result: [`ServerOk::ShareStatus`] describing the observed channel.
    ShareJoinLan {
        code: String,
        observer_name: String,
        observer_instance_id: String,
    },
    /// Join a LAN share at a known address (the code is still the proof);
    /// used when discovery is unavailable. Result: [`ServerOk::ShareStatus`].
    ShareJoinLanDirect {
        code: String,
        addr: String,
        observer_name: String,
        observer_instance_id: String,
    },
    /// Join a Pro share by its ticket (iroh QUIC, relay fallback). Result:
    /// [`ServerOk::ShareStatus`].
    ShareJoinPro {
        ticket: String,
        observer_name: String,
        observer_instance_id: String,
    },
    /// Stop observing. Result: [`ServerOk::Ack`].
    ShareLeave,
    /// Ask the core server to check for an update on `channel`. The server owns
    /// the manifest, the download and the staged files; the answer carries the
    /// state and progress arrives as `CoreEvent::Update` pushes. Result:
    /// [`ServerOk::UpdateState`].
    UpdateCheck {
        base_url: String,
        channel: String,
        skipped_version: Option<String>,
        manual: bool,
    },
    /// Start downloading the pending update into the staging directory (the
    /// server transfers and verifies every file). Result: [`ServerOk::Ack`].
    UpdateDownload,
    /// Hand the staged build to the swap-in updater; the frontend exits after
    /// this so the staged process can replace it. Result: [`ServerOk::Ack`].
    UpdateApply,
    /// Run one AI chat turn on the core server: it owns the provider client, the
    /// streamed turn and the tool loop, so the assistant keeps working while the
    /// window is busy. Tokens arrive as `CoreEvent::Ai` pushes. Result:
    /// [`ServerOk::Ack`].
    AiTurn(AiTurnParams),
    /// Cancel the running AI turn on this connection. Result: [`ServerOk::Ack`].
    AiCancel,    /// Read the update state (checking flag, pending update, phase). Result:
    /// [`ServerOk::UpdateState`].
    UpdateState,
    /// Observer 鈫?sharer: ask to open a shared item (the sharer answers with a
    /// fresh snapshot). Result: [`ServerOk::Ack`].
    ShareOpenItem { item_id: String },
    /// Observer: open a local listener for a shared port item and forward its
    /// traffic through the channel to the sharer's target (the server owns both
    /// the tunnel and the listener, so it lives as long as the share does).
    /// Result: [`ServerOk::Ack`].
    ShareOpenPort {
        item_id: String,
        /// Bind address for the local listener (`host:port`).
        bind_addr: String,
        /// `"tcp"` or `"udp"`.
        protocol: String,
    },
    /// Observer: stop forwarding a shared port item and close its listener.
    /// Result: [`ServerOk::Ack`].
    ShareClosePort { item_id: String },
    /// Observer 鈫?sharer: collaborative input for an item (raw terminal bytes,
    /// or a semantic pointer event the sharer applies against its own mouse
    /// mode when `mouse` is set). Result: [`ServerOk::Ack`].
    ShareInput {
        item_id: String,
        data: Vec<u8>,
        /// `Some((x, y, button, pressed))` for a pointer event.
        mouse: Option<(u16, u16, u8, bool)>,
    },
    /// Open a session (local shell or SSH). Result: [`ServerResult::Open`]
    /// carries the assigned `sid`.
    SessionOpen(OpenSessionParams),
    /// Publish this connection's **volatile host document**: the hosts a
    /// frontend holds in memory that are not (or no longer) what
    /// `SshFromConfig` would read from the daemon's host file.
    ///
    /// Same text format as the file (`[[folders]]` / `[[ptys]]`), kept in this
    /// connection's memory only — never written, never visible to another
    /// connection — so the daemon stays the **only** place that turns a host
    /// into a connection. A frontend that has nothing unsaved sends `None` (the
    /// file is the authority); `document: None` also clears an earlier overlay.
    ///
    /// While it is set, `SshFromConfig` expands hosts from it, which is what
    /// makes a host that has never been saved (a new/duplicated entry, or any
    /// entry while this process reads its own file) openable by id.
    /// Result: [`ServerOk::Ack`]; a document that does not parse is
    /// `hosts_overlay_invalid`, never a silent fallback to the file.
    SshHostsOverlay { document: Option<String> },
    /// Feed PTY bytes (keystrokes / paste) to session `sid`.
    SessionWrite { sid: Sid, params: SessionWriteParams },
    /// Resize session `sid` to the given cell grid.
    SessionResize { sid: Sid, params: SessionResizeParams },
    /// Close session `sid` (PTY terminated / SSH disconnected).
    SessionClose { sid: Sid },
    /// Ask for more details about an open session (e.g. current cwd).
    SessionQuery { sid: Sid },
    /// Read the independently verified offline certificate and device status.
    OfflineLicenseStatus,
    /// Read the daemon's current verified entitlements without presenting a JWT.
    /// Available without a paid plan or vault authentication. Result:
    /// [`ServerOk::LicenseStatus`].
    LicenseStatus,
    /// Start recording session `sid` to a `.cast` file daemon-side (Pro-gated).
    /// Result: [`ServerOk::RecordingStarted`].
    RecordingStart { sid: Sid, params: RecordingStartParams },
    /// Stop recording session `sid`. Result: [`ServerOk::RecordingStopped`].
    RecordingStop { sid: Sid },
    /// Forward a web request to the client's management server via the daemon,
    /// which gates the `feature` against the verified plan. Result:
    /// [`ServerOk::WebProxy`]. The daemon only ever forwards to the client's
    /// declared `base_url` host (no open proxy).
    WebProxy(WebProxyParams),
    /// Ask the daemon whether the connection is allowed a gated feature (e.g.
    /// "lua"). Result: [`ServerOk::FeatureAllowed`].
    FeatureAllowed { feature: String },
    /// The daemon's account/license state for this installation. Result:
    /// [`ServerOk::AccountState`]. The daemon owns the account manager — its
    /// tokens, the stored auth record and the device identity — so a frontend
    /// reads the state here instead of holding a second copy of it.
    AccountState,
    /// Run one named account operation on the daemon's account manager:
    /// `op` is a `SyncMgr` method name, `payload` its arguments, and the reply
    /// ([`ServerOk::AccountValue`]) is the serialized return value. One variant
    /// instead of one per method; the typed methods above stay for the
    /// operations whose semantics a generic call cannot express.
    AccountCall { op: String, payload: serde_json::Value },
    /// Run cloud account synchronization without issuing, renewing or clearing
    /// the local license, and report the outcome. Result:
    /// [`ServerOk::AccountSynced`]. The frontend only renders the answer.
    AccountSyncNow,
    /// End the session the daemon holds: revoke the refresh token server-side
    /// (best effort) and clear the stored login. Result: [`ServerOk::Ack`]. The
    /// frontend drops its own copy of the session afterwards — it must not write
    /// the record itself.
    AccountLogout,
    /// Open (idempotently) the daemon's DSH stream hub (Pro or above): the
    /// daemon connects `$events` + `session/control` + `workspace/follow` on the host
    /// connection it owns and pushes every decoded
    /// `easyterm_dsh::hub::HubEvent` as [`CoreEvent::DshHub`].
    /// Result: [`ServerOk::Ack`].
    DshHubOpen,
    /// Follow one session/subagent address on the daemon's hub.
    /// `address` is a serialized `easyterm_dsh::api::session::SessionAddress`;
    /// the follow key is `easyterm_dsh::hub::follow_key` of it, computed
    /// identically on both sides. Result: [`ServerOk::Ack`].
    DshFollow {
        address: serde_json::Value,
        #[serde(default)]
        max_messages: Option<u32>,
        #[serde(default)]
        assistant_stream: bool,
    },
    /// Stop following one key. Result: [`ServerOk::Ack`].
    DshUnfollow { key: String },
    /// Answer one pending waterfall on the daemon's hub (the daemon holds the
    /// `$events` client id). `outcome` is a serialized
    /// `easyterm_dsh::client::EventOutcome`. Result: [`ServerOk::Ack`].
    DshAnswer {
        event_id: String,
        outcome: serde_json::Value,
    },
    /// Drop the daemon's stream hub (every stream ends; the next
    /// `DshHubOpen` rebuilds it). Result: [`ServerOk::Ack`].
    DshHubClose,
    /// Bring the daemon's DSH connection up (Pro or above): probe
    /// `params.host:port`, spawn the local sidecar when `auto_start` allows and the address is
    /// unreachable, bootstrap the auth cookie and keep the client. Result:
    /// [`ServerOk::DshState`]; later changes arrive as [`CoreEvent::DshState`].
    DshConnect(DshConnectParams),
    /// The daemon's current DSH connection state. Result: [`ServerOk::DshState`].
    DshStatus,
    /// Bring the DSH connection up from the endpoint the daemon already knows
    /// (the parameters of the last [`ClientMethod::DshConnect`]).
    ///
    /// A frontend that has no DSH settings of its own — the TUI, whose only
    /// screen is a terminal — cannot fill in host/port/token/auto_start, and the
    /// daemon is the one that owns the connection anyway. The daemon re-runs its
    /// remembered parameters; when it has none it answers
    /// `no_dsh_params` instead of pretending to try. Result:
    /// [`ServerOk::DshState`]; later changes arrive as [`CoreEvent::DshState`].
    DshReconnect,
    /// Stop the daemon's DSH connection and any sidecar it spawned.
    /// Result: [`ServerOk::Ack`].
    DshStop,
    /// Run one DSH RPC on the daemon's connection (Pro or above). The daemon
    /// owns the client and its cookie, so the GUI never holds host
    /// credentials. Result: [`ServerOk::DshValue`].
    DshCall {
        /// The host RPC verb, e.g. `session/list` or `$events/result`.
        method: String,
        /// The `payload.args` object of the call.
        args: serde_json::Value,
    },
    /// Ask the daemon for one followed session's conversation state: its rows,
    /// projections and queued inbox, as the daemon's own chat log holds them
    /// (any frontend renders the same rows). Result: [`ServerOk::DshChat`].
    DshChatOpen { key: String },
    /// Stop following one session's conversation state (its follow was
    /// cancelled, so the daemon drops the log). Result: [`ServerOk::Ack`].
    DshChatClose { key: String },
    /// Feed local file paths to the daemon session's zmodem upload (`rz`): the
    /// backend owns the transfer, so the paths travel to it. Result:
    /// [`ServerOk::Ack`].
    ZmodemUpload { sid: Sid, paths: Vec<String> },
    /// Cancel the daemon session's in-flight zmodem transfer. Result:
    /// [`ServerOk::Ack`].
    ZmodemCancel { sid: Sid },
    /// Run `cmd` over an SSH exec channel on session `sid` (daemon-side, since
    /// the daemon owns the russh connection). Result: [`ServerOk::SshExecStdout`].
    SshExec { sid: Sid, params: SshExecParams },
    /// Execute on this local shell session's daemon host, using its current
    /// working directory and original environment. Result: SshExecStdout.
    ShellExec { sid: Sid, params: SshExecParams },
    /// Execute an SSH command already requested by the daemon's Lua runtime.
    /// The daemon validates the opaque ticket and consumes the matching request
    /// from its issuing connection's runtime before executing;
    /// a client cannot use this endpoint for an arbitrary command. Result:
    /// [`ServerOk::SshExecStdout`].
    LuaSshExec { sid: Sid, request_id: u64, ticket: String, params: SshExecParams },
    /// SFTP list a directory on session `sid`. Result: [`ServerOk::SftpListing`].
    SftpList { sid: Sid, params: SftpListParams },
    /// SFTP read a remote file (download) on session `sid`; the daemon writes
    /// the bytes to `params.local` (daemon/client share the filesystem, like
    /// recording). Result: [`ServerOk::SftpTransferFinished`].
    SftpGet { sid: Sid, params: SftpGetParams },
    /// SFTP write a local file to the remote host (upload) on session `sid`.
    /// Result: [`ServerOk::SftpTransferFinished`].
    SftpPut { sid: Sid, params: SftpPutParams },
    /// SFTP remove a file/dir on session `sid`. Result: [`ServerOk::Ack`].
    SftpRemove { sid: Sid, params: SftpRemoveParams },
    /// SFTP create a directory on session `sid`. Result: [`ServerOk::Ack`].
    SftpMkdir { sid: Sid, params: SftpMkdirParams },
    /// SFTP rename on session `sid`. Result: [`ServerOk::Ack`].
    SftpRename { sid: Sid, params: SftpRenameParams },
    /// Freeze (`frozen: true`) or resume the session's local PTY child — the
    /// process lives daemon-side, so the daemon must do it. Result:
    /// [`ServerOk::Ack`].
    SessionFreeze { sid: Sid, frozen: bool },
    /// Kill the session's local PTY child tree (daemon-side). Result:
    /// [`ServerOk::Ack`].
    SessionKillProcess { sid: Sid },
    /// Add a local闁愁偅濮癳mote port forward on session `sid`. Result: [`ServerOk::Ack`].
    PortMapAdd { sid: Sid, params: PortMapAddParams },
    /// Remove a port forward by local port on session `sid`. Result: [`ServerOk::Ack`].
    PortMapDel { sid: Sid, params: PortMapDelParams },
    /// List port forwards on session `sid`. Result: [`ServerOk::PortMapList`].
    PortMapList { sid: Sid },
    /// The port forwards the session's **host config declares** (`-L`/`-R`/`-D`
    /// presets).
    ///
    /// The daemon owns the host document, so it — not the frontend — is the
    /// authority for this list: the frontend renders it in its port-mapping
    /// panel and the user decides which entries to open with `PortMapAdd`.
    /// Nothing here is started automatically.
    ///
    /// Answering means re-resolving the session's host from the document that
    /// session was opened with (its connection's overlay when it published one,
    /// else the daemon's file), so the list follows the config, including a host
    /// that was never saved. A session opened from a plain spec has no document
    /// to declare anything: empty list.
    /// Result: [`ServerOk::SshForwardPresets`]; a session whose host no longer
    /// resolves is `config_resolve_failed`.
    SshForwardPresets { sid: Sid },
    /// Turn one forward's debug feed on/off (see [`PortMapDebugParams`]).
    ///
    /// Opt-in per forward and per session: nothing is reported until a frontend
    /// asks, which is also what keeps an old frontend from ever receiving the
    /// events. Result: [`ServerOk::Ack`].
    PortMapDebug { sid: Sid, params: PortMapDebugParams },
    /// Ping the SSH server on session `sid` (latency). Result: [`ServerOk::Pong`].
    SshPing { sid: Sid },
    /// Resolve the current working directory of session `sid`'s SSH shell
    /// (daemon-side /proc scan, so the GUI's SFTP pane can follow the session's
    /// cwd in daemon mode). Result: [`ServerOk::Cwd`].
    SshCwd { sid: Sid },
    /// Cancel one daemon-side SFTP transfer on session `sid`, identified by the
    /// `req_id` its `SftpGet`/`SftpPut` carried. Result: [`ServerOk::Ack`]. The
    /// daemon sets that transfer's cancel flag, which its loop checks and aborts
    /// early on. An unknown `req_id` is acknowledged without effect: the transfer
    /// may have finished between the GUI drawing the row and the click. Passing
    /// the id (rather than cancelling "whatever runs") keeps a cancel aimed at one
    /// row from aborting a concurrent transfer started by someone else.
    SftpCancel { sid: Sid, req_id: u64 },
    /// One UI pass of the daemon-hosted Lua runtime: the inputs the frontend
    /// collected while drawing, and who owns the focus. `inputs` is a serialized
    /// `Vec<term_lua::UiInput>`. Result: [`ServerOk::LuaPass`], carrying the
    /// frame to draw (`term_lua::LuaPass`).
    LuaPass {
        focused_chanid: u64,
        focused_sid: u64,
        inputs: serde_json::Value,
    },
    /// Any other command for the daemon-hosted Lua runtime (`term_lua::LuaCommand`,
    /// serialized): session hooks, plugin catalog control, surface closes.
    /// Result: [`ServerOk::Ack`]; whatever the scripts ask for arrives as
    /// [`CoreEvent::LuaEvent`] on [`LUA_MUX_SID`].
    LuaCommand { command: serde_json::Value },
    /// Ask for the plugin catalog. Result: [`ServerOk::LuaPlugins`].
    LuaPlugins,
}

/// Web-proxy params (client 闁?daemon).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebProxyParams {
    /// Feature gate key (e.g. "ai", "history"); maps to a required plan.
    pub feature: String,
    /// Management server base URL (e.g. `https://easyterm.top`).
    pub base_url: String,
    /// Path appended to `base_url` (e.g. `/api/history`).
    pub path: String,
    /// HTTP method (e.g. "GET", "POST").
    pub method: String,
    /// Extra headers (the bearer/API auth the server checks).
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Request body (raw bytes).
    #[serde(default, with = "serde_bytes")]
    pub body: Vec<u8>,
}

/// Recording start params (client 闁?daemon).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingStartParams {
    /// Output `.cast` path (written by the daemon).
    pub path: String,
    pub font_size: f32,
}

/// One command the daemon's backend recorded (prompt detector): the command
/// line, its exit code, an output preview and its timing. The GUI feeds these
/// into its command history and the history sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandRecordWire {
    pub host: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub output_preview: String,
    pub started_at: u64,
    pub duration_ms: u64,
}

/// DSH connection + sidecar params (client 闁?daemon): the projection the GUI's
/// settings page produces, so the daemon can own the connection, the sidecar
/// process and its launch token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DshConnectParams {
    /// Master switch: whether the daemon manages a local `dsh web` sidecar.
    pub enabled: bool,
    /// Loopback host the host/sidecar listens on (dsh only supports 127.0.0.1).
    pub host: String,
    /// Port the host/sidecar listens on.
    pub port: u16,
    /// Launch token of an EXTERNAL host, already normalized (a locally spawned
    /// sidecar captures its own token from the process stdout instead).
    pub token: Option<String>,
    /// Start the sidecar automatically when the address is unreachable.
    pub auto_start: bool,
    /// Explicit dsh executable; `None` discovers `dsh`/`npx` on PATH.
    pub dsh_command: Option<String>,
    /// Working directory the spawned process runs in.
    pub run_path: Option<String>,
    /// Readiness probes before giving up.
    pub ready_attempts: u32,
    /// Delay between readiness probes (milliseconds).
    pub ready_delay_ms: u64,
}

/// DSH connection state (daemon 闁?client), mirrored by the GUI's status rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DshStateWire {
    /// Monotonic daemon state revision. Frontends ignore queued older pushes
    /// after a newer connect/status reply; older daemons omit this field.
    #[serde(default)]
    pub revision: u64,
    /// `disconnected` | `connecting` | `spawning` | `spawned` | `connected` |
    /// `unreachable`.
    pub state: String,
    /// The address the GUI shows next to the state.
    pub port: u16,
    /// Which failure this is, so the GUI picks its localized template:
    /// `connect_failed` | `no_auto_start` | `spawn_failed` | `ready_failed` |
    /// `license_required` | `account_changed`.
    pub error_kind: Option<String>,
    /// Raw detail of the last failure (technical text, never localized here).
    pub error_detail: Option<String>,
    /// True when the connection is backed by a sidecar this daemon spawned.
    pub local_sidecar: bool,
}

/// Outcome of one relayed DSH RPC (`DshCall`): either the host's business
/// answer, or the transport-level failure — shaped so the GUI's client can
/// rebuild the identical error it would have seen talking to the host directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DshCallOutcomeWire {
    /// The host's business value for a successful call.
    Value(serde_json::Value),
    /// The host answered with a business error (`rpcId` echo + error envelope).
    Rpc {
        code: String,
        message: String,
        #[serde(default)]
        details: Option<serde_json::Value>,
    },
    /// The host refused the request as unauthenticated (HTTP 401).
    Unauthorized { body: String },
    /// The host answered with a non-2xx status.
    Http { status: u16, body: String },
    /// The answer did not speak the protocol (bad echo, unparsable body).
    Protocol { message: String },
}

/// UI-safe status; never contains the device private key or an account credential.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OfflineLicenseStatusInfo {
    pub state: String,
    pub message: Option<String>,
    pub subject_id: Option<String>,
    pub plan: Option<String>,
    pub validity: Option<String>,
    pub expires_at: Option<i64>,
    pub protection_level: Option<String>,
    pub device_key_id: Option<String>,
    pub binding_id: Option<String>,
    pub certificate_id: Option<String>,
    /// Display-only purchased product name, retained for offline presentation.
    #[serde(default)]
    pub product_title: Option<String>,
}

/// SSH exec params (client 闁?daemon).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshExecParams {
    /// Shell command to run on the remote host.
    pub cmd: String,
    /// Timeout (ms) bounding the command run.
    pub timeout_ms: u64,
}

/// SFTP list params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SftpListParams {
    /// Remote path to list.
    pub path: String,
}

/// SFTP download params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SftpGetParams {
    /// Remote source path.
    pub remote: String,
    /// Local destination path (daemon and client share the filesystem, so the
    /// daemon writes the same path the client would).
    pub local: String,
    /// Client-supplied transfer id, echoed back in progress events so the GUI
    /// can attribute progress to the exact row for repeated same-file transfers.
    #[serde(default)]
    pub req_id: u64,
}

/// SFTP upload params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SftpPutParams {
    /// Local source path.
    pub local: String,
    /// Remote destination path.
    pub remote: String,
    /// Client-supplied transfer id, echoed back in progress events so the GUI
    /// can attribute progress to the exact row for repeated same-file transfers.
    #[serde(default)]
    pub req_id: u64,
}

/// SFTP remove params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SftpRemoveParams {
    /// Remote path to remove.
    pub path: String,
    /// True when removing a directory.
    pub is_dir: bool,
}

/// SFTP mkdir params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SftpMkdirParams {
    /// Remote directory to create.
    pub path: String,
}

/// SFTP rename params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SftpRenameParams {
    pub old_path: String,
    pub new_path: String,
}

/// Which way a port forward runs (mirrors the config's `-L` / `-R` / `-D`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PortMapKind {
    /// `-L`: listen on `bind`, dial `host:port` from the **remote** side.
    Local,
    /// `-R`: ask the server to listen on `bind`, and dial `host:port` from this
    /// side for every connection it accepts.
    Remote,
    /// `-D`: listen on `bind` as a SOCKS5 proxy; the client names the target.
    /// `host`/`port` are unused.
    Dynamic,
}

/// Port-forward add params. `bind_host`/`bind_port` are the listener;
/// `host`/`port` the destination (`Dynamic` ignores those two).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortMapAddParams {
    pub kind: PortMapKind,
    /// Address to listen on. Empty = "127.0.0.1" (local/dynamic) or the
    /// server's default for a remote forward.
    pub bind_host: String,
    /// Listener port. 0 asks the server to pick one (`-R` only).
    pub bind_port: u32,
    /// Destination host (`Local`: dialled from the remote side; `Remote`:
    /// dialled from here).
    pub host: String,
    /// Destination port.
    pub port: u32,
}

/// Port-forward remove params. The listener is named by its bind address and
/// port — the same pair the add used, so two forwards that differ only in their
/// destination stay distinguishable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortMapDelParams {
    pub kind: PortMapKind,
    pub bind_host: String,
    pub bind_port: u32,
}

/// Turn one forward's **debug feed** on or off.
///
/// On: the daemon starts reporting that forward's connections and traffic
/// (`CoreEvent::PortMapDebug`) — who connected, who left, how many bytes each
/// way, and a 1 Hz rate while it runs. Off: it stops and forgets, so a
/// frontend's log only ever holds what happened while debugging was on.
/// Result: [`ServerOk::Ack`]; `portmap_not_found` when the forward is gone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortMapDebugParams {
    pub kind: PortMapKind,
    pub bind_host: String,
    pub bind_port: u32,
    pub on: bool,
}

/// One port-forward debug event.
///
/// Bytes are `(uploaded, downloaded)` from the **forward's** point of view:
/// uploaded = sent to the target through the tunnel, downloaded = received from
/// it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PortMapDebugEventWire {
    /// A peer connected to the forward's listener.
    Connected { peer: String },
    /// That peer's connection ended. `seconds` is how long it lasted.
    Disconnected {
        peer: String,
        uploaded: u64,
        downloaded: u64,
        seconds: u64,
    },
    /// A forwarded connection could not be established (the far side refused it,
    /// or never answered).
    Failed { peer: String, error: String },
    /// One second of this forward: the rates over that second, how many
    /// connections are open, and one of their peers (for the status line).
    Stats {
        uploaded_bps: u64,
        downloaded_bps: u64,
        peers: u32,
        peer: Option<String>,
    },
}

/// One SFTP directory entry. Mirrors the GUI's `(name, is_dir, modified,
/// owner, perm_bits, size_bytes)` tuple so the daemon can round-trip a listing
/// losslessly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SftpEntry {
    pub name: String,
    pub is_dir: bool,
    /// Modification time (unix seconds).
    pub modified: Option<i64>,
    /// Owner and group (e.g. `alice:staff`), with numeric IDs when names cannot be resolved.
    pub owner: Option<String>,
    /// POSIX permission bits.
    pub mode: Option<u32>,
    /// File size in bytes.
    pub size: Option<u64>,
}

/// One active port forward, as `PortMapList` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortForwardInfo {
    pub kind: PortMapKind,
    /// The listener that was actually bound: `bind_port` is the resolved port,
    /// so a `-R` request for port 0 reports what the server picked.
    pub bind_host: String,
    pub bind_port: u32,
    /// Destination (`Dynamic` leaves both empty — the peer names it).
    pub host: String,
    pub port: u32,
}

/// Daemon-verified effective license entitlements (reported to the client).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LicenseInfo {
    pub plan: String,
    pub is_pro: bool,
    pub is_team: bool,
    /// Explicitly retained through every local capability gate.
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub validity: LicenseValidity,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LicenseValidity {
    #[default]
    Unlicensed,
    Perpetual,
    Until { expires_at: i64 },
}

impl LicenseInfo {
    pub fn is_expired_at(&self, now: i64) -> bool {
        match self.validity {
            LicenseValidity::Unlicensed => true,
            LicenseValidity::Perpetual => false,
            LicenseValidity::Until { expires_at } => expires_at <= now,
        }
    }

    pub fn allows_local_at(&self, now: i64) -> bool {
        self.scope.len() == 1 && self.scope[0] == "local" && !self.is_expired_at(now)
    }
}

/// The daemon's view of this installation's account/license state
/// ([`ClientMethod::AccountState`]).
///
/// The daemon owns the account manager (its tokens, the stored auth record and
/// the device identity), so a frontend asks for this instead of keeping its own
/// copy of the same database rows. Display/diagnostic fields only: nothing here
/// is a credential — the tokens and the device secret stay daemon-side.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountState {
    /// A refreshable session is held (access token valid, or refresh valid).
    pub logged_in: bool,
    pub username: Option<String>,
    /// The independent offline certificate's verified plan, when available.
    pub plan: Option<String>,
    /// The plan the daemon would authorize with right now (free when the
    /// license is expired, refused by the trusted clock, or absent).
    pub effective_plan: String,
    /// Management-server base URL this daemon's account manager talks to.
    pub base_url: String,
    #[serde(default)]
    pub uid: Option<i64>,
    #[serde(default)]
    pub certificate_id: Option<String>,
    #[serde(default)]
    pub cloud_license_status: Option<String>,
    #[serde(default)]
    pub clock_hold: bool,
    #[serde(default)]
    pub license_now_secs: i64,
    #[serde(default)]
    pub login_session_id: Option<String>,
    #[serde(default)]
    pub token_exp: Option<i64>,
    #[serde(default)]
    pub cloud_expires_at: Option<i64>,
    #[serde(default)]
    pub license_expires_at: Option<i64>,
    #[serde(default)]
    pub last_cloud_sync: Option<i64>,
    /// Server-delivered share configuration the daemon's manager holds.
    #[serde(default)]
    pub iroh_relay_addr: Option<String>,
    #[serde(default)]
    pub force_relay: bool,
    #[serde(default)]
    pub share_quota: Option<serde_json::Value>,
    pub license_locked: bool,
}

/// One selectable entry in the daemon's local filesystem. Paths are absolute
/// and use the daemon's platform syntax; the frontend must not resolve them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalPathEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    /// File length; directories have no size.
    pub size: Option<u64>,
}

/// A bounded directory snapshot for transfer pickers. Hidden files are included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalPathListInfo {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<LocalPathEntry>,
    pub separator: char,
    /// Daemon-provided volume roots (for example `C:\\` and `D:\\`). Empty on
    /// platforms without drive letters and when reading an older daemon reply.
    #[serde(default)]
    pub roots: Vec<LocalPathEntry>,
    /// Some entries were omitted because the response limit was reached or
    /// their native names could not be represented by a protocol string.
    pub truncated: bool,
}

/// What to open in `session/open`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenSessionParams {
    pub spec: OpenSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OpenSpec {
    /// Local shell PTY.
    Shell(ShellOpenSpec),
    /// A serial device owned by the daemon.
    Serial(SerialOpenSpec),
    /// SSH session (connection + main shell channel).
    Ssh(SshOpenSpec),
    /// SSH session described by a **config id**: the server expands the saved
    /// host (folder inheritance, jump-server chain, SOCKS proxy) itself, so a
    /// frontend does not have to duplicate that logic 闁?and cannot silently
    /// drop fields its own spec type lacks.
    SshFromConfig {
        config_id: u32,
        cols: u16,
        rows: u16,
    },
}

/// Default for a serial line option that stays on unless a caller turns it
/// off explicitly: frontends built before the option existed omit the field.
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SerialOpenSpec {
    pub device: String,
    pub baud_rate: u32,
    pub data_bits: u8,
    pub stop_bits: u8,
    pub parity: String,
    pub flow_control: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_encoding: Option<String>,
    /// Keyboard/paste newline mapping: cr, lf, crlf, or raw.
    pub newline: String,
    pub local_echo: bool,
    /// Assert DTR when the port opens. Absent means yes; most USB-CDC devices
    /// keep their command console silent while DTR is low.
    #[serde(default = "default_true")]
    pub dtr: bool,
    /// Assert RTS when the port opens. Absent means yes. Hardware flow control
    /// drives the line itself, so the backend skips this in that mode.
    #[serde(default = "default_true")]
    pub rts: bool,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialPortInfo {
    pub device: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellOpenSpec {
    /// Encoding of terminal output; absent means UTF-8. Input remains UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_encoding: Option<String>,
    /// Shell binary; default = `$COMSPEC` on Windows / `$SHELL` elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Optional working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Extra argv passed to `shell`. A shell-group child is `cmd args`, and the
    /// GUI used to spawn that PTY itself; now the server runs it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment for the child (`WT_SESSION`, `COLORTERM`, user vars).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    /// Initial grid size in cells.
    pub cols: u16,
    pub rows: u16,
}

/// SSH connection parameters. Credentials are references, never material.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshOpenSpec {
    /// Encoding of terminal output; absent means UTF-8. Input remains UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_encoding: Option<String>,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// Ordered authentication candidates; first success wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth: Vec<SshAuthRef>,
    /// Initial grid size in cells.
    pub cols: u16,
    pub rows: u16,
    /// SOCKS5 proxy to dial through (`host:port` or `socks5://[user:pass@]host:port`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socks_proxy: Option<String>,
    /// Keepalive interval in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive_secs: Option<u32>,
    /// Connect timeout in seconds: how long the daemon may spend dialing this
    /// hop and completing its SSH handshake before giving up and reporting the
    /// failure. This is the session setting "timeout(s)". Without it an address
    /// that accepts a TCP connection but never sends an SSH banner (blackholed
    /// route, filtered port, a non-SSH service) hangs the open forever, because
    /// russh's `inactivity_timeout` is deliberately off.
    ///
    /// Auth is not covered: an interactive prompt may take as long as it takes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_timeout_secs: Option<u32>,
    /// Bastion to dial first; the server opens a direct-tcpip channel through
    /// it to reach `host`. Each bastion carries its own settings and auth refs;
    /// a nested `jump` is connected first, forming a chain of SSH tunnels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<Box<SshOpenSpec>>,
    /// Ordered key-exchange algorithm names the client offers for this hop.
    /// Empty = russh's default preference. Legacy algorithms (diffie-hellman-group1-sha1,
    /// etc.) may be added freely; unknown names are ignored when the list is applied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kex_algorithms: Vec<String>,
    /// Ordered symmetric cipher names the client offers. Empty = russh's default.
    /// Includes CBC/CTR/GCM/ChaCha20 variants and (when built with the `des`
    /// feature) `3des-cbc` for very old routers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ciphers: Vec<String>,
    /// Ordered MAC algorithm names the client offers. Empty = russh's default
    /// (SHA-1 variants excluded). Legacy `hmac-sha1` may be added explicitly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub macs: Vec<String>,
    /// Ordered host/public-key signature algorithm names the client accepts.
    /// Empty = russh's default (ed25519/ecdsa/rsa-sha2). Legacy `ssh-rsa`
    /// (SHA-1) and `ssh-dss` may be added for old devices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_key_algorithms: Vec<String>,
    /// Enable zlib compression for this hop. `None` = russh's default
    /// (off unless the `flate2` feature is built and the server asks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<bool>,
}

/// Wire reference to an authentication credential. No plaintext secrets 闁?/// passwords/keys live in the client's vault and are referenced by id.
/// Anything the server still needs from the user (a vault unlock, a live
/// password) goes through the `ServerAsk`/`ClientAnswer` quadrants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SshAuthRef {
    /// Vault password entry id.
    PassId(String),
    /// Vault key entry id (key content + optional passphrase live client-side).
    KeyId(String),
    /// Keyboard-interactive; optional cached response id.
    Interact(Option<String>),
    /// ssh-agent on the server host.
    SshAgent,
    /// Pageant (Windows) on the server host.
    Pageant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionWriteParams {
    /// Raw bytes to write into the PTY (UTF-8 text or terminal control seqs).
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionResizeParams {
    pub cols: u16,
    pub rows: u16,
}

/// Server 闁?client reply to a [`ClientRequest`], echoing `rpc_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerReply {
    pub rpc_id: RpcId,
    pub result: ServerResult,
}

/// A document whose save destination is resolved by the daemon itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigDocument { Settings, Ssh, Shell, Serial }

/// The daemon's config files, as `ConfigGetAll` returns them.
///
/// Contents are verbatim file text: the frontend is the editor and the daemon
/// is the owner, so there is no re-encoding step in between that could drop a
/// field one side does not model. A file that does not exist yet is `None`
/// (a fresh install), which is not an error — an unreadable or malformed
/// `settings.toml` is, and lands in `errors` so the frontend can say which.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    /// The frontend's `settings.toml` (the daemon reads it to find the rest).
    pub settings: Option<String>,
    /// The declared `ssh_setting_path` file (saved hosts).
    pub ssh: Option<String>,
    /// The declared `shell_setting_path` file (shells and shell groups).
    pub shell: Option<String>,
    /// The declared serial settings file. Absent on older daemons.
    #[serde(default)]
    pub serial: Option<String>,
    /// `(path, reason)` for each config file that exists but could not be read.
    #[serde(default)]
    pub errors: Vec<(String, String)>,
}

impl ConfigSnapshot {
    /// The first read error, for a one-line status message.
    pub fn first_error(&self) -> Option<&(String, String)> {
        self.errors.first()
    }
}

/// One saved SSH host, as a picker needs it (`SessionList`).
///
/// Deliberately display-only: the frontend renders these rows and sends back an
/// `OpenSpec::SshFromConfig` `config_id`, and the daemon resolves the real
/// connection (including any vault reference) itself. The `auth` entries are
/// **method names** (`password`/`pass_id`/`key_id`/`interact`/`sshagent`/
/// `pageant`), never the reference or secret behind them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEntryWire {
    /// The `session/open` argument.
    pub config_id: u32,
    /// The user's label for the host.
    pub name: String,
    /// Resolved address (folder inheritance already applied).
    pub host: String,
    pub port: u16,
    pub username: String,
    /// Folder chain, `"prod/web"`, when the host sits in one.
    #[serde(default)]
    pub folder: Option<String>,
    /// Auth method names in the file's order (first success wins).
    #[serde(default)]
    pub auth: Vec<String>,
    /// Bastion, when the host has a jump server: its name (else its address).
    #[serde(default)]
    pub jump: Option<String>,
}

impl SessionEntryWire {
    /// `user@host:port`, with the port elided when it is the default 22.
    pub fn target(&self) -> String {
        let user = if self.username.is_empty() {
            String::new()
        } else {
            format!("{}@", self.username)
        };
        if self.port == 22 {
            format!("{user}{}", self.host)
        } else {
            format!("{user}{}:{}", self.host, self.port)
        }
    }

    /// `password+key_id` style hint for a row's auth column.
    pub fn auth_label(&self) -> String {
        if self.auth.is_empty() {
            "-".to_string()
        } else {
            self.auth.join("+")
        }
    }
}

/// Business result slot: ok value or error code+message (design doc 閹?.2
/// `result = { ok:true, value } | { ok:false, error:{code,message} }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerResult {
    Ok(ServerOk),
    Err(ServerErr),
}

/// Success values produced by session methods.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerOk {
    /// Reply to `Hello`: the daemon's protocol version and the capability tags
    /// it serves (see [`DAEMON_FEATURES`]).
    Hello { proto_version: u32, features: Vec<String>, auth_id: String, server_nonce: String },
    /// Server's proof of the same per-run connection secret and transcript.
    ConnectionAuthed { proof: ConnectionProof },
    /// Reply to `Unlock`: the per-run socket token to reuse on later
    /// connections.
    Unlocked { token: String },
    /// Reply to `Auth`: this connection may use vault-backed operations.
    Authed,
    /// Reply to `VaultStatus`.
    VaultStatus {
        /// The server currently holds the master key.
        unlocked: bool,
        /// The key file exists and is password-protected (false = no key file
        /// yet, or a legacy plaintext key).
        has_password: bool,
        /// This run's random nonce (hex). Public 闁?a frontend that already
        /// holds the master key mixes it into the socket token instead of
        /// sending the password over the socket.
        nonce: String,
    },
    /// Echoed back from `SessionOpen`; the session to target with later calls.
    Open { sid: Sid },
    /// Generic empty ack.
    Ack,
    /// Reply to `SessionSelectionText`; `None` for an empty/expired selection.
    SelectionText { text: Option<String> },
    /// Reply to `SessionQuery`.
    Query { cwd: Option<String> },
    OfflineLicenseStatus(OfflineLicenseStatusInfo),
    /// Reply to `LicenseStatus`: the daemon's current verified entitlements,
    /// or `None` when no current license is available.
    LicenseStatus(Option<LicenseInfo>),
    /// Reply to `RecordingStart`: the `.cast` file path.
    RecordingStarted { path: String },
    /// Reply to `RecordingStop`: the `.cast` file path.
    RecordingStopped { path: String },
    /// Reply to `WebProxy`: the forwarded HTTP response.
    WebProxy { status: u16, body: Vec<u8> },
    /// Reply to `FeatureAllowed`: whether the connection's plan permits it.
    FeatureAllowed { allowed: bool },
    /// Reply to `AccountState`: the daemon's account/license state.
    AccountState(AccountState),
    /// Reply to `AccountCall`: the serialized return value of the operation.
    AccountValue(serde_json::Value),
    /// Reply to `AccountSyncNow`: the daemon ran the whole post-auth sync
    /// (event queue, pending seat release, license verification, subscription
    /// refresh). `error` is `None` on success, otherwise the daemon's reason.
    AccountSynced { #[serde(default)] error: Option<String> },

    /// Reply to `DshConnect`/`DshStatus`: the connection state.
    DshState(DshStateWire),
    /// Reply to `DshCall`: what the host answered.
    DshValue(DshCallOutcomeWire),
    /// Reply to `DshChatOpen`: the session's conversation state.
    DshChat(serde_json::Value),
    /// Reply to `SessionList`: the saved SSH hosts, in file order.
    Sessions(Vec<SessionEntryWire>),
    /// Serial devices attached to the daemon's machine.
    SerialPorts { ports: Vec<SerialPortInfo> },
    /// Reply to `LocalPathList`: daemon-local paths, directories first.
    LocalPathList(LocalPathListInfo),
    /// Reply to `ConfigGetAll`: the daemon's config files, verbatim.
    Config(ConfigSnapshot),
    /// Reply to `SessionSetCloseOnQuit`: whether a saved host had that id.
    SessionUpdated { found: bool },
    /// Reply to `LuaPass`: the frame to draw, the answers to the pass's
    /// requests, and whatever the scripts asked for while it ran (a serialized
    /// `term_lua::LuaPass`).
    LuaPass(serde_json::Value),
    /// Reply to `LuaPlugins`: the plugin catalog (serialized
    /// `Vec<term_lua::PluginInfo>`).
    LuaPlugins(serde_json::Value),
    /// Reply to `SshExec`: the command's stdout bytes and exit code.
    SshExecStdout { stdout: Vec<u8>, exit_code: i32 },
    /// Reply to `SftpList`: the directory entries.
    SftpListing { entries: Vec<SftpEntry> },
    /// Reply to a started `SftpGet`/`SftpPut` after its progress relay drained.
    /// `current` is actual transferred bytes, including zero for empty files;
    /// `error` carries the transfer failure/cancellation, if any. Keeping the
    /// final counters and outcome in one reply avoids races with event pumps.
    SftpTransferFinished { current: u64, total: u64, error: Option<ServerErr> },
    /// Reply to `PortMapList`.
    PortMapList { forwards: Vec<PortForwardInfo> },    /// Reply to `PortMapAdd`: the listener that was actually bound (`bind_port`
    /// is resolved, so a request for port 0 reports what the OS or the server
    /// picked).
    PortMapAdded { forward: PortForwardInfo },
    /// Reply to `SshForwardPresets`: what the session's host config declares, in
    /// file order, with each listener normalised (an empty bind address becomes
    /// the default) so the frontend can offer it as-is and compare it against
    /// [`ServerOk::PortMapList`].
    SshForwardPresets { forwards: Vec<PortForwardInfo> },
    /// Reply to `SshPing`: round-trip latency in ms.
    Pong { ms: u64 },
    /// Reply to `SshCwd`: the session's SSH shell working directory.
    Cwd { cwd: String },
    /// Reply to `VaultList`: stored passwords as `(id, service)` and the ids of
    /// stored private keys.
    VaultList {
        passwords: Vec<(String, String)>,
        keys: Vec<String>,
    },
    /// A configuration value encrypted/decrypted with the vault's retained key.
    VaultText { text: String },
    DaemonValue(serde_json::Value),
    /// Reply to `VaultGetPassword`. Both fields are `None` when the id is
    /// unknown; `service` is the display name, the password is decrypted.
    VaultPassword {
        service: Option<String>,
        password: Option<String>,
    },
    /// Reply to `VaultGetKey`. Both `None` when the id is unknown.
    VaultKey {
        key_content: Option<String>,
        passphrase: Option<String>,
    },
    /// Reply to `UpdateCheck` / `UpdateState`: what the update dialog renders.
    UpdateState(UpdateStateWire),
    /// Reply to `ShareStatus`: everything the share/join panels render.
    ShareStatus(ShareStatusWire),
    /// Reply to `ShareStartLan` / `ShareStartPro`: the code an observer types
    /// (LAN) or the ticket it pastes (Pro).
    ShareStarted {
        code_or_ticket: String,
        /// Where a LAN share can be dialed directly (127.0.0.1:port-style
        /// host:port), when the server knows it. None for Pro (the ticket
        /// carries its own addressing).
        addr: Option<String>,
    },
    /// Reply to `ShareAddTab` / `ShareAddFile` / `ShareAddChat` /
    /// `ShareAddPort`: the new item's id.
    ShareItemAdded { item_id: String },
}

// 閳光偓閳光偓 Direct sharing: the core server owns the share, the frontend renders it 閳光偓閳光偓

/// Permission on the wire, mirroring the share protocol's `Permission`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SharePermission {
    ReadOnly,
    Collaborative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShareModeWire {
    Lan,
    Pro,
}

/// What is being shared. A terminal is referenced by the **server's** `sid`:
/// the server owns the session and therefore the content it streams.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ShareItemKindWire {
    Tab { sid: Sid },
    File { path: String, size: Option<u64> },
    Port { local_addr: String },
    Chat,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShareItemWire {
    pub id: String,
    pub name: String,
    pub kind: ShareItemKindWire,
    pub permission: SharePermission,
    /// Traffic this item has carried on the active share (the panels' speed and
    /// total rows read it).
    pub stats: TrafficStatsWire,
}

/// Cumulative traffic on one share connection or item, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TrafficStatsWire {
    pub bytes_in: u64,
    pub bytes_out: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShareObserverWire {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub instance_id: String,
    pub is_relay: bool,
    pub viewing_items: Vec<String>,
    /// Traffic on this observer's connection.
    pub stats: TrafficStatsWire,
    /// Per-item traffic on this connection (`item id`, stats).
    pub item_stats: Vec<(String, TrafficStatsWire)>,
}

/// The share state a frontend draws: the share it hosts and/or the share it
/// watches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShareStatusWire {
    /// This process is sharing something.
    pub sharing: bool,
    /// Stable id of the active share (both roles). `None` when idle.
    pub share_id: Option<String>,
    /// The sharer's synced account name, for the member list.
    pub sharer_name: String,
    /// The sharer's client instance id (4 hex chars), disambiguating several
    /// logins of the same account.
    pub sharer_instance_id: String,
    /// Permission the channel grants its observers.
    pub permission: SharePermission,
    /// LAN code / Pro ticket of the active outgoing share.
    pub code_or_ticket: Option<String>,
    pub mode: Option<ShareModeWire>,
    pub items: Vec<ShareItemWire>,
    pub observers: Vec<ShareObserverWire>,
    /// This process is watching someone else's share.
    pub observing: bool,
    /// Items of the observed share.
    pub observed_items: Vec<ShareItemWire>,
}

/// One AI chat turn: everything the server needs to talk to the provider. The
/// credential is resolved by the server (it owns the vault), so no API key
/// travels on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiTurnParams {
    /// Provider base URL (empty = the provider's default).
    pub base_url: String,
    /// `"open_ai"` or `"anthropic"`.
    pub provider: String,
    pub model: String,
    /// Whether the provider may use its reasoning/thinking mode.
    pub enable_thinking: bool,
    /// Vault entry holding the API token (the server resolves it).
    pub token_pass_id: Option<String>,
    /// Legacy inline encrypted token, used when no vault entry is set.
    pub token_encrypted: Option<String>,
    /// Ordered conversation, oldest first.
    pub messages: Vec<AiMessageWire>,
    pub timeout_secs: u64,
    /// Tools the model may call. The frontend owns the registry, so it sends the
    /// definitions; the calls themselves come back over the ask corridor.
    #[serde(default)]
    pub tools: Vec<AiToolDefWire>,
}

/// One tool the model may call, as advertised to the provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiToolDefWire {
    /// Registered name (`namespace.short_name`).
    pub name: String,
    pub description: String,
    /// JSON Schema object for the tool's parameters.
    pub parameters: serde_json::Value,
    /// Per-tool timeout in seconds (0 = the engine's default).
    #[serde(default)]
    pub timeout_secs: u64,
}

/// One message of an AI conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiMessageWire {
    /// `"user"`, `"assistant"` or `"system"`.
    pub role: String,
    pub content: String,
    /// Image data URLs attached to this message.
    #[serde(default)]
    pub images: Vec<String>,
}

/// AI pushes: the events the in-process engine produced, minus its in-process
/// cancellation handles (cancelling is an `AiCancel` call).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AiEventWire {
    /// A token (or token batch) of the answer.
    Delta(String),
    /// A token of the model's reasoning stream.
    Reasoning(String),
    /// The answer finished.
    Done,
    /// The turn failed (provider error, transport, bad config).
    Error(String),
    /// A tool call started; `args` is the provider's JSON arguments.
    ToolStarted {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    /// A tool call finished; `result` is the `{ok, result, error}` envelope the
    /// engine builds.
    ToolFinished {
        id: String,
        result: serde_json::Value,
        duration_ms: u64,
    },
}

/// The update dialog's state, as the core server reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UpdateStateWire {
    /// A check is running.
    pub checking: bool,
    /// The pending update, when one was found.
    pub update: Option<UpdateInfoWire>,
    pub phase: UpdatePhaseWire,
    /// Feedback line for the settings panel's "check now" button.
    pub last_check_msg: Option<String>,
    /// Why the check or the download failed, when it did.
    pub error: Option<String>,
    /// Bytes downloaded / total for the running transfer.
    pub downloaded: u64,
    pub total: u64,
}

/// The pending update's identity, size and notes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateInfoWire {
    pub version: String,
    pub notes: String,
    /// The release must be installed before the app is usable again.
    pub force: bool,
    pub download_bytes: u64,
    pub channel: String,
}

/// Which part of the update flow the dialog shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum UpdatePhaseWire {
    /// Release notes, waiting for the user's choice.
    #[default]
    Prompt,
    Downloading,
    /// Download verified, waiting for a restart.
    Ready,
    Error,
}

/// Update pushes: the same events the engine used to send in-process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UpdateEventWire {
    /// A check finished; `state.update` is `None` when the build is current.
    CheckDone { state: UpdateStateWire },
    /// Transfer progress.
    Progress { downloaded: u64, total: u64 },
    /// The transfer finished (`error` is `None` on success).
    DownloadDone {
        error: Option<String>,
        state: UpdateStateWire,
    },
}

/// Share pushes. Either side's state or content, depending on the role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ShareEventWire {
    /// The observed channel's item list changed (join, add, remove, kick).
    List { items: Vec<ShareItemWire> },
    /// The watcher list changed (sharer side).
    Observers { observers: Vec<ShareObserverWire> },
    /// A chat message arrived in a chat item.
    ChatMessage {
        item_id: String,
        sender: String,
        text: String,
    },
    /// Chat member list changed.
    ChatMembers { item_id: String, members: Vec<String> },
    /// The share ended (stopped locally, or the peer went away).
    Closed,
    /// Something went wrong (relay denied, quota, transport).
    Error(String),
    /// Full-screen snapshot of an observed item (opens/rebuilds its pane).
    Snapshot {
        item_id: String,
        #[serde(with = "serde_bytes")]
        ansi: Vec<u8>,
        cols: u16,
        rows: u16,
        cell_w: f32,
        cell_h: f32,
    },
    /// Incremental output of an observed item.
    Data {
        item_id: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// The sharer removed one item from the channel.
    ItemClosed { item_id: String },
    /// One observed item was closed for this observer only.
    KickedFromItem { item_id: String },
    /// The sharer refused an `ShareOpenItem` for this item.
    OpenRejected { item_id: String, reason: String },
    /// History of an observed chat item.
    ChatHistory {
        item_id: String,
        messages: Vec<(String, String)>,
    },
    /// A file download started for an observed item.
    FileOffer {
        item_id: String,
        name: String,
        size: u64,
    },
    /// One chunk of an observed download arrived.
    FileProgress { item_id: String, received: u64 },
    /// The download finished; `temp_path` holds the bytes.
    FileDone {
        item_id: String,
        temp_path: String,
        md5_hex: String,
    },
    /// The download was aborted.
    FileAborted { item_id: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerErr {
    pub code: String,
    pub message: String,
}

/// Server 闁?client push, scoped to a session.
/// Fire-and-forget; no `rpc_id` (a client that cares tracks its own state).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerEvent {
    pub sid: Sid,
    pub event: CoreEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CoreEvent {
    /// New terminal output was parsed; client should mark its screen dirty
    /// and pull the next snapshot. Mirrors `PtyEvent` wakeup semantics.
    Wake,
    /// A viewport frame (cells + cursor + mode bits): the **only** content
    /// channel of a session. The frontend paints the cells it receives and keeps
    /// no emulator of its own, so the daemon's grid is the only terminal.
    /// `damage` carries only the rows that changed, so an echo costs one row.
    Frame(FramePayload),
    /// OSC title changed (e.g. `ESC ] 0 ; title BEL`).
    Title { title: String },
    /// Terminal bell.
    Bell,
    /// PTY exited / SSH channel closed. `code` when known, `reason` when the
    /// daemon's transport reported why it ended (a serial port unplugged or
    /// failed, for instance) so a frontend can show it instead of a bare exit.
    Exit {
        code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Lifecycle policy captured from the configuration used for this open.
    /// Sent before the session's Exit event; direct SSH/shell default to false.
    SessionSettings { close_on_quit: bool },
    /// Somebody saved a config file on the daemon (another frontend, or a tool
    /// talking the protocol). Sent to **every** connection, including the one
    /// that saved, on [`CONFIG_MUX_SID`]: the daemon owns the files, so a
    /// frontend that cares re-pulls with [`ClientMethod::ConfigGetAll`] rather
    /// than trusting its own copy.
    ///
    /// `path` is the daemon-relative path that changed (`configs/pty_settings.toml`).
    ConfigChanged { path: String },
    /// SSH session state change (connecting 闁?authenticated 闁?pty ready).
    SshState(SshState),
    /// One step of bringing an SSH session up, pushed while `session/open` is still
    /// running so the tab can show progress (the terminal is not up yet). `stage` is
    /// wire-stable: "connect", "kex", "auth", "pty"; `ok=false` marks the step that
    /// failed, after which the open returns its error.
    ConnectStage { stage: String, ok: bool },
    /// The daemon's DSH connection changed state (connect/sidecar lifecycle).
    DshState(DshStateWire),
    /// One DSH conversation update: the rows a frontend should replace from
    /// `from_seq` onwards (or all of them), plus any projections/queue that
    /// changed. Sent on [`DSH_MUX_SID`].
    DshChat { update: serde_json::Value },
    /// One decoded DSH hub event (`easyterm_dsh::hub::HubEvent`, serialized):
    /// the daemon owns the mux and the streams, every frontend renders the same
    /// feed. Sent on [`DSH_MUX_SID`].
    DshHub { event: serde_json::Value },
    /// One Lua runtime event that is not the answer to a request
    /// (`term_lua::LuaEvent`, serialized): script effects produced by a timer,
    /// a hook or a load, the plugin catalog, a load error. Sent on
    /// [`LUA_MUX_SID`].
    LuaEvent { event: serde_json::Value },
    /// SFTP transfer progress relayed by the daemon (daemon-mode SSH, where the
    /// transfer runs daemon-side and the GUI cannot see `SshPubEvent` progress).
    /// `is_upload` distinguishes upload from download; `name` is the file's
    /// remote path (download) or local path (upload), matching the GUI's
    /// existing `ReportSftp*Progress` naming so the transfer row matches up.
    SftpProgress { is_upload: bool, name: String, current: u64, total: u64, req_id: u64 },
    /// Zmodem transfer progress/state for a session the daemon owns (the GUI's
    /// in-process `ReportZmodem*` events have no producer any more).
    Zmodem(ZmodemEventWire),
    /// One debug event for one of the session's port forwards.
    ///
    /// Only sent while a frontend has asked for that forward's feed
    /// (`ClientMethod::PortMapDebug`), so a frontend that never asks never sees
    /// this variant. `forward` identifies which listener the event belongs to.
    PortMapDebug {
        forward: PortForwardInfo,
        event: PortMapDebugEventWire,
    },
    /// The backend's OSC title returned to none (its title stack was popped
    /// or its config was reloaded). No shell escape produces this — an empty
    /// `OSC 0;` arrives as `Title("")` — so a frontend treats it as "no
    /// title" rather than expecting it often.
    ResetTitle,
    /// OSC 9;4 progress for a session (`state` 0 clears it, otherwise
    /// `progress` is a 0..=100 percentage).
    Progress { state: u8, progress: u8 },
    /// Child-process diff for a session (Windows shell PTYs): processes that
    /// appeared / disappeared since the previous diff.
    ChildDiff {
        added: Vec<(u32, String)>,
        removed: Vec<(u32, String)>,
    },
    /// Commands recorded by the daemon's prompt detector for a session it runs
    /// (the GUI's mirrors never see the detector). Pro-gating happens in the
    /// frontend, which is what stores or discards the history.
    CommandRecords { sid: Sid, records: Vec<CommandRecordWire> },
    /// Share state/content push (the server owns the share session).
    Share(ShareEventWire),
    /// Update progress/state push (the server owns the update engine).
    Update(UpdateEventWire),
    /// AI turn push: deltas, reasoning, tool calls, completion (the server owns
    /// the assistant's provider client and tool loop).
    Ai(AiEventWire),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SshState {
    Connecting,
    Authenticating,
    Ready,
    Disconnected,
}

/// One color of a frame cell.
///
/// The frame carries the terminal's **palette slot**, never a resolved RGB: the
/// frontend owns the theme, so a theme change repaints every session without
/// asking the daemon for new content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameColor {
    /// Named palette slot, numbered like
    /// `alacritty_terminal::vte::ansi::NamedColor` (0..=15 are the ANSI colors,
    /// 256+ the foreground/background/cursor slots). A number keeps this crate
    /// independent of the terminal crate.
    Named(u16),
    /// 256-color palette index (16..=255).
    Indexed(u8),
    /// True color, `0xRRGGBB`.
    Spec(u32),
}

/// Cursor shape as the application asked for it (`DECSCUSR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameCursorShape {
    Block,
    Beam,
    Underline,
    HollowBlock,
    Hidden,
}

/// Per-cell reference to a slice of a placed terminal graphic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameGraphicCell {
    pub id: u64,
    pub row: u16,
    pub col: u16,
    pub total_rows: u16,
    pub total_cols: u16,
    pub src_x: u16,
    pub src_y: u16,
    pub src_w: u16,
    pub src_h: u16,
    pub z_index: i16,
}

/// Decoded RGBA8 graphic image synced once from daemon to frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameGraphic {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    #[serde(with = "serde_bytes")]
    pub pixels: Vec<u8>,
}

/// One character cell of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameCell {
    pub ch: char,
    pub fg: FrameColor,
    pub bg: FrameColor,
    /// `alacritty_terminal::term::cell::Flags` bits: bold, italic, the underline
    /// flavours, inverse, strikeout, and the wide-char markers.
    pub flags: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graphic: Option<FrameGraphicCell>,
}

/// One grid row of a frame.
///
/// Trailing default-blank cells are trimmed: the frontend fills the remaining
/// columns with the terminal background, which is also what keeps a keystroke's
/// frame at one cell per typed character instead of a whole line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameRow {
    /// Grid line, 0-based in the viewport.
    pub line: i32,
    pub cells: Vec<FrameCell>,
}

/// Selection semantics, matching the terminal emulator's selection modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SelectionKind {
    Simple,
    Block,
    Semantic,
    Lines,
}

/// Deliberate keyboard scrolling, independent of terminal mouse reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryScrollStep {
    PageUp,
    PageDown,
    Top,
    Bottom,
}

/// Content coordinates use negative lines for scrollback, with the scroll
/// counter identifying the frame at which those coordinates were recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionSpec {
    pub kind: SelectionKind,
    pub start: (i32, u32),
    pub start_right: bool,
    pub end: (i32, u32),
    pub end_right: bool,
    pub content_scrolled: u64,
}

/// A viewport frame: what to paint, where the cursor is, and the mode bits a
/// frontend needs to interpret input.
///
/// This is the **thin-client** content channel: a frontend that asks for frames
/// (`ClientMethod::ContentMode { frames: true }`) paints cells directly and keeps
/// no terminal emulator of its own, so a session has exactly one emulator — the
/// daemon's — and there is no second state machine to keep in sync.
///
/// One search step sent to the daemon ([`ClientMethod::SessionSearch`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SearchStep {
    /// Start (or restart) a search. `regex` selects the regex matcher.
    Begin { query: String, regex: bool },
    /// Move to the next (`forward`) or previous match.
    Next { forward: bool },
    /// Stop searching and clear the highlights.
    End,
}

/// One search match, in viewport cell coordinates (start is inclusive, end is
/// the last cell of the match).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameSpan {
    pub start: (i32, u32),
    pub end: (i32, u32),
}

/// Viewport-relative summary of a structured command block sent inside [`FramePayload`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameBlock {
    pub id: u32,
    pub start_row: i32,
    pub end_row: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_row: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_row: Option<i32>,
    #[serde(default)]
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub output_lines: u32,
    #[serde(default)]
    pub is_semantic: bool,
    #[serde(default)]
    pub clipped_top: bool,
    #[serde(default)]
    pub clipped_bottom: bool,
}

/// **Damage:** unless [`FramePayload::full`] is set, `damage` holds only the
/// lines that differ from this session's previous frame. `full` is set for the
/// first frame after attach, after a resize, and whenever the display offset
/// moved (scrolling shows other lines).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FramePayload {
    pub cols: u16,
    pub screen_lines: u16,
    /// Scrollback length, so the frontend can draw a proportional scrollbar.
    pub history: usize,
    /// Lines scrolled up from the bottom (0 = following the output).
    pub display_offset: usize,
    /// Cumulative lines the session's content has scrolled. The frontend uses
    /// the delta between two frames to move content-anchored state (a text
    /// selection) together with the text; the cells alone cannot express that.
    /// Defaults to 0 so a frame from an older daemon still decodes (it just
    /// cannot report scrolling).
    #[serde(default)]
    pub content_scrolled: u64,
    /// Cursor cell, `None` while the application has it hidden (`?25l`).
    pub cursor: Option<(i32, u32)>,
    pub cursor_shape: FrameCursorShape,
    pub cursor_blinking: bool,
    /// `alacritty_terminal::term::TermMode` bits (app cursor, bracketed paste,
    /// alt screen, mouse modes, …): the frontend needs them to encode input.
    pub mode: u32,
    /// `true` when `damage` covers the whole viewport.
    pub full: bool,
    pub damage: Vec<FrameRow>,
    /// Search matches the daemon found. It owns the history, so it is the only
    /// side that can search beyond the viewport. Only the spans that overlap the
    /// viewport are listed; [`Self::highlight_total`] is the real count.
    pub highlight: Vec<FrameSpan>,
    /// Total matches found, including the ones scrolled out of view: a "N
    /// matches" counter must show this, not the number of spans above.
    pub highlight_total: usize,
    /// The match the search is currently on (viewport coordinates), so the
    /// frontend can paint it differently from the other matches.
    pub current_match: Option<FrameSpan>,
    /// 0-based index of that match in the whole list, for a `3 / 7` counter.
    pub highlight_current: Option<usize>,
    /// Newly added decoded graphics not yet sent to this frontend.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub graphics_added: Vec<FrameGraphic>,
    /// IDs of graphics evicted/deleted since the previous frame.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub graphics_dropped: Vec<u64>,
    /// Structured command blocks intersecting the viewport (L1/L2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<FrameBlock>,
}

// 闁冲厜鍋撻柍鍏夊亾 Host-question quadrant (server 闁?client ask / client 闁?server answer) 闁冲厜鍋撻柍鍏夊亾

/// Server 闁?client question that the host UI/TUI must answer (kbdint,
/// passphrase, host-key confirmation闁?. Design doc 閹?.1 闁? the question is
/// UI-agnostic; the platform renders it, the answer travels back here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerAsk {
    pub ask_id: u64,
    pub sid: Option<Sid>,
    pub kind: AskKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AskKind {
    /// Keyboard-interactive (server challenge or client password fallback).
    KbdInteractive(KbdInteractiveAsk),
    /// Confirm an unknown SSH host key before continuing.
    HostKeyConfirm(HostKeyAsk),
    /// Ask the client to resolve a vault credential reference (PassId/KeyId).
    /// The vault lives in the client; the daemon cannot open it, so it asks
    /// the client to supply the concrete secret through the corridor.
    VaultResolve(VaultResolveAsk),
    /// Ask the client to run one AI tool call. The engine's tool loop runs in
    /// the daemon, but the tools (plugin/Lua ones especially) live in the
    /// frontend, so each call travels the corridor and the answer carries the
    /// engine's `{ok, result, error}` envelope back.
    AiToolCall(AiToolCallAsk),
    /// Ask the frontend for a zmodem transfer's local path. The transfer runs in
    /// the daemon with the SSH session it owns, but picking a file/destination is
    /// UI: the answer is [`AnswerResult::Submit`] with the chosen path, or
    /// [`AnswerResult::Cancel`] to abort the transfer.
    ZmodemPath(ZmodemPathAsk),
}

/// One zmodem path prompt for the frontend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZmodemPathAsk {
    /// The daemon session the transfer belongs to (the frontend maps it to its
    /// own channel for the status row).
    pub sid: Sid,
    /// Human label of the session, for the dialog title.
    pub session_label: String,
    /// `true` for a download (choose where to save; `name` is the suggested
    /// file name), `false` for an upload (choose the file to send).
    pub save: bool,
    /// Suggested file name for a download.
    pub name: Option<String>,
    /// Size the host announced for a download, when known.
    pub size: Option<u64>,
}

/// Zmodem transfer events the daemon pushes for a session it owns (the frontend
/// renders them as transfer rows / history entries; the transfer itself, its
/// files and its progress are daemon-side).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ZmodemEventWire {
    /// Files were dropped on the window: an upload of these paths is starting.
    UploadDropped { sid: Sid, paths: Vec<String> },
    /// Upload progress for one file of one transfer.
    UploadProgress {
        sid: Sid,
        file_idx: u64,
        total_files: u64,
        /// Original local upload path, used by file history actions.
        name: String,
        current: u64,
        total: u64,
    },
    /// Download progress (`current`/`total` bytes).
    DownloadProgress { sid: Sid, current: u64, total: u64 },
    /// Cancellation or a protocol error was reported; recovery may still be running.
    Cancelled { sid: Sid },
    /// The transfer ended (or was cancelled): drop the status row.
    Ended { sid: Sid },
    /// An accepted upload batch, including files queued behind a running transfer.
    UploadQueued { sid: Sid, files: u64 },
    /// A queued batch expired before the upload handshake started.
    UploadDiscarded { sid: Sid, files: u64 },
}

/// One AI tool call for the frontend to execute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiToolCallAsk {
    /// Registered tool name (`namespace.short_name`).
    pub tool: String,
    /// The provider's JSON arguments.
    pub args: serde_json::Value,
    /// Per-tool timeout the registry advertised, in seconds.
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KbdInteractiveAsk {
    /// Human label for the target session.
    pub session_label: String,
    pub name: String,
    pub instructions: String,
    /// (prompt_text, echo, optional prefill)
    pub prompts: Vec<(String, bool, Option<String>)>,
    /// ServerChallenge vs client-side PasswordFallback (localize the latter).
    pub kind: KbdKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KbdKind {
    ServerChallenge,
    PasswordFallback,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostKeyAsk {
    pub host: String,
    /// The key the server offered (base64 blob), for the host UI to display.
    pub key_blob: String,
    /// The host is already known but offered a *different* key: the UI should
    /// warn about key rotation / a possible MITM instead of showing a plain
    /// first-use prompt. The server asks only when the key is unknown or
    /// changed 闁?already-accepted keys are trusted without a round trip.
    #[serde(default)]
    pub changed: bool,
}

/// Server 闁?client: resolve one vault credential reference for an SSH open.
/// The client unlocks its vault (asking the user for the master password if
/// needed) and answers with [`AnswerResult::VaultSecret`]; the secret then
/// lives only in the corridor answer 闁?never in `session/open`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VaultResolveAsk {
    /// Human label for the target session (user@host).
    pub session_label: String,
    /// Which stored credential the open references.
    pub kind: VaultRefKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VaultRefKind {
    /// Stored password (vault `pass_id`).
    Password { pass_id: String },
    /// Stored private key (vault `key_id`).
    PrivateKey { key_id: String },
}

/// Client 闁?server answer to a [`ClientAnswer`] for a `VaultResolve` ask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultSecret {
    /// For `Password` refs: the stored password.
    pub password: Option<String>,
    /// For `PrivateKey` refs: the key material plus an optional passphrase.
    pub private_key: Option<VaultKeySecret>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultKeySecret {
    /// PEM (or OpenSSH-format) private key the vault stored.
    pub pem: String,
    /// Passphrase of the stored key, if any.
    pub passphrase: Option<String>,
}

/// Client 闁?server answer to a [`ServerAsk`], echoing `ask_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientAnswer {
    pub ask_id: u64,
    pub result: AnswerResult,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AnswerResult {
    /// Submit typed responses (kbdint prompts).
    Submit(Vec<String>),
    /// User cancelled the question (auth aborts).
    Cancel,
    /// Host key accepted / rejected.
    AcceptHostKey,
    RejectHostKey,
    /// Resolved vault secret for a `VaultResolve` ask (client-side vault).
    VaultSecret(VaultSecret),
    /// Result of an `AiToolCall` ask: the engine's `{ok, result, error}`
    /// envelope, exactly as the in-process dispatcher would have produced it.
    ToolResult(serde_json::Value),
}

/// Client 闁?server acknowledgment (reserved for rare ack-required pushes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientAck {
    pub seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_snapshot_discards_legacy_credentials_in_json_and_msgpack() {
        let state = AccountState {
            logged_in: true,
            username: Some("synthetic-user".into()),
            effective_plan: "pro".into(),
            base_url: "https://accounts.example.test".into(),
            uid: Some(42),
            login_session_id: Some("synthetic-session".into()),
            ..Default::default()
        };
        // Older daemons included the account bearer. Accept their named-map
        // payload, but do not retain it in memory, Debug output or any reply.
        let mut old = serde_json::to_value(&state).unwrap();
        old["access_token"] = "synthetic-access-secret".into();
        old["refresh_token"] = "synthetic-refresh-secret".into();
        let json_state: AccountState = serde_json::from_value(old.clone()).unwrap();
        let old_msgpack = rmp_serde::to_vec_named(&old).unwrap();
        let msgpack_state: AccountState = rmp_serde::from_slice(&old_msgpack).unwrap();
        for received in [state.clone(), json_state, msgpack_state] {
            assert_eq!(received, state);
            let json = serde_json::to_value(&received).unwrap();
            assert!(json.get("access_token").is_none());
            assert!(json.get("refresh_token").is_none());
            let reply = ServerReply {
                rpc_id: 7,
                result: ServerResult::Ok(ServerOk::AccountState(received)),
            };
            let encoded = rmp_serde::to_vec_named(&reply).unwrap();
            for secret in ["synthetic-access-secret", "synthetic-refresh-secret"] {
                assert!(!json.to_string().contains(secret));
                assert!(!format!("{reply:?}").contains(secret));
                assert!(!encoded.windows(secret.len()).any(|window| window == secret.as_bytes()));
            }
            assert_eq!(rmp_serde::from_slice::<ServerReply>(&encoded).unwrap(), reply);
        }
    }

    #[test]
    fn connection_credentials_roundtrip_without_debug_exposure() {
        let secret = "synthetic-connection-secret";
        let req = ClientRequest {
            rpc_id: 1,
            method: ClientMethod::ConnectionAuth { client_nonce: "0".repeat(64), proof: secret.to_owned().into() },
        };
        assert!(!format!("{req:?}").contains(secret));
        let encoded = rmp_serde::to_vec_named(&req).unwrap();
        assert_eq!(rmp_serde::from_slice::<ClientRequest>(&encoded).unwrap(), req);
    }

    #[test]
    fn dsh_state_revision_defaults_for_older_daemons() {
        let state: DshStateWire = serde_json::from_value(serde_json::json!({
            "state": "disconnected", "port": 0, "error_kind": null,
            "error_detail": null, "local_sidecar": false,
        })).unwrap();
        assert_eq!(state.revision, 0);
    }

    #[test]
    fn lua_ssh_exec_roundtrip_preserves_the_runtime_request() {
        let req = ClientRequest {
            rpc_id: 31,
            method: ClientMethod::LuaSshExec {
                sid: 17,
                request_id: 23,
                ticket: "opaque-runtime-issued-ticket".into(),
                params: SshExecParams {
                    cmd: "printf 'Lua exec'".into(),
                    timeout_ms: 2500,
                },
            },
        };
        let bytes = rmp_serde::to_vec_named(&req).unwrap();
        let back: ClientRequest = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn shell_open_roundtrip_via_msgpack() {
        let req = ClientRequest {
            rpc_id: 7,
            method: ClientMethod::SessionOpen(OpenSessionParams {
                spec: OpenSpec::Shell(ShellOpenSpec {
                    display_encoding: Some("GB18030".into()),
                    args: Vec::new(),
                    env: Vec::new(),
                    shell: None,
                    cwd: None,
                    cols: 80,
                    rows: 24,
                }),
            }),
        };
        let bytes = rmp_serde::to_vec_named(&req).unwrap();
        let back: ClientRequest = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn ssh_open_roundtrip_with_auth_refs() {
        let req = ClientRequest {
            rpc_id: 1,
            method: ClientMethod::SessionOpen(OpenSessionParams {
                spec: OpenSpec::Ssh(SshOpenSpec {
                    display_encoding: Some("Big5".into()),
                    host: "example.com".into(),
                    port: 22,
                    username: "root".into(),
                    auth: vec![
                        SshAuthRef::KeyId("k1".into()),
                        SshAuthRef::PassId("p1".into()),
                        SshAuthRef::Pageant,
                    ],
                    cols: 80,
                    rows: 24,
                socks_proxy: None,
                keepalive_secs: None,
                connect_timeout_secs: None,
                jump: None,
                kex_algorithms: Vec::new(),
                ciphers: Vec::new(),
                macs: Vec::new(),
                host_key_algorithms: Vec::new(),
                compression: None,
                }),
            }),
        };
        let bytes = rmp_serde::to_vec_named(&req).unwrap();
        let back: ClientRequest = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, req);
        // No plaintext anywhere on the wire.
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("BEGIN") && !text.contains("secret"));
    }

    #[test]
    fn ask_answer_roundtrip_via_msgpack() {
        let ask = ServerAsk {
            ask_id: 9,
            sid: Some(3),
            kind: AskKind::KbdInteractive(KbdInteractiveAsk {
                session_label: "prod".into(),
                name: "pam".into(),
                instructions: "enter token".into(),
                prompts: vec![("Password".into(), false, None)],
                kind: KbdKind::PasswordFallback,
            }),
        };
        let bytes = rmp_serde::to_vec_named(&ask).unwrap();
        let back: ServerAsk = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, ask);

        let ans = ClientAnswer {
            ask_id: 9,
            result: AnswerResult::Submit(vec!["s3cret".into()]),
        };
        let bytes = rmp_serde::to_vec_named(&ans).unwrap();
        let back: ClientAnswer = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, ans);
    }

    #[test]
    fn host_key_ask_roundtrip() {
        let ask = ServerAsk {
            ask_id: 2,
            sid: None,
            kind: AskKind::HostKeyConfirm(HostKeyAsk {
                host: "1.2.3.4".into(),
                key_blob: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI...".into(),
                changed: false,
            }),
        };
        let bytes = rmp_serde::to_vec_named(&ask).unwrap();
        let back: ServerAsk = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, ask);
    }

    #[test]
    fn session_write_carries_sid() {
        let req = ClientRequest {
            rpc_id: 2,
            method: ClientMethod::SessionWrite {
                sid: 5,
                params: SessionWriteParams { data: b"echo hi\r".to_vec() },
            },
        };
        let bytes = rmp_serde::to_vec_named(&req).unwrap();
        let back: ClientRequest = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn vault_resolve_roundtrip_via_msgpack() {
        let ask = ServerAsk {
            ask_id: 11,
            sid: Some(4),
            kind: AskKind::VaultResolve(VaultResolveAsk {
                session_label: "alice@example.com".into(),
                kind: VaultRefKind::PrivateKey {
                    key_id: "key-7".into(),
                },
            }),
        };
        let bytes = rmp_serde::to_vec_named(&ask).unwrap();
        let back: ServerAsk = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, ask);

        let ans = ClientAnswer {
            ask_id: 11,
            result: AnswerResult::VaultSecret(VaultSecret {
                password: None,
                private_key: Some(VaultKeySecret {
                    pem: "-----BEGIN OPENSSH PRIVATE KEY-----...".into(),
                    passphrase: None,
                }),
            }),
        };
        let bytes = rmp_serde::to_vec_named(&ans).unwrap();
        let back: ClientAnswer = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, ans);
    }

    #[test]
    fn reply_roundtrip_via_msgpack() {
        let reply = ServerReply {
            rpc_id: 7,
            result: ServerResult::Ok(ServerOk::Open { sid: 42 }),
        };
        let bytes = rmp_serde::to_vec_named(&reply).unwrap();
        let back: ServerReply = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, reply);
    }

    #[test]
    fn sftp_transfer_finished_roundtrip_via_msgpack() {
        for (current, total, error) in [
            (0, 0, None),
            (8192, 16384, Some(ServerErr {
                code: "sftp_upload_failed".into(),
                message: "sftp upload cancelled".into(),
            })),
        ] {
            let reply = ServerReply {
                rpc_id: 7,
                result: ServerResult::Ok(ServerOk::SftpTransferFinished { current, total, error }),
            };
            let bytes = rmp_serde::to_vec_named(&reply).unwrap();
            let back: ServerReply = rmp_serde::from_slice(&bytes).unwrap();
            assert_eq!(back, reply);
        }
    }

    #[test]
    fn frame_roundtrip_via_msgpack() {
        let ev = ServerEvent {
            sid: 1,
            event: CoreEvent::Frame(FramePayload {
                cols: 80,
                screen_lines: 24,
                history: 0,
                display_offset: 0,
                content_scrolled: 0,
                cursor: Some((0, 0)),
                cursor_shape: FrameCursorShape::Block,
                cursor_blinking: false,
                mode: 0,
                full: true,
                damage: vec![FrameRow {
                    line: 0,
                    cells: vec![FrameCell {
                        ch: 'h',
                        fg: FrameColor::Named(256),
                        bg: FrameColor::Named(257),
                        flags: 0,
                        graphic: None,
                    }],
                }],
                highlight: Vec::new(),
                highlight_total: 0,
                current_match: None,
                highlight_current: None,
                graphics_added: Vec::new(),
                graphics_dropped: Vec::new(),
                blocks: Vec::new(),
            }),
        };
        let bytes = rmp_serde::to_vec_named(&ev).unwrap();
        let back: ServerEvent = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn exit_event_carries_a_transport_reason_and_tolerates_an_older_payload() {
        let with_reason = CoreEvent::Exit {
            code: Some(3),
            reason: Some("device removed".into()),
        };
        let bytes = rmp_serde::to_vec_named(&with_reason).unwrap();
        assert_eq!(rmp_serde::from_slice::<CoreEvent>(&bytes).unwrap(), with_reason);

        // A daemon built before the field existed sends `Exit { code }` only.
        #[derive(serde::Serialize)]
        enum LegacyExit {
            Exit { code: Option<i32> },
        }
        let legacy = rmp_serde::to_vec_named(&LegacyExit::Exit { code: Some(0) }).unwrap();
        assert_eq!(
            rmp_serde::from_slice::<CoreEvent>(&legacy).unwrap(),
            CoreEvent::Exit {
                code: Some(0),
                reason: None,
            }
        );
    }

    #[test]
    fn serial_open_and_enumeration_roundtrip_without_credentials() {
        let spec = OpenSpec::Serial(SerialOpenSpec {
            device: "COM12".into(), baud_rate: 115_200, data_bits: 8,
            stop_bits: 1, parity: "none".into(), flow_control: "none".into(),
            display_encoding: Some("GBK".into()), newline: "crlf".into(),
            local_echo: true, dtr: true, rts: false, cols: 120, rows: 40,
        });
        let request = ClientRequest { rpc_id: 19, method: ClientMethod::SessionOpen(OpenSessionParams { spec }) };
        let bytes = rmp_serde::to_vec_named(&request).unwrap();
        assert_eq!(rmp_serde::from_slice::<ClientRequest>(&bytes).unwrap(), request);
        let reply = ServerReply { rpc_id: 20, result: ServerResult::Ok(ServerOk::SerialPorts {
            ports: vec![SerialPortInfo { device: "COM12".into(), label: "USB 串口".into() }],
        }) };
        let bytes = rmp_serde::to_vec_named(&reply).unwrap();
        assert_eq!(rmp_serde::from_slice::<ServerReply>(&bytes).unwrap(), reply);
        let legacy = serde_json::json!({ "settings": null, "ssh": null, "shell": null });
        let snapshot: ConfigSnapshot = serde_json::from_value(legacy).unwrap();
        assert_eq!(snapshot.serial, None);
    }

    #[test]
    fn shell_open_has_defaultable_shell() {
        // A client that omits `shell` must deserialize fine.
        let bytes = rmp_serde::to_vec_named(&OpenSpec::Shell(ShellOpenSpec {
            display_encoding: None,
            args: Vec::new(),
            env: Vec::new(),
            shell: None,
            cwd: None,
            cols: 80,
            rows: 24,
        }))
        .unwrap();
        let back: OpenSpec = rmp_serde::from_slice(&bytes).unwrap();
        assert!(matches!(back, OpenSpec::Shell(_)));
    }

    #[test]
    fn older_open_specs_default_to_utf8_without_changing_the_wire() {
        let shell_payload = serde_json::json!({ "cols": 80, "rows": 24 });
        let bytes = rmp_serde::to_vec_named(&shell_payload).unwrap();
        let shell: ShellOpenSpec = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(shell.display_encoding, None);
        assert!(serde_json::to_value(&shell).unwrap().get("display_encoding").is_none());

        let ssh_payload = serde_json::json!({
            "host": "example.com", "port": 22, "username": "user",
            "cols": 80, "rows": 24,
        });
        let bytes = rmp_serde::to_vec_named(&ssh_payload).unwrap();
        let ssh: SshOpenSpec = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(ssh.display_encoding, None);
        assert!(serde_json::to_value(&ssh).unwrap().get("display_encoding").is_none());
    }

    #[test]
    fn an_older_session_scroll_payload_still_decodes() {
        // A frontend from before the pointer fields only sent sid+delta. The
        // daemon must still accept that payload; the wheel then falls back to
        // the old (0,0) cell instead of failing the call.
        #[derive(Serialize)]
        enum OldMethod {
            SessionScroll { sid: u64, delta: i32 },
        }
        let bytes =
            rmp_serde::to_vec_named(&OldMethod::SessionScroll { sid: 7, delta: -2 }).unwrap();
        let decoded: ClientMethod = rmp_serde::from_slice(&bytes).unwrap();
        assert!(matches!(
            decoded,
            ClientMethod::SessionScroll {
                sid: 7,
                delta: -2,
                column: 0,
                line: 0,
                modifiers: 0,
            }
        ));
    }
}
