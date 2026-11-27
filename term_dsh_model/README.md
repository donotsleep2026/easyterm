# term_dsh_model

Serializable DSH data and the conversation model shared by EasyTerm's GUI,
TUI and daemon. The upstream data baseline is `dsh-v0.2.1-rc.2`.

The crate contains session/workspace DTOs, conversation rows and side tables,
event folding, hub frame decoders, answerable-event outcomes and RPC envelope
data. Its only dependencies are `serde` and `serde_json`.

The TUI receives these values through `term_client` and `term_proto`. HTTP,
WebSocket, host authentication, process startup, licensing and daemon execution
remain outside this crate. `easyterm-dsh` re-exports the shared types at their
existing paths for GUI and daemon compatibility.

When changing the data contract, retain the serde field names and defaults,
the append/replace event semantics, the per-turn side tables and history
merging. The existing DSH chat parity guide describes those invariants.
