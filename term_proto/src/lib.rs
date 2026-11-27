//! `term_proto` 鈥?UI-free wire protocol for the terminal-core C/S split.
//!
//! Stage-2 protocol of the design doc (`docs/core-ui-separation-design.md`
//! 搂4): a frame codec with the same layout as `easyterm-shared`'s share
//! codec (`[u32 LE len][u8 type][payload]`, msgpack payload) plus typed
//! four-quadrant messages covering:
//!
//! - 搂4.4 session methods: `session/open` (local shell **or** SSH with
//!   credential references), `session/write`, `session/resize`,
//!   `session/close`;
//! - 搂4.3 content layer: ANSI snapshot events plus incremental PTY bytes;
//! - 搂2.1 鈶?host questions: `ServerAsk` (kbdint / passphrase / host-key)
//!   answered via `ClientAnswer`, so authentication can leave any UI.
//!
//! Deliberately has **no** egui/easyterm-dsh/easyterm-shared dependency:
//! this crate is the neutral protocol that the GUI, a future ratatui TUI,
//! and the `term_core_server` daemon all link against.

pub mod frame;
pub mod msg;

pub use frame::{decode_frame, encode_frame, CoreFrameType};

/// The listener requests a TLS handshake before accepting the protocol Hello.
pub const TLS_REQUIRED: &str = "tls_required";
/// Hello capability advertised by a TLS listener that requires a master-password login.
pub const NETWORK_MASTER_PASSWORD: &str = "network_master_password";

/// Non-secret parameters needed to create the remote GUI's single viewport.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct GuiPresentation {
    pub vsync: bool,
    pub hide_titlebar: bool,
    #[serde(default)]
    pub maximized: bool,
    pub transparent: bool,
    pub ui_theme: Option<String>,
    pub locale: Option<String>,
}
pub use msg::{
    AnswerResult, AskKind, ClientAck, ClientAnswer, ClientMethod, ClientRequest,
    CoreEvent, DAEMON_FEATURES, DSH_MUX_SID, HostKeyAsk, KbdInteractiveAsk, KbdKind, LicenseInfo,
    AccountState, HistoryScrollStep, LocalPathEntry, LocalPathListInfo,
    LicenseValidity, OfflineLicenseStatusInfo, LUA_MUX_SID, CONFIG_MUX_SID, PROTO_VERSION,
    OpenSessionParams, OpenSpec, PortForwardInfo, PortMapAddParams, PortMapDebugEventWire,
    PortMapDebugParams, PortMapDelParams, PortMapKind,
    RpcId, ServerAsk, ServerErr, ServerEvent, ServerOk, ServerReply, ServerResult,
    RecordingStartParams,
    SessionResizeParams, SessionWriteParams, ShellOpenSpec, SerialOpenSpec, SerialPortInfo, Sid,
    FrameBlock, FrameCell, FrameColor, FrameCursorShape, FrameGraphic, FrameGraphicCell, FramePayload,
    FrameRow, FrameSpan, SearchStep,
    SelectionKind, SelectionSpec,
    SessionEntryWire, ConfigSnapshot, ConfigDocument,
    SftpEntry, SftpGetParams, SftpListParams, SftpMkdirParams, SftpPutParams, SftpRemoveParams,
    SftpRenameParams, SshAuthRef, SshExecParams, SshOpenSpec, SshState, VaultKeySecret, VaultRefKind, VaultResolveAsk,
    VaultSecret, WebProxyParams,
    ShareEventWire, ShareItemKindWire, ShareItemWire, ShareModeWire, ShareObserverWire,
    SharePermission, ShareStatusWire, TrafficStatsWire,
    AiEventWire, AiMessageWire, AiToolCallAsk, AiToolDefWire, AiTurnParams,
    DshCallOutcomeWire, DshConnectParams, DshStateWire,
    CommandRecordWire,
    ZmodemEventWire, ZmodemPathAsk,
    UpdateEventWire, UpdateInfoWire, UpdatePhaseWire, UpdateStateWire,
};
