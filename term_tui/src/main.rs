//! term_tui binary: ratatui frontend for a term_core_server daemon.
//!
//! Usage:
//!   `term_tui [127.0.0.1:PORT]`
//!       Show the daemon's saved-host list; `Enter` opens the selected host.
//!   `term_tui [HOST:PORT] --list`
//!       Print the daemon's saved hosts to stdout and exit (no terminal needed).
//!   `term_tui [HOST:PORT] --ssh user@host[:port]`
//!       Skip the picker and open one address directly.
//!   `term_tui [HOST:PORT] --shell`
//!       Skip the picker and open a local shell (the pre-picker default).
//!
//! Every session is opened on the daemon and mirrored locally: the daemon's
//! ANSI snapshots are parsed into a local headless Term, the grid is rendered
//! with ratatui, and keys typed go back over the wire. An SSH open raises host
//! questions (host-key confirm, kbdint/password): they are shown as an
//! interactive overlay and answered while the open is still in flight (the
//! daemon waits on each answer before resolving the open).
//!
//! **Sessions are not the screen.** This frontend holds several sessions at
//! once — each with its own connection, mirror and scrollback — and the screen
//! shows one of them ([`Screen`]). `Ctrl-]` (configurable, see
//! `term_tui::keys::Chord`) leaves the session on screen for the **manager**,
//! the list of everything that is open, without closing anything; the manager
//! attaches, opens, renames and closes. That is the difference from the
//! one-session version, where leaving a session destroyed it and there was
//! nothing to come back to.
//!
//! Inside a session every other key goes to the remote (including `q`, `Esc`
//! and `Ctrl-C`, so vim's `:q`, less and a shell's interrupt behave normally);
//! `Shift-PgUp/PgDn`, `Shift-Up/Down` and `Shift-Home/End` move through the
//! mirror's own scrollback, which the remote never sees. `?` in the manager
//! and the picker lists all of it.
//!
//! Both the host list and the expansion of a chosen host live in the daemon:
//! the picker renders `ClientMethod::SessionList` and sends back only a
//! `config_id` (`OpenSpec::SshFromConfig`), so the TUI never reads a config
//! file and never holds a credential.

use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::index::Point;
use alacritty_terminal::vte::ansi::CursorShape;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event as CEvent, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use log::{debug, info, warn};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use term_client::Client;
use ratatui::Terminal;

use term_view::{BackendCommand, InputModifiers};
use term_tui_mirror::ask::AskHost;
use term_tui_mirror::{MirrorSession, MirrorSessionParams};
use term_dsh_model::api::session::PromptMode;
use term_proto::{AnswerResult, AskKind, CoreEvent, OpenSpec, ServerAsk, SessionEntryWire};
use term_tui::ask::{AskBridge, PendingAsk};
use term_tui::dsh::{DshFeed, Pending};
use term_tui::keys::{self, Chord, Scroll, SessionKey};
use term_tui::manager::{
    Manager, Outcome as ManagerOutcome, Row, SessionId, State as SessionState,
};
use term_tui::dshview::{self, DshView};
use term_tui::render::draw_grid;
use term_tui::sessions;

mod files;

/// Placeholder grid for a spec built before the terminal is measured.
///
/// The real size comes from the window (`session_grid`), which is written into a
/// spec before it is opened and forwarded again whenever the window changes.
/// These values only exist so the struct can be built in `--ssh`/`--shell`
/// argument parsing and in the picker.
const PLACEHOLDER_COLS: u16 = 80;
const PLACEHOLDER_ROWS: u16 = 24;

/// How long the one-shot vault-status connection may take.
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the frame loop waits for input before looking at the sessions.
///
/// A full-screen program animates at 20-60 fps (`sl`, `htop`, `watch`), so
/// waiting 50 ms capped the frontend at 20 fps no matter how fast the daemon
/// streamed; waiting one frame is what a terminal viewer should do. Redrawing
/// is separately gated by `App::dirty`, so an idle frontend costs a wakeup and
/// nothing else.
const FRAME_POLL: Duration = Duration::from_millis(16);

/// How long the frontend may go without a redraw while something is happening
/// (`dirty` should cover every change; this is the safety valve that keeps a
/// missed flag from freezing the screen).
const HEARTBEAT: Duration = Duration::from_millis(500);

// How much of the feed a mirror replays is `term_tui::feed`'s decision.

/// How many characters the picker's `/` filter may hold.
const MAX_FILTER: usize = 64;

/// No keyboard modifiers, for a synthetic scroll that has none.
const NO_MODS: InputModifiers = InputModifiers {
    shift: false,
    ctrl: false,
    alt: false,
    logo: false,
};

/// Where leaving the DSH page goes.
///
/// Pure, because the choice is the whole point: it must never land on a list
/// that has nothing in it (that is the bug — an empty manager saying "no
/// sessions" to someone whose only session was the conversation they were
/// reading).
fn leave_agent_target(origin: Option<Screen>, session_alive: bool, has_sessions: bool) -> Screen {
    match origin {
        // Back to the terminal they came from, if it is still there.
        Some(Screen::Session(id)) if session_alive => Screen::Session(id),
        // Back to the host list, which is where hosts are opened.
        Some(Screen::Picker) => Screen::Picker,
        // Back to the manager only if it can show something.
        _ if has_sessions => Screen::Manager,
        _ => Screen::Picker,
    }
}

/// The outer terminal's title, written only when it changes: an escape sequence
/// per frame is a write and a flush for nothing.
#[derive(Debug, Default)]
struct TerminalTitle {
    last: Option<String>,
}

impl TerminalTitle {
    /// Whether `title` differs from what the terminal is showing.
    fn changed(&mut self, title: &str) -> bool {
        if self.last.as_deref() == Some(title) {
            return false;
        }
        self.last = Some(title.to_string());
        true
    }
}

/// Set the outer terminal's window/tab title (OSC 0), so the terminal's tab strip
/// says what this frontend is doing — and, inside a session, what the remote
/// calls itself.
fn write_window_title(title: &str) {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, crossterm::terminal::SetTitle(title.to_string()));
}

/// Pass a bell through to the outer terminal, so the user's emulator can notify
/// (a task finishing in a session the user is watching is exactly what a bell is
/// for; a bell from a session they are *not* watching stays the manager's mark).
fn write_bell() {
    use std::io::Write;
    let mut stdout = io::stdout();
    let _ = stdout.write_all(b"\x07");
    let _ = stdout.flush();
}

/// How the frontend was started.
struct Cli {
    /// Explicit daemon address. Otherwise the address is discovered locally.
    daemon_addr: Option<String>,
    /// What the frontend shows first.
    start: Start,
    /// The chord that leaves a session for the manager (`term_tui::keys`).
    detach: Chord,
}

/// The first screen to show.
enum Start {
    /// The saved-host list (`Enter` opens, `q` quits).
    Picker,
    /// Print the daemon's saved hosts to stdout and exit — no terminal needed.
    /// Lets a script (or a bug report) see exactly what the daemon offers.
    List,
    /// Open this `user@host[:port]` (no saved config).
    Ssh(String),
    /// Open a local shell.
    Shell,
    /// Open the DSH agent page.
    Dsh,
}

/// What the screen shows. Sessions that are not this are still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    /// The daemon's saved hosts.
    Picker,
    /// Every open session.
    Manager,
    /// One session's terminal.
    Session(SessionId),
    /// The DSH agent page: the daemon's conversation with it.
    Dsh,
    /// The key help, on top of whatever was there.
    Help,
}

impl Screen {
    /// The session on screen, when the screen is a terminal.
    fn session(self) -> Option<SessionId> {
        match self {
            Screen::Session(id) => Some(id),
            _ => None,
        }
    }
}

/// What one step of the frame loop decided.
enum Step {
    Continue,
    Quit,
}

#[derive(Debug, PartialEq, Eq)]
enum EndedSessionKey { Reconnect, Back, Ignore }

fn ended_session_key(key: &KeyEvent, detach: Chord) -> EndedSessionKey {
    match key.code {
        KeyCode::Enter => EndedSessionKey::Reconnect,
        KeyCode::Esc => EndedSessionKey::Back,
        _ if detach.matches(key) => EndedSessionKey::Back,
        _ => EndedSessionKey::Ignore,
    }
}

/// The picker's own state: where the cursor is and what is being searched for.
struct PickerState {
    /// Cursor into the **filtered** rows.
    selected: usize,
    /// Persistent so a 400-host list scrolls with the cursor instead of
    /// redrawing from the top every frame.
    list: ListState,
    /// The `/` filter text, empty when not filtering.
    filter: String,
    /// Whether the filter has the keyboard: then plain letters are search text,
    /// not cursor moves.
    filtering: bool,
}

impl Default for PickerState {
    fn default() -> Self {
        Self {
            selected: 0,
            list: ListState::default(),
            filter: String::new(),
            filtering: false,
        }
    }
}

/// The frontend's half of one session: its connection, its mirror and the host
/// questions it is being asked.
struct Live {
    /// The open in flight; `Some` until it resolves or fails.
    open: Option<tokio::task::JoinHandle<Result<MirrorSession, String>>>,
    /// The mirror, once the open resolved.
    mirror: Option<MirrorSession>,
    /// Host questions the daemon asks through this session's bridge.
    asks: tokio::sync::mpsc::UnboundedReceiver<(
        ServerAsk,
        tokio::sync::oneshot::Sender<AnswerResult>,
    )>,
    /// The question waiting for an answer (shown only while this session is on
    /// screen, but recorded either way so the manager can say who is blocked).
    ask: Option<PendingAsk>,
    /// When a frame last arrived (the daemon's only content channel).
    last_data: Option<std::time::Instant>,
    file_picker: Option<files::FileChooser>,
    path_task: Option<tokio::task::JoinHandle<Result<term_proto::LocalPathListInfo, String>>>,
    transfer: files::TransferProgress,
}

impl Live {
    fn discard_finished_ask(&mut self) -> bool {
        if self.ask.as_ref().is_some_and(|ask| !ask.is_pending()) {
            self.ask = None;
            self.file_picker = None;
            if let Some(task) = self.path_task.take() { task.abort(); }
            true
        } else {
            false
        }
    }

    fn zmodem_event(&mut self, event: &term_proto::ZmodemEventWire) {
        self.transfer.event(event);
        // The reader correlates expiry by ask_id before splitting its queues.
        // A delayed Ended event must not cancel a newer transfer's question.
        self.discard_finished_ask();
    }
}

/// An open whose slot was closed before it resolved. Polled so the daemon
/// session it eventually creates is closed too, instead of leaking a session
/// nobody can reach.
type Orphan = tokio::task::JoinHandle<Result<MirrorSession, String>>;

/// The frontend: what is open, what is on screen, and what the daemon said.
struct App {
    addr: String,
    /// The chord that leaves a session for the manager.
    detach: Chord,
    screen: Screen,
    /// Where `?` was opened from, so closing help returns there.
    help_return: Option<Screen>,
    help_scroll: u16,
    help_max_scroll: u16,
    /// The vault token this run unlocked with: reused for every later open, so
    /// the master password is asked once per run, not once per host.
    vault_token: Option<String>,
    mgr: Manager<Live>,
    /// The DSH agent page: the daemon's DSH connection and its row feed.
    dsh: DshFeed,
    /// The page's own state (input, scroll, the session overlay).
    dsh_view: DshView,

    picker: PickerState,
    manager_filtering: bool,
    /// Last browsed daemon directory; never interpreted on the TUI machine.
    transfer_directory: Option<String>,
    hosts: Vec<SessionEntryWire>,
    picker_refresh: Option<tokio::task::JoinHandle<Result<Vec<SessionEntryWire>, String>>>,
    /// Why the current screen is showing again (a failed open, a session that
    /// ended, a closed session).
    status: Option<String>,
    /// Closing a session is destructive, and quitting closes all of them: ask
    /// before doing either to something that is still connected.
    confirm: Option<String>,
    confirm_close: Option<SessionId>,
    confirm_quit: bool,
    /// The window size the sessions were last told about.
    last_grid: Option<(u16, u16)>,
    /// The outer terminal's title, written only when it changes.
    title_writer: TerminalTitle,
    /// A bell arrived for the session on screen and has not been passed on yet.
    bell: bool,
    /// Where the DSH page was opened from, so leaving it goes back there instead
    /// of to a hub that may have nothing in it.
    dsh_return: Option<Screen>,
    /// The reader reached the top of the DSH transcript and asked for older
    /// turns: the page fetches them on the next frame (fetching is async, and a
    /// key handler should not wait on the host).
    dsh_wants_older: bool,
    /// Something changed since the last redraw. A terminal that redraws 60 times
    /// a second while nothing moves burns CPU for nothing (a grid draw plus a
    /// `sync()` of a long scrollback is milliseconds of work).
    dirty: bool,
    /// When the screen was last drawn, for the heartbeat.
    last_draw: std::time::Instant,
    orphans: Vec<Orphan>,
}

impl App {
    fn new(addr: String, detach: Chord) -> Self {
        Self {
            addr,
            detach,
            screen: Screen::Picker,
            help_return: None,
            help_scroll: 0,
            help_max_scroll: 0,
            vault_token: None,
            mgr: Manager::new(),
            dsh: DshFeed::new(),
            dsh_view: DshView::default(),

            picker: PickerState::default(),
            manager_filtering: false,
            transfer_directory: None,
            hosts: Vec::new(),
            picker_refresh: None,
            status: None,
            confirm: None,
            confirm_close: None,
            confirm_quit: false,
            last_grid: None,
            title_writer: TerminalTitle::default(),
            bell: false,
            dsh_wants_older: false,
            dsh_return: None,
            dirty: true,
            last_draw: std::time::Instant::now(),
            orphans: Vec::new(),
        }
    }

    /// Run until the user quits, then close every session.
    ///
    /// Quitting closes them on purpose: the daemon owns the PTYs, and a
    /// frontend that walks away from a session it cannot reattach to would just
    /// leak it. The confirm prompt says so when anything is still connected.
    async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            if let Step::Quit = self.frame(terminal).await? {
                break;
            }
        }
        self.close_all().await;
        Ok(())
    }

    /// One frame: pump every session, forward a resize, draw, handle one event.
    async fn frame(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<Step, Box<dyn std::error::Error>> {
        let (cols, rows) = session_grid(terminal)?;
        self.pump(cols, rows).await;
        // The DSH page has its own daemon connection and its own feed; it only
        // runs while the page is on screen (or being waited for), so a frontend
        // that never opens it never connects to DSH.
        if self.screen == Screen::Dsh {
            self.pump_dsh().await;
        }
        self.resize_sessions(cols, rows).await;
        if self.dirty || self.last_draw.elapsed() >= HEARTBEAT {
            self.draw(terminal)?;
            self.dirty = false;
            self.last_draw = std::time::Instant::now();
        }

        if event::poll(FRAME_POLL)? {
            // Anything the user did may change the screen (a caret, a cursor, a
            // key on its way to the remote), so the next frame draws.
            self.dirty = true;
            match event::read()? {
                CEvent::Key(k) if k.kind == KeyEventKind::Press => {
                    return self.on_key(terminal, k).await
                }
                CEvent::Paste(text) => self.on_paste(text).await,
                _ => {}
            }
        }
        Ok(Step::Continue)
    }

    /// Take everything the sessions have to say.
    ///
    /// Every session is pumped, not only the one on screen: the manager has to
    /// be truthful about the others (their titles, their output, and whether
    /// they are waiting for an answer or have exited), and their mirrors have
    /// to stay in sync so attaching shows the right screen immediately.
    async fn pump(&mut self, cols: u16, rows: u16) {
        if self.picker_refresh.as_ref().is_some_and(|task| task.is_finished()) {
            if let Some(task) = self.picker_refresh.take() {
                match task.await {
                    Ok(Ok(hosts)) => {
                        self.hosts = hosts;
                        self.picker.selected = self.picker.selected.min(self.filtered_hosts().len().saturating_sub(1));
                        if self.screen == Screen::Picker { self.status = None; }
                    }
                    result => {
                        if self.screen == Screen::Picker {
                            self.status = Some(match result { Ok(Err(error)) => error, Err(error) => error.to_string(), _ => unreachable!() });
                        }
                    }
                }
                self.dirty = true;
            }
        }
        // Host questions first: an open is blocked on them.
        let mut asked = false;
        for slot in self.mgr.iter_mut() {
            let Some(live) = slot.handle.as_mut() else { continue };
            if live.discard_finished_ask() { asked = true; }
            if live.ask.is_none() {
                while let Ok((ask, reply)) = live.asks.try_recv() {
                    let pending = PendingAsk::new(ask, reply);
                    if !pending.is_pending() { continue; }
                    debug!("session {}: host ask {:?}", slot.id, pending.ask.kind);
                    if let AskKind::ZmodemPath(path) = &pending.ask.kind {
                        live.file_picker = Some(files::FileChooser::new(path.save, path.name.as_deref(), self.transfer_directory.clone()));
                    }
                    live.ask = Some(pending);
                    asked = true;
                    break;
                }
            }
            slot.awaiting = live.ask.is_some();
        }
        if asked {
            // The overlay has to appear (or disappear) even if the session it
            // belongs to is not on screen: the manager marks it.
            self.dirty = true;
        }

        // Opens that resolved.
        for id in self.mgr.ids() {
            let finished = self
                .mgr
                .get(id)
                .and_then(|s| s.handle.as_ref())
                .and_then(|l| l.open.as_ref())
                .map(|t| t.is_finished())
                .unwrap_or(false);
            if !finished {
                continue;
            }
            let task = match self
                .mgr
                .get_mut(id)
                .and_then(|s| s.handle.as_mut())
                .and_then(|l| l.open.take())
            {
                Some(t) => t,
                None => continue,
            };
            self.dirty = true;
            match task.await {
                Ok(Ok(m)) => {
                    info!("session {} connected: sid {}", id, m.sid());
                    if let Some(live) = self.mgr.get_mut(id).and_then(|s| s.handle.as_mut()) {
                        live.mirror = Some(m);
                    }
                    self.mgr.mark_open(id, cols, rows);
                    // Only go there if that is where the user already is: an
                    // open that resolves after they moved on must not yank the
                    // screen out from under them.
                    if self.mgr.active() == Some(id) {
                        self.screen = Screen::Session(id);
                    }
                }
                Ok(Err(e)) => {
                    warn!("session {id} failed to open: {e}");
                    self.fail(id, format!("could not open: {e}"));
                }
                Err(e) => {
                    self.fail(id, format!("open task panicked: {e}"));
                }
            }
        }

        // Directory I/O runs on the daemon and never stalls terminal input.
        for slot in self.mgr.iter_mut() {
            let Some(live) = slot.handle.as_mut() else { continue; };
            if live.path_task.as_ref().is_some_and(|task| task.is_finished()) {
                if let Some(task) = live.path_task.take() {
                    let result = task.await.unwrap_or_else(|error| Err(format!("directory request failed: {error}")));
                    if let Some(picker) = live.file_picker.as_mut() { picker.loaded(result); }
                    self.dirty = true;
                }
            }
            if live.path_task.is_none() {
                if let (Some(picker), Some(mirror)) = (live.file_picker.as_mut(), live.mirror.as_ref()) {
                    if let Some(path) = picker.take_request() {
                        let writer = mirror.writer().clone();
                        live.path_task = Some(tokio::spawn(async move {
                            tokio::time::timeout(STATUS_TIMEOUT, writer.local_path_list(path.as_deref())).await
                                .map_err(|_| "daemon directory request timed out; press r to retry".to_string())?
                        }));
                    }
                }
            }
        }

        // Output, titles, bells and exits.
        let on_screen = self.screen.session();
        for id in self.mgr.ids() {
            let mut saw_output = false;
            let mut saw_bell = false;
            let mut exited = false;
            let mut close_on_quit = None;
            let mut new_title: Option<String> = None;
            let disconnected;
            {
                let Some(live) = self.mgr.get_mut(id).and_then(|s| s.handle.as_mut()) else {
                    continue;
                };
                while let Some(ev) = live.mirror.as_mut().and_then(MirrorSession::next_event_now) {
                    match &ev.event {
                        CoreEvent::Exit { code, reason } => {
                            info!("session {id} exited (code {code:?}, reason {reason:?})");
                            exited = true;
                        }
                        CoreEvent::SessionSettings { close_on_quit: close } => close_on_quit = Some(*close),
                        CoreEvent::Bell => saw_bell = true,
                        CoreEvent::Frame(_) => {
                            // The only content channel: the daemon pushes the
                            // cells that changed, and the mirror applies them.
                            saw_output = true;
                            live.last_data = Some(std::time::Instant::now());
                        }
                        CoreEvent::Wake => saw_output = true,
                        CoreEvent::Title { title } => new_title = Some(title.clone()),
                        CoreEvent::Zmodem(progress) => {
                            live.zmodem_event(progress);
                            saw_output = true;
                        }
                        _ => {}
                    }
                    if let Some(mirror) = live.mirror.as_mut() { mirror.apply_event(&ev); }
                }
                disconnected = live.mirror.as_ref().is_some_and(MirrorSession::is_disconnected);
            }
            if disconnected && self.mgr.get(id).is_some_and(|slot| slot.state == SessionState::Open) { exited = true; }
            if saw_output || saw_bell || exited || new_title.is_some() {
                // Only what is drawn matters: the session on screen, and the
                // manager, whose rows carry every session's state and marks.
                if on_screen == Some(id) || exited || self.screen == Screen::Manager {
                    self.dirty = true;
                }
            }
            if let Some(slot) = self.mgr.get_mut(id) {
                // Attention marks are for sessions the user is *not* looking at.
                if on_screen != Some(id) {
                    if saw_output {
                        slot.unread = true;
                    }
                    if saw_bell {
                        slot.bell = true;
                        slot.unread = true;
                    }
                }
                // A bell from the session on screen goes out to the terminal, so
                // the user's emulator can do whatever it does with bells.
                if saw_bell && on_screen == Some(id) {
                    self.bell = true;
                }
                if let Some(t) = new_title {
                    slot.remote_title = Some(t);
                }
                if let Some(close) = close_on_quit { slot.close_on_quit = close; }
            }
            if exited { self.session_exited(id); }
        }

        self.reap_orphans().await;
    }

    /// Return to existing live sessions, or refresh the daemon's saved hosts.
    /// This also works for a TUI started directly with --ssh/--shell.
    fn return_to_session_selection(&mut self) {
        self.mgr.detach();
        if self.mgr.live_count() > 0 {
            self.screen = Screen::Manager;
        } else {
            self.screen = Screen::Picker;
            if self.picker_refresh.is_none() {
                let addr = self.addr.clone();
                self.picker_refresh = Some(tokio::spawn(async move { sessions::load(&addr).await }));
                self.status = Some("Loading saved hosts…".into());
            }
        }
        self.dirty = true;
    }

    fn session_exited(&mut self, id: SessionId) {
        let Some(slot) = self.mgr.get_mut(id) else { return; };
        slot.state = SessionState::Exited;
        slot.awaiting = false;
        if let Some(live) = slot.handle.as_mut() {
            if let Some(ask) = live.ask.as_mut() { ask.cancel(); }
            live.ask = None;
            live.file_picker = None;
            if let Some(task) = live.path_task.take() { task.abort(); }
            live.transfer.active = false;
            live.transfer.label = None;
        }
        if slot.close_on_quit {
            // The daemon has already released this terminal. Dropping its
            // mirror closes this frontend connection without another RPC.
            drop(self.mgr.remove(id));
            if self.screen.session() == Some(id) || (self.screen == Screen::Manager && self.mgr.is_empty()) {
                self.return_to_session_selection();
            }
            let selection = if self.mgr.live_count() > 0 { Screen::Manager } else { Screen::Picker };
            if self.help_return == Some(Screen::Session(id)) { self.help_return = Some(selection); }
        }
        self.dirty = true;
    }

    fn leave_ended_session(&mut self, id: SessionId) {
        // Escape closes this ended terminal, matching the GUI's close action.
        // Other live sessions and their connections stay in the manager.
        drop(self.mgr.remove(id));
        self.return_to_session_selection();
    }

    /// Report a failed open the way the manager shows it.
    fn fail(&mut self, id: SessionId, reason: String) {
        let title = self
            .mgr
            .get(id)
            .map(|s| s.title.clone())
            .unwrap_or_else(|| id.to_string());
        self.mgr.set_failed(id, reason.clone());
        self.status = Some(format!("{title}: {reason}"));
        if self.screen.session() == Some(id) {
            self.mgr.detach();
            self.screen = Screen::Manager;
        }
    }

    /// Close daemon sessions whose slot disappeared while the open was in
    /// flight.
    async fn reap_orphans(&mut self) {
        let mut i = 0;
        while i < self.orphans.len() {
            if self.orphans[i].is_finished() {
                let task = self.orphans.swap_remove(i);
                match task.await {
                    Ok(Ok(m)) => {
                        info!("closing session {} that nobody owns any more", m.sid());
                        let _ = m.close().await;
                    }
                    Ok(Err(e)) => debug!("orphaned open failed as expected: {e}"),
                    Err(e) => warn!("orphaned open task panicked: {e}"),
                }
            } else {
                i += 1;
            }
        }
    }

    /// Tell every session the window's size, once per change.
    ///
    /// All of them, not only the one on screen: they share one window, so they
    /// share one grid, and a session that is attached to later is already the
    /// right shape.
    async fn resize_sessions(&mut self, cols: u16, rows: u16) {
        if self.last_grid == Some((cols, rows)) {
            return;
        }
        for slot in self.mgr.iter_mut() {
            let res = match slot.handle.as_mut().and_then(|l| l.mirror.as_mut()) {
                Some(m) => Some(m.resize_all(cols, rows).await),
                None => None,
            };
            if let Some(Err(e)) = res {
                // A resize that does not reach the daemon is worth knowing
                // about, but it must not end the session.
                warn!("resize to {cols}x{rows} failed: {e}");
            }
            if slot.state.is_live() {
                slot.cols = cols;
                slot.rows = rows;
            }
        }
        self.last_grid = Some((cols, rows));
        // The grid the session has is the grid it draws.
        self.dirty = true;
    }

    /// Paint one frame and place the cursor.
    fn draw(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Destructured up front: the draw closure needs several fields at once,
        // and `self` cannot be borrowed partially inside it.
        let App {
            addr,
            detach,
            screen,
            mgr,
            dsh,
            dsh_view,
            picker,
            hosts,
            status,
            confirm,
            manager_filtering,
            help_scroll,
            help_max_scroll,
            title_writer,
            bell,
            ..
        } = self;
        let mut cursor_cell: Option<(u16, u16, CursorShape, bool)> = None;
        let mut overlay = false;
        // What the outer terminal's title should say, computed while the screens
        // are drawn (they are the only place with the state).
        let mut window_title: Option<String> = None;

        terminal.draw(|f| match *screen {
            Screen::Picker => {
                window_title = Some(format!("easyterm — {} saved host(s)", hosts.len()));
                draw_picker(f, addr, hosts, status.as_deref(), picker)
            }
            Screen::Manager => {
                window_title = Some(format!("easyterm — {}", mgr.status_line()));
                draw_manager(f, addr, mgr, status.as_deref(), confirm.as_deref(), *manager_filtering)
            }
            Screen::Session(id) => {
                // The title follows the session, and a remote that names itself
                // (`vim`, `ssh`) names the tab.
                window_title = mgr.get(id).map(|s| {
                    format!(
                        "{} [{}/{}] — easyterm",
                        s.display_title(),
                        mgr.index_of(id).map(|i| i + 1).unwrap_or(0),
                        mgr.len()
                    )
                });
                // The grid first: it borrows the manager mutably, so the host
                // question (an immutable read of the same session) is looked up
                // afterwards.
                cursor_cell = draw_session(f, mgr, id, detach);
                let area = f.area();
                if let Some(live) = mgr.get_mut(id).and_then(|s| s.handle.as_mut()) {
                    if let Some(picker) = live.file_picker.as_mut() {
                        overlay = true;
                        picker.draw(f.buffer_mut(), area, addr);
                    } else if let Some(p) = live.ask.as_ref() {
                        overlay = true;
                        draw_overlay(p, f.buffer_mut(), area);
                    }
                }
            }
            Screen::Dsh => {
                window_title = Some(format!("easyterm — dsh · {}", dsh.title()));
                // The composer's caret is a plain block: the chat has no
                // application-chosen cursor shape to honour.
                cursor_cell = dshview::draw_dsh(f, dsh, dsh_view)
                    .map(|(x, y)| (x, y, CursorShape::Block, true));
            }
            Screen::Help => *help_max_scroll = draw_help(f, addr, detach, help_scroll),
        })?;

        // Place the real cursor so the user can see where typing goes (a
        // terminal without a cursor is unusable for editing). The overlay owns
        // the screen while a host question is up, so hide it then.
        apply_cursor(cursor_cell, overlay);

        // The terminal's own affordances: what the tab says, and a bell the user
        // can hear. Both are escape sequences, so they are written only when
        // something actually changed.
        if let Some(title) = &window_title {
            if title_writer.changed(title) {
                write_window_title(title);
            }
        }
        if *bell {
            write_bell();
            *bell = false;
        }
        Ok(())
    }

    /// Handle one key press on the current screen.
    async fn on_key(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        k: KeyEvent,
    ) -> Result<Step, Box<dyn std::error::Error>> {
        if self.screen == Screen::Help {
            let page = terminal.size()?.height.saturating_sub(4).max(1) as i32;
            let delta = match keys::help_key(&k) {
                keys::HelpKey::Move(delta) => delta,
                keys::HelpKey::Page(delta) => delta * page,
                keys::HelpKey::First => { self.help_scroll = 0; 0 }
                keys::HelpKey::Last => { self.help_scroll = self.help_max_scroll; 0 }
                keys::HelpKey::Back => {
                    self.screen = self.help_return.take().unwrap_or(Screen::Manager);
                    0
                }
                keys::HelpKey::None => 0,
            };
            self.help_scroll = (i32::from(self.help_scroll) + delta)
                .clamp(0, i32::from(self.help_max_scroll)) as u16;
            return Ok(Step::Continue);
        }
        if self.confirm_quit {
            self.confirm_quit = false;
            let yes = matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y'));
            self.confirm = None;
            return Ok(if yes { Step::Quit } else { Step::Continue });
        }
        if let Some(id) = self.confirm_close.take() {
            self.confirm = None;
            if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.close_session(id).await;
            }
            return Ok(Step::Continue);
        }
        match self.screen {
            Screen::Picker => self.on_picker_key(terminal, k).await,
            Screen::Manager => self.on_manager_key(terminal, k).await,
            Screen::Session(_) => self.on_session_key(terminal, k).await,
            Screen::Dsh => self.on_dsh_key(k).await,
            Screen::Help => Ok(Step::Continue),
        }
    }

    /// A bracketed paste from the terminal, forwarded to the session on screen.
    async fn on_paste(&mut self, text: String) {
        let Some(id) = self.screen.session() else { return };
        if !self.mgr.get(id).is_some_and(|slot| slot.state.is_live()) { return; }
        if let Some(live) = self.mgr.get_mut(id).and_then(|slot| slot.handle.as_mut()) {
            if let Some(picker) = live.file_picker.as_mut() {
                picker.paste(&text);
                self.dirty = true;
                return;
            }
            if let Some(ask) = live.ask.as_mut() {
                if matches!(&ask.ask.kind, AskKind::KbdInteractive(_)) { ask.input.push_str(&text); }
                self.dirty = true;
                return;
            }
        }
        // Whether the *remote* wants the paste markers: it asked for them with
        // `?2004h`, and the mirror's Term knows because it parsed that.
        let bracketed = self
            .mgr
            .get_mut(id)
            .and_then(|s| s.handle.as_mut())
            .and_then(|l| l.mirror.as_mut())
            .map(|m| m.backend().bracketed_paste_active())
            .unwrap_or(false);
        let mut bytes = Vec::new();
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
        } else {
            // Without bracket markers the remote sees a stream of keystrokes,
            // so a line break has to be the key a terminal sends: CR.
            for ch in text.chars() {
                bytes.push(if ch == '\n' { b'\r' } else { ch as u8 });
            }
            bytes.retain(|b| *b != 0);
        }
        debug!("pasting {} byte(s) into session {id}", bytes.len());
        self.send_bytes(id, bytes).await;
    }

    // ─────────────────────────── picker ───────────────────────────

    /// Show the host list, refreshing it from the daemon.
    ///
    /// Asked every time, so hosts added in the GUI show up without restarting
    /// the TUI. A daemon that cannot answer still gets a picker (with the
    /// reason in its status line) rather than an exit.
    async fn show_picker(&mut self) {
        self.screen = Screen::Picker;
        self.picker = PickerState::default();
        match sessions::load(&self.addr).await {
            Ok(hosts) => {
                self.hosts = hosts;
                self.status = None;
            }
            Err(e) => {
                self.hosts.clear();
                self.status = Some(e);
            }
        }
    }

    /// Indices of the hosts matching the current filter.
    fn filtered_hosts(&self) -> Vec<usize> {
        let needle = self.picker.filter.trim().to_lowercase();
        if needle.is_empty() {
            return (0..self.hosts.len()).collect();
        }
        (0..self.hosts.len())
            .filter(|i| host_matches(&self.hosts[*i], &needle))
            .collect()
    }

    fn move_picker(&mut self, delta: i32, len: usize) {
        if len == 0 {
            self.picker.selected = 0;
            return;
        }
        let last = len as i32 - 1;
        self.picker.selected = (self.picker.selected as i32 + delta).clamp(0, last) as usize;
    }

    async fn on_picker_key(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        k: KeyEvent,
    ) -> Result<Step, Box<dyn std::error::Error>> {
        if !self.picker.filtering {
            match k.code {
                KeyCode::Char('?') => {
                    self.open_help();
                    return Ok(Step::Continue);
                }
                // The agent page is reachable from the first screen: the manager
                // is not, because it lists sessions and there are none yet.
                KeyCode::Char('a') => {
                    self.open_agent();
                    return Ok(Step::Continue);
                }
                _ => {}
            }
        }
        let shown = self.filtered_hosts();
        match keys::list_key(&k, self.picker.filtering) {
            keys::ListKey::Move(d) => self.move_picker(d, shown.len()),
            keys::ListKey::Page(d) => self.move_picker(d, shown.len()),
            keys::ListKey::First => self.picker.selected = 0,
            keys::ListKey::Last => self.picker.selected = shown.len().saturating_sub(1),
            keys::ListKey::Filter => {
                if self.picker.filtering {
                    // Esc leaves the filter; the list is whole again.
                    self.picker.filter.clear();
                    self.picker.filtering = false;
                } else {
                    self.picker.filtering = true;
                }
                self.picker.selected = 0;
            }
            keys::ListKey::FilterChar(ch) => {
                if self.picker.filter.chars().count() < MAX_FILTER {
                    self.picker.filter.push(ch);
                }
                self.picker.selected = 0;
            }
            keys::ListKey::FilterBackspace => {
                self.picker.filter.pop();
                self.picker.selected = 0;
            }
            keys::ListKey::Open => {
                let picked = shown
                    .get(self.picker.selected)
                    .and_then(|i| self.hosts.get(*i))
                    .map(|e| (e.config_id, e.name.clone(), e.target()));
                if let Some((config_id, name, target)) = picked {
                    info!("picked config {config_id} ({name})");
                    let spec = OpenSpec::SshFromConfig {
                        config_id,
                        cols: PLACEHOLDER_COLS,
                        rows: PLACEHOLDER_ROWS,
                    };
                    self.begin_open(terminal, spec, name, target).await?;
                }
            }
            keys::ListKey::Back => {
                self.picker.filtering = false;
                self.picker.filter.clear();
                // The picker is the exit when nothing is open (there would be
                // no manager to go back to); otherwise it is a step back.
                if self.mgr.is_empty() {
                    if self.hosts.is_empty() {
                        self.status = None;
                    }
                    return Ok(Step::Quit);
                }
                self.screen = Screen::Manager;
            }
            keys::ListKey::None => {}
        }
        Ok(Step::Continue)
    }

    // ─────────────────────────── manager ───────────────────────────

    async fn on_manager_key(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        k: KeyEvent,
    ) -> Result<Step, Box<dyn std::error::Error>> {
        if self.manager_filtering {
            let mut query = self.mgr.filter().to_string();
            match k.code {
                KeyCode::Esc => {
                    self.manager_filtering = false;
                    self.mgr.set_filter("");
                    return Ok(Step::Continue);
                }
                KeyCode::Enter => self.manager_filtering = false,
                KeyCode::Backspace => { query.pop(); self.mgr.set_filter(query); return Ok(Step::Continue); }
                KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.mgr.set_filter("");
                    return Ok(Step::Continue);
                }
                KeyCode::Char(ch) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    if query.chars().count() < MAX_FILTER { query.push(ch); }
                    self.mgr.set_filter(query);
                    return Ok(Step::Continue);
                }
                _ => {
                    // Navigation remains available while typing a filter.
                    if matches!(k.code, KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End | KeyCode::Tab | KeyCode::BackTab) {
                        self.mgr.apply(keys::manager_command(&k));
                    }
                    return Ok(Step::Continue);
                }
            }
        } else if k.code == KeyCode::Char('/') {
            self.manager_filtering = true;
            self.status = None;
            return Ok(Step::Continue);
        } else if k.code == KeyCode::Esc && !self.mgr.filter().is_empty() {
            self.mgr.set_filter("");
            return Ok(Step::Continue);
        }
        if k.code == KeyCode::Char('?') {
            self.open_help();
            return Ok(Step::Continue);
        }
        let cmd = keys::manager_command(&k);
        match self.mgr.apply(cmd) {
            ManagerOutcome::Done => {}
            ManagerOutcome::Attach(id) => {
                self.mgr.activate(id);
                self.screen = Screen::Session(id);
            }
            ManagerOutcome::Retry(id) => self.retry(terminal, id).await?,
            ManagerOutcome::NewSession => self.show_picker().await,
            ManagerOutcome::Agent => self.open_agent(),
            ManagerOutcome::Close(id) => {
                let live = self.mgr.get(id).map(|s| s.state.is_live()).unwrap_or(false);
                if live {
                    let title = self
                        .mgr
                        .get(id)
                        .map(|s| s.title.clone())
                        .unwrap_or_else(|| id.to_string());
                    self.confirm = Some(format!(
                        "close \"{title}\"? it is still connected   [y] close   [n] keep"
                    ));
                    self.confirm_close = Some(id);
                } else {
                    self.close_session(id).await;
                }
            }
            ManagerOutcome::Rename(id) => {
                let current = self
                    .mgr
                    .get(id)
                    .map(|s| s.title.clone())
                    .unwrap_or_default();
                if let Some(name) = prompt_line(terminal, "rename session", &current).await? {
                    let name = name.trim().to_string();
                    if !name.is_empty() {
                        if let Some(slot) = self.mgr.get_mut(id) {
                            slot.title = name;
                            // The remote's own title would otherwise keep
                            // shadowing the name the user just chose.
                            slot.remote_title = None;
                        }
                    }
                }
            }
            ManagerOutcome::Quit => return self.request_quit(),
            ManagerOutcome::Help => self.open_help(),
        }
        Ok(Step::Continue)
    }

    /// Quit, asking first when sessions would be closed with the frontend.
    fn request_quit(&mut self) -> Result<Step, Box<dyn std::error::Error>> {
        let live = self.mgr.live_count();
        if live == 0 {
            return Ok(Step::Quit);
        }
        self.confirm = Some(format!(
            "quit and close {live} live session(s)?   [y] quit   [n] stay"
        ));
        self.confirm_quit = true;
        Ok(Step::Continue)
    }

    /// Open the DSH agent page (from the manager, the host list, or a session).
    ///
    /// Entering the page is the request to use it: the page's own upkeep then
    /// connects, lists the host's sessions and follows the most recent one,
    /// reporting in the page whatever fails.
    fn open_agent(&mut self) {
        if self.screen != Screen::Dsh {
            self.dsh_return = Some(self.screen);
        }
        // Entering the page is also a request to use it: when it is showing no
        // conversation yet, let the upkeep list and land again. The first
        // attempt can have run against a connection that was still coming up
        // (or a hub that was not open), and without this the page would keep
        // saying "no session" for the rest of the run.
        if self.dsh.session_id.is_none() {
            self.dsh.sessions_loaded = false;
            self.dsh.landing_done = false;
        }
        self.screen = Screen::Dsh;
    }

    /// Leave the DSH page.
    ///
    /// Not "the session manager": that lists *terminal* sessions, so a user who
    /// opened the agent from the host list — or who has no terminals open at all
    /// — arrived at an empty screen saying no sessions, with no sign that the
    /// conversation they were reading still exists. Leaving goes back where the
    /// page was opened from, and when that is nowhere to return to (a session
    /// that has since been closed) it lands on whichever list has something in
    /// it.
    fn leave_agent(&mut self) {
        let origin = self.dsh_return.take();
        // A session that has since been closed is not somewhere to go back to.
        let alive = match origin {
            Some(Screen::Session(id)) => self.mgr.get(id).is_some(),
            _ => false,
        };
        self.screen = leave_agent_target(origin, alive, !self.mgr.is_empty());
    }

    fn open_help(&mut self) {
        self.help_return = Some(self.screen);
        self.help_scroll = 0;
        self.screen = Screen::Help;
    }

    /// Close a session: drop its slot, close it on the daemon, and (for an open
    /// still in flight) reap it when it lands.
    async fn close_session(&mut self, id: SessionId) {
        let live = self.mgr.remove(id);
        let was_on_screen = self.screen.session() == Some(id);
        let Some(live) = live else { return };
        if let Some(m) = live.mirror {
            if let Err(e) = m.close().await {
                warn!("closing session {id} failed: {e}");
            }
        }
        if let Some(task) = live.open {
            self.orphans.push(task);
        }
        if was_on_screen {
            self.screen = if self.mgr.is_empty() {
                Screen::Picker
            } else {
                Screen::Manager
            };
            if self.screen == Screen::Picker {
                self.show_picker().await;
            }
        }
        self.status = Some(format!("closed session {id}"));
    }

    /// Close everything on the way out.
    async fn close_all(&mut self) {
        for id in self.mgr.ids() {
            let live = self.mgr.remove(id);
            if let Some(live) = live {
                if let Some(m) = live.mirror {
                    let _ = m.close().await;
                }
            }
        }
    }

    // ─────────────────────────── dsh ───────────────────────────

    /// Keep the DSH page's link and rows current.
    ///
    /// The page drives this itself (`DshFeed::upkeep`): it connects once, then
    /// lists the host's sessions and follows one as soon as the daemon reports
    /// the connection is up — which is not the moment it was asked for, so this
    /// cannot be a one-shot at attach time.
    async fn pump_dsh(&mut self) {
        let addr = self.addr.clone();
        // A running turn animates: the spinner turns and the elapsed time counts.
        // That is ten frames a second, not sixty — the streamed content already
        // forces a redraw when it changes, and the animation tick only decides
        // how often the *spinner* moves.
        let changed = self.dsh.upkeep(&addr).await;
        if self.dsh.take_close_requested() {
            self.status = self.dsh.status.clone();
            self.leave_agent();
            self.dsh_view = DshView::default();
            self.dsh_wants_older = false;
            self.dirty = true;
            return;
        }
        if std::mem::take(&mut self.dsh_wants_older) {
            match self.dsh.load_older().await {
                Ok(added) if added > 0 => {
                    // The page was prepended, which pushes the window down by
                    // exactly the lines it added: move the reader with it so the
                    // line they were reading stays on screen.
                    self.dsh_view.refresh_transcript(&self.dsh);
                    let lines = self.dsh_view.lines_in_first(added);
                    self.dsh_view.scroll = self.dsh_view.scroll.saturating_add(lines);
                    self.dsh.status = Some(format!("{added} older row(s)"));
                    self.dirty = true;
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("dsh page: {e}");
                    self.dsh.status = Some(e);
                }
            }
        }
        let animate = self.dsh.running
            && self.dsh_view.animation_due(std::time::Instant::now());
        if changed || animate {
            self.dirty = true;
        }
    }

    /// Handle one key on the DSH page.
    async fn on_dsh_key(&mut self, k: KeyEvent) -> Result<Step, Box<dyn std::error::Error>> {
        // A waterfall owns the keyboard: the agent is blocked on the answer.
        match self.dsh.pending.clone() {
            Some(Pending::Approval { tool, .. }) => {
                match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        info!("allowing {tool} once");
                        self.dsh.answer_approval(true).await;
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        info!("rejecting {tool}");
                        self.dsh.answer_approval(false).await;
                    }
                    _ => {}
                }
                return Ok(Step::Continue);
            }
            Some(Pending::Question(_)) => {
                self.on_question_key(k).await;
                return Ok(Step::Continue);
            }
            None => {}
        }
        // The session list, when it is open. A host can list hundreds of
        // sessions, so `/` filters and the cursor moves through the matches.
        if self.dsh_view.sessions {
            if self.dsh_view.session_filtering {
                match k.code {
                    // Esc clears the filter first, and closes the list once there
                    // is nothing left to clear.
                    KeyCode::Esc => {
                        if self.dsh_view.session_filter.is_empty() {
                            self.dsh_view.session_filtering = false;
                        } else {
                            self.dsh_view.session_filter.clear();
                        }
                        self.dsh_view.session_cursor = 0;
                    }
                    KeyCode::Enter => {
                        let picked = self.picked_session();
                        self.dsh_view.sessions = false;
                        self.dsh_view.session_filtering = false;
                        if let Some(id) = picked {
                            if let Err(e) = self.dsh.open(&id).await {
                                warn!("dsh page: {e}");
                                self.dsh.status = Some(format!("could not open {id}: {e}"));
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        self.dsh_view.session_filter.pop();
                        self.dsh_view.session_cursor = 0;
                    }
                    KeyCode::Down => {
                        self.dsh_view.session_cursor = self.dsh_view.session_cursor.saturating_add(1);
                    }
                    KeyCode::Up => {
                        self.dsh_view.session_cursor = self.dsh_view.session_cursor.saturating_sub(1);
                    }
                    KeyCode::Char(ch) => {
                        self.dsh_view.session_filter.push(ch);
                        self.dsh_view.session_cursor = 0;
                    }
                    _ => {}
                }
                self.clamp_session_cursor();
                return Ok(Step::Continue);
            }
            match k.code {
                KeyCode::Char('/') => {
                    self.dsh_view.session_filtering = true;
                    self.dsh_view.session_cursor = 0;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.dsh_view.session_cursor = self.dsh_view.session_cursor.saturating_add(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.dsh_view.session_cursor = self.dsh_view.session_cursor.saturating_sub(1);
                }
                KeyCode::Enter => {
                    let picked = self.picked_session();
                    self.dsh_view.sessions = false;
                    if let Some(id) = picked {
                        if let Err(e) = self.dsh.open(&id).await {
                            warn!("dsh page: {e}");
                            self.dsh.status = Some(format!("could not open {id}: {e}"));
                        }
                    }
                }
                KeyCode::Esc | KeyCode::Tab | KeyCode::Char('q') => self.dsh_view.sessions = false,
                _ => {}
            }
            self.clamp_session_cursor();
            return Ok(Step::Continue);
        }
        match keys::session_key(&k, self.detach) {
            // Leaving the page: the daemon keeps the conversation, so coming
            // back is instant.
            SessionKey::Detach => self.leave_agent(),
            SessionKey::Scroll(s) => self.dsh_scroll(s),
            // `Esc` is the composer's only exception: it means "back", like it
            // does on every other screen here, and unlike the other keys it has
            // nothing to type into the composer.
            _ if k.code == KeyCode::Esc => self.leave_agent(),
            // Every other key is the composer's (`session_key` would have sent
            // it to a remote; here the remote is a chat, not a terminal).
            _ => self.dsh_edit(k).await,
        }
        Ok(Step::Continue)
    }

    /// The session the list's cursor is on, in the filtered rows.
    fn picked_session(&self) -> Option<String> {
        self.dsh_view
            .filtered_sessions(&self.dsh.sessions)
            .get(self.dsh_view.session_cursor)
            .map(|s| s.session_id.clone())
    }

    /// Keep the list's cursor on a row that exists.
    fn clamp_session_cursor(&mut self) {
        let len = self.dsh_view.filtered_sessions(&self.dsh.sessions).len();
        self.dsh_view.session_cursor =
            self.dsh_view.session_cursor.min(len.saturating_sub(1));
    }

    /// Move through the transcript.
    fn dsh_scroll(&mut self, s: Scroll) {
        {
            let view = &mut self.dsh_view;
            match s {
                Scroll::PageUp => view.scroll = view.scroll.saturating_add(10),
                Scroll::PageDown => view.scroll = view.scroll.saturating_sub(10),
                Scroll::LineUp => view.scroll = view.scroll.saturating_add(1),
                Scroll::LineDown => view.scroll = view.scroll.saturating_sub(1),
                // The renderer clamps to however much there is to scroll.
                Scroll::Top => view.scroll = usize::MAX,
                Scroll::Bottom => view.scroll = 0,
            }
        }
        // Already at the very top and asked to go further back: that is a request
        // for the page the host is holding (the reader keeps pressing, so the
        // cooldown in `load_older` is what paces the fetches).
        if s != Scroll::Bottom && self.dsh_view.at_top() {
            self.dsh_wants_older = true;
        }
    }

    /// The composer: editing, sending, stopping, and the session list.
    async fn dsh_edit(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Enter => self.dsh_send(PromptMode::Queue).await,
            // Steer folds the message into the turn that is running instead of
            // waiting for the next one.
            KeyCode::Char('s') if ctrl => self.dsh_send(PromptMode::Steer).await,
            KeyCode::Char('c') if ctrl => {
                if self.dsh.running {
                    match self.dsh.cancel_turn().await {
                        Ok(()) => self.dsh.status = Some("asked the agent to stop".into()),
                        Err(e) => self.dsh.status = Some(format!("stop failed: {e}")),
                    }
                } else {
                    self.dsh.status = Some("nothing is running".into());
                }
            }
            KeyCode::Char('x') if ctrl => {
                let oldest = self.dsh.queue.first().map(|q| q.id.clone());
                match oldest {
                    Some(id) => match self.dsh.drop_queued(&id).await {
                        Ok(()) => self.dsh.status = Some("dropped the first queued message".into()),
                        Err(e) => self.dsh.status = Some(format!("drop failed: {e}")),
                    },
                    None => self.dsh.status = Some("the queue is empty".into()),
                }
            }
            KeyCode::Tab => {
                // The list is the manual way to the same thing `upkeep` does
                // automatically: whichever session the user picks becomes the
                // page's session, and `landing_done` keeps upkeep from overriding
                // it later.
                self.dsh_view.sessions = true;
                self.dsh_view.session_cursor = 0;
                self.dsh_view.session_filter.clear();
                self.dsh_view.session_filtering = false;
                self.dsh.landing_done = true;
                if let Err(e) = self.dsh.refresh_sessions().await {
                    self.dsh.status = Some(e);
                }
            }
            KeyCode::Char('u') if ctrl => self.dsh_view.clear(),
            KeyCode::Char('w') if ctrl => self.dsh_view.delete_word(),
            KeyCode::Backspace => self.dsh_view.backspace(),
            KeyCode::Delete => self.dsh_view.delete(),
            KeyCode::Left => self.dsh_view.left(),
            KeyCode::Right => self.dsh_view.right(),
            KeyCode::Home => self.dsh_view.home(),
            KeyCode::End => self.dsh_view.end(),
            KeyCode::Char(ch) if !ctrl => self.dsh_view.insert(ch),
            _ => {}
        }
    }

    /// Send what is in the composer, handing it back if the daemon refuses.
    async fn dsh_send(&mut self, mode: PromptMode) {
        if self.dsh_view.input.trim().is_empty() {
            return;
        }
        let text = self.dsh_view.take();
        let preview: String = text.trim().chars().take(40).collect();
        let mode_name = match mode {
            PromptMode::Queue => "queued",
            PromptMode::Steer => "steering",
        };
        match self.dsh.send(&text, mode).await {
            Ok(()) => self.dsh.status = Some(format!("{mode_name}: {preview}")),
            Err(e) => {
                // Do not eat what the user typed.
                self.dsh_view.input = text;
                self.dsh_view.end();
                self.dsh.status = Some(format!("send failed: {e}"));
            }
        }
    }

    /// One key while the agent's question is on screen.
    async fn on_question_key(&mut self, k: KeyEvent) {
        let (mut submit, mut cancel) = (false, false);
        {
            let Some(Pending::Question(q)) = self.dsh.pending.as_mut() else {
                return;
            };
            if q.typing {
                match k.code {
                    KeyCode::Enter => {
                        let typed = q.input.trim().to_string();
                        if let Some(slot) = q.custom.get_mut(q.index) {
                            *slot = typed;
                        }
                        q.input.clear();
                        q.typing = false;
                        submit = true;
                    }
                    KeyCode::Esc => {
                        q.typing = false;
                        q.input.clear();
                    }
                    KeyCode::Backspace => {
                        q.input.pop();
                    }
                    KeyCode::Char(ch) => q.input.push(ch),
                    _ => {}
                }
            } else {
                match k.code {
                    KeyCode::Char(ch @ '1'..='9') => q.pick(ch as usize - '0' as usize),
                    // `t` types an answer instead of picking an option.
                    KeyCode::Char('t') => q.typing = true,
                    KeyCode::Enter => submit = true,
                    KeyCode::Esc => cancel = true,
                    _ => {}
                }
            }
        }
        if submit {
            self.dsh.submit_question().await;
        }
        if cancel {
            self.dsh.cancel_question().await;
        }
    }
    // ─────────────────────────── sessions ───────────────────────────

    /// Reserve a slot and start opening it on the daemon.
    ///
    /// The open runs in a task because an SSH open blocks until its host
    /// questions are answered, and those answers come from this loop.
    async fn begin_open(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        spec: OpenSpec,
        title: String,
        target: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (cols, rows) = session_grid(terminal)?;
        let spec = with_grid(spec, cols, rows);
        // A config-backed SSH open is vault-gated on the daemon side, so make
        // sure the connection can present a token before opening. Shells and
        // agent auth do not need one.
        let token = match &spec {
            OpenSpec::Shell(_) | OpenSpec::Serial(_) => None,
            _ => match unlock_before_open(terminal, &self.addr, &mut self.vault_token).await? {
                UnlockOutcome::Token(t) => t,
                UnlockOutcome::Abort(msg) => {
                    self.status = Some(msg);
                    return Ok(());
                }
            },
        };
        let id = self.mgr.insert(title, target, Some(spec.clone()));
        let live = spawn_open(&self.addr, spec, cols, rows, token);
        self.mgr.set_handle(id, live);
        if let Some(slot) = self.mgr.get_mut(id) {
            slot.cols = cols;
            slot.rows = rows;
        }
        self.mgr.activate(id);
        self.screen = Screen::Session(id);
        self.status = None;
        Ok(())
    }

    /// Open a session again from the spec it was created with.
    async fn retry(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        id: SessionId,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(spec) = self.mgr.get(id).and_then(|s| s.spec.clone()) else {
            return Ok(());
        };
        let (cols, rows) = session_grid(terminal)?;
        let spec = with_grid(spec, cols, rows);
        let token = match &spec {
            OpenSpec::Shell(_) | OpenSpec::Serial(_) => None,
            _ => match unlock_before_open(terminal, &self.addr, &mut self.vault_token).await? {
                UnlockOutcome::Token(t) => t,
                UnlockOutcome::Abort(msg) => {
                    self.status = Some(msg);
                    return Ok(());
                }
            },
        };
        // The old handle (a dead mirror, or a finished task) goes first: its
        // slot, id and position are reused, so the manager row stays put.
        if let Some(slot) = self.mgr.get_mut(id) {
            slot.handle = Some(spawn_open(&self.addr, spec, cols, rows, token));
            slot.state = SessionState::Connecting;
            slot.awaiting = false;
            slot.unread = false;
            slot.bell = false;
            slot.remote_title = None;
            slot.cols = cols;
            slot.rows = rows;
        }
        self.mgr.activate(id);
        self.screen = Screen::Session(id);
        self.status = None;
        Ok(())
    }

    async fn on_session_key(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, k: KeyEvent) -> Result<Step, Box<dyn std::error::Error>> {
        let Some(id) = self.screen.session() else {
            return Ok(Step::Continue);
        };
        if !self.mgr.get(id).is_some_and(|slot| slot.state.is_live()) {
            match ended_session_key(&k, self.detach) {
                EndedSessionKey::Reconnect => self.retry(terminal, id).await?,
                EndedSessionKey::Back => self.leave_ended_session(id),
                EndedSessionKey::Ignore => {}
            }
            return Ok(Step::Continue);
        }
        // A host question owns the keyboard: the daemon is waiting on it, and
        // nothing else can make progress until it is answered.
        let has_ask = self
            .mgr
            .get(id)
            .and_then(|s| s.handle.as_ref())
            .map(|l| l.ask.is_some())
            .unwrap_or(false);
        if has_ask {
            if let Some(live) = self.mgr.get_mut(id).and_then(|s| s.handle.as_mut()) {
                if let Some(picker) = live.file_picker.as_mut() {
                    match picker.key(k) {
                        files::Choice::Pending => {}
                        files::Choice::Paths(paths) => {
                            self.transfer_directory = picker.directory_path().map(str::to_string);
                            let save = live.ask.as_ref().is_some_and(|ask| matches!(&ask.ask.kind, AskKind::ZmodemPath(path) if path.save));
                            live.transfer.selected(save, &paths);
                            if let Some(ask) = live.ask.as_mut() { ask.submit_paths(paths); }
                            live.ask = None;
                            live.file_picker = None;
                        }
                        files::Choice::Cancel => {
                            if let Some(ask) = live.ask.as_mut() { ask.cancel(); }
                            live.ask = None;
                            live.file_picker = None;
                        }
                    }
                    if live.file_picker.is_none() {
                        if let Some(task) = live.path_task.take() { task.abort(); }
                    }
                    return Ok(Step::Continue);
                }
                if let Some(p) = live.ask.as_mut() {
                    handle_ask_key(p, &k);
                    if !p.is_pending() {
                        live.ask = None;
                    }
                }
            }
            return Ok(Step::Continue);
        }

        if k.code == KeyCode::Char('x') && k.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(live) = self.mgr.get_mut(id).and_then(|s| s.handle.as_mut()).filter(|live| live.transfer.active) {
                if let Some(mirror) = live.mirror.as_ref() {
                    let result = mirror.writer().call(term_proto::ClientMethod::ZmodemCancel { sid: mirror.sid() }).await;
                    live.transfer.label = Some(match result {
                        Ok(term_proto::ServerResult::Ok(_)) => "cancellation requested".into(),
                        Ok(term_proto::ServerResult::Err(error)) => format!("cancel failed: {}", error.message),
                        Err(error) => format!("cancel failed: {error}"),
                    });
                }
                return Ok(Step::Continue);
            }
        }

        if let Some(live) = self.mgr.get_mut(id).and_then(|s| s.handle.as_mut()) {
            if !live.transfer.active { live.transfer.label = None; }
        }

        match keys::session_key(&k, self.detach) {
            // Leaving the session is the whole point of the manager: it keeps
            // running, and its row is where the user left it.
            SessionKey::Detach => {
                self.mgr.detach();
                self.mgr.select(id);
                self.screen = Screen::Manager;
            }
            SessionKey::Scroll(s) => self.scroll(id, s).await,
            SessionKey::Input(bytes) => self.send_bytes(id, bytes).await,
            SessionKey::Ignore => {}
        }
        Ok(Step::Continue)
    }

    /// Feed bytes to a session's mirror, which echoes them locally and
    /// forwards what its terminal logic decided to the remote.
    async fn send_bytes(&mut self, id: SessionId, bytes: Vec<u8>) {
        if !self.mgr.get(id).is_some_and(|slot| slot.state == SessionState::Open) { return; }
        let Some(m) = self
            .mgr
            .get_mut(id)
            .and_then(|s| s.handle.as_mut())
            .and_then(|l| l.mirror.as_mut())
        else {
            return;
        };
        m.inject_key(bytes);
        if let Err(e) = m.forward_output().await {
            warn!("session {id}: forwarding input failed: {e}");
        }
    }

    /// Move through the mirror's scrollback. The remote never sees this.
    async fn scroll(&mut self, id: SessionId, s: Scroll) {
        let Some(m) = self
            .mgr
            .get_mut(id)
            .and_then(|slot| slot.handle.as_mut())
            .and_then(|l| l.mirror.as_mut())
        else {
            return;
        };
        let cmd = match s {
            Scroll::PageUp => BackendCommand::ScrollPageUp,
            Scroll::PageDown => BackendCommand::ScrollPageDown,
            Scroll::Top => BackendCommand::ScrollToTop,
            Scroll::Bottom => BackendCommand::ScrollToBottom,
            Scroll::LineUp => BackendCommand::Scroll(1, NO_MODS, Point::default()),
            Scroll::LineDown => BackendCommand::Scroll(-1, NO_MODS, Point::default()),
        };
        m.backend().process_command(cmd);
        if let Err(error) = m.forward_output().await {
            warn!("session {id}: forwarding scroll failed: {error}");
        }
    }
}

/// Start one open on the daemon, with the ask bridge that answers its host
/// questions.
fn spawn_open(addr: &str, spec: OpenSpec, cols: u16, rows: u16, token: Option<String>) -> Live {
    let (bridge, asks) = AskBridge::channel();
    let ask_host: Arc<dyn AskHost> = Arc::new(bridge);
    let addr = addr.to_string();
    let open = tokio::spawn(async move {
        MirrorSession::open(
            &addr[..],
            MirrorSessionParams::with_ask_host(spec, cols, rows, ask_host)
                .with_socket_token(token),
        )
        .await
        .map_err(|e| e.to_string())
    });
    Live {
        open: Some(open),
        mirror: None,
        asks,
        ask: None,
        last_data: None,
        file_picker: None,
        path_task: None,
        transfer: files::TransferProgress::default(),
    }
}

/// Write the window's grid into a spec (the daemon allocates the PTY from it).
fn with_grid(spec: OpenSpec, cols: u16, rows: u16) -> OpenSpec {
    match spec {
        OpenSpec::SshFromConfig { config_id, .. } => OpenSpec::SshFromConfig {
            config_id,
            cols,
            rows,
        },
        OpenSpec::Ssh(mut s) => {
            s.cols = cols;
            s.rows = rows;
            OpenSpec::Ssh(s)
        }
        OpenSpec::Shell(mut s) => {
            s.cols = cols;
            s.rows = rows;
            OpenSpec::Shell(s)
        }
        OpenSpec::Serial(mut s) => {
            s.cols = cols;
            s.rows = rows;
            OpenSpec::Serial(s)
        }
    }
}

/// Whether a host matches the picker's filter text.
///
/// Searches what the user can see: the name, the target, the folder and the
/// auth label. Names are what people remember, but a filter that only searched
/// names would not find a host by address.
fn host_matches(h: &SessionEntryWire, needle: &str) -> bool {
    let hay = [
        h.name.to_lowercase(),
        h.target().to_lowercase(),
        h.folder.clone().unwrap_or_default().to_lowercase(),
        h.auth_label().to_lowercase(),
    ];
    hay.iter().any(|s| s.contains(needle))
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os().skip(1).any(|arg| arg == "--help" || arg == "-h") {
        println!("EasyTerm TUI\n\
Usage: term_tui [HOST:PORT] [--list | --shell | --ssh USER@HOST[:PORT] | --dsh]\n\n\
Start the matching EasyTerm daemon with: easyterm --daemon\n\
Without an address, the current user's daemon is discovered automatically.\n\
Ctrl-] opens the session manager; ? opens help in the manager or saved-host list.\n\
Run rz/sz in an SSH session for ZMODEM. File paths belong to the daemon's machine.\n\
--help, -h     Show this help without connecting\n\
--version, -V  Show the frontend and protocol versions");
        return Ok(());
    }
    if std::env::args_os().skip(1).any(|arg| arg == "--version" || arg == "-V") {
        println!("term_tui {} (protocol {})", env!("CARGO_PKG_VERSION"), term_proto::PROTO_VERSION);
        return Ok(());
    }
    env_logger::init();

    let Cli {
        daemon_addr,
        start,
        detach,
    } = parse_args()?;
    let explicit = daemon_addr.is_some();
    let addr = match resolve_daemon_addr(daemon_addr) {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let mut vault_token = None;
    if explicit {
        term_client::configure_external_endpoint(addr.clone(), term_client::daemon_trust_dir())?;
        let probe = Client::connect_external(addr.as_str()).await?;
        if probe.requires_master_password() {
            let (_, has_password, _) = probe.vault_status().await?;
            if !has_password {
                return Err("Set a master password on the daemon machine before connecting remotely".into());
            }
            drop(probe);
            let Some(token) = remote_login(&addr).await? else { return Ok(()) };
            term_client::set_external_vault_token(Some(token.clone()));
            vault_token = Some(token);
        }
    }
    info!("daemon {addr}");

    // `--list` is a plain stdout command: no raw mode, no alternate screen.
    if matches!(start, Start::List) {
        let hosts = sessions::load(&addr).await?;
        println!("daemon {addr}: {} saved host(s)", hosts.len());
        for h in &hosts {
            let mut line = format!("  {:>5}  {}  {}", h.config_id, h.name, h.target());
            if let Some(folder) = &h.folder {
                line.push_str(&format!("  [{folder}]"));
            }
            if let Some(jump) = &h.jump {
                line.push_str(&format!("  via {jump}"));
            }
            line.push_str(&format!("  {}", h.auth_label()));
            println!("{line}");
        }
        return Ok(());
    }

    // Raw terminal on: everything below talks to the daemon and renders.
    // Bracketed paste so a paste arrives as one event (and can be forwarded
    // with the markers the remote asked for) instead of a burst of keys.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(addr, detach);
    app.vault_token = vault_token;
    if matches!(start, Start::Dsh) {
        app.open_agent();
    }
    let startup = match start {
        Start::List | Start::Dsh => None,
        Start::Picker => {
            app.show_picker().await;
            None
        }
        Start::Ssh(target) => match direct_spec(&target) {
            Ok(spec) => Some((spec, target.clone(), target)),
            Err(e) => {
                app.show_picker().await;
                app.status = Some(e);
                None
            }
        },
        Start::Shell => Some((
            OpenSpec::Shell(term_proto::ShellOpenSpec {
                display_encoding: None,
                args: Vec::new(),
                env: Vec::new(),
                shell: None,
                cwd: None,
                cols: PLACEHOLDER_COLS,
                rows: PLACEHOLDER_ROWS,
            }),
            "local shell".to_string(),
            "local shell".to_string(),
        )),
    };
    let res = match startup {
        Some((spec, title, target)) => match app.begin_open(&mut terminal, spec, title, target).await {
            Ok(()) => app.run(&mut terminal).await,
            Err(e) => Err(e),
        },
        None => app.run(&mut terminal).await,
    };

    let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
    // The title is the terminal's, not ours: hand it back rather than leaving a
    // dead session's name on the user's tab.
    write_window_title("");
    disable_raw_mode()?;
    res
}

async fn remote_login(addr: &str) -> Result<Option<String>, Box<dyn std::error::Error>> {
    while let Some(password) = read_remote_password(addr)? {
        let client = Client::connect_external(addr).await?;
        if !client.requires_master_password() {
            return Err("The daemon no longer offers secure master-password login".into());
        }
        let result = client.unlock(&password).await;
        drop(password);
        match result {
            Ok(token) => return Ok(Some(token)),
            Err(error) => eprintln!("Sign-in failed: {error}"),
        }
    }
    Ok(None)
}

fn read_remote_password(addr: &str) -> io::Result<Option<String>> {
    if !io::stdin().is_terminal() {
        return Err(io::Error::other("Remote login needs a terminal to enter the master password"));
    }
    eprint!("Master password for {addr}: ");
    io::stderr().flush()?;
    enable_raw_mode()?;
    let result = (|| {
        let mut password = String::new();
        loop {
            let CEvent::Key(key) = event::read()? else { continue };
            if key.kind == KeyEventKind::Release { continue; }
            match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
                KeyCode::Enter if !password.is_empty() => return Ok(Some(password)),
                KeyCode::Backspace => { password.pop(); }
                KeyCode::Char(ch) if password.len() + ch.len_utf8() <= 4096 => password.push(ch),
                _ => {}
            }
        }
    })();
    let restored = disable_raw_mode();
    eprintln!();
    restored?;
    result
}

/// Parse argv. Unknown arguments are reported but not fatal (the frontend may
/// be launched by a script that passes extra flags through).
fn parse_args() -> Result<Cli, String> {
    let mut daemon_addr: Option<String> = None;
    let mut ssh_target: Option<String> = None;
    let mut local_shell = false;
    let mut list_hosts = false;
    let mut agent = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--ssh" => ssh_target = args.next(),
            "--shell" => local_shell = true,
            "--list" => list_hosts = true,
            "--dsh" => agent = true,
            endpoint if !endpoint.starts_with('-') && daemon_addr.is_none()
                && endpoint.rsplit_once(':').is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok()) => {
                daemon_addr = Some(a);
            }
            other => return Err(format!("unrecognized argument {other:?}; use --help")),
        }
    }

    // `--list` is a query, not a session: it wins over the others.
    let start = if list_hosts {
        Start::List
    } else if agent {
        Start::Dsh
    } else {
        // `--ssh` wins over `--shell`; the picker is the default (the
        // pre-picker default, a local shell, is still reachable with `--shell`).
        match (ssh_target, local_shell) {
            (Some(target), _) => Start::Ssh(target),
            (None, true) => Start::Shell,
            (None, false) => Start::Picker,
        }
    };

    Ok(Cli {
        daemon_addr,
        start,
        detach: Chord::from_env(),
    })
}

/// Where the daemon is.
///
/// An explicit address wins over local discovery.
///
/// There is deliberately **no default port**: guessing one is how a frontend ends
/// up talking to nothing (or to something else) and reporting it as a daemon
/// problem. Not knowing where the daemon is, is a thing the frontend can say
/// clearly and immediately.
fn resolve_daemon_addr(explicit: Option<String>) -> Result<String, String> {
    pick_addr(explicit, term_discovery::discover_addr)
}

fn pick_addr(explicit: Option<String>, discover: impl FnOnce() -> Option<String>) -> Result<String, String> {
    explicit.or_else(discover).ok_or_else(|| {
        "No daemon has announced itself on this machine. Start one with `easyterm --daemon`, \
         or give its address as `term_tui HOST:PORT`.".into()
    })
}

/// Build the `SshOpenSpec` for `user@host[:port]` (no saved config).
fn direct_spec(target: &str) -> Result<OpenSpec, String> {
    let (user, hostport) = target
        .split_once('@')
        .ok_or_else(|| format!("--ssh expects user@host[:port], got {target:?}"))?;
    let (host, port) = match hostport.split_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|e| format!("bad port in {target:?}: {e}"))?,
        ),
        None => (hostport.to_string(), 22u16),
    };
    Ok(OpenSpec::Ssh(term_proto::SshOpenSpec {
        display_encoding: None,
        host,
        port,
        username: user.to_string(),
        // Live input: the daemon asks (host-key then kbdint/password) over the
        // corridor; the overlay answers.
        auth: vec![term_proto::SshAuthRef::Interact(None)],
        cols: PLACEHOLDER_COLS,
        rows: PLACEHOLDER_ROWS,
        socks_proxy: None,
        keepalive_secs: None,
        connect_timeout_secs: None,
        jump: None,
        kex_algorithms: Vec::new(),
        ciphers: Vec::new(),
        macs: Vec::new(),
        host_key_algorithms: Vec::new(),
        compression: None,
    }))
}

/// The grid a session should use: the terminal's size, less the status bar row.
fn session_grid(
    terminal: &Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<(u16, u16), Box<dyn std::error::Error>> {
    let size = terminal.size()?;
    Ok((size.width.max(1), size.height.saturating_sub(1).max(1)))
}

// ─────────────────────── vault unlock ───────────────────────

/// The result of the pre-open vault check.
enum UnlockOutcome {
    /// Open with this token (or no token when the daemon needs none).
    Token(Option<String>),
    /// The user cancelled (or the check failed); the string is the status
    /// message for whichever screen is showing.
    Abort(String),
}

/// Ask the daemon's vault status and, when a token is required, prompt for the
/// master password and unlock it.
///
/// The daemon gates a config-backed SSH open on an authenticated connection and
/// the TUI has no vault of its own, so the master password is the only way in.
///
/// `cached` is the token this frontend already unlocked with: it is reused for
/// every later open, so picking a second host does not ask again. A cached token
/// that the daemon rejects (it restarted, or another frontend re-keyed the
/// vault) is dropped and the password is asked for once more, rather than
/// failing every open from then on.
async fn unlock_before_open(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    addr: &str,
    cached: &mut Option<String>,
) -> Result<UnlockOutcome, Box<dyn std::error::Error>> {

    // Already unlocked this run: prove the token still works and go.
    if let Some(token) = cached.clone() {
        match tokio::time::timeout(STATUS_TIMEOUT, Client::connect(addr)).await {
            Ok(Ok(check)) => match check.auth(&token).await {
                Ok(()) => {
                    log::debug!("reusing this run's vault token");
                    return Ok(UnlockOutcome::Token(Some(token)));
                }
                Err(e) => {
                    log::warn!("the cached vault token was rejected ({e}); asking again");
                    *cached = None;
                }
            },
            Ok(Err(e)) => return Ok(UnlockOutcome::Abort(format!("daemon unreachable: {e}"))),
            Err(_) => return Ok(UnlockOutcome::Abort("daemon connect timed out".to_string())),
        }
    }

    // A one-shot connection just for the status question: the session's own
    // connection is opened later by `MirrorSession`.
    let client = match tokio::time::timeout(STATUS_TIMEOUT, Client::connect(addr)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Ok(UnlockOutcome::Abort(format!("daemon unreachable: {e}"))),
        Err(_) => return Ok(UnlockOutcome::Abort("daemon connect timed out".to_string())),
    };
    let (_unlocked, _has_password, _nonce) = match client.vault_status().await {
        Ok(v) => v,
        Err(e) => {
            return Ok(UnlockOutcome::Abort(format!(
                "daemon vault status failed: {e}"
            )));
        }
    };

    // The token is only obtainable by unlocking, and `VaultStatus` never
    // carries one, so a password is needed the first time. (A locked vault with
    // no key file yet reports `has_password: false` and still refuses the open;
    // asking anyway surfaces the daemon's own first-run hint instead of a token
    // that cannot exist.)
    let password = match prompt_master_password(terminal, addr).await? {
        Some(p) => p,
        None => {
            return Ok(UnlockOutcome::Abort(
                "unlock cancelled; a saved SSH host needs the daemon's vault".to_string(),
            ));
        }
    };
    let token = match client.unlock(&password).await {
        Ok(token) => token,
        Err(e) => return Ok(UnlockOutcome::Abort(format!("unlock failed: {e}"))),
    };

    // Prove the token works on a *new* connection (that is the one the session
    // will use); a stale or foreign token would otherwise fail only at open.
    match tokio::time::timeout(STATUS_TIMEOUT, Client::connect(addr)).await {
        Ok(Ok(check)) => {
            if let Err(e) = check.auth(&token).await {
                return Ok(UnlockOutcome::Abort(format!("daemon rejected the token: {e}")));
            }
        }
        Ok(Err(e)) => return Ok(UnlockOutcome::Abort(format!("daemon unreachable: {e}"))),
        Err(_) => return Ok(UnlockOutcome::Abort("daemon connect timed out".to_string())),
    }
    *cached = Some(token.clone());
    Ok(UnlockOutcome::Token(Some(token)))
}

/// Modal master-password prompt. `Some(password)` on Enter, `None` on Esc.
async fn prompt_master_password(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    addr: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    use ratatui::layout::Alignment;

    let mut input = String::new();
    loop {
        terminal.draw(|f| {
            let area = f.area();
            let w = (area.width * 3) / 5;
            let h = 7.min(area.height);
            let box_area = Rect::new(
                area.x + (area.width - w) / 2,
                area.y + (area.height - h) / 2,
                w,
                h,
            );
            let text = format!(
                "The daemon at {addr} needs its vault unlocked\nto read saved SSH credentials.\n\nMaster password: {}\n\n[Enter] unlock   [Esc] cancel",
                "*".repeat(input.chars().count())
            );
            f.render_widget(
                Paragraph::new(text)
                    .block(Block::default().borders(Borders::ALL).title("Vault unlock"))
                    .style(Style::default().fg(Color::Yellow))
                    .wrap(ratatui::widgets::Wrap { trim: true })
                    .alignment(Alignment::Left),
                box_area,
            );
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let CEvent::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match k.code {
                    KeyCode::Enter => return Ok(Some(input)),
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(ch) => input.push(ch),
                    _ => {}
                }
            }
        }
    }
}

/// Modal one-line prompt (session rename). `Some(text)` on Enter, `None` on Esc.
async fn prompt_line(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    title: &str,
    initial: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut input = initial.to_string();
    loop {
        terminal.draw(|f| {
            let area = f.area();
            let w = (area.width * 3) / 5;
            let h = 5.min(area.height);
            let box_area = Rect::new(
                area.x + (area.width - w) / 2,
                area.y + (area.height - h) / 2,
                w,
                h,
            );
            let text = format!("{input}\n\n[Enter] ok   [Esc] cancel");
            f.render_widget(
                Paragraph::new(text)
                    .block(Block::default().borders(Borders::ALL).title(title.to_string()))
                    .style(Style::default().fg(Color::Yellow)),
                box_area,
            );
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let CEvent::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match k.code {
                    KeyCode::Enter => return Ok(Some(input)),
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(ch) => input.push(ch),
                    _ => {}
                }
            }
        }
    }
}

// ─────────────────────────── drawing ───────────────────────────

/// Paint one picker frame: daemon header, the host list (or the empty-list
/// hint), and the key hints / status line.
///
/// Takes the frame (not a terminal) so tests can render it into a
/// `TestBackend` and assert the rows without a terminal.
fn draw_picker(
    f: &mut ratatui::Frame<'_>,
    addr: &str,
    hosts: &[SessionEntryWire],
    status: Option<&str>,
    picker: &mut PickerState,
) {
    use ratatui::layout::{Constraint, Direction, Layout};

    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

    let needle = picker.filter.trim().to_lowercase();
    let shown: Vec<&SessionEntryWire> = hosts
        .iter()
        .filter(|h| needle.is_empty() || host_matches(h, &needle))
        .collect();

    let title = if hosts.is_empty() {
        " EasyTerm — no saved SSH hosts ".to_string()
    } else if needle.is_empty() {
        format!(" EasyTerm — {} saved host(s) ", hosts.len())
    } else {
        format!(" EasyTerm — {} of {} host(s) ", shown.len(), hosts.len())
    };
    let header = Paragraph::new(Line::from(vec![
        Span::styled("daemon ", Style::default().fg(Color::DarkGray)),
        Span::raw(addr.to_string()),
    ]))
    .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(header, chunks[0]);

    if shown.is_empty() {
        let hint = if hosts.is_empty() {
            Paragraph::new(vec![
                Line::from("The daemon listed no saved SSH hosts."),
                Line::from(""),
                Line::from("Add hosts in the EasyTerm GUI, or use --ssh user@host / --shell."),
            ])
        } else {
            Paragraph::new(vec![
                Line::from(format!("No host matches \"{}\".", picker.filter)),
                Line::from(""),
                Line::from("[Esc] clear the filter"),
            ])
        }
        .style(Style::default().fg(Color::Yellow));
        f.render_widget(hint, chunks[1]);
    } else {
        let items: Vec<ListItem> = shown.iter().map(|h| session_row(h)).collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Hosts "))
            .highlight_style(
                Style::default()
                    .bg(Color::Indexed(238))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        picker.selected = picker.selected.min(shown.len() - 1);
        picker.list.select(Some(picker.selected));
        f.render_stateful_widget(list, chunks[1], &mut picker.list);
    }

    let footer = if picker.filtering {
        Line::from(vec![
            Span::styled(" /", Style::default().fg(Color::Yellow)),
            Span::styled(picker.filter.clone(), Style::default().fg(Color::White)),
            Span::styled(
                "   [Enter] connect   [Esc] clear filter ",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else {
        match status {
            Some(msg) => Line::from(Span::styled(
                format!(" {msg}"),
                Style::default().fg(Color::Yellow),
            )),
            None => Line::from(Span::styled(
                " [Up/Down] move   [Enter] connect   [/] filter   [a] dsh agent   [?] keys   [Esc] back ",
                Style::default().fg(Color::DarkGray),
            )),
        }
    };
    f.render_widget(Paragraph::new(footer), chunks[2]);
}

/// One host row: `folder/name  user@host:port  via bastion  [auth]`.
fn session_row(h: &SessionEntryWire) -> ListItem<'static> {
    let mut spans = Vec::new();
    if let Some(folder) = &h.folder {
        spans.push(Span::styled(
            format!("{folder}/"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::styled(
        h.name.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(h.target(), Style::default().fg(Color::Cyan)));
    if let Some(jump) = &h.jump {
        spans.push(Span::styled(
            format!("  via {jump}"),
            Style::default().fg(Color::Magenta),
        ));
    }
    spans.push(Span::styled(
        format!("  [{}]", h.auth_label()),
        Style::default().fg(Color::DarkGray),
    ));
    ListItem::new(Line::from(spans))
}

/// Paint the manager: the daemon header, one row per session, the selected
/// row's detail, and the key hints (or the confirmation prompt).
fn draw_manager<S>(
    f: &mut ratatui::Frame<'_>,
    addr: &str,
    mgr: &Manager<S>,
    status: Option<&str>,
    confirm: Option<&str>,
    filtering: bool,
) {
    use ratatui::layout::{Constraint, Direction, Layout};

    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    let rows = mgr.rows();
    let header = Paragraph::new(Line::from(vec![
        Span::styled("daemon ", Style::default().fg(Color::DarkGray)),
        Span::raw(addr.to_string()),
        Span::styled(
            format!("   {}", mgr.status_line()),
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" EasyTerm — sessions "),
    );
    f.render_widget(header, chunks[0]);

    if rows.is_empty() {
        let lines = if !mgr.filter().is_empty() {
            vec![
                Line::from(format!("No sessions match \"{}\".", mgr.filter())),
                Line::from("[Esc] clear the filter"),
            ]
        } else { vec![
            Line::from("No terminal sessions are open."),
            Line::from(""),
            Line::from("[n] open a saved host"),
            Line::from("[a] the DSH agent"),
        ] };
        let hint = Paragraph::new(lines)
        .style(Style::default().fg(Color::Yellow));
        f.render_widget(hint, chunks[1]);
    } else {
        let items: Vec<ListItem> = rows.iter().map(manager_row).collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(if mgr.filter().is_empty() {
                " Sessions ".to_string()
            } else {
                format!(" Sessions — {} / {} match ", rows.len(), mgr.len())
            }))
            .highlight_style(
                Style::default()
                    .bg(Color::Indexed(238))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
        let mut state = ListState::default();
        state.select(Some(mgr.selection_index().min(rows.len() - 1)));
        f.render_stateful_widget(list, chunks[1], &mut state);
    }

    // The selected row in full: a failing session's reason, or where the
    // session points and how long it has been up.
    let detail = rows
        .get(mgr.selection_index().min(rows.len().saturating_sub(1)))
        .filter(|_| !rows.is_empty())
        .map(|row| match &row.failure {
            Some(reason) => Line::from(vec![
                Span::styled("  failed: ", Style::default().fg(Color::Red)),
                Span::styled(reason.clone(), Style::default().fg(Color::Red)),
                Span::styled("   [Enter] retry ", Style::default().fg(Color::DarkGray)),
            ]),
            None => Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(row.target.clone(), Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("   {}x{}   up {}", row.cols, row.rows, short_age(row.age_secs)),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
        })
        .unwrap_or_else(|| Line::from(""));
    f.render_widget(Paragraph::new(detail), chunks[2]);

    let footer = match (confirm, status) {
        (Some(prompt), _) => Line::from(Span::styled(
            format!(" {prompt}"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        (None, _) if filtering || !mgr.filter().is_empty() => Line::from(Span::styled(
            format!(" /{}{}   {} / {} match   [Enter] attach  [Esc] clear  [Ctrl-U] erase ",
                mgr.filter(), if filtering { "_" } else { "" }, rows.len(), mgr.len()),
            Style::default().fg(Color::Cyan),
        )),
        (None, Some(msg)) => Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(Color::Yellow),
        )),
        (None, None) => Line::from(Span::styled(
            " [Enter] attach  [/] search  [Tab] attention  [n] new  [x] close  [r] rename  [a] dsh agent  [d] back  [?] keys  [q] quit ",
            Style::default().fg(Color::DarkGray),
        )),
    };
    f.render_widget(Paragraph::new(footer), chunks[3]);
}

/// One manager row: number, attention marks, title, target, state, grid, age.
fn manager_row(row: &Row) -> ListItem<'static> {
    let state_style = match (row.state, row.live) {
        ("open", _) => Style::default().fg(Color::Green),
        ("connecting", _) => Style::default().fg(Color::Yellow),
        ("failed", _) => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::DarkGray),
    };
    // The marks come from the row itself (`Row::flags`), so what is drawn and
    // what the tests assert cannot drift apart.
    let flags: Vec<char> = row.flags().chars().collect();
    let attached = flags.first().copied().unwrap_or(' ').to_string();
    let attention = flags.get(1).copied().unwrap_or(' ').to_string();
    let mut spans = vec![
        Span::styled(
            format!("{:>2} ", row.number),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(attached, Style::default().fg(Color::LightGreen)),
        Span::styled(attention, Style::default().fg(Color::LightYellow)),
        Span::raw(" "),
        Span::styled(
            row.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(row.target.clone(), Style::default().fg(Color::Cyan)),
        Span::styled(format!("  {}", row.state), state_style),
    ];
    if row.awaiting {
        spans.push(Span::styled(
            " [needs an answer]",
            Style::default().fg(Color::LightYellow),
        ));
    }
    spans.push(Span::styled(
        format!("  {}x{}  {}", row.cols, row.rows, short_age(row.age_secs)),
        Style::default().fg(Color::DarkGray),
    ));
    ListItem::new(Line::from(spans))
}

/// `8s` / `4m12s` / `2h05m`, for a session's age.
fn short_age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// Paint one session's grid, status bar and (when the session has one) the
/// terminal cursor. Returns where that cursor belongs, in screen cells.
fn draw_session(
    f: &mut ratatui::Frame<'_>,
    mgr: &mut Manager<Live>,
    id: SessionId,
    detach: &Chord,
) -> Option<(u16, u16, CursorShape, bool)> {
    let area = f.area();
    // One row at the bottom is the status bar.
    let content = Rect::new(
        area.x,
        area.y,
        area.width,
        area.height.saturating_sub(1),
    );

    // The status text is read before the mirror is borrowed mutably below.
    let position = mgr.index_of(id).map(|i| i + 1).unwrap_or(0);
    let total = mgr.len();
    let title = mgr
        .get(id)
        .map(|s| s.display_title())
        .unwrap_or_else(|| id.to_string());
    let state = mgr.get(id).map(|s| s.state.label()).unwrap_or("gone");
    let transfer = mgr.get(id).and_then(|s| s.handle.as_ref())
        .and_then(|live| live.transfer.label.as_ref().map(|label| (label.clone(), live.transfer.active)));
    // Other sessions that want attention, so a session running a build in the
    // background can announce itself without being on screen.
    let mut attention = String::new();
    for (i, slot) in mgr.iter().enumerate() {
        if slot.id == id {
            continue;
        }
        if slot.awaiting {
            attention.push_str(&format!(" {}!", i + 1));
        } else if slot.bell {
            attention.push_str(&format!(" {}*", i + 1));
        } else if slot.unread {
            attention.push_str(&format!(" {}+", i + 1));
        }
    }

    let mut cursor = None;
    let mut scroll = None;
    if let Some(m) = mgr
        .get_mut(id)
        .and_then(|s| s.handle.as_mut())
        .and_then(|l| l.mirror.as_mut())
    {
        // Stream backends never set input_dirty themselves; force it.
        let content_state = {
            m.backend().mark_dirty();
            m.backend().sync()
        };
        draw_grid(content_state, f.buffer_mut(), content);
        // The read-only view already projects the cursor into viewport cells.
        // Scrollback position is separate metadata, not fake grid history.
        let grid = &content_state.grid;
        let line = grid.cursor.point.line.0;
        let col = grid.cursor.point.column.0 as u16;
        if content_state.cursor_shape != CursorShape::Hidden
            && line >= 0
            && (line as u16) < content.height
            && col < content.width
        {
            cursor = Some((
                content.x + col,
                content.y + line as u16,
                content_state.cursor_shape,
                content_state.cursor_blinking,
            ));
        }
        let offset = content_state.display_offset;
        if offset > 0 {
            scroll = Some((offset, content_state.history_size));
        }
    }

    let ended = state == "exited" || state == "failed";
    let mut spans = if ended {
        vec![Span::styled(" SESSION ENDED   [Enter] reconnect   [Esc] sessions ",
            Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD))]
    } else if let Some((label, active)) = transfer {
        vec![
            Span::styled(format!(" ZMODEM {label} "), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(if active { "[Ctrl-X] cancel " } else { "" }, Style::default().fg(Color::Yellow)),
        ]
    } else { vec![
        Span::styled(
            format!(" {position}/{total} "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(title, Style::default().fg(Color::White)),
        Span::styled(
            format!(" [{state}] "),
            Style::default().fg(if state == "open" {
                Color::Green
            } else {
                Color::Yellow
            }),
        ),
    ] };
    if let Some((offset, history)) = scroll {
        spans.push(Span::styled(
            format!("[scroll {offset}/{history}  Shift-End for the bottom] "),
            Style::default().fg(Color::LightYellow),
        ));
    }
    if !attention.is_empty() {
        spans.push(Span::styled(
            format!("[{attention} ] "),
            Style::default().fg(Color::LightYellow),
        ));
    }
    spans.push(Span::styled(
        format!("[{}] manager ", detach.label()),
        Style::default().fg(Color::DarkGray),
    ));
    let bar_area = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(1),
        area.width,
        1,
    );
    let paragraph = Paragraph::new(Line::from(spans));
    f.render_widget(if ended { paragraph.style(Style::default().bg(Color::Yellow).fg(Color::Black)) } else { paragraph }, bar_area);

    if ended { None } else { cursor }
}

/// Paint the key help.
fn draw_help(f: &mut ratatui::Frame<'_>, addr: &str, detach: &Chord, scroll: &mut u16) -> u16 {
    let detach = detach.label();
    let lines = vec![
        Line::from(Span::styled(
            "Sessions keep running while you are away from them.",
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("in a session   ", Style::default().fg(Color::Green)),
            Span::styled(format!("{detach}"), Style::default().fg(Color::Yellow)),
            Span::raw("            leave for the session manager (nothing is closed)"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("Shift-PgUp/PgDn", Style::default().fg(Color::Yellow)),
            Span::raw("     scroll the session's own scrollback"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("Shift-Up/Down", Style::default().fg(Color::Yellow)),
            Span::raw("       scroll one line"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("Shift-Home/End", Style::default().fg(Color::Yellow)),
            Span::raw("      jump to the oldest line / back to live"),
        ]),
        Line::from("               every other key goes to the remote unchanged"),
        Line::from(vec![
            Span::styled("marks          ", Style::default().fg(Color::Green)),
            Span::styled("*", Style::default().fg(Color::Yellow)),
            Span::raw(" on screen   "),
            Span::styled("!", Style::default().fg(Color::Yellow)),
            Span::raw(" needs an answer   "),
            Span::styled("B", Style::default().fg(Color::Yellow)),
            Span::raw(" rang the bell   "),
            Span::styled("+", Style::default().fg(Color::Yellow)),
            Span::raw(" new output"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("manager        ", Style::default().fg(Color::Green)),
            Span::styled("j/k \u{2191}/\u{2193}", Style::default().fg(Color::Yellow)),
            Span::raw("           move       "),
            Span::styled("Enter/1-9", Style::default().fg(Color::Yellow)),
            Span::raw("  attach"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("/", Style::default().fg(Color::Yellow)),
            Span::raw("               search title, address or state; Esc clears"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("Tab / Shift-Tab", Style::default().fg(Color::Yellow)),
            Span::raw(" next / previous session needing attention"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("n", Style::default().fg(Color::Yellow)),
            Span::raw("               new session (saved hosts)"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("x", Style::default().fg(Color::Yellow)),
            Span::raw("               close the selected session"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("r", Style::default().fg(Color::Yellow)),
            Span::raw("               rename it"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("a", Style::default().fg(Color::Yellow)),
            Span::raw("               the DSH agent page"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("d / Esc", Style::default().fg(Color::Yellow)),
            Span::raw("         back to the session you came from"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("q", Style::default().fg(Color::Yellow)),
            Span::raw("               quit (closes every session)"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("saved hosts    ", Style::default().fg(Color::Green)),
            Span::styled("j/k \u{2191}/\u{2193}", Style::default().fg(Color::Yellow)),
            Span::raw("           move       "),
            Span::styled("/", Style::default().fg(Color::Yellow)),
            Span::raw("         filter"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw("             connect    "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw("       back"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("a", Style::default().fg(Color::Yellow)),
            Span::raw("                 the DSH agent page (also from the manager)"),
        ]),
        Line::from(""),
        Line::from(""),
        Line::from(vec![
            Span::styled("dsh agent      ", Style::default().fg(Color::Green)),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw("            send    "),
            Span::styled("Ctrl-S", Style::default().fg(Color::Yellow)),
            Span::raw("         steer the running turn"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("Ctrl-C", Style::default().fg(Color::Yellow)),
            Span::raw("           stop    "),
            Span::styled("Ctrl-X", Style::default().fg(Color::Yellow)),
            Span::raw("         drop the first queued message"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("Tab", Style::default().fg(Color::Yellow)),
            Span::raw("              switch session    "),
            Span::styled("y/n", Style::default().fg(Color::Yellow)),
            Span::raw("          allow or reject a tool"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("Shift-PgUp/PgDn", Style::default().fg(Color::Yellow)),
            Span::raw("     scroll it; at the top, Shift-Home loads older turns"),
        ]),
        Line::from(vec![
            Span::styled("               ", Style::default()),
            Span::styled("/", Style::default().fg(Color::Yellow)),
            Span::raw("                filter the session list (Esc clears)"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            format!("Override the manager chord with EASYTERM_TUI_DETACH_KEY (e.g. ctrl-^, f12, alt-q). daemon {addr}"),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "Help: j/k or arrows scroll; PgUp/PgDn page; Home/End jump; Esc/q/? returns.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let block = Block::default().borders(Borders::ALL).title(" EasyTerm — keys ");
    let inner = block.inner(f.area());
    f.render_widget(block, f.area());
    let body = Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(1));
    let lines = wrap_help_lines(lines, body.width);
    let max_scroll = lines.len().saturating_sub(body.height as usize).min(u16::MAX as usize) as u16;
    *scroll = (*scroll).min(max_scroll);
    let total = lines.len();
    f.render_widget(Paragraph::new(lines).scroll((*scroll, 0)), body);
    if inner.height > 0 {
        let footer = format!(" {}/{}  [Esc/q/?] back  [↑/↓ PgUp/PgDn] scroll  [Home/End] jump",
            usize::from(*scroll).saturating_add(body.height as usize).min(total), total);
        f.render_widget(Paragraph::new(footer).style(Style::default().fg(Color::Cyan)),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1));
    }
    max_scroll
}

/// Wrap once so scrolling and its end position use the same visible rows.
/// Styles and wide characters survive even in a narrow terminal.
fn wrap_help_lines(lines: Vec<Line<'_>>, width: u16) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthChar;
    if width == 0 { return Vec::new(); }
    let mut wrapped = Vec::new();
    for line in lines {
        let mut spans = Vec::new();
        let mut used = 0;
        for span in line.spans {
            let mut text = String::new();
            for ch in span.content.chars() {
                let cells = ch.width().unwrap_or(0).min(width as usize);
                if used + cells > width as usize {
                    if !text.is_empty() { spans.push(Span::styled(std::mem::take(&mut text), span.style)); }
                    wrapped.push(Line::from(std::mem::take(&mut spans)));
                    used = 0;
                }
                text.push(ch);
                used += cells;
            }
            if !text.is_empty() { spans.push(Span::styled(text, span.style)); }
        }
        wrapped.push(Line::from(spans));
    }
    wrapped
}

/// Show or hide the terminal cursor, and shape it like the application asked.
fn apply_cursor(cell: Option<(u16, u16, CursorShape, bool)>, overlay: bool) {
    let mut stdout = io::stdout();
    match cell {
        Some((x, y, shape, blink)) if !overlay => {
            let _ = execute!(stdout, crossterm::cursor::MoveTo(x, y));
            let style = match shape {
                CursorShape::Block => crossterm::cursor::SetCursorStyle::SteadyBlock,
                CursorShape::Beam => crossterm::cursor::SetCursorStyle::SteadyBar,
                CursorShape::Underline => crossterm::cursor::SetCursorStyle::SteadyUnderScore,
                CursorShape::HollowBlock => crossterm::cursor::SetCursorStyle::SteadyBlock,
                CursorShape::Hidden => crossterm::cursor::SetCursorStyle::DefaultUserShape,
            };
            let style = if blink {
                match style {
                    crossterm::cursor::SetCursorStyle::SteadyBlock => {
                        crossterm::cursor::SetCursorStyle::BlinkingBlock
                    }
                    crossterm::cursor::SetCursorStyle::SteadyBar => {
                        crossterm::cursor::SetCursorStyle::BlinkingBar
                    }
                    crossterm::cursor::SetCursorStyle::SteadyUnderScore => {
                        crossterm::cursor::SetCursorStyle::BlinkingUnderScore
                    }
                    other => other,
                }
            } else {
                style
            };
            let _ = execute!(stdout, style);
            let _ = execute!(stdout, crossterm::cursor::Show);
        }
        _ => {
            let _ = execute!(stdout, crossterm::cursor::Hide);
        }
    }
}

/// Answer one overlay question from a key press.
fn handle_ask_key(p: &mut PendingAsk, k: &crossterm::event::KeyEvent) {
    match &p.ask.kind {
        AskKind::HostKeyConfirm(_) => {
            if let KeyCode::Char(ch) = k.code {
                if let Some(ans) = term_tui::ask::host_key_decision(ch) {
                    match ans {
                        AnswerResult::AcceptHostKey => p.accept_host_key(),
                        AnswerResult::RejectHostKey => p.reject_host_key(),
                        _ => {}
                    }
                }
            } else if k.code == KeyCode::Esc {
                p.cancel();
            }
        }
        AskKind::KbdInteractive(_) => match k.code {
            KeyCode::Char(ch) => p.input.push(ch),
            KeyCode::Backspace => {
                p.input.pop();
            }
            KeyCode::Enter => p.submit(),
            KeyCode::Esc => p.cancel(),
            _ => {}
        },
        AskKind::AiToolCall(_) => {
            // The TUI runs no AI tools: refusing the call stops the daemon's
            // tool loop instead of leaving it waiting forever.
            p.cancel();
        }
        // The TUI has no vault; a VaultResolve ask can only be cancelled.
        AskKind::VaultResolve(_) => {
            if k.code == KeyCode::Esc || k.code == KeyCode::Char('q') {
                p.cancel();
            }
        }
        // ZMODEM uses the daemon-backed browser before this fallback is reached.
        AskKind::ZmodemPath(_) => {
            if k.code == KeyCode::Esc || k.code == KeyCode::Char('q') {
                p.cancel();
            }
        }
    }
}

/// Paint the pending host question over the whole screen area.
fn draw_overlay(p: &PendingAsk, buf: &mut ratatui::buffer::Buffer, area: Rect) {
    use ratatui::layout::Alignment;
    use ratatui::widgets::{Widget, Wrap};

    // Centered, ~60% width box.
    let w = (area.width * 3) / 5;
    let h = 12.min(area.height);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    let box_area = Rect::new(x, y, w, h);

    let mut lines: Vec<String> = Vec::new();
    let hint: String;
    match &p.ask.kind {
        AskKind::HostKeyConfirm(hk) => {
            lines.push(format!("The authenticity of host '{}' can't be established.", hk.host));
            lines.push("Key fingerprint (OpenSSH):".into());
            lines.push(hk.key_blob.clone());
            hint = "[y] accept   [n] reject   [Esc] cancel".into();
        }
        AskKind::KbdInteractive(kb) => {
            if !kb.session_label.is_empty() {
                lines.push(format!("[{}]", kb.session_label));
            }
            if !kb.instructions.is_empty() {
                lines.push(kb.instructions.clone());
            }
            if let Some((prompt, echo, _prefill)) = kb.prompts.first() {
                lines.push(format!(
                    "{prompt} {}",
                    term_tui::ask::input_display(&p.input, !echo)
                ));
            } else {
                lines.push("(no prompts)".into());
            }
            hint = "[Enter] submit   [Esc] cancel".into();
        }
        AskKind::AiToolCall(call) => {
            // The TUI runs no AI tools: report the call and refuse it, so the
            // daemon's tool loop stops instead of waiting forever.
            lines.push(format!("[ai] tool call refused: {}", call.tool));
            hint = "[Esc] cancel".into();
        }
        AskKind::VaultResolve(vr) => {
            lines.push(format!("[{}] vault credential requested", vr.session_label));
            match &vr.kind {
                term_proto::VaultRefKind::Password { pass_id } => {
                    lines.push(format!("stored password: {pass_id}"));
                }
                term_proto::VaultRefKind::PrivateKey { key_id } => {
                    lines.push(format!("stored private key: {key_id}"));
                }
            }
            lines.push("(the TUI has no vault; cancel to abort the open)".into());
            hint = "[Esc] cancel".into();
        }
        AskKind::ZmodemPath(z) => {
            lines.push(format!("[{}] zmodem transfer needs a local path", z.session_label));
            if let Some(name) = &z.name {
                lines.push(format!(
                    "{} {name}",
                    if z.save { "save as:" } else { "send:" }
                ));
            }
            lines.push("Waiting for the daemon file browser…".into());
            hint = "[Esc] cancel".into();
        }
    }
    lines.push(hint);

    let text = lines.join("\n");

    Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title("Host question"))
        .style(Style::default().fg(Color::Yellow))
        .wrap(Wrap { trim: true })
        .alignment(Alignment::Left)
        .render(box_area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[tokio::test]
    async fn automatic_exit_closes_only_its_session_and_preserves_foreground_focus() {
        let mut app = App::new("127.0.0.1:1".into(), Chord::Ctrl(']'));
        let background = app.mgr.insert("background", "bg", None);
        let foreground = app.mgr.insert("foreground", "fg", None);
        app.mgr.get_mut(background).unwrap().close_on_quit = true;
        app.mgr.activate(foreground);
        app.screen = Screen::Session(foreground);
        app.session_exited(background);
        assert!(app.mgr.get(background).is_none());
        assert!(app.mgr.get(foreground).unwrap().state.is_live());
        assert_eq!(app.screen, Screen::Session(foreground));
        assert_eq!(app.mgr.active(), Some(foreground));
        app.mgr.get_mut(foreground).unwrap().close_on_quit = true;
        app.session_exited(foreground);
        assert!(app.mgr.is_empty());
        assert_eq!(app.screen, Screen::Picker, "last exit returns to hosts, never quits the TUI");
        if let Some(task) = app.picker_refresh.take() { task.abort(); }

        // A daemon can drop every transport together. If the foreground is
        // drained first, the last background exit must leave the empty hub.
        let mut app = App::new("127.0.0.1:1".into(), Chord::Ctrl(']'));
        let first = app.mgr.insert("first", "a", None);
        let second = app.mgr.insert("second", "b", None);
        for id in [first, second] { app.mgr.get_mut(id).unwrap().close_on_quit = true; }
        app.mgr.activate(first);
        app.screen = Screen::Session(first);
        app.session_exited(first);
        assert_eq!(app.screen, Screen::Manager);
        app.session_exited(second);
        assert_eq!(app.screen, Screen::Picker);
        if let Some(task) = app.picker_refresh.take() { task.abort(); }
    }

    #[tokio::test]
    async fn retained_exit_keeps_its_retry_spec_and_returns_to_selection_on_escape() {
        let mut app = App::new("127.0.0.1:1".into(), Chord::Ctrl(']'));
        let spec = direct_spec("deploy@example.com:2222").unwrap();
        let id = app.mgr.insert("saved", "target", Some(spec.clone()));
        app.mgr.mark_open(id, 80, 24);
        app.mgr.activate(id);
        app.screen = Screen::Session(id);
        app.session_exited(id);
        assert_eq!(app.screen, Screen::Session(id), "retain the final output");
        assert_eq!(app.mgr.get(id).unwrap().spec, Some(spec));
        assert_eq!(app.mgr.get(id).unwrap().state, SessionState::Exited);
        assert_eq!(app.mgr.apply(term_tui::manager::Command::Select), ManagerOutcome::Retry(id));
        assert_eq!(ended_session_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), app.detach), EndedSessionKey::Reconnect);
        for key in [KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)] {
            assert_eq!(ended_session_key(&key, app.detach), EndedSessionKey::Ignore, "dead sessions never forward input");
        }
        app.on_paste("must not be sent".into()).await;
        app.leave_ended_session(id);
        assert_eq!(app.screen, Screen::Picker);
        assert!(app.mgr.is_empty(), "Esc releases the ended terminal and its connection");
        if let Some(task) = app.picker_refresh.take() { task.abort(); }
    }

    #[test]
    fn an_exited_session_draws_reconnect_and_return_hints_and_hides_the_cursor() {
        let mut manager: Manager<Live> = Manager::new();
        let id = manager.insert("host", "target", None);
        manager.set_exited(id);
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        let mut cursor = None;
        terminal.draw(|frame| { cursor = draw_session(frame, &mut manager, id, &Chord::Ctrl(']')); }).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        for hint in ["SESSION ENDED", "[Enter] reconnect", "[Esc] sessions"] { assert!(text.contains(hint), "{hint}: {text}"); }
        assert!(cursor.is_none());
        assert_eq!(terminal.backend().buffer()[(0, 7)].bg, Color::Yellow);
    }

    #[tokio::test]
    async fn expired_zmodem_questions_release_the_picker_and_directory_task() {
        for event in [term_proto::ZmodemEventWire::Cancelled { sid: 7 }, term_proto::ZmodemEventWire::Ended { sid: 7 }] {
            let (_, asks) = tokio::sync::mpsc::unbounded_channel();
            let (reply, answer) = tokio::sync::oneshot::channel();
            let directory = tokio::spawn(std::future::pending::<Result<term_proto::LocalPathListInfo, String>>());
            let abort = directory.abort_handle();
            let mut live = Live {
                open: None, mirror: None, asks,
                ask: Some(PendingAsk::new(ServerAsk { ask_id: 3, sid: Some(7), kind: AskKind::ZmodemPath(term_proto::ZmodemPathAsk {
                    sid: 7, session_label: "test".into(), save: false, name: None, size: None,
                }) }, reply)),
                last_data: None, file_picker: Some(files::FileChooser::new(false, None, None)),
                path_task: Some(directory), transfer: files::TransferProgress::default(),
            };
            live.zmodem_event(&term_proto::ZmodemEventWire::UploadProgress {
                sid: 7, file_idx: 0, total_files: 2, name: "/daemon/a.txt".into(), current: 12, total: 100,
            });
            assert!(live.ask.is_some() && live.file_picker.is_some(), "ordinary progress never expires a path question");
            // AskPump drops its answer receiver when the reader retires this
            // exact ask_id, even if its terminal event queue is consumed later.
            drop(answer);
            live.zmodem_event(&event);
            assert!(live.ask.is_none() && live.file_picker.is_none() && live.path_task.is_none());
            tokio::task::yield_now().await;
            assert!(abort.is_finished(), "directory browse tasks must not outlive their picker");
        }
    }

    fn entry(id: u32, name: &str, host: &str, port: u16, user: &str) -> SessionEntryWire {
        SessionEntryWire {
            config_id: id,
            name: name.to_string(),
            host: host.to_string(),
            port,
            username: user.to_string(),
            folder: None,
            auth: vec!["pass_id".to_string()],
            jump: None,
        }
    }

    /// Flatten a rendered buffer into text rows.
    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area;
        let mut out = String::new();
        for y in area.top()..area.bottom() {
            let mut row = String::new();
            for x in area.left()..area.right() {
                if let Some(c) = buf.cell((x, y)) {
                    row.push_str(c.symbol());
                }
            }
            out.push_str(row.trim_end());
            out.push('\n');
        }
        out
    }

    fn render_picker(hosts: &[SessionEntryWire], status: Option<&str>) -> String {
        render_picker_with(hosts, status, PickerState::default())
    }

    fn render_picker_with(
        hosts: &[SessionEntryWire],
        status: Option<&str>,
        mut picker: PickerState,
    ) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 14)).expect("test terminal");
        if !hosts.is_empty() {
            picker.list.select(Some(picker.selected));
        }
        terminal
            .draw(|f| draw_picker(f, "127.0.0.1:13999", hosts, status, &mut picker))
            .expect("draw");
        buffer_text(terminal.backend().buffer())
    }

    fn render_manager(mgr: &Manager<String>, status: Option<&str>, confirm: Option<&str>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).expect("test terminal");
        terminal
            .draw(|f| draw_manager(f, "127.0.0.1:13999", mgr, status, confirm, false))
            .expect("draw");
        buffer_text(terminal.backend().buffer())
    }

    fn manager_with(titles: &[&str]) -> Manager<String> {
        let mut mgr = Manager::new();
        for t in titles {
            let id = mgr.insert(*t, format!("{t}@host:22"), None);
            mgr.set_open(id, format!("handle-{t}"), 120, 30);
        }
        mgr
    }

    #[test]
    fn picker_lists_each_saved_host_with_its_target_and_auth() {
        let hosts = vec![
            entry(1, "web1", "10.0.0.1", 2222, "deploy"),
            entry(2, "db1", "10.0.0.2", 22, "root"),
        ];
        let text = render_picker(&hosts, None);
        assert!(text.contains("web1"), "host name rendered:\n{text}");
        assert!(
            text.contains("deploy@10.0.0.1:2222"),
            "non-default port in the target:\n{text}"
        );
        assert!(
            text.contains("root@10.0.0.2") && !text.contains("root@10.0.0.2:"),
            "default port elided:\n{text}"
        );
        assert!(text.contains("[pass_id]"), "auth hint rendered:\n{text}");
        assert!(
            text.contains("[a] dsh agent"),
            "the agent page is reachable without a session:\n{text}"
        );
        assert!(text.contains("2 saved host(s)"), "count in the title:\n{text}");
        assert!(text.contains("127.0.0.1:13999"), "daemon address shown:\n{text}");
    }

    #[test]
    fn picker_renders_folder_and_jump_server() {
        let mut h = entry(3, "behind", "10.0.0.4", 22, "target");
        h.folder = Some("prod/web".to_string());
        h.jump = Some("bastion".to_string());
        let text = render_picker(&[h], None);
        assert!(text.contains("prod/web/"), "folder prefix rendered:\n{text}");
        assert!(text.contains("via bastion"), "jump server rendered:\n{text}");
    }

    #[test]
    fn picker_explains_an_empty_list_and_shows_status() {
        let text = render_picker(&[], Some("could not open config 7: nope"));
        assert!(text.contains("no saved SSH hosts"), "empty title:\n{text}");
        assert!(
            text.contains("listed no saved SSH hosts"),
            "empty-list hint:\n{text}"
        );
        // A status message replaces the key hints so the reason stays visible.
        assert!(text.contains("could not open config 7"), "status line:\n{text}");
    }

    #[test]
    fn the_picker_filter_narrows_the_list_and_names_the_match_count() {
        let hosts = vec![
            entry(1, "web1", "10.0.0.1", 22, "deploy"),
            entry(2, "db1", "10.0.0.2", 22, "root"),
            entry(3, "cache", "10.0.0.3", 22, "root"),
        ];
        let picker = PickerState {
            filter: "db".to_string(),
            filtering: true,
            ..PickerState::default()
        };
        let text = render_picker_with(&hosts, None, picker);
        assert!(text.contains("1 of 3 host(s)"), "match count:\n{text}");
        assert!(text.contains("db1"), "the match is listed:\n{text}");
        assert!(!text.contains("web1"), "non-matches are gone:\n{text}");
        // The filter is visibly in force, with its own key hints.
        assert!(text.contains("/db"), "the filter text is shown:\n{text}");
        assert!(text.contains("[Esc] clear filter"), "filter hints:\n{text}");

        // A filter that matches nothing says so instead of showing a stale row.
        let picker = PickerState {
            filter: "nothing-like-this".to_string(),
            filtering: true,
            ..PickerState::default()
        };
        let text = render_picker_with(&hosts, None, picker);
        assert!(text.contains("No host matches"), "empty result:\n{text}");
    }

    #[test]
    fn a_host_matches_on_name_address_folder_or_auth() {
        let mut h = entry(1, "web1", "10.0.0.1", 2222, "deploy");
        h.folder = Some("prod/web".to_string());
        for needle in ["web1", "deploy", "10.0.0.1", "2222", "prod", "pass_id"] {
            assert!(host_matches(&h, needle), "{needle} should match");
        }
        assert!(!host_matches(&h, "db1"), "an unrelated name does not match");
    }

    #[test]
    fn the_manager_lists_every_session_with_its_state_grid_and_age() {
        let mgr = manager_with(&["alpha", "beta"]);
        let text = render_manager(&mgr, None, None);
        assert!(text.contains("alpha"), "first session:\n{text}");
        assert!(text.contains("beta"), "second session:\n{text}");
        assert!(text.contains("alpha@host:22"), "target:\n{text}");
        assert!(text.contains("open"), "state:\n{text}");
        assert!(text.contains("120x30"), "its grid:\n{text}");
        assert!(text.contains("2 live sessions"), "the count:\n{text}");
        assert!(text.contains("[Enter] attach"), "key hints:\n{text}");
        assert!(text.contains("[a] dsh agent"), "the agent page is reachable:\n{text}");
        assert!(text.contains("127.0.0.1:13999"), "daemon:\n{text}");
    }

    #[test]
    fn the_manager_marks_the_session_on_screen_and_the_ones_wanting_attention() {
        let mut mgr = manager_with(&["alpha", "beta", "gamma"]);
        let ids = mgr.ids();
        mgr.activate(ids[1]);
        mgr.select(ids[0]);
        {
            let slot = mgr.get_mut(ids[0]).unwrap();
            slot.bell = true;
            slot.unread = true;
        }
        mgr.get_mut(ids[2]).unwrap().awaiting = true;

        let text = render_manager(&mgr, None, None);
        let lines: Vec<&str> = text.lines().collect();
        let alpha = lines.iter().find(|l| l.contains("alpha")).expect("alpha row");
        let beta = lines.iter().find(|l| l.contains("beta")).expect("beta row");
        let gamma = lines.iter().find(|l| l.contains("gamma")).expect("gamma row");
        // Each mark says exactly one thing: the bell is `B`, the session on
        // screen is `*`, and beta is the one on screen even though the cursor
        // has moved to alpha.
        assert!(alpha.contains('B'), "bell mark on alpha: {alpha}");
        assert!(!alpha.contains('*'), "alpha is not the one on screen: {alpha}");
        assert!(beta.contains('*'), "attached mark on beta: {beta}");
        assert!(!beta.contains('B'), "beta's bell is not ringing: {beta}");
        assert!(gamma.contains('!'), "input mark on gamma: {gamma}");
        assert!(gamma.contains("[needs an answer]"), "blocked on input: {gamma}");
        // The cursor is on alpha, so its detail line is the one shown.
        assert!(text.contains("alpha@host:22"), "selected detail:\n{text}");
    }

    #[test]
    fn the_manager_shows_a_failure_and_offers_a_retry() {
        let mut mgr = manager_with(&["alpha"]);
        let id = mgr.ids()[0];
        mgr.set_failed(id, "connect refused");
        let text = render_manager(&mgr, None, None);
        assert!(text.contains("failed"), "state:\n{text}");
        assert!(text.contains("connect refused"), "reason:\n{text}");
        assert!(text.contains("[Enter] retry"), "retry hint:\n{text}");
    }

    #[test]
    fn the_manager_footer_carries_the_confirmation_or_the_status() {
        let mgr = manager_with(&["alpha"]);
        let text = render_manager(&mgr, Some("closed session #1"), None);
        assert!(text.contains("closed session #1"), "status:\n{text}");
        let text = render_manager(&mgr, None, Some("quit and close 1 live session(s)?   [y] quit"));
        assert!(text.contains("[y] quit"), "confirm prompt:\n{text}");
        assert!(!text.contains("[Enter] attach"), "hints give way to the prompt:\n{text}");
    }

    #[test]
    fn the_manager_explains_search_results_and_an_empty_match() {
        let mut mgr = manager_with(&["alpha", "beta"]);
        mgr.set_filter("beta");
        let text = render_manager(&mgr, None, None);
        assert!(text.contains("1 / 2 match"), "match count: {text}");
        assert!(text.contains("beta@host:22"), "selected detail: {text}");
        assert!(!text.contains("alpha"), "hidden session is not drawn: {text}");
        mgr.set_filter("missing");
        let text = render_manager(&mgr, None, None);
        assert!(text.contains("No sessions match"), "empty match: {text}");
        assert!(text.contains("[Esc] clear"), "recovery: {text}");
    }

    #[test]
    fn an_empty_manager_offers_a_new_session_and_the_agent_page() {
        let mgr: Manager<String> = Manager::new();
        let text = render_manager(&mgr, None, None);
        assert!(text.contains("No terminal sessions are open"), "empty state:\n{text}");
        assert!(text.contains("[n] open a saved host"), "the way out:\n{text}");
        assert!(text.contains("[a] the DSH agent"), "and the agent:\n{text}");
        assert!(text.contains("no live sessions"), "count:\n{text}");
    }

    #[test]
    fn the_agent_page_remembers_where_it_was_opened_from() {
        // The reported path: the host list is the first screen, `a` opens the
        // agent, `Ctrl-]` leaves it — and it used to land on the session manager,
        // which lists terminal sessions and had none, so the reader got an empty
        // "No sessions are open" with their conversation nowhere in sight.
        let mut app = App::new("127.0.0.1:1".to_string(), Chord::Ctrl(']'));
        assert_eq!(app.screen, Screen::Picker);
        app.open_agent();
        assert_eq!(app.screen, Screen::Dsh);
        assert_eq!(app.dsh_return, Some(Screen::Picker), "it remembers the origin");
        // Nothing is followed yet, so entering again lets the page retry.
        assert!(!app.dsh.sessions_loaded && !app.dsh.landing_done);
        app.leave_agent();
        assert_eq!(app.screen, Screen::Picker, "back to the hosts, not an empty hub");

        // From the manager, with a session in it: back to the manager.
        let id = app.mgr.insert("host", "user@host:22", None);
        app.screen = Screen::Manager;
        app.open_agent();
        app.leave_agent();
        assert_eq!(app.screen, Screen::Manager);

        // From a session, that has since been closed: the manager is empty, so
        // the host list is the useful place to be.
        app.screen = Screen::Session(id);
        app.open_agent();
        app.mgr.remove(id);
        app.leave_agent();
        assert_eq!(app.screen, Screen::Picker);
    }

    #[test]
    fn leaving_the_agent_page_goes_back_to_something_that_exists() {
        // The bug: `Ctrl-]` from the agent page always went to the session
        // manager, so someone who opened the agent from the host list — or who
        // has no terminals at all — arrived at an empty list saying no sessions.
        let session = Screen::Session(SessionId(1));

        // Back to the terminal they came from.
        assert_eq!(leave_agent_target(Some(session), true, true), session);
        // That terminal is gone: the manager, which still has others.
        assert_eq!(
            leave_agent_target(Some(session), false, true),
            Screen::Manager
        );
        // …and with no sessions at all, the host list rather than an empty hub.
        assert_eq!(leave_agent_target(Some(session), false, false), Screen::Picker);
        assert_eq!(leave_agent_target(Some(Screen::Manager), false, false), Screen::Picker);
        assert_eq!(leave_agent_target(None, false, false), Screen::Picker);
        // Opened from the host list: back to the host list, even with sessions
        // open (that is where hosts are chosen).
        assert_eq!(leave_agent_target(Some(Screen::Picker), false, true), Screen::Picker);
        // Opened from the manager with something in it: back to the manager.
        assert_eq!(leave_agent_target(Some(Screen::Manager), false, true), Screen::Manager);
    }

    #[test]
    fn the_help_names_the_reserved_chords_and_the_daemon() {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
        terminal
            .draw(|f| { draw_help(f, "127.0.0.1:13999", &Chord::Ctrl(']'), &mut 0); })
            .expect("draw");
        let text = buffer_text(terminal.backend().buffer());
        for needle in [
            "Ctrl-]",
            "leave for the session manager",
            "Shift-PgUp/PgDn",
            "Shift-Home/End",
            "every other key goes to the remote unchanged",
            "EASYTERM_TUI_DETACH_KEY",
            "127.0.0.1:13999",
            "dsh agent",
            "switch session",
            "allow or reject a tool",
            "needs an answer",
            "rang the bell",
        ] {
            assert!(text.contains(needle), "{needle} is missing from:\n{text}");
        }
        // A configured chord is named as configured, not as the default.
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
        terminal
            .draw(|f| { draw_help(f, "a", &Chord::F(12), &mut 0); })
            .expect("draw");
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("F12"), "the chord in force:\n{text}");
        assert!(!text.contains("Ctrl-]"), "not the default:\n{text}");
    }

    #[test]
    fn help_can_reach_the_end_in_a_small_terminal_and_clamps_after_resize() {
        let mut terminal = Terminal::new(TestBackend::new(55, 12)).expect("terminal");
        let mut scroll = u16::MAX;
        let mut max = 0;
        terminal.draw(|f| { max = draw_help(f, "127.0.0.1:13999", &Chord::Ctrl(']'), &mut scroll); }).unwrap();
        assert!(max > 0);
        assert_eq!(scroll, max);
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("127.0.0.1:13999"), "bottom of help: {text}");
        assert!(text.contains("[Esc/q/?] back"), "persistent close hint: {text}");
        let mut large = Terminal::new(TestBackend::new(140, 70)).unwrap();
        large.draw(|f| { draw_help(f, "127.0.0.1:13999", &Chord::Ctrl(']'), &mut scroll); }).unwrap();
        assert_eq!(scroll, 0, "resize cannot leave the reader beyond the last help row");
    }

    #[test]
    fn the_window_size_is_written_into_every_kind_of_spec() {
        let ssh = direct_spec("deploy@host:2222").expect("spec");
        match with_grid(ssh, 132, 43) {
            OpenSpec::Ssh(s) => {
                assert_eq!((s.cols, s.rows), (132, 43));
                assert_eq!(s.port, 2222);
            }
            other => panic!("expected an ssh spec, got {other:?}"),
        }
        match with_grid(
            OpenSpec::SshFromConfig {
                config_id: 7,
                cols: 80,
                rows: 24,
            },
            132,
            43,
        ) {
            OpenSpec::SshFromConfig {
                config_id,
                cols,
                rows,
            } => {
                assert_eq!(config_id, 7);
                assert_eq!((cols, rows), (132, 43));
            }
            other => panic!("expected a config spec, got {other:?}"),
        }
    }

    #[test]
    fn an_explicit_daemon_address_skips_local_discovery() {
        let explicit = Some("127.0.0.1:1".to_string());
        assert_eq!(pick_addr(explicit, || panic!("discovery should not run")), Ok("127.0.0.1:1".into()));
        assert_eq!(pick_addr(None, || Some("127.0.0.1:2".into())), Ok("127.0.0.1:2".into()));
    }

    #[test]
    fn with_nothing_to_talk_to_the_frontend_says_so_instead_of_guessing() {
        let error = pick_addr(None, || None).expect_err("no daemon");
        assert!(error.contains("No daemon has announced itself"), "{error}");
        assert!(error.contains("easyterm --daemon"), "how to start one: {error}");
        assert!(error.contains("term_tui HOST:PORT"), "how to point at one: {error}");
        // The old guess is gone: nothing mentions a made-up port.
        assert!(!error.contains("13999"), "{error}");
    }

    #[test]
    fn a_direct_spec_needs_a_user_and_a_host() {
        assert!(direct_spec("host").is_err());
        assert!(direct_spec("user@host:notaport").is_err());
        match direct_spec("user@host").expect("default port") {
            OpenSpec::Ssh(s) => assert_eq!(s.port, 22),
            other => panic!("expected an ssh spec, got {other:?}"),
        }
    }

    #[test]
    fn the_window_title_is_written_only_when_it_changes() {
        // It is an escape sequence: writing it every frame would be a write and a
        // flush sixty times a second for something that changes once a session.
        let mut title = TerminalTitle::default();
        assert!(title.changed("a — easyterm"), "the first one is written");
        assert!(!title.changed("a — easyterm"), "the same one is not");
        assert!(title.changed("b — easyterm"), "a different one is");
        assert!(title.changed(""), "clearing it on the way out counts");
        assert!(!title.changed(""));
    }

    #[test]
    fn ages_read_the_way_a_person_says_them() {
        assert_eq!(short_age(0), "0s");
        assert_eq!(short_age(59), "59s");
        assert_eq!(short_age(60), "1m00s");
        assert_eq!(short_age(252), "4m12s");
        assert_eq!(short_age(3600), "1h00m");
        assert_eq!(short_age(7500), "2h05m");
    }
}
