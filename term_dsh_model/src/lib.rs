//! Transport-free DSH data and conversation model.
//!
//! Frontends receive these values from the daemon through `term_proto`.
//! This crate contains only serializable data, event folding and presentation
//! helpers: no HTTP/WebSocket client, host credentials, license verification,
//! process launcher or daemon implementation.

/// Upstream DSH release whose session/event shapes this model consumes.
pub const DSH_WEB_PARITY_TAG: &str = "dsh-v0.2.1-rc.2";

pub mod api;
pub mod chat;
pub mod events;
pub mod hub;
pub mod wire;

pub use events::{EventOutcome, EventRejection};
pub use hub::{
    ControlBaseline, ControlFrame, FollowSnapshot, HubEvent, WorkspaceBaseline, WorkspaceFrame,
};
