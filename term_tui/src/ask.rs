//! Interactive host-question handling for the ratatui frontend (B2).
//!
//! The bridge primitives now live in the shared frontend base
//! [`term_client::ask`] (the egui GUI reuses the same `AskBridge` /
//! `PendingAsk` / `Decision` machinery for its dialog queue). This module
//! re-exports them so the TUI code paths keep their existing
//! `term_tui::ask::*` names; the interactive overlay rendering and key
//! plumbing live in `main.rs`.

pub use term_client::ask::{
    AskBridge, BridgeAsk, Decision, PendingAsk, answer_bridge_ask, host_key_decision,
    input_display,
};
