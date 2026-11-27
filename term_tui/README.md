# EasyTerm TUI

A terminal frontend for a running EasyTerm daemon. This repository contains
the TUI, its protocol client, terminal display/mirror, discovery and DSH data
model. Shell/SSH sessions, saved hosts, credentials, file transfers and license
enforcement run in the separately distributed, closed-source daemon.

Use a daemon distributed with the matching EasyTerm release. The frontend and
daemon must agree on `term_proto::PROTO_VERSION`; a mismatched version is
rejected during the connection handshake. Start the EasyTerm GUI, or run
`easyterm --daemon`, before connecting. On Windows the executable is
`easyterm.exe`.

## Build and run

```sh
cargo build --release -p term_tui
cargo test --workspace
cargo run -p term_tui
cargo run -p term_tui -- --help
cargo run -p term_tui -- --list
cargo run -p term_tui -- --ssh user@example.com:22
cargo run -p term_tui -- --shell
cargo run -p term_tui -- --dsh
```

The default screen lists the daemon's saved hosts. Daemon discovery is local
to the current OS user. To select a daemon explicitly, pass its address as
`term_tui HOST:PORT`. The daemon handles vault unlock and SSH authentication prompts.
DSH uses the daemon's endpoint and requires its verified Pro-or-higher license.

Pass only the address for a remote daemon. The TUI prompts for the master
password in the terminal before opening a session or listing saved hosts.

```powershell
term_tui server.example:1133 --list
```

Remote connections automatically use TLS. The TUI remembers the certificate
on first contact and checks it on later connections.

## Keys

| Screen | Keys | Action |
| --- | --- | --- |
| Session | `Ctrl-]` | Session manager; leave the session running |
| Ended session | `Enter`, `Esc` | Reconnect, or return to session selection |
| Session | `Shift-PgUp/PgDn`, `Shift-Up/Down`, `Shift-Home/End` | Scroll the local display history |
| Session | Other keys, including `q`, `Esc`, `Ctrl-C` | Forward to the session |
| Saved hosts | Arrows or `j/k`, `Enter`, `/`, `Esc` | Select, connect, filter, return |
| Manager | Arrows or `j/k`, `Enter` or `1-9` | Select and attach |
| Manager | `/`, `Esc` | Search title/address/state; clear the search |
| Manager | `Tab`, `Shift-Tab` | Next/previous session needing attention |
| Manager | `n`, `x`, `r`, `a`, `q` | New session, close, rename, DSH, quit |
| Manager or saved hosts | `?` | Scrollable help |
| Help | Arrows or `j/k`, `PgUp/PgDn`, `Home/End` | Scroll the help text |
| Help | `Esc`, `q`, `?` | Return |
| DSH | `Enter`, `Ctrl-S`, `Ctrl-C`, `Ctrl-X` | Send, steer, stop, remove first queued message |
| DSH | `Tab`, `/`, `y/n` | Choose a session, filter its list, answer approval |
| DSH | `Shift-PgUp/PgDn`, `Shift-Home` | Scroll; load older turns from the top |

Set `EASYTERM_TUI_DETACH_KEY` to `ctrl-^`, `alt-q`, `f12` or another supported
chord to replace `Ctrl-]`. `EASYTERM_TUI_CLOSE_KEY` is the older alias.

The daemon supplies the saved host's `close_on_quit` setting. When enabled,
an ended session closes automatically and the frontend returns to session
selection. When disabled, the final screen remains visible with `Enter` to
reconnect and `Esc` to return. Other sessions continue running. Direct SSH and
shell sessions use the default of keeping the final screen. This requires a
matching daemon with protocol 15 or later.

## ZMODEM transfers

Run `rz` in the remote session to select files for upload, or `sz PATH` to
select a download directory. The daemon handles the transfer and prompts the
TUI to choose paths on the **daemon's machine**. The session displays transfer
progress; `Ctrl-X` cancels an active transfer. Outside a transfer, that key
continues to reach the remote session.

| File chooser | Action |
| --- | --- |
| Arrows or `j/k`, `PgUp/PgDn`, `Home/End` | Navigate the list |
| `Enter` or Right on a directory | Open the directory |
| `Enter` on a file, or `Space` | Toggle that file's selection |
| `a` | Toggle all files in this directory; retain selections in other directories |
| Backspace or Left | Parent directory |
| `Ctrl-D`, or Enter on the Drives row | Choose a drive on the daemon's machine |
| Backspace at a drive root | Return to the drive list |
| `Ctrl-L` | Edit the daemon directory path (`~/` and absolute paths are supported) |
| `Ctrl-S` | Send selected files, or save downloads in the current directory |
| `Esc` | Cancel the chooser |
| `y/n` at an overwrite prompt | Confirm or reject replacing an existing file |

On Windows, the drive list includes the daemon's `C:\`, `D:\` and other mounted
drive letters. Selections persist across drives. An inaccessible drive reports
an error while keeping the drive list available. Other platforms show their
normal directory tree.

## Source boundary

The standalone workspace contains exactly `term_tui`, `term_client`,
`term_proto`, `term_discovery`, `term_view`, `term_tui_mirror` and
`term_dsh_model`. Its Cargo manifests have explicit versions and need no
private workspace members or inherited dependencies. Cargo generates a fresh
`Cargo.lock` for this public dependency graph.

The protocol and local connection handshake are public. Authorization remains
the daemon's responsibility; changing the frontend's display or capability
checks does not change the official daemon's permissions. The public DSH model
contains serializable data and event folding, without a direct host client,
host token, WebSocket transport or process launcher.

The private repository produces this standalone workspace with
`python tools/export_tui.py` (Python 3.11+). `--check` validates the allowlist
without creating files, and `--output PATH` selects a fresh destination. The
exporter copies reviewed Rust sources and public protocol tests, never the
parent lockfile, private integration tests, configs, keys or runtime state.

## License

These seven public crates and their public tests are licensed under the
[MIT License](LICENSE). Each crate includes its own license notice. Third-party
dependencies retain their respective licenses.

The separate GUI, daemon, management server and private backend crates are
outside this source package. Exporting prepares a local source bundle and does
not publish a repository.
