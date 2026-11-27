//! ratatui frontend for the terminal-core C/S split (design doc §4.3 case A /
//! stage C1 acceptance frontend).
//!
//! Architecture (mirrors the GUI observer `SharedTabPane`):
//!
//! ```text
//!   term_core_server (daemon)          term_tui (this binary)
//!   ───────────────────────            ─────────────────────
//!   real Shell/SSH session             ┌ term_client::Client (wire only)
//!       │ snapshot ansi (100ms dirty) ─┼─▶ local Stream mirror backend
//!       │                              │      (term_tui_mirror, headless Term)
//!       │                              │   ┌ ratatui render loop: draw the
//!       │                              │   │ mirror's grid into a Buffer
//!       │ key bytes ◀─── term_client ──┼───┤
//!       │                              │   └ crossterm event loop: keys up
//!       └──────────────────────────────┘      over the wire
//! ```
//!
//! The mirror backend ([`term_tui_mirror::MirrorSession`]) applies the server's
//! absolute cell frames to a viewport grid; this crate only renders that grid and
//! feeds keys in.
//!
//! The binary starts in a **saved-host picker** ([`sessions`]): the list itself
//! comes from the daemon (`ClientMethod::SessionList`), and choosing a row sends
//! only `OpenSpec::SshFromConfig { config_id }` — the daemon expands folder
//! inheritance, the jump chain and the SOCKS proxy itself, so no credential ever
//! passes through the TUI.
//!
//! `--ssh user@host[:port]` and `--shell` skip the picker and open directly.
//!
//! Sessions are held by [`manager::Manager`], not by the screen: several run at
//! once, each with its own connection and mirror, and the reserved chords in
//! [`keys`] switch between them (`Ctrl-]` leaves the session on screen for the
//! manager without closing anything).

pub mod ask;
pub mod dsh;
pub mod dshview;
pub mod keys;
pub mod markdown;
pub mod manager;
pub mod render;
pub mod sessions;

pub use ask::{AskBridge, Decision, PendingAsk, host_key_decision, input_display};
pub use render::draw_grid;
