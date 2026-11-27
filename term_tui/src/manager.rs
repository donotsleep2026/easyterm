//! The set of sessions this frontend has open, and which one is on screen.
//!
//! A frontend that can only ever hold one session has to destroy it to look at
//! anything else — which is what the old `Ctrl-]` did: it called
//! `MirrorSession::close`, so "going back to the host list" killed the SSH
//! session and there was nothing left to return to. Every session opened here
//! therefore stays alive in this process (its connection, its local mirror and
//! its scrollback), and switching is only a change of which one is drawn.
//!
//! Nothing in this module touches the network or the terminal: it is the pure
//! bookkeeping half, generic over the session handle so the tests can drive it
//! with `()`. `main.rs` owns the [`MirrorSession`] handles and the screen.
//!
//! [`MirrorSession`]: term_mirror::mirror::MirrorSession

use std::time::Instant;

use term_proto::OpenSpec;

/// A stable name for a slot.
///
/// Indices cannot be used across async work: an open in flight, or a host
/// question waiting to be answered, keeps pointing at its session while other
/// slots are added or removed. Numbers shown to the user are positions in the
/// list ([`Row::number`]) and may shift; ids never do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub u64);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// How far a session has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// `open_session` is in flight (an SSH open may be waiting on a host
    /// question that only this frontend can answer).
    Connecting,
    /// The daemon is streaming a live terminal.
    Open,
    /// The open failed. The slot stays so the reason can be read and retried.
    Failed(String),
    /// The daemon reported the session gone (shell exited, SSH dropped).
    Exited,
}

impl State {
    /// The word shown in the manager's state column.
    pub fn label(&self) -> &'static str {
        match self {
            State::Connecting => "connecting",
            State::Open => "open",
            State::Failed(_) => "failed",
            State::Exited => "exited",
        }
    }

    /// Whether the daemon still has a terminal for this session, i.e. whether
    /// typing into it can achieve anything.
    pub fn is_live(&self) -> bool {
        matches!(self, State::Connecting | State::Open)
    }
}

/// One session slot.
pub struct Managed<S> {
    /// Stable id (see [`SessionId`]).
    pub id: SessionId,
    /// What the user calls it: the saved host's name, the address for `--ssh`,
    /// "local shell". Renamable with `r` in the manager.
    pub title: String,
    /// The address the daemon was asked for (`user@host:port`), for the detail
    /// line. Kept because a renamed session otherwise loses its provenance.
    pub target: String,
    /// How to open it again (`Retry` from the manager).
    pub spec: Option<OpenSpec>,
    /// Where the session is in its life.
    pub state: State,
    /// Resolved by the daemon for this open, matching the GUI's tab policy.
    pub close_on_quit: bool,
    /// The live mirror, once the open resolved.
    pub handle: Option<S>,
    /// Grid the daemon currently has for this session.
    pub cols: u16,
    pub rows: u16,
    /// Output arrived while this session was not the one on screen.
    pub unread: bool,
    /// The remote rang the bell while this session was not on screen (a build
    /// finishing, `\a` in a prompt) — worth more attention than plain output.
    pub bell: bool,
    /// A host question (host key, password) is waiting for an answer. The
    /// question is only *shown* for the session on screen, so the manager has
    /// to say which session is blocked on input.
    pub awaiting: bool,
    /// The title the remote set with `OSC 0/2`, when it sets one.
    pub remote_title: Option<String>,
    /// When the slot was created (the manager shows how long it has been up).
    pub opened_at: Instant,
}

impl<S> Managed<S> {
    /// The name to draw: the remote's own title once it sets one, else what the
    /// session was opened as. A program that titles itself (`vim`, `ssh`)
    /// describes the session better than the host name does.
    pub fn display_title(&self) -> String {
        match &self.remote_title {
            Some(t) if !t.trim().is_empty() => t.clone(),
            _ => self.title.clone(),
        }
    }

    /// How long ago the session was opened, as `12m` / `2h05m` / `8s`.
    pub fn age_secs(&self) -> u64 {
        self.opened_at.elapsed().as_secs()
    }
}

/// One line of the manager's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// 1-based position, which is also the digit that attaches to it.
    pub number: usize,
    pub id: SessionId,
    pub title: String,
    pub target: String,
    pub state: &'static str,
    /// The daemon still owns a terminal here.
    pub live: bool,
    /// This is the session the screen would show.
    pub active: bool,
    /// Something arrived while it was off screen.
    pub unread: bool,
    /// The bell rang while it was off screen.
    pub bell: bool,
    /// The open is waiting for an answer from this frontend.
    pub awaiting: bool,
    pub cols: u16,
    pub rows: u16,
    pub age_secs: u64,
    /// Why it failed, for the selected row.
    pub failure: Option<String>,
}

impl Row {
    /// The marks drawn before the title, in the order a reader scans them:
    /// attachment (`*`), then attention (`!` needing an answer beats `B` a
    /// bell, which beats `+` plain new output).
    ///
    /// The renderer draws exactly this, so the manager column and the tests
    /// cannot drift apart; the letters are ASCII because a terminal that
    /// cannot show a dingbat must still show which session wants attention.
    pub fn flags(&self) -> String {
        let attached = if self.active { '*' } else { ' ' };
        let attention = if self.awaiting {
            '!'
        } else if self.bell {
            'B'
        } else if self.unread {
            '+'
        } else {
            ' '
        };
        format!("{attached}{attention}")
    }
}

/// What a key press in the manager asks the frontend to do.
///
/// The screen itself owns no policy: [`Manager::apply`] turns a key into one of
/// these, and `main.rs` performs the ones that need the network (attaching is
/// local, closing and opening are not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Nothing this screen cares about.
    None,
    /// Show the move selection by `delta` (may be larger than the list).
    Move(i32),
    First,
    Last,
    /// Select the next/previous visible session needing input or attention.
    Attention(i32),
    /// Attach to the selected session, or retry it when it is not live.
    Select,
    /// Attach straight to the 1-based position (a digit key).
    SelectNumber(usize),
    /// Re-attach to whatever was on screen before the manager was opened.
    Back,
    /// Open the host picker for a new session.
    NewSession,
    /// Close the selected session (the caller confirms when it is live).
    Close,
    /// Rename the selected session.
    Rename,
    /// Leave the frontend.
    Quit,
    /// Show the key help.
    Help,
    /// Open the DSH agent page.
    Agent,
}

/// What the caller has to do about a [`Command`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Handled inside the manager (selection moved, nothing else to do).
    Done,
    /// Put this session on screen.
    Attach(SessionId),
    /// Open this session again from its stored spec.
    Retry(SessionId),
    /// Ask for a new session (host picker).
    NewSession,
    /// Close this session; the caller confirms first when it is live.
    Close(SessionId),
    /// Prompt for a new title for this session.
    Rename(SessionId),
    /// Quit the frontend.
    Quit,
    /// Show the help overlay.
    Help,
    /// Show the DSH agent page.
    Agent,
}

/// Every session this frontend has open.
pub struct Manager<S> {
    sessions: Vec<Managed<S>>,
    /// The session on screen. `None` while the picker or the manager is up:
    /// all sessions keep running either way.
    active: Option<SessionId>,
    /// What to return to with `Back` / `Esc`.
    last_active: Option<SessionId>,
    /// Which row the manager's cursor is on, as an index into `sessions`.
    cursor: usize,
    /// Local display filter; it never changes a session or its authorization.
    filter: String,
    next_id: u64,
}

impl<S> Default for Manager<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Manager<S> {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            active: None,
            last_active: None,
            cursor: 0,
            filter: String::new(),
            next_id: 1,
        }
    }

    /// Reserve a slot for a session that is about to be opened.
    pub fn insert(&mut self, title: impl Into<String>, target: impl Into<String>, spec: Option<OpenSpec>) -> SessionId {
        let id = SessionId(self.next_id);
        self.next_id += 1;
        self.sessions.push(Managed {
            id,
            title: title.into(),
            target: target.into(),
            spec,
            state: State::Connecting,
            close_on_quit: false,
            handle: None,
            cols: 0,
            rows: 0,
            unread: false,
            bell: false,
            awaiting: false,
            remote_title: None,
            opened_at: Instant::now(),
        });
        id
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// How many sessions the daemon still has a terminal for.
    pub fn live_count(&self) -> usize {
        self.sessions.iter().filter(|s| s.state.is_live()).count()
    }

    pub fn ids(&self) -> Vec<SessionId> {
        self.sessions.iter().map(|s| s.id).collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Managed<S>> {
        self.sessions.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Managed<S>> {
        self.sessions.iter_mut()
    }

    /// Index of a slot, for callers that need the position as well.
    pub fn index_of(&self, id: SessionId) -> Option<usize> {
        self.sessions.iter().position(|s| s.id == id)
    }

    pub fn get(&self, id: SessionId) -> Option<&Managed<S>> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn get_mut(&mut self, id: SessionId) -> Option<&mut Managed<S>> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    /// The slot at a 1-based position.
    pub fn nth(&self, number: usize) -> Option<&Managed<S>> {
        number.checked_sub(1).and_then(|i| self.sessions.get(i))
    }

    /// Hand the live handle to a slot without claiming the open resolved: an
    /// SSH open is still answering host questions at this point.
    pub fn set_handle(&mut self, id: SessionId, handle: S) {
        if let Some(s) = self.get_mut(id) {
            s.handle = Some(handle);
        }
    }

    /// The open resolved: the daemon is streaming a terminal for this session.
    pub fn mark_open(&mut self, id: SessionId, cols: u16, rows: u16) {
        if let Some(s) = self.get_mut(id) {
            s.state = State::Open;
            s.cols = cols;
            s.rows = rows;
            s.awaiting = false;
        }
    }

    /// The open resolved: hand the live mirror to the slot.
    pub fn set_open(&mut self, id: SessionId, handle: S, cols: u16, rows: u16) {
        self.set_handle(id, handle);
        self.mark_open(id, cols, rows);
    }

    /// The open failed; keep the slot (and any handle) so the reason is
    /// readable and `Retry` is possible.
    pub fn set_failed(&mut self, id: SessionId, reason: impl Into<String>) {
        if let Some(s) = self.get_mut(id) {
            s.state = State::Failed(reason.into());
            s.awaiting = false;
        }
    }

    /// The daemon reported the session gone. The handle is dropped by the
    /// caller, which owns closing it.
    pub fn set_exited(&mut self, id: SessionId) {
        if let Some(s) = self.get_mut(id) {
            s.state = State::Exited;
        }
    }

    /// Forget a slot, returning its handle so the caller can close it.
    pub fn remove(&mut self, id: SessionId) -> Option<S> {
        let idx = self.index_of(id)?;
        let slot = self.sessions.remove(idx);
        if self.active == Some(id) {
            self.active = None;
        }
        if self.last_active == Some(id) {
            self.last_active = None;
        }
        if self.cursor >= self.sessions.len() {
            self.cursor = self.sessions.len().saturating_sub(1);
        }
        slot.handle
    }

    /// The session on screen, if any.
    pub fn active(&self) -> Option<SessionId> {
        self.active
    }

    pub fn active_slot(&self) -> Option<&Managed<S>> {
        self.active.and_then(|id| self.get(id))
    }

    pub fn active_slot_mut(&mut self) -> Option<&mut Managed<S>> {
        let id = self.active?;
        self.get_mut(id)
    }

    /// Put a session on screen. Its attention marks are cleared: the user is
    /// looking at it now.
    pub fn activate(&mut self, id: SessionId) -> bool {
        if self.index_of(id).is_none() {
            return false;
        }
        if self.active != Some(id) {
            self.last_active = self.active;
        }
        self.active = Some(id);
        self.cursor = self.index_of(id).unwrap_or(0);
        if let Some(s) = self.get_mut(id) {
            s.unread = false;
            s.bell = false;
        }
        true
    }

    /// Take the session off screen without touching it — the whole point of the
    /// manager: the session keeps running.
    pub fn detach(&mut self) {
        if let Some(id) = self.active.take() {
            self.last_active = Some(id);
        }
    }

    /// The session the manager was entered from, if it is still there.
    pub fn previous(&self) -> Option<SessionId> {
        self.last_active.filter(|id| self.index_of(*id).is_some())
    }

    /// Which session `Back` returns to.
    pub fn back_target(&self) -> Option<SessionId> {
        self.previous().or_else(|| {
            // No history (the frontend started in the manager): the selected
            // live session, so `Esc` does something sensible.
            self.selection().filter(|id| {
                self.get(*id).map(|s| s.state.is_live()).unwrap_or(false)
            })
        })
    }

    /// Move to the next live session in list order, wrapping, and return it.
    pub fn cycle(&mut self, delta: i32) -> Option<SessionId> {
        let n = self.sessions.len();
        if n == 0 {
            return None;
        }
        let start = self
            .active
            .and_then(|id| self.index_of(id))
            .unwrap_or_else(|| self.cursor.min(n - 1));
        let step: i32 = if delta < 0 { -1 } else { 1 };
        for hop in 1..=n as i32 {
            let idx = ((start as i32 + step * hop).rem_euclid(n as i32)) as usize;
            if self.sessions[idx].state.is_live() {
                let id = self.sessions[idx].id;
                self.activate(id);
                return Some(id);
            }
        }
        None
    }

    /// Which row the manager's cursor is on.
    pub fn selection(&self) -> Option<SessionId> {
        let visible = self.visible_indices();
        visible.iter().copied().find(|i| *i == self.cursor)
            .or_else(|| visible.first().copied())
            .map(|i| self.sessions[i].id)
    }

    pub fn selection_index(&self) -> usize {
        let selected = self.selection();
        self.visible_indices().iter()
            .position(|i| Some(self.sessions[*i].id) == selected)
            .unwrap_or(0)
    }

    pub fn filter(&self) -> &str { &self.filter }

    /// Keep the selection if it still matches; otherwise use the first match.
    pub fn set_filter(&mut self, query: impl Into<String>) {
        self.filter = query.into();
        if let Some(id) = self.selection() {
            self.cursor = self.index_of(id).unwrap_or(0);
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        let query = self.filter.trim().to_lowercase();
        self.sessions.iter().enumerate().filter_map(|(i, s)| {
            let matches = query.is_empty() || [
                s.title.as_str(), s.remote_title.as_deref().unwrap_or(""),
                s.target.as_str(), s.state.label(),
            ].iter().any(|value| value.to_lowercase().contains(&query));
            matches.then_some(i)
        }).collect()
    }

    /// Put the cursor on a row (the caller may be following the active one).
    pub fn select(&mut self, id: SessionId) {
        if let Some(idx) = self.index_of(id) {
            self.cursor = idx;
        }
    }

    /// One line per session, in list order, for the manager to draw.
    pub fn rows(&self) -> Vec<Row> {
        let visible = self.visible_indices();
        self.sessions
            .iter()
            .enumerate()
            .filter(|(i, _)| visible.binary_search(i).is_ok())
            .map(|(i, s)| Row {
                number: i + 1,
                id: s.id,
                title: s.display_title(),
                target: s.target.clone(),
                state: s.state.label(),
                live: s.state.is_live(),
                active: self.active == Some(s.id),
                unread: s.unread,
                bell: s.bell,
                awaiting: s.awaiting,
                cols: s.cols,
                rows: s.rows,
                age_secs: s.age_secs(),
                failure: match &s.state {
                    State::Failed(reason) => Some(reason.clone()),
                    _ => None,
                },
            })
            .collect()
    }

    /// Turn a key into what the frontend should do.
    ///
    /// The keys are plain letters on purpose: inside a session every printable
    /// key belongs to the remote, but the manager is our own screen — nothing
    /// else is listening — so the usual `j`/`k`/`Enter` vocabulary costs
    /// nothing and needs no chord.
    pub fn apply(&mut self, cmd: Command) -> Outcome {
        match cmd {
            Command::None => Outcome::Done,
            Command::Move(delta) => {
                let visible = self.visible_indices();
                if !visible.is_empty() {
                    let idx = (self.selection_index() as i64 + i64::from(delta))
                        .clamp(0, visible.len() as i64 - 1) as usize;
                    self.cursor = visible[idx];
                }
                Outcome::Done
            }
            Command::First => {
                self.cursor = self.visible_indices().first().copied().unwrap_or(0);
                Outcome::Done
            }
            Command::Last => {
                self.cursor = self.visible_indices().last().copied().unwrap_or(0);
                Outcome::Done
            }
            Command::Attention(delta) => {
                let visible = self.visible_indices();
                let start = self.selection_index() as i64;
                let step = if delta < 0 { -1 } else { 1 };
                for hop in 1..=visible.len() {
                    let idx = (start + step * hop as i64).rem_euclid(visible.len() as i64) as usize;
                    let slot = &self.sessions[visible[idx]];
                    if slot.awaiting || slot.bell || slot.unread {
                        self.cursor = visible[idx];
                        break;
                    }
                }
                Outcome::Done
            }
            Command::Select => match self.selection() {
                Some(id) if self.get(id).map(|s| s.state.is_live()).unwrap_or(false) => {
                    Outcome::Attach(id)
                }
                // A dead or failed session is reopened from its spec — the
                // reason it died is on screen, and Enter is the obvious retry.
                Some(id) => {
                    if self.get(id).and_then(|s| s.spec.as_ref()).is_some() {
                        Outcome::Retry(id)
                    } else {
                        Outcome::Done
                    }
                }
                None => Outcome::Done,
            },
            Command::SelectNumber(n) => match self.nth(n) {
                Some(s) if s.state.is_live() => Outcome::Attach(s.id),
                Some(s) => match s.spec.as_ref() {
                    Some(_) => Outcome::Retry(s.id),
                    None => Outcome::Done,
                },
                None => Outcome::Done,
            },
            Command::Back => match self.back_target() {
                Some(id) => Outcome::Attach(id),
                None => Outcome::Quit,
            },
            Command::NewSession => Outcome::NewSession,
            Command::Close => match self.selection() {
                Some(id) => Outcome::Close(id),
                None => Outcome::Done,
            },
            Command::Rename => match self.selection() {
                Some(id) => Outcome::Rename(id),
                None => Outcome::Done,
            },
            Command::Quit => Outcome::Quit,
            Command::Help => Outcome::Help,
            Command::Agent => Outcome::Agent,
        }
    }

    /// How the status bar describes the manager.
    pub fn status_line(&self) -> String {
        match self.live_count() {
            0 => "no live sessions".to_string(),
            1 => "1 live session".to_string(),
            n => format!("{n} live sessions"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manager whose slots carry a plain `String` handle: the bookkeeping
    /// under test does not care what a handle is, and neither does the caller.
    fn manager_with(titles: &[&str]) -> (Manager<String>, Vec<SessionId>) {
        let mut m = Manager::new();
        let ids = titles
            .iter()
            .map(|t| m.insert(*t, format!("{t}:22"), None))
            .collect();
        (m, ids)
    }

    #[test]
    fn sessions_get_stable_ids_not_positions() {
        let (mut m, ids) = manager_with(&["a", "b", "c"]);
        assert_eq!(ids.len(), 3);
        assert!(ids.iter().all(|id| *id != SessionId(0)));
        // Removing the first slot renumbers the rows but must not re-point the
        // ids: an open in flight for `c` keeps tracking `c`.
        m.remove(ids[0]);
        assert_eq!(m.len(), 2);
        assert_eq!(m.nth(1).map(|s| s.title.clone()), Some("b".to_string()));
        assert_eq!(m.nth(2).map(|s| s.title.clone()), Some("c".to_string()));
        assert_eq!(m.get(ids[2]).map(|s| s.title.clone()), Some("c".to_string()));
        assert_eq!(m.rows()[1].number, 2, "rows are numbered 1..n");
    }

    #[test]
    fn detaching_keeps_the_session_alive_and_remembers_it() {
        let (mut m, ids) = manager_with(&["a", "b"]);
        assert!(m.activate(ids[1]));
        assert_eq!(m.active(), Some(ids[1]));
        m.detach();
        assert_eq!(m.active(), None, "nothing on screen");
        assert_eq!(m.len(), 2, "and nothing closed");
        assert_eq!(m.live_count(), 2);
        assert_eq!(m.back_target(), Some(ids[1]), "Esc returns where we left");
    }

    #[test]
    fn activating_clears_attention_marks() {
        let (mut m, ids) = manager_with(&["a", "b"]);
        m.activate(ids[0]);
        m.detach();
        assert_eq!(m.rows()[0].flags(), "  ", "nothing to report yet");
        {
            let slot = m.get_mut(ids[0]).unwrap();
            slot.unread = true;
            slot.bell = true;
        }
        // While it is off screen the row reports what arrived…
        assert_eq!(m.rows()[0].flags(), " B", "a bell outranks plain output");
        assert!(m.rows()[0].unread);
        // …and looking at it again clears both.
        m.activate(ids[0]);
        assert!(!m.rows()[0].unread, "looking at it clears it");
        assert!(!m.rows()[0].bell);
        assert_eq!(m.rows()[0].flags(), "* ");
        // Needing an answer outranks a bell: it blocks the session.
        {
            let slot = m.get_mut(ids[0]).unwrap();
            slot.bell = true;
            slot.awaiting = true;
        }
        assert_eq!(m.rows()[0].flags(), "*!");
    }

    #[test]
    fn cycling_skips_dead_sessions_and_wraps() {
        let (mut m, ids) = manager_with(&["a", "b", "c"]);
        m.set_exited(ids[1]);
        assert!(m.activate(ids[0]));
        assert_eq!(m.cycle(1), Some(ids[2]), "b is skipped");
        assert_eq!(m.cycle(1), Some(ids[0]), "wraps past the dead one");
        assert_eq!(m.cycle(-1), Some(ids[2]), "and backwards");
    }

    #[test]
    fn cycling_a_list_with_no_live_session_keeps_the_current_one() {
        let (mut m, ids) = manager_with(&["a"]);
        m.set_failed(ids[0], "no route to host");
        m.activate(ids[0]);
        assert_eq!(m.cycle(1), None);
        assert_eq!(m.active(), Some(ids[0]), "nothing to switch to, nothing lost");
    }

    #[test]
    fn selecting_a_dead_session_offers_a_retry_only_with_a_spec() {
        let mut m: Manager<String> = Manager::new();
        let shell = OpenSpec::Shell(term_proto::ShellOpenSpec {
            display_encoding: None,
            args: Vec::new(),
            env: Vec::new(),
            shell: None,
            cwd: None,
            cols: 80,
            rows: 24,
        });
        let with_spec = m.insert("saved", "user@host:22", Some(shell));
        let without = m.insert("ad-hoc", "user@host:22", None);
        m.set_failed(with_spec, "connect refused");
        m.set_exited(without);

        m.select(with_spec);
        assert_eq!(m.apply(Command::Select), Outcome::Retry(with_spec));
        m.select(without);
        assert_eq!(m.apply(Command::Select), Outcome::Done, "nothing to retry with");
        assert!(m.rows()[0].failure.is_some(), "the reason is shown");
    }

    #[test]
    fn selecting_a_live_session_attaches() {
        let (mut m, ids) = manager_with(&["a", "b"]);
        m.select(ids[1]);
        assert_eq!(m.apply(Command::Select), Outcome::Attach(ids[1]));
        assert_eq!(m.apply(Command::SelectNumber(1)), Outcome::Attach(ids[0]));
        assert_eq!(m.apply(Command::SelectNumber(9)), Outcome::Done, "no such row");
    }

    #[test]
    fn the_cursor_cannot_move_off_the_list() {
        let (mut m, _ids) = manager_with(&["a", "b"]);
        m.apply(Command::Move(-5));
        assert_eq!(m.selection_index(), 0);
        m.apply(Command::Move(5));
        assert_eq!(m.selection_index(), 1, "clamped to the last row");
        m.apply(Command::First);
        assert_eq!(m.selection_index(), 0);
        m.apply(Command::Last);
        assert_eq!(m.selection_index(), 1);
    }

    #[test]
    fn an_empty_manager_quits_on_back_and_reports_so() {
        let mut m: Manager<String> = Manager::new();
        assert_eq!(m.apply(Command::Back), Outcome::Quit);
        assert_eq!(m.apply(Command::Select), Outcome::Done);
        assert_eq!(m.apply(Command::Close), Outcome::Done);
        assert_eq!(m.status_line(), "no live sessions");
    }

    #[test]
    fn closing_removes_the_slot_and_hands_the_handle_back_once() {
        let (mut m, ids) = manager_with(&["a", "b"]);
        m.set_open(ids[0], "handle-a".to_string(), 80, 24);
        assert_eq!(m.remove(ids[0]), Some("handle-a".to_string()));
        assert_eq!(m.remove(ids[0]), None, "no double close");
        assert_eq!(m.len(), 1);
        assert_eq!(m.status_line(), "1 live session");
    }

    #[test]
    fn an_exited_session_leaves_the_active_slot_empty_without_losing_the_list() {
        let (mut m, ids) = manager_with(&["a", "b"]);
        m.set_open(ids[0], "h".to_string(), 100, 40);
        m.activate(ids[0]);
        m.set_exited(ids[0]);
        assert_eq!(m.active(), Some(ids[0]), "still shown: the user reads the exit");
        assert!(!m.rows()[0].live);
        assert_eq!(m.live_count(), 1);
        assert_eq!(m.rows()[0].cols, 100, "its last grid is still reported");
    }

    #[test]
    fn an_open_resolving_into_a_removed_slot_is_dropped_quietly() {
        // The user closed a session while its SSH open was still in flight. The
        // open must not resurrect the slot or panic.
        let (mut m, ids) = manager_with(&["a"]);
        m.remove(ids[0]);
        m.set_open(ids[0], "late".to_string(), 80, 24);
        m.set_failed(ids[0], "gone");
        m.set_exited(ids[0]);
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn the_display_title_prefers_what_the_remote_says() {
        let (mut m, ids) = manager_with(&["host"]);
        assert_eq!(m.rows()[0].title, "host");
        m.get_mut(ids[0]).unwrap().remote_title = Some("~/.cargo — vim".to_string());
        assert_eq!(m.rows()[0].title, "~/.cargo — vim");
        m.get_mut(ids[0]).unwrap().remote_title = Some("   ".to_string());
        assert_eq!(m.rows()[0].title, "host", "an empty OSC title is not a name");
    }

    #[test]
    fn filtering_keeps_operations_on_the_visible_stable_session() {
        let (mut m, ids) = manager_with(&["alpha", "beta", "production"]);
        m.get_mut(ids[1]).unwrap().remote_title = Some("BUILD output".into());
        m.set_filter("build");
        assert_eq!(m.rows().len(), 1);
        assert_eq!(m.rows()[0].number, 2, "display numbers keep their global shortcuts");
        assert_eq!(m.apply(Command::Select), Outcome::Attach(ids[1]));
        assert_eq!(m.apply(Command::Close), Outcome::Close(ids[1]));

        m.set_filter("PRODUCTION:22");
        assert_eq!(m.selection(), Some(ids[2]), "address search ignores case");
        m.set_failed(ids[0], "host unavailable");
        m.set_filter("failed");
        assert_eq!(m.selection(), Some(ids[0]), "state is searchable");
        m.set_filter("no match");
        assert!(m.rows().is_empty());
        assert_eq!(m.apply(Command::Close), Outcome::Done, "hidden slots cannot be closed");
        assert_eq!(m.apply(Command::Select), Outcome::Done);
        m.set_filter("");
        assert_eq!(m.len(), 3, "search never drops a session");
    }

    #[test]
    fn filtered_navigation_and_attention_wrap_only_through_matches() {
        let (mut m, ids) = manager_with(&["prod-a", "dev", "prod-b", "prod-c"]);
        m.get_mut(ids[1]).unwrap().awaiting = true;
        m.get_mut(ids[2]).unwrap().bell = true;
        m.get_mut(ids[3]).unwrap().unread = true;
        m.set_filter("prod");
        m.apply(Command::Move(1));
        assert_eq!(m.selection(), Some(ids[2]));
        assert_eq!(m.selection_index(), 1);
        m.apply(Command::Attention(1));
        assert_eq!(m.selection(), Some(ids[3]));
        m.apply(Command::Attention(1));
        assert_eq!(m.selection(), Some(ids[2]), "wraps and skips a hidden prompt");
        m.apply(Command::Attention(-1));
        assert_eq!(m.selection(), Some(ids[3]));
        m.apply(Command::First);
        assert_eq!(m.selection(), Some(ids[0]));
        m.apply(Command::Last);
        assert_eq!(m.selection(), Some(ids[3]));
        m.set_filter("absent");
        m.apply(Command::Attention(1));
        assert_eq!(m.selection(), None, "empty search does not divide by zero");
    }
}
