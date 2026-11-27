//! The DSH page: the daemon's DSH connection, the conversation it feeds, and
//! the calls a chat needs.
//!
//! DSH requires Pro or above, and the daemon owns everything about it: the
//! sidecar, the launch token, the auth cookie, the RPC client and the stream hub. This
//! module is the frontend half, and it deliberately holds **no** DSH
//! configuration: it asks the daemon to connect from the daemon's own endpoint
//! ([`ClientMethod::DshReconnect`]), follows a session, and renders the rows the
//! daemon builds in `term_dsh_model::chat` — the same rows the GUI renders.
//!
//! Nothing here draws. It is the state machine the TUI's frame loop pumps, so
//! it can be tested against the daemon's own update shapes without a terminal.
//!
//! Flow:
//!
//! ```text
//! attach()   DshStatus → (DshReconnect) → DshHubOpen → session/list
//! open(id)   DshFollow(session:<id>) → DshChatOpen(key) → snapshot rows
//! pump()     CoreEvent::DshChat / DshHub on DSH_MUX_SID
//!            → replace the rows from `from_seq` on, queue, projections
//!            → waterfall events become the overlay's pending question
//! send(text) DshCall session/prompt   (queue or steer)
//! Ctrl-C     DshCall session/cancel
//! y/n        DshAnswer <event_id> {kind: result, value: "allowed-once"|"rejected"}
//! ```
//!
//! [`ClientMethod::DshReconnect`]: term_proto::ClientMethod::DshReconnect

use std::collections::HashMap;
use std::time::Duration;

use term_dsh_model::api::session::{
    PromptContentPart, PromptMode, SessionAddress, SessionPage, SessionSummary,
};
use term_dsh_model::chat::{
    is_synthetic_seq, log_message_seq, ChatLog, ChatMessage, ChatSnapshotWire,
    ChatUpdateWire, QueuedItem,
};
use term_dsh_model::events::EventOutcome;
use term_dsh_model::hub::{follow_key, HubEvent};
use term_dsh_model::wire::mint_request_id;
use serde_json::{json, Value};
use term_client::{Client, Writer};
use term_proto::{CoreEvent, ServerEvent, DSH_MUX_SID};

/// How many rows a follow asks the daemon for.
///
/// The daemon holds this page and pushes replacements as the turn runs, so this
/// is "how much of the tail a terminal screen can usefully scroll", not a page
/// size: older history is `session/page` work, which needs a pager UI (the GUI
/// has one; this page says so instead of pretending).
const MAX_MESSAGES: u32 = 200;

/// How many rows one `session/page` asks for.
const PAGE_MESSAGES: u32 = 50;

/// How many times a page may re-ask for the conversation's snapshot while the
/// daemon has not built it from the host's baseline yet.
///
/// A follow's opening snapshot lands *after* the follow is opened, so the first
/// `DshChatOpen` can be answered from a log with no cursor yet (`cursor: 0,
/// has_more: false`). Taking that as the truth tells the reader their
/// conversation starts at the tail when a thousand rows sit before it, and
/// nothing would ever correct it: only a *snapshot* carries the cursor, and the
/// incremental updates never do.
const RESEED_TRIES: u8 = 6;

/// How long to wait between those attempts (the baseline arrives in well under a
/// second in practice).
const RESEED_EVERY: Duration = Duration::from_millis(300);

/// How long after a page before another may be fetched.
///
/// The trigger is "the reader is at the very top", which stays true while they
/// hold the key: without a floor the page would page the whole conversation in
/// as fast as the host answers.
const PAGE_COOLDOWN: Duration = Duration::from_millis(400);

/// How long one relayed call may take before the UI gives up on it.
///
/// The calls are small and the daemon is local, but the *daemon* is relaying to
/// the host: bounding the wait keeps a wedged host from freezing the whole
/// frontend, and the status line reports what happened instead.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Entitlements can change on another frontend while this page is open.
const AUTHORIZATION_CHECK_EVERY: Duration = Duration::from_secs(1);
const DSH_LICENSE_REQUIRED: &str =
    "DSH requires Pro or above. Activate a valid paid license to use DSH.";

#[derive(Debug, Clone, PartialEq, Eq)]
struct AccountIdentity {
    base_url: String,
    uid: Option<i64>,
    logged_in: bool,
    login_session_id: Option<String>,
}

impl From<&term_proto::AccountState> for AccountIdentity {
    fn from(state: &term_proto::AccountState) -> Self {
        Self {
            base_url: state.base_url.clone(),
            uid: state.uid,
            logged_in: state.logged_in,
            login_session_id: state.login_session_id.clone(),
        }
    }
}

/// Where the daemon's DSH connection stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// Nothing asked yet.
    Idle,
    /// The daemon is connecting, or spawning its sidecar.
    Connecting,
    /// Live: calls and streams work.
    Connected,
    /// The last attempt failed; the string is the daemon's own detail.
    Failed(String),
    /// The daemon requires a valid Pro-or-higher license for DSH.
    NeedsLicense,
}

impl Link {
    /// The word the header shows.
    pub fn label(&self) -> String {
        match self {
            Link::Idle => "not connected".into(),
            Link::Connecting => "connecting".into(),
            Link::Connected => "connected".into(),
            Link::Failed(_) => "unreachable".into(),
            Link::NeedsLicense => "Pro or above required".into(),
        }
    }

    /// Whether the page can talk to the host.
    pub fn live(&self) -> bool {
        *self == Link::Connected
    }
}

/// One question the agent asked (`user-questions/request`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionItem {
    /// The `id` the answer must echo.
    pub id: String,
    /// The short header, when the host sends one.
    pub header: String,
    /// The question itself.
    pub text: String,
    /// The option labels, in the host's order.
    pub options: Vec<String>,
    /// Whether more than one option may be picked.
    pub multi: bool,
}

/// A `user-questions/request` in progress: the questions, the picks so far and
/// where the user is in them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Questions {
    pub event_id: String,
    pub items: Vec<QuestionItem>,
    /// Picked labels per question.
    pub picked: Vec<Vec<String>>,
    /// Free-text answer per question (an alternative to picking).
    pub custom: Vec<String>,
    /// The question being answered.
    pub index: usize,
    /// The free-text buffer for the current question.
    pub input: String,
    /// True while the user is typing a free-text answer.
    pub typing: bool,
}

impl Questions {
    /// Build the overlay state from a `user-questions/request` payload.
    ///
    /// The shape is the host's: `{questions: [{id, header, question, options:
    /// [{label, description?}], multiSelect}]}`.
    pub fn from_request(event_id: &str, request: &Value) -> Self {
        let mut items = Vec::new();
        if let Some(list) = request.get("questions").and_then(Value::as_array) {
            for q in list {
                let options = q
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|opts| {
                        opts.iter()
                            .filter_map(|o| o.get("label").and_then(Value::as_str))
                            .map(|s| s.to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                items.push(QuestionItem {
                    id: q
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    header: q
                        .get("header")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    text: q
                        .get("question")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    options,
                    multi: q
                        .get("multiSelect")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                });
            }
        }
        let n = items.len();
        Self {
            event_id: event_id.to_string(),
            items,
            picked: vec![Vec::new(); n],
            custom: vec![String::new(); n],
            index: 0,
            input: String::new(),
            typing: false,
        }
    }

    /// The question in front of the user.
    pub fn current(&self) -> Option<&QuestionItem> {
        self.items.get(self.index)
    }

    /// Whether the question on screen has an answer yet.
    pub fn current_answered(&self) -> bool {
        self.picked
            .get(self.index)
            .map(|p| !p.is_empty())
            .unwrap_or(false)
            || self
                .custom
                .get(self.index)
                .map(|c| !c.trim().is_empty())
                .unwrap_or(false)
    }

    /// Toggle (multi-select) or set (single-select) an option by 1-based index.
    pub fn pick(&mut self, number: usize) {
        let Some(item) = self.items.get(self.index).cloned() else {
            return;
        };
        let Some(label) = item.options.get(number.saturating_sub(1)).cloned() else {
            return;
        };
        let slot = &mut self.picked[self.index];
        if item.multi {
            if let Some(pos) = slot.iter().position(|l| *l == label) {
                slot.remove(pos);
            } else {
                slot.push(label);
            }
        } else {
            *slot = vec![label];
        }
    }

    /// The answer value for the whole waterfall (`{answers: [{id, selected,
    /// custom?}]}`), with the free-text buffer folded in.
    pub fn answer_value(&self) -> Value {
        let mut answers = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            let mut selected = self.picked.get(i).cloned().unwrap_or_default();
            let typed = self
                .custom
                .get(i)
                .map(|c| c.trim().to_string())
                .unwrap_or_default();
            // A typed answer replaces picks unless the host allows both.
            if !item.multi && !typed.is_empty() {
                selected.clear();
            }
            let mut answer = json!({ "id": item.id, "selected": selected });
            if !typed.is_empty() {
                answer["custom"] = Value::String(typed);
            }
            answers.push(answer);
        }
        json!({ "answers": answers })
    }
}

/// A waterfall the daemon is holding open for an answer from this frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// A tool call needs permission before it runs.
    Approval {
        event_id: String,
        tool: String,
        reason: Option<String>,
    },
    /// The agent asked the user something.
    Question(Box<Questions>),
}

/// The DSH page's state.
pub struct DshFeed {
    /// The daemon connection (requests). `None` until [`DshFeed::attach`].
    writer: Option<Writer>,
    /// The same connection's event feed (`DSH_MUX_SID` frames).
    events: Option<tokio::sync::mpsc::UnboundedReceiver<ServerEvent>>,
    account_identity: Option<AccountIdentity>,
    last_authorization_check: Option<std::time::Instant>,
    /// Latest connect/status reply or state push from this daemon connection.
    state_revision: u64,
    /// Cleared by the outer UI after it has returned to the ordinary page.
    close_requested: bool,
    /// The daemon's DSH connection state.
    pub link: Link,
    /// Rows of the followed conversation, as the daemon built them.
    ///
    /// The per-turn tables the rows cannot carry (produced files, turn ends,
    /// clocks, compaction summaries) live on the log too (`ChatLog::turn_end`
    /// and friends), merged by [`ChatLog::merge_side_tables`] — one copy, so the
    /// renderer and the merge rule cannot disagree.
    pub log: ChatLog,
    /// Projections by key (`title`, `stats`, `todos`, …).
    pub projections: HashMap<String, Value>,
    /// The authoritative queued/steering inbox.
    pub queue: Vec<QueuedItem>,
    /// The follow key being shown (`session:<id>`).
    pub key: Option<String>,
    /// The session id behind [`Self::key`].
    pub session_id: Option<String>,
    /// `session/list` rows.
    pub sessions: Vec<SessionSummary>,
    /// Titles from the list's projection blocks, by session id.
    pub titles: HashMap<String, String>,
    /// A waterfall waiting for the user.
    pub pending: Option<Pending>,
    /// The last thing worth telling the user (a failure, most often).
    pub status: Option<String>,
    /// True while a turn is running (a live draft, or the agent says so).
    pub running: bool,
    /// Whether `session/list` has been asked for since the daemon's DSH became
    /// live. The daemon answers `connecting` first and only reaches `connected`
    /// a moment later (it runs the attempt in the background), so listing has to
    /// wait for that, not happen once at attach time.
    pub sessions_loaded: bool,
    /// Whether the page has decided which session to show (or that there is
    /// none): the decision must happen once per connect, not every frame.
    pub landing_done: bool,
    /// The follow snapshot's inclusive log cut, which `session/page` pages
    /// backwards from.
    pub cursor: i64,
    /// Whether the host has older turns than the log holds.
    pub has_more: bool,
    /// A page is in flight.
    pub loading_older: bool,
    /// When the last page was fetched (see [`PAGE_COOLDOWN`]).
    last_page_at: Option<std::time::Instant>,
    /// Attempts made to re-ask for a snapshot whose cursor was not real yet.
    reseed_tries: u8,
    /// When the last such attempt was made.
    last_reseed_at: Option<std::time::Instant>,
    /// Whether this page asked the daemon for its stream hub.
    ///
    /// `DshFollow` is served by that hub, so following before asking for it fails
    /// with "the daemon has no live DSH hub" — the hub lives on the daemon and is
    /// shared, but a *second* frontend (this page) still has to open it.
    pub hub_opened: bool,
}

impl Default for DshFeed {
    fn default() -> Self {
        Self::new()
    }
}

impl DshFeed {
    pub fn new() -> Self {
        Self {
            writer: None,
            events: None,
            account_identity: None,
            last_authorization_check: None,
            state_revision: 0,
            close_requested: false,
            link: Link::Idle,
            log: ChatLog::default(),
            projections: HashMap::new(),
            queue: Vec::new(),
            key: None,
            session_id: None,
            sessions: Vec::new(),
            titles: HashMap::new(),
            pending: None,
            status: None,
            running: false,
            sessions_loaded: false,
            landing_done: false,
            hub_opened: false,
            cursor: -1,
            has_more: false,
            loading_older: false,
            last_page_at: None,
            reseed_tries: 0,
            last_reseed_at: None,
        }
    }

    /// The page's title: the daemon's projected title, else the short id.
    pub fn title(&self) -> String {
        if let Some(title) = self.projections.get("title").and_then(Value::as_str) {
            if !title.trim().is_empty() {
                return title.to_string();
            }
        }
        match &self.session_id {
            Some(id) => short_id(id),
            None => "no session".to_string(),
        }
    }

    /// The conversation on screen.
    pub fn rows(&self) -> &[ChatMessage] {
        &self.log.messages
    }

    /// Whether the page has a connection to the daemon's DSH.
    pub fn attached(&self) -> bool {
        self.writer.is_some()
    }

    /// Bring the daemon's DSH connection up and list its sessions.
    ///
    /// A frontend with no DSH settings of its own (this one) asks the daemon to
    /// use the endpoint it already knows. When the daemon has never been given
    /// one, the reason is reported as-is rather than replaced by a guess.
    pub async fn attach(&mut self, addr: &str) -> Result<(), String> {
        self.close_requested = false;
        self.state_revision = 0;
        self.link = Link::Connecting;
        let client = match tokio::time::timeout(CALL_TIMEOUT, Client::connect(addr)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return self.fail(format!("daemon unreachable: {e}")),
            Err(_) => return self.fail("daemon connect timed out".to_string()),
        };
        // Licensing is owned by the daemon's persistent offline activation.
        // No environment-supplied bearer JWT can change its entitlements.
        match tokio::time::timeout(CALL_TIMEOUT, client.feature_allowed("dsh")).await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => return self.fail_dsh("license_required: dsh requires Pro or above".into()),
            Ok(Err(error)) => return self.fail_dsh(error),
            Err(_) => return self.fail("dsh authorization check timed out".into()),
        }
        let (writer, events, _asks) = client.into_parts();
        self.account_identity = match tokio::time::timeout(CALL_TIMEOUT, writer.account_state()).await {
            Ok(Ok(state)) => Some(AccountIdentity::from(&state)),
            _ => None,
        };
        self.last_authorization_check = Some(std::time::Instant::now());

        let state = match self.call_state(&writer, "status").await {
            Ok(state) => state,
            Err(e) => return self.fail_dsh(e),
        };
        let state = if state.state == "connected" {
            state
        } else {
            // The daemon's own endpoint, not a guess: only the daemon knows
            // what it was configured with.
            match tokio::time::timeout(CALL_TIMEOUT, writer.dsh_reconnect()).await {
                Ok(Ok(state)) => state,
                Ok(Err(e)) => return self.fail_dsh(e),
                Err(_) => return self.fail("dsh connect timed out".to_string()),
            }
        };
        self.apply_state(&state);
        self.writer = Some(writer);
        self.events = Some(events);
        if self.link == Link::Connected {
            if let Err(e) = self.refresh_sessions().await {
                self.status = Some(e);
            }
        } else if self.link == Link::Idle {
            // The daemon answered, and its DSH connection is off (the `[dsh]`
            // switch in the GUI's settings, or a connect that was never asked
            // for). Better than an empty page with no explanation.
            self.status = Some(
                "the daemon's DSH connection is off: turn DSH on in the GUI's DSH settings \
                 (the daemon reads `[dsh]` from settings.toml)"
                    .to_string(),
            );
        }
        if !self.link.live() {
            // Nothing to list yet: `upkeep` asks once the daemon reports it is
            // connected.
            self.sessions_loaded = false;
            self.landing_done = false;
            self.hub_opened = false;
        }
        Ok(())
    }

    /// Open the daemon's stream hub, once per connection.
    ///
    /// Idempotent on the daemon side (the hub is shared by every frontend), and
    /// required: `DshFollow` fails with "no live DSH hub" without it.
    async fn open_hub(&mut self) -> Result<(), String> {
        if self.hub_opened {
            return Ok(());
        }
        let writer = self
            .writer
            .clone()
            .ok_or_else(|| "the dsh page has no daemon connection".to_string())?;
        match tokio::time::timeout(CALL_TIMEOUT, writer.dsh_hub_open()).await {
            Ok(Ok(())) => {
                self.hub_opened = true;
                log::info!("dsh page: opened the daemon's stream hub");
                Ok(())
            }
            Ok(Err(e)) => {
                let e = format!("dsh hub: {e}");
                self.status = Some(e.clone());
                Err(e)
            }
            Err(_) => {
                let e = "dsh hub open timed out".to_string();
                self.status = Some(e.clone());
                Err(e)
            }
        }
    }

    /// One frame of upkeep: attach on first use, list the host's sessions once
    /// the link is live, and follow the most recent one — once.
    ///
    /// The sequence matters. `DshReconnect` answers `connecting` immediately (the
    /// daemon runs the attempt in the background), so a page that listed and
    /// landed at attach time would do it against a connection that is not up
    /// yet: it would show `connected` with no session, and nothing would ever ask
    /// again. Returns whether anything changed, so a frontend that redraws only
    /// on change knows.
    pub async fn upkeep(&mut self, addr: &str) -> bool {
        if self.close_requested {
            return false;
        }
        let mut changed = false;
        if !self.attached() {
            if let Err(e) = self.attach(addr).await {
                log::warn!("dsh page: {e}");
            }
            changed = true;
        }
        if self.close_requested {
            return true;
        }
        changed |= self.check_authorization().await;
        if self.close_requested {
            return true;
        }
        changed |= self.pump();
        if !self.link.live() {
            return changed;
        }
        // A log with no cursor is not a conversation with no history: ask again
        // (bounded, and spaced out) until the daemon answers from the host's
        // baseline.
        if self.session_id.is_some()
            && self.cursor <= 0
            && self.reseed_tries < RESEED_TRIES
            && reseed_due(self.last_reseed_at, std::time::Instant::now())
        {
            self.reseed_tries += 1;
            self.last_reseed_at = Some(std::time::Instant::now());
            match self.refresh_snapshot().await {
                Ok(()) => {
                    log::debug!("dsh page: re-read the snapshot (cursor={})", self.cursor);
                    changed = true;
                }
                Err(e) => log::debug!("dsh page: re-reading the snapshot failed: {e}"),
            }
        }
        if !self.sessions_loaded {
            self.sessions_loaded = true;
            // The hub first: following without it fails on the daemon.
            if let Err(e) = self.open_hub().await {
                self.status = Some(e);
            }
            match self.refresh_sessions().await {
                Ok(()) => changed = true,
                Err(e) => self.status = Some(e),
            }
        }
        if self.session_id.is_none() && !self.landing_done {
            self.landing_done = true;
            // Try the candidates in order: a row that cannot be followed (a
            // subagent child, a session the host refuses) must not leave the page
            // on nothing when the next one works.
            let candidates: Vec<String> = landing_candidates(&self.sessions)
                .into_iter()
                .map(|s| s.to_string())
                .take(5)
                .collect();
            let mut last_error = None;
            for id in &candidates {
                match self.open(id).await {
                    Ok(()) => {
                        changed = true;
                        last_error = None;
                        break;
                    }
                    Err(e) => {
                        log::warn!("dsh page: {e}");
                        last_error = Some(format!("could not open {id}: {e}"));
                    }
                }
            }
            if let Some(e) = last_error {
                self.status = Some(e);
            } else if self.session_id.is_none() {
                self.status = Some(if self.sessions.is_empty() {
                    "the host has no sessions".to_string()
                } else {
                    "no session with content; [Tab] to pick one".to_string()
                });
            }
        }
        changed
    }

    /// The daemon's current DSH state, as a wire value.
    async fn call_state(
        &self,
        writer: &Writer,
        what: &str,
    ) -> Result<term_proto::DshStateWire, String> {
        match tokio::time::timeout(CALL_TIMEOUT, writer.dsh_status()).await {
            Ok(Ok(state)) => Ok(state),
            Ok(Err(e)) => Err(format!("dsh {what}: {e}")),
            Err(_) => Err(format!("dsh {what} timed out")),
        }
    }

    /// Record a failure in the page's own state and return it.
    fn fail(&mut self, message: String) -> Result<(), String> {
        self.link = Link::Failed(message.clone());
        self.status = Some(message.clone());
        Err(message)
    }

    /// A failed DSH call: close the page and explain a missing paid entitlement.
    fn fail_dsh(&mut self, error: String) -> Result<(), String> {
        if error.contains("license_required") {
            self.close_page(Link::NeedsLicense, DSH_LICENSE_REQUIRED.to_string());
            return Err(error);
        }
        self.fail(error)
    }

    /// Whether the outer UI must leave DSH and discard its view/composer state.
    pub fn take_close_requested(&mut self) -> bool {
        std::mem::take(&mut self.close_requested)
    }

    fn close_page(&mut self, link: Link, message: String) {
        self.writer = None;
        self.events = None;
        self.account_identity = None;
        self.last_authorization_check = None;
        self.state_revision = 0;
        self.log = ChatLog::default();
        self.projections.clear();
        self.queue.clear();
        self.key = None;
        self.session_id = None;
        self.sessions.clear();
        self.titles.clear();
        self.pending = None;
        self.running = false;
        self.sessions_loaded = false;
        self.landing_done = false;
        self.hub_opened = false;
        self.cursor = -1;
        self.has_more = false;
        self.loading_older = false;
        self.last_page_at = None;
        self.reseed_tries = 0;
        self.last_reseed_at = None;
        self.link = link;
        self.status = Some(message);
        self.close_requested = true;
    }

    fn account_changed(&mut self, identity: AccountIdentity) -> bool {
        if self.account_identity.as_ref().is_some_and(|previous| previous != &identity) {
            self.close_page(Link::Idle, "DSH closed because the account changed.".into());
            return true;
        }
        self.account_identity = Some(identity);
        false
    }

    async fn check_authorization(&mut self) -> bool {
        let now = std::time::Instant::now();
        if self.last_authorization_check.is_some_and(|last| now.duration_since(last) < AUTHORIZATION_CHECK_EVERY) {
            return false;
        }
        let Some(writer) = self.writer.clone() else { return false; };
        self.last_authorization_check = Some(now);
        match tokio::time::timeout(CALL_TIMEOUT, writer.call(term_proto::ClientMethod::FeatureAllowed {
            feature: "dsh".into(),
        })).await {
            Ok(Ok(term_proto::ServerResult::Ok(term_proto::ServerOk::FeatureAllowed { allowed: true }))) => {}
            Ok(Ok(term_proto::ServerResult::Ok(term_proto::ServerOk::FeatureAllowed { allowed: false }))) => {
                let _ = self.fail_dsh("license_required: dsh requires Pro or above".into());
                return true;
            }
            Ok(Ok(term_proto::ServerResult::Err(error))) if error.code == "license_required" => {
                let _ = self.fail_dsh(format!("license_required: {}", error.message));
                return true;
            }
            _ => return false,
        }
        // Some socket/vault configurations do not expose account diagnostics to
        // this connection. The paid capability check above remains authoritative.
        if let Ok(Ok(state)) = tokio::time::timeout(CALL_TIMEOUT, writer.account_state()).await {
            return self.account_changed(AccountIdentity::from(&state));
        }
        false
    }

    /// Fold one daemon state push into the page.
    fn apply_state(&mut self, state: &term_proto::DshStateWire) {
        if self.close_requested || state.revision < self.state_revision {
            return;
        }
        self.state_revision = state.revision;
        if state.error_kind.as_deref() == Some("license_required") {
            let _ = self.fail_dsh("license_required: dsh requires Pro or above".into());
            return;
        }
        if state.error_kind.as_deref() == Some("account_changed") {
            self.close_page(Link::Idle, "DSH closed because the account changed.".into());
            return;
        }
        let link = match state.state.as_str() {
            "connected" => Link::Connected,
            "connecting" | "spawning" | "spawned" => Link::Connecting,
            "disconnected" => Link::Idle,
            _ => Link::Failed(
                state
                    .error_detail
                    .clone()
                    .unwrap_or_else(|| state.state.clone()),
            ),
        };
        if link != self.link {
            log::info!("dsh link: {:?}", link);
        }
        self.link = link;
        if let Some(detail) = &state.error_detail {
            self.status = Some(detail.clone());
        }
    }

    /// Ask the host for its sessions, most recently updated first.
    pub async fn refresh_sessions(&mut self) -> Result<(), String> {
        // `session/list` names its one parameter `_request` on this wire tag;
        // every other `session/*` verb names it `request`.
        let value = self.call("session/list", json!({ "_request": {} })).await?;
        let items = value.get("items").cloned().unwrap_or(Value::Null);
        let sessions: Vec<SessionSummary> =
            serde_json::from_value(items).map_err(|e| format!("session/list: {e}"))?;
        for s in &sessions {
            // The list rows carry their title in a projection block.
            if let Some(block) = &s.projections {
                if let Some(title) = block.values.get("title").and_then(Value::as_str) {
                    self.titles.insert(s.session_id.clone(), title.to_string());
                }
            }
        }
        let mut sessions = sessions;
        sessions.sort_by_key(|s| -s.updated_at);
        self.sessions = sessions;
        Ok(())
    }

    /// Show one session: follow it and take the daemon's snapshot of its rows.
    pub async fn open(&mut self, session_id: &str) -> Result<(), String> {
        let address = SessionAddress::session(session_id);
        let key = follow_key(&address);
        let address = serde_json::to_value(&address).map_err(|e| e.to_string())?;
        let writer = self
            .writer
            .clone()
            .ok_or_else(|| "the dsh page has no daemon connection".to_string())?;
        match tokio::time::timeout(
            CALL_TIMEOUT,
            writer.dsh_follow(address, Some(MAX_MESSAGES), true),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let e = format!("dsh follow: {e}");
                self.status = Some(e.clone());
                return Err(e);
            }
            Err(_) => {
                let e = "dsh follow timed out".to_string();
                self.status = Some(e.clone());
                return Err(e);
            }
        }
        // The snapshot is authoritative: rows, per-turn tables, queue, cursor.
        let snapshot = match tokio::time::timeout(CALL_TIMEOUT, writer.dsh_chat_open(&key)).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(format!("dsh chat open: {e}")),
            Err(_) => return Err("dsh chat open timed out".to_string()),
        };
        let snapshot: ChatSnapshotWire =
            serde_json::from_value(snapshot).map_err(|e| format!("chat snapshot: {e}"))?;
        self.session_id = Some(session_id.to_string());
        self.key = Some(key);
        self.apply_snapshot(snapshot);
        self.reseed_tries = 0;
        self.last_reseed_at = Some(std::time::Instant::now());
        self.status = None;
        Ok(())
    }

    /// Ask for the conversation's snapshot again.
    ///
    /// The only message that carries the paging cursor, so a log whose cursor is
    /// not real yet has to be re-read rather than patched up.
    async fn refresh_snapshot(&mut self) -> Result<(), String> {
        let Some(key) = self.key.clone() else {
            return Ok(());
        };
        let Some(writer) = self.writer.clone() else {
            return Ok(());
        };
        let value = match tokio::time::timeout(CALL_TIMEOUT, writer.dsh_chat_open(&key)).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(format!("dsh chat open: {e}")),
            Err(_) => return Err("dsh chat open timed out".to_string()),
        };
        let snapshot: ChatSnapshotWire =
            serde_json::from_value(value).map_err(|e| format!("chat snapshot: {e}"))?;
        self.apply_snapshot(snapshot);
        Ok(())
    }

    /// Adopt a snapshot wholesale (the daemon's log is authoritative).
    fn apply_snapshot(&mut self, snapshot: ChatSnapshotWire) {
        self.log = ChatLog::default();
        self.log.messages = snapshot.rows;
        // Merge rather than replace: turns this page paged in itself are not in
        // the daemon's log. (Nothing pages here yet, but the rule is the GUI's
        // and costs nothing to keep.)
        self.log.merge_side_tables(&snapshot.side_tables);
        self.log.mark_all_done();
        self.projections = snapshot.projections;
        self.queue = snapshot.queue;
        self.cursor = snapshot.cursor;
        self.has_more = snapshot.has_more;
        self.running = false;
    }

    /// Apply one incremental update: replace the rows from `from_seq` on.
    ///
    /// Ported from the GUI's `dsh_drain_chat_updates`, including the two rules
    /// that are easy to get wrong: a live streaming draft is never carried over
    /// (its synthetic seq is minted per log, so keeping ours *and* appending the
    /// daemon's would render one draft twice), and only a full rebuild may
    /// finish the drafts (`done`), because an incremental update carries live
    /// ones whose `done: false` is the state.
    pub fn apply_update(&mut self, update: ChatUpdateWire) {
        let full_rebuild = update.from_seq.is_none();
        let floor = update
            .rows
            .first()
            .map(log_message_seq)
            .filter(|seq| *seq > 0);
        let keep: Vec<ChatMessage> = self
            .log
            .messages
            .iter()
            .filter(|m| {
                let seq = log_message_seq(m);
                if seq == 0 || is_synthetic_seq(seq) {
                    return false;
                }
                match update.from_seq {
                    Some(from) => seq < from,
                    // A rebuild replaces the daemon's whole row set; rows from
                    // an older *page* (below the new first row) are still
                    // history and stay.
                    None => floor.map(|f| seq < f).unwrap_or(false),
                }
            })
            .cloned()
            .collect();
        let mut rows = keep;
        rows.extend(update.rows);
        self.log.messages = rows;
        if full_rebuild {
            self.log.mark_all_done();
        }
        self.log.merge_side_tables(&update.side_tables);
        self.log.reindex();
        for (key, value) in update.projections {
            self.projections.insert(key, value);
        }
        if let Some(queue) = update.queue {
            self.queue = queue;
        }
        self.running = self.log.has_streaming();
    }

    /// Drain the daemon's feed: state pushes, chat updates and hub events.
    ///
    /// Synchronous on purpose: the frame loop calls this every frame whether or
    /// not anything arrived. Returns whether anything changed, so a frontend
    /// that only redraws on change knows.
    pub fn pump(&mut self) -> bool {
        if self.close_requested {
            return false;
        }
        let mut changed = false;
        // Drained into a batch first: applying an event needs `&mut self`, and
        // the receiver is part of it.
        let mut batch = Vec::new();
        if let Some(events) = self.events.as_mut() {
            while let Ok(event) = events.try_recv() {
                batch.push(event);
            }
        }
        for ServerEvent { sid, event } in batch {
            if self.close_requested {
                break;
            }
            if sid != DSH_MUX_SID {
                continue;
            }
            match event {
                CoreEvent::DshState(state) => {
                    self.apply_state(&state);
                    changed = true;
                }
                CoreEvent::DshChat { update } => match serde_json::from_value::<ChatUpdateWire>(update)
                {
                    Ok(update) => {
                        // Updates for a session this page is not showing are
                        // other frontends' business (the daemon keeps a log per
                        // followed key; this page follows one).
                        if self.key.as_deref() == Some(update.key.as_str()) {
                            self.apply_update(update);
                            changed = true;
                        }
                    }
                    Err(e) => log::warn!("undecodable dsh chat update: {e}"),
                },
                CoreEvent::DshHub { event } => match serde_json::from_value::<HubEvent>(event) {
                    Ok(event) => {
                        self.apply_hub(event);
                        changed = true;
                    }
                    Err(e) => log::warn!("undecodable dsh hub event: {e}"),
                },
                _ => {}
            }
        }
        self.running = self.running || self.log.has_streaming();
        changed
    }

    /// Fold one decoded hub event in. The page cares about waterfells (they
    /// block the agent until answered) and the control stream's queue.
    fn apply_hub(&mut self, event: HubEvent) {
        match event {
            HubEvent::Waterfall {
                event,
                event_id,
                request,
                ..
            } => match event.as_str() {
                "approval/request" => {
                    let tool = request
                        .get("toolName")
                        .and_then(Value::as_str)
                        .unwrap_or("a tool")
                        .to_string();
                    let reason = request
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(|s| s.to_string());
                    log::info!("dsh approval requested for {tool}");
                    self.pending = Some(Pending::Approval {
                        event_id,
                        tool,
                        reason,
                    });
                }
                "user-questions/request" => {
                    log::info!("dsh question requested");
                    self.pending = Some(Pending::Question(Box::new(Questions::from_request(
                        &event_id, &request,
                    ))));
                }
                other => {
                    // An answerable event this frontend does not handle: hand it
                    // to the next composed answerer instead of leaving the host
                    // blocked on us (web `next()` parity).
                    log::debug!("dsh waterfall {other}: delegating to the next answerer");
                    self.pending = None;
                    let _ = self.answer_raw(&event_id, EventOutcome::Next);
                }
            },
            HubEvent::WaterfallCancel { event_id } => {
                let drop_it = match &self.pending {
                    Some(Pending::Approval { event_id: id, .. }) => *id == event_id,
                    Some(Pending::Question(q)) => q.event_id == event_id,
                    None => false,
                };
                if drop_it {
                    log::info!("dsh waterfall {event_id} was cancelled by the host");
                    self.pending = None;
                }
            }
            HubEvent::Control(frame) => self.apply_control(frame),
            HubEvent::FollowEnded { key, reason } => {
                if self.key.as_deref() == Some(key.as_str()) {
                    self.status = Some(format!("the conversation stream ended: {reason}"));
                }
            }
            HubEvent::HubEnded(reason) => {
                self.status = Some(format!("the dsh event hub ended: {reason}"));
                self.link = Link::Failed(reason);
            }
            _ => {}
        }
    }

    /// The `session/control` stream: the authoritative projections (the queue
    /// dock reads the `inbox` projection since 0.1.7).
    fn apply_control(&mut self, frame: term_dsh_model::hub::ControlFrame) {
        match frame {
            term_dsh_model::hub::ControlFrame::Projection {
                session_id,
                key,
                value,
                ..
            } => {
                if self.session_id.as_deref() == Some(session_id.as_str()) {
                    if key == "inbox" {
                        self.queue = term_dsh_model::chat::parse_inbox_projection(&value);
                    }
                    self.projections.insert(key, value);
                }
            }
            _ => {}
        }
    }

    /// Send a prompt: `queue` sends it as the next turn, `steer` folds it into
    /// the turn that is running.
    pub async fn send(&mut self, text: &str, mode: PromptMode) -> Result<(), String> {
        // Nothing to send is a no-op whatever the session state: the composer
        // calls this on Enter, and an empty line must not be an error.
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let Some(session_id) = self.session_id.clone() else {
            return Err("no session is open on this page".to_string());
        };
        let args = prompt_args(&session_id, &mint_request_id(), mode, text);
        self.call("session/prompt", args).await?;
        Ok(())
    }

    /// Abort the turn that is running; queued work survives.
    pub async fn cancel_turn(&mut self) -> Result<(), String> {
        let Some(session_id) = self.session_id.clone() else {
            return Err("no session is open on this page".to_string());
        };
        self.call("session/cancel", json!({ "request": { "sessionId": session_id } }))
            .await?;
        Ok(())
    }

    /// Drop one queued item (`session/updateQueue`, `kind: remove`).
    pub async fn drop_queued(&mut self, item_id: &str) -> Result<(), String> {
        let Some(session_id) = self.session_id.clone() else {
            return Err("no session is open on this page".to_string());
        };
        self.call(
            "session/updateQueue",
            json!({ "request": {
                "sessionId": session_id,
                "itemId": item_id,
                "action": { "kind": "remove" },
            }}),
        )
        .await?;
        Ok(())
    }

    /// The seq of the oldest row the log holds: what a page pages before.
    fn earliest_seq(&self) -> Option<i64> {
        self.rows()
            .iter()
            .map(log_message_seq)
            .find(|seq| *seq > 0 && !is_synthetic_seq(*seq))
    }

    /// Fetch and merge the page of history older than what the log holds.
    ///
    /// The follow snapshot carries a window and a `hasMore`; the turns before it
    /// are `session/page`, paged backwards from that snapshot's cursor. Returns
    /// how many rows were added, which the page uses to keep the reader on the
    /// line they were reading (the prepend would otherwise push the view down by
    /// the same number of lines).
    pub async fn load_older(&mut self) -> Result<usize, String> {
        if !self.has_more || self.loading_older || self.session_id.is_none() {
            return Ok(0);
        }
        if let Some(last) = self.last_page_at {
            if last.elapsed() < PAGE_COOLDOWN {
                return Ok(0);
            }
        }
        let Some(before) = self.earliest_seq() else {
            return Ok(0);
        };
        if self.cursor <= 0 {
            // Without the follow snapshot's cut the host cannot align a page.
            return Ok(0);
        }
        let session_id = self.session_id.clone().unwrap_or_default();
        let args = page_args(&session_id, self.cursor, before, PAGE_MESSAGES);
        self.last_page_at = Some(std::time::Instant::now());
        self.loading_older = true;
        let outcome = self.call("session/page", args).await;
        self.loading_older = false;
        let page: SessionPage = serde_json::from_value(outcome?)
            .map_err(|e| format!("session/page: {e}"))?;
        self.has_more = page.has_more;
        log::info!(
            "dsh page: {} record(s) before seq {before}, has_more={}",
            page.records.len(),
            page.has_more
        );
        Ok(self.absorb_older(&page))
    }

    /// Merge a fetched page into the log: the older events are replayed into a
    /// log of their own and prepended (the GUI's `merge_history_older`), and
    /// their per-turn tables ride along so a turn that straddles the page
    /// boundary keeps its file list.
    fn absorb_older(&mut self, page: &SessionPage) -> usize {
        let mut older = ChatLog::default();
        for entry in &page.records {
            older.apply_event(&entry.event);
        }
        self.absorb_history(older)
    }

    /// [`Self::absorb_older`]'s merge half, without the wire events.
    fn absorb_history(&mut self, mut older: ChatLog) -> usize {
        if older.messages.is_empty() {
            return 0;
        }
        let before = self.log.messages.len();
        self.log.absorb_history_messages(&mut older);
        self.log.merge_side_tables(&older.side_tables());
        // History is final state, never a live draft.
        self.log.mark_all_done();
        self.log.messages.len().saturating_sub(before)
    }

    /// Answer the pending approval: the waterfall value IS the outcome string
    /// (`allowed-once` | `rejected`), not an object.
    pub async fn answer_approval(&mut self, allowed: bool) {
        let Some(Pending::Approval { event_id, .. }) = self.pending.take() else {
            return;
        };
        let value = json!(if allowed { "allowed-once" } else { "rejected" });
        if let Err(e) = self.answer_raw(&event_id, EventOutcome::result(value)) {
            self.status = Some(e);
        }
    }

    /// Submit the question in front of the user, moving to the next one; the
    /// last one answers the waterfall.
    pub async fn submit_question(&mut self) {
        let Some(Pending::Question(q)) = self.pending.as_mut() else {
            return;
        };
        // Fold the free-text buffer into the answer being built.
        if q.typing {
            let typed = q.input.trim().to_string();
            if let Some(slot) = q.custom.get_mut(q.index) {
                *slot = typed;
            }
            q.input.clear();
            q.typing = false;
        }
        if !q.current_answered() {
            return;
        }
        if q.index + 1 < q.items.len() {
            q.index += 1;
            return;
        }
        let Some(Pending::Question(q)) = self.pending.take() else {
            return;
        };
        let value = q.answer_value();
        if let Err(e) = self.answer_raw(&q.event_id, EventOutcome::result(value)) {
            self.status = Some(e);
        }
    }

    /// Cancel the pending question: reject it with the error the host's
    /// question service recognizes, so the agent resumes instead of waiting.
    pub async fn cancel_question(&mut self) {
        let Some(Pending::Question(q)) = self.pending.take() else {
            return;
        };
        let outcome = EventOutcome::Rejected {
            error: term_dsh_model::events::EventRejection {
                name: "UserQuestionError".to_string(),
                message: "the user cancelled the question".to_string(),
                code: Some("ASK_CANCELLED".to_string()),
                details: None,
            },
        };
        if let Err(e) = self.answer_raw(&q.event_id, outcome) {
            self.status = Some(e);
        }
    }

    /// Hand one waterfall answer to the daemon.
    fn answer_raw(&mut self, event_id: &str, outcome: EventOutcome) -> Result<(), String> {
        let Some(writer) = self.writer.clone() else {
            return Err("no daemon connection".to_string());
        };
        let outcome = serde_json::to_value(outcome).map_err(|e| e.to_string())?;
        let event_id = event_id.to_string();
        // Fire and forget: an answer is only useful to a host that is waiting,
        // and the frame loop must not stall on the relay.
        tokio::spawn(async move {
            match tokio::time::timeout(CALL_TIMEOUT, writer.dsh_answer(&event_id, outcome)).await {
                Ok(Ok(())) => log::info!("dsh waterfall {event_id} answered"),
                Ok(Err(e)) => log::warn!("dsh waterfall {event_id} answer failed: {e}"),
                Err(_) => log::warn!("dsh waterfall {event_id} answer timed out"),
            }
        });
        Ok(())
    }


    /// One relayed host RPC, returning the host's business value.
    async fn call(&mut self, method: &str, args: Value) -> Result<Value, String> {
        let Some(writer) = self.writer.clone() else {
            return Err("the dsh page has no daemon connection".to_string());
        };
        let outcome = match tokio::time::timeout(CALL_TIMEOUT, writer.dsh_call(method, args)).await
        {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(e)) => {
                if e.contains("license_required") {
                    let _ = self.fail_dsh(e.clone());
                    return Err(e);
                }
                self.status = Some(format!("{method}: {e}"));
                return Err(e);
            }
            Err(_) => {
                let e = format!("{method} timed out");
                self.status = Some(e.clone());
                return Err(e);
            }
        };
        match outcome {
            term_proto::DshCallOutcomeWire::Value(value) => Ok(value),
            term_proto::DshCallOutcomeWire::Rpc {
                code,
                message,
                details,
            } => {
                let e = match details {
                    Some(d) => format!("{method} failed: {code}: {message} ({d})"),
                    None => format!("{method} failed: {code}: {message}"),
                };
                self.status = Some(e.clone());
                Err(e)
            }
            // The daemon rebuilt the transport failure the direct path would
            // have produced; a frontend shows it as-is.
            term_proto::DshCallOutcomeWire::Unauthorized { body } => {
                let e = format!("{method}: the host refused the request as unauthenticated: {body}");
                self.status = Some(e.clone());
                Err(e)
            }
            term_proto::DshCallOutcomeWire::Http { status, body } => {
                let e = format!("{method}: the host answered HTTP {status}: {body}");
                self.status = Some(e.clone());
                Err(e)
            }
            term_proto::DshCallOutcomeWire::Protocol { message } => {
                let e = format!("{method}: unexpected answer from the host: {message}");
                self.status = Some(e.clone());
                Err(e)
            }
        }
    }


}

/// The `session/prompt` arguments, in the shape the host names them.
///
/// The daemon relays `args` to the host verbatim, so this is the wire contract
/// and nothing else: `{request: {requestId, sessionId, mode, content}}`, with a
/// text part tagged `type: "text"`. (The GUI reaches the same shape through
/// `easyterm_dsh`'s typed `session_prompt`; both must agree byte for byte.)
pub fn prompt_args(session_id: &str, request_id: &str, mode: PromptMode, text: &str) -> Value {
    json!({ "request": {
        "requestId": request_id,
        "sessionId": session_id,
        "mode": mode,
        "content": [PromptContentPart::Text { text: text.to_string() }],
    }})
}

/// The sessions the page may show itself, best first.
///
/// `refresh_sessions` sorts newest first, so this is a filter: a session with
/// content, that is not a **subagent child** (a child is addressed through its
/// parent — `SessionAddress::Subagent` — so following one as a plain session
/// fails), newest first.
pub fn landing_candidates(sessions: &[SessionSummary]) -> Vec<&str> {
    sessions
        .iter()
        .filter(|s| !s.blank && !is_subagent_child(s))
        .map(|s| s.session_id.as_str())
        .collect()
}

/// Whether a `session/list` row is a subagent child rather than a conversation.
fn is_subagent_child(s: &SessionSummary) -> bool {
    s.parent_session_id.is_some() || s.origin.as_deref() == Some("subagent")
}

/// Whether it is time to ask for the snapshot again.
fn reseed_due(last: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    match last {
        Some(at) => now.saturating_duration_since(at) >= RESEED_EVERY,
        None => true,
    }
}

/// The `session/page` arguments, in the shape the host names them.
///
/// Like [`prompt_args`], this is the wire contract: the daemon relays `args`
/// verbatim, so the key names are the whole interface.
pub fn page_args(session_id: &str, through_seq: i64, before_seq: i64, max_messages: u32) -> Value {
    json!({ "request": {
        "address": SessionAddress::session(session_id),
        "throughSeq": through_seq,
        "beforeSeq": before_seq,
        "maxMessages": max_messages,
    }})
}

/// The first eight characters of a session id, for a header.
pub fn short_id(id: &str) -> String {
    if id.chars().count() <= 8 {
        return id.to_string();
    }
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use term_dsh_model::chat::{ChatSideTables, QueuedItem, TurnEndReason};

    fn user(text: &str, seq: i64) -> ChatMessage {
        ChatMessage::User {
            text: text.to_string(),
            images: Vec::new(),
            seq,
            turn: Some(1),
            rpc_id: None,
            time: None,
        }
    }

    fn assistant(text: &str, seq: i64, done: bool) -> ChatMessage {
        ChatMessage::Assistant {
            text: text.to_string(),
            thinking: String::new(),
            images: Vec::new(),
            seq,
            turn: Some(1),
            step: Some(1),
            done,
        }
    }

    fn update(from_seq: Option<i64>, rows: Vec<ChatMessage>) -> ChatUpdateWire {
        ChatUpdateWire {
            key: "session:s1".to_string(),
            from_seq,
            rows,
            projections: HashMap::new(),
            side_tables: ChatSideTables::default(),
            queue: None,
        }
    }

    fn feed_with(rows: Vec<ChatMessage>) -> DshFeed {
        let mut feed = DshFeed::new();
        feed.key = Some("session:s1".to_string());
        feed.session_id = Some("s1".to_string());
        feed.log.messages = rows;
        feed
    }

    #[test]
    fn an_incremental_update_replaces_the_tail_and_keeps_the_history() {
        let mut feed = feed_with(vec![user("hello", 1), assistant("hi", 2, true)]);
        // The daemon rewrites from seq 2: the tail moves, the history stays.
        feed.apply_update(update(Some(2), vec![assistant("hi there", 2, true)]));
        let texts: Vec<String> = feed
            .rows()
            .iter()
            .map(|m| match m {
                ChatMessage::User { text, .. } => text.clone(),
                ChatMessage::Assistant { text, .. } => text.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(texts, vec!["hello".to_string(), "hi there".to_string()]);
    }

    #[test]
    fn a_full_rebuild_replaces_the_rows_but_keeps_older_pages() {
        // A rebuild from seq 10 must keep rows this page paged in earlier
        // (below the rebuild's first row) and drop the daemon's old tail.
        let mut feed = feed_with(vec![user("old page", 3), assistant("stale tail", 20, true)]);
        feed.apply_update(update(None, vec![assistant("fresh", 10, true)]));
        let seqs: Vec<i64> = feed.rows().iter().map(log_message_seq).collect();
        assert_eq!(seqs, vec![3, 10], "history below the rebuild's floor stays");
    }

    #[test]
    fn a_rebuild_with_no_rows_clears_the_tail() {
        let mut feed = feed_with(vec![user("a", 1), assistant("b", 2, true)]);
        feed.apply_update(update(None, Vec::new()));
        assert!(feed.rows().is_empty(), "nothing is stale-but-kept here");
    }

    #[test]
    fn a_live_draft_is_never_carried_over() {
        // Live drafts carry synthetic seqs minted per log. Keeping ours *and*
        // appending the daemon's would render one draft twice.
        let mut feed = feed_with(vec![user("q", 1), assistant("partial", i64::MAX / 4 + 7, false)]);
        feed.apply_update(update(Some(1), vec![assistant("partial", i64::MAX / 4 + 7, false)]));
        let drafts = feed
            .rows()
            .iter()
            .filter(|m| matches!(m, ChatMessage::Assistant { done: false, .. }))
            .count();
        assert_eq!(drafts, 1, "one draft, not two");
    }

    #[test]
    fn only_a_rebuild_finishes_a_streaming_row() {
        let mut feed = feed_with(vec![assistant("live", i64::MAX / 4 + 1, false)]);
        // An incremental update carries the live draft as it is: still running.
        feed.apply_update(update(Some(1), vec![assistant("live more", i64::MAX / 4 + 1, false)]));
        assert!(
            feed.rows().iter().any(|m| matches!(m, ChatMessage::Assistant { done: false, .. })),
            "the incremental row stays unfinished"
        );
        // A rebuild is final state: every row is done.
        feed.apply_update(update(None, vec![assistant("finished", 9, false)]));
        assert!(
            feed.rows().iter().all(|m| !matches!(m, ChatMessage::Assistant { done: false, .. })),
            "a rebuild finishes the drafts it carries"
        );
    }

    #[test]
    fn updates_carry_the_queue_and_the_projections() {
        let mut feed = feed_with(vec![]);
        let mut u = update(Some(0), vec![user("hi", 1)]);
        u.projections.insert("title".into(), json!("build the thing"));
        u.queue = Some(vec![QueuedItem {
            id: "q1".into(),
            placement: "queued".into(),
            preview: "next".into(),
            text: Some("next".into()),
            has_image: false,
        }]);
        feed.apply_update(u);
        assert_eq!(feed.title(), "build the thing");
        assert_eq!(feed.queue.len(), 1);
    }

    #[test]
    fn side_tables_ride_the_update_that_changes_their_rows() {
        let mut feed = feed_with(vec![]);
        let mut u = update(None, vec![user("hi", 1)]);
        u.side_tables.turn_end.insert(
            1,
            TurnEndReason {
                kind: "completed".into(),
                message: String::new(),
                code: None,
                seq: 3,
            },
        );
        feed.apply_update(u);
        assert!(
            feed.log.turn_end.contains_key(&1),
            "the per-turn tables ride the update that dirties their rows"
        );
    }

    #[test]
    fn the_header_falls_back_to_the_session_id() {
        let mut feed = feed_with(vec![]);
        feed.session_id = Some("0123456789abcdef".to_string());
        assert_eq!(feed.title(), "01234567");
        feed.projections.insert("title".into(), json!("  "));
        assert_eq!(feed.title(), "01234567", "a blank title is not a title");
        feed.projections.insert("title".into(), json!("real title"));
        assert_eq!(feed.title(), "real title");
    }

    #[test]
    fn the_prompt_arguments_are_the_shape_the_host_named() {
        let args = prompt_args("s1", "rpc-7", PromptMode::Queue, "hello");
        // The daemon relays this verbatim, so the wrapper is `request` — every
        // `session/*` verb but `session/list` names its parameter that way.
        let request = args.get("request").expect("wrapped in `request`");
        assert_eq!(request["requestId"], json!("rpc-7"));
        assert_eq!(request["sessionId"], json!("s1"));
        assert_eq!(request["mode"], json!("queue"));
        assert_eq!(request["content"], json!([{ "type": "text", "text": "hello" }]));
        // Steering is the same call with a different mode.
        let steer = prompt_args("s1", "rpc-8", PromptMode::Steer, "wait");
        assert_eq!(steer["request"]["mode"], json!("steer"));
    }

    #[test]
    fn a_question_value_matches_the_hosts_shape() {
        let request = json!({
            "questions": [
                {
                    "id": "q1",
                    "header": "Pick one",
                    "question": "Which branch?",
                    "options": [{ "label": "main" }, { "label": "dev" }],
                    "multiSelect": false
                }
            ]
        });
        let mut q = Questions::from_request("ev1", &request);
        assert_eq!(q.items.len(), 1);
        assert_eq!(q.current().map(|i| i.text.as_str()), Some("Which branch?"));
        assert!(!q.current_answered());
        q.pick(2);
        assert_eq!(q.picked[0], vec!["dev".to_string()]);
        assert!(q.current_answered());
        // A single-select pick is replaced, not accumulated.
        q.pick(1);
        assert_eq!(q.picked[0], vec!["main".to_string()]);
        assert_eq!(
            q.answer_value(),
            json!({ "answers": [{ "id": "q1", "selected": ["main"] }] })
        );
    }

    #[test]
    fn a_multi_select_question_accumulates_and_can_be_unpicked() {
        let request = json!({
            "questions": [{
                "id": "q2", "question": "Which files?", "multiSelect": true,
                "options": [{ "label": "a.rs" }, { "label": "b.rs" }]
            }]
        });
        let mut q = Questions::from_request("ev2", &request);
        q.pick(1);
        q.pick(2);
        assert_eq!(q.picked[0], vec!["a.rs".to_string(), "b.rs".to_string()]);
        q.pick(1);
        assert_eq!(q.picked[0], vec!["b.rs".to_string()], "a second press unpicks");
        // An out-of-range digit is ignored rather than panicking.
        q.pick(9);
        assert_eq!(q.picked[0], vec!["b.rs".to_string()]);
    }

    #[test]
    fn a_typed_answer_replaces_a_pick_unless_the_host_allows_both() {
        let single = json!({ "questions": [{
            "id": "q1", "question": "why?", "multiSelect": false,
            "options": [{ "label": "a" }]
        }]});
        let mut q = Questions::from_request("e", &single);
        q.pick(1);
        q.custom[0] = "because".to_string();
        assert_eq!(
            q.answer_value(),
            json!({ "answers": [{ "id": "q1", "selected": [], "custom": "because" }] })
        );

        let multi = json!({ "questions": [{
            "id": "q1", "question": "why?", "multiSelect": true,
            "options": [{ "label": "a" }]
        }]});
        let mut q = Questions::from_request("e", &multi);
        q.pick(1);
        q.custom[0] = "and also".to_string();
        assert_eq!(
            q.answer_value(),
            json!({ "answers": [{ "id": "q1", "selected": ["a"], "custom": "and also" }] })
        );
    }

    #[test]
    fn a_host_question_without_fields_does_not_break_the_overlay() {
        let q = Questions::from_request("e", &json!({}));
        assert!(q.items.is_empty());
        assert_eq!(q.answer_value(), json!({ "answers": [] }));
    }

    fn summary(id: &str, updated_at: i64, blank: bool) -> SessionSummary {
        SessionSummary {
            session_id: id.to_string(),
            updated_at,
            running: false,
            blank,
            agent_available: false,
            parent_session_id: None,
            projections: None,
            origin: None,
            cwd: None,
            format_status: None,
        }
    }

    #[test]
    fn the_page_lands_on_the_most_recent_session_with_content() {
        // `refresh_sessions` sorts newest first, so the first non-blank row is
        // the one a person means by "the current conversation".
        let sessions = vec![
            summary("newest-blank", 300, true),
            summary("newest-real", 200, false),
            summary("older", 100, false),
        ];
        assert_eq!(landing_candidates(&sessions), vec!["newest-real", "older"]);
        // Nothing to land on is a normal state, not an error.
        assert!(landing_candidates(&[]).is_empty());
        assert!(landing_candidates(&[summary("only-blank", 1, true)]).is_empty());
    }

    #[test]
    fn a_subagent_child_is_not_a_landing_candidate() {
        // The host lists subagent children among the sessions, and a child is
        // addressed through its parent: following one as a plain session fails
        // (measured against a real host: "could not open session-963a…").
        let mut child = summary("child", 500, false);
        child.parent_session_id = Some("parent".to_string());
        let mut spawned = summary("spawned", 400, false);
        spawned.origin = Some("subagent".to_string());
        let root = summary("root", 100, false);
        let rows = [child, spawned, root];
        let candidates = landing_candidates(&rows);
        assert_eq!(candidates, vec!["root"], "only a real conversation");
    }

    #[test]
    fn sessions_are_listed_only_once_the_daemon_says_it_is_connected() {
        // The daemon answers `connecting` first (it runs the attempt in the
        // background) and `connected` a moment later. A page that listed at
        // attach time did it against a dead connection and then never asked
        // again: it showed "connected" with no session and `send` failed.
        let mut feed = DshFeed::new();
        assert!(!feed.sessions_loaded);
        assert!(!feed.landing_done);
        // Attaching to a daemon that has no DSH endpoint leaves both undone, so
        // the upkeep will list and land as soon as a link exists.
        feed.link = Link::Failed("no endpoint".into());
        assert!(!feed.link.live());
        feed.sessions_loaded = false;
        feed.landing_done = false;
        assert!(!feed.sessions_loaded && !feed.landing_done);
    }

    #[test]
    fn an_unseeded_snapshot_is_re_read_rather_than_believed() {
        use std::time::{Duration, Instant};
        // The follow's baseline arrives after the follow: the first snapshot can
        // say cursor 0 / has_more false, which would hide real history. The page
        // re-reads, spaced out.
        let now = Instant::now();
        assert!(reseed_due(None, now), "the first check may go");
        assert!(!reseed_due(Some(now), now), "not immediately again");
        assert!(reseed_due(Some(now - RESEED_EVERY), now), "after the wait");
        assert!(
            !reseed_due(Some(now - Duration::from_millis(299)), now),
            "and only once the wait has really passed"
        );
    }

    #[test]
    fn the_page_arguments_are_the_shape_the_host_named() {
        let args = page_args("s1", 120, 40, 50);
        let request = args.get("request").expect("wrapped in `request`");
        assert_eq!(request["throughSeq"], json!(120), "the follow snapshot's cut");
        assert_eq!(request["beforeSeq"], json!(40), "the oldest row held");
        assert_eq!(request["maxMessages"], json!(50));
        assert_eq!(request["address"]["kind"], json!("session"));
        assert_eq!(request["address"]["sessionId"], json!("s1"));
    }

    #[tokio::test]
    async fn paging_declines_when_there_is_nothing_to_page() {
        // Each of these must answer without touching the network: the trigger is
        // "the reader is at the top", which fires repeatedly.
        let mut feed = feed_with(vec![user("hello", 10)]);
        feed.session_id = Some("s1".into());
        feed.cursor = 100;
        // has_more is false by default: the host says there is nothing older.
        assert_eq!(feed.load_older().await, Ok(0));

        feed.has_more = true;
        feed.cursor = -1; // no snapshot cut yet: the host cannot align a page
        assert_eq!(feed.load_older().await, Ok(0));

        feed.cursor = 100;
        feed.log.messages.clear(); // no rows: nothing to page before
        assert_eq!(feed.load_older().await, Ok(0));

        // A page in flight or a fresh page is not fetched again.
        feed.log.messages = vec![user("hello", 10)];
        feed.loading_older = true;
        assert_eq!(feed.load_older().await, Ok(0));
        feed.loading_older = false;
        feed.last_page_at = Some(std::time::Instant::now());
        assert_eq!(feed.load_older().await, Ok(0));

        // Past the cooldown with everything in place it *tries*: with no daemon
        // connection that is an honest error, not a silent nothing.
        feed.last_page_at = None;
        assert!(feed.load_older().await.is_err());
    }

    #[test]
    fn a_fetched_page_is_prepended_and_keeps_its_tables() {
        let mut feed = feed_with(vec![user("newer", 100)]);
        feed.session_id = Some("s1".into());
        let mut older = ChatLog::default();
        older.messages = vec![user("older one", 10), assistant("older two", 20, true)];
        older.merge_side_tables(&ChatSideTables {
            produced: std::collections::HashMap::from([(
                1,
                vec![(20, "src/old.rs".to_string())],
            )]),
            ..Default::default()
        });
        let added = feed.absorb_history(older);
        assert_eq!(added, 2);
        let seqs: Vec<i64> = feed.rows().iter().map(log_message_seq).collect();
        assert_eq!(seqs, vec![10, 20, 100], "older first, order kept");
        assert!(
            feed.log.produced.contains_key(&1),
            "the page's per-turn tables came with it"
        );
        assert!(
            feed.rows().iter().all(|m| !matches!(m, ChatMessage::Assistant { done: false, .. })),
            "fetched history is final state, never a live draft"
        );
        // An empty page adds nothing (and must not look like progress).
        assert_eq!(feed.absorb_history(ChatLog::default()), 0);
    }

    #[test]
    fn an_approval_waterfall_becomes_a_pending_approval() {
        let mut feed = DshFeed::new();
        feed.key = Some("session:s1".into());
        feed.apply_hub(HubEvent::Waterfall {
            event: "approval/request".into(),
            event_id: "ev9".into(),
            agent_id: "s1".into(),
            request: json!({ "toolName": "shell", "reason": "writes a file" }),
        });
        match feed.pending {
            Some(Pending::Approval { event_id, tool, reason }) => {
                assert_eq!(event_id, "ev9");
                assert_eq!(tool, "shell");
                assert_eq!(reason.as_deref(), Some("writes a file"));
            }
            other => panic!("expected a pending approval, got {other:?}"),
        }
    }

    #[test]
    fn a_cancelled_waterfall_stops_being_pending() {
        let mut feed = DshFeed::new();
        feed.pending = Some(Pending::Approval {
            event_id: "ev1".into(),
            tool: "shell".into(),
            reason: None,
        });
        feed.apply_hub(HubEvent::WaterfallCancel {
            event_id: "ev2".into(),
        });
        assert!(feed.pending.is_some(), "a different waterfall's cancel");
        feed.apply_hub(HubEvent::WaterfallCancel {
            event_id: "ev1".into(),
        });
        assert!(feed.pending.is_none(), "the host gave up on ours");
    }

    #[test]
    fn the_link_describes_what_the_daemon_says() {
        let mut feed = DshFeed::new();
        feed.apply_state(&term_proto::DshStateWire {
            revision: 1,
            state: "connecting".into(),
            port: 1,
            error_kind: None,
            error_detail: None,
            local_sidecar: false,
        });
        assert_eq!(feed.link, Link::Connecting);
        feed.apply_state(&term_proto::DshStateWire {
            revision: 2,
            state: "connected".into(),
            port: 1,
            error_kind: None,
            error_detail: None,
            local_sidecar: true,
        });
        assert!(feed.link.live());
        feed.apply_state(&term_proto::DshStateWire {
            revision: 3,
            state: "unreachable".into(),
            port: 1,
            error_kind: Some("connect_failed".into()),
            error_detail: Some("nothing listening on 127.0.0.1:1".into()),
            local_sidecar: false,
        });
        assert_eq!(feed.link.label(), "unreachable");
        assert!(feed.status.as_deref().unwrap_or_default().contains("nothing listening"));
    }

    #[test]
    fn license_rejection_closes_the_page_and_discards_the_old_conversation() {
        let mut feed = feed_with(vec![user("private conversation", 1)]);
        feed.link = Link::Connected;
        feed.key = Some("session:old".into());
        feed.session_id = Some("old".into());
        feed.sessions = vec![summary("old", 1, false)];
        feed.projections.insert("title".into(), json!("private title"));
        feed.pending = Some(Pending::Approval {
            event_id: "old-approval".into(), tool: "shell".into(), reason: None,
        });
        feed.running = true;
        feed.hub_opened = true;
        assert!(feed.fail_dsh("license_required: dsh requires Pro or above".into()).is_err());
        assert_eq!(feed.link, Link::NeedsLicense);
        assert!(feed.rows().is_empty());
        assert!(feed.sessions.is_empty());
        assert!(feed.projections.is_empty());
        assert!(feed.key.is_none() && feed.session_id.is_none());
        assert!(feed.pending.is_none());
        assert!(!feed.running && !feed.hub_opened);
        assert!(feed.status.as_deref().unwrap().contains("Pro or above"));
        assert!(feed.take_close_requested());
        assert!(!feed.take_close_requested());
    }

    #[test]
    fn switching_paid_accounts_closes_the_page_but_a_plan_or_token_refresh_does_not() {
        let mut feed = feed_with(vec![user("old account", 1)]);
        let mut state = term_proto::AccountState {
            base_url: "https://accounts.example".into(),
            uid: Some(1),
            logged_in: true,
            login_session_id: Some("login-1".into()),
            effective_plan: "pro".into(),
            ..Default::default()
        };
        assert!(!feed.account_changed(AccountIdentity::from(&state)));
        state.effective_plan = "team".into();
        state.token_exp = Some(2_000_000_000);
        assert!(!feed.account_changed(AccountIdentity::from(&state)));
        assert!(!feed.take_close_requested());
        state.uid = Some(2);
        assert!(feed.account_changed(AccountIdentity::from(&state)));
        assert!(feed.rows().is_empty());
        assert!(feed.take_close_requested());
        assert!(feed.status.as_deref().unwrap().contains("account changed"));
    }

    #[test]
    fn daemon_account_and_license_changes_close_the_page() {
        for kind in ["account_changed", "license_required"] {
            let mut feed = feed_with(vec![user("private conversation", 1)]);
            feed.link = Link::Connected;
            feed.apply_state(&term_proto::DshStateWire {
                revision: 1,
                state: "disconnected".into(), port: 0,
                error_kind: Some(kind.into()), error_detail: None, local_sidecar: false,
            });
            assert!(feed.take_close_requested(), "{kind}");
            assert!(feed.rows().is_empty(), "{kind}");
            assert!(!feed.link.live(), "{kind}");
        }
    }

    #[test]
    fn an_older_queued_revocation_does_not_close_a_newer_connected_page() {
        let mut feed = feed_with(vec![user("current account", 1)]);
        feed.apply_state(&term_proto::DshStateWire {
            revision: 3,
            state: "connected".into(), port: 1,
            error_kind: None, error_detail: None, local_sidecar: false,
        });
        feed.apply_state(&term_proto::DshStateWire {
            revision: 2,
            state: "disconnected".into(), port: 0,
            error_kind: Some("license_required".into()), error_detail: None, local_sidecar: false,
        });
        assert!(feed.link.live());
        assert!(!feed.take_close_requested());
        assert_eq!(feed.rows().len(), 1);

        feed.apply_state(&term_proto::DshStateWire {
            revision: 4,
            state: "disconnected".into(), port: 0,
            error_kind: Some("license_required".into()), error_detail: None, local_sidecar: false,
        });
        assert!(!feed.link.live());
        assert!(feed.rows().is_empty());
        assert_eq!(feed.state_revision, 0, "a closed page discards its connection baseline");
        assert!(feed.take_close_requested());
    }

    #[test]
    fn an_update_for_another_session_is_ignored() {
        let mut feed = feed_with(vec![user("mine", 1)]);
        let mut other = update(Some(0), vec![user("theirs", 1)]);
        other.key = "session:other".to_string();
        // `pump` filters by key; `apply_update` is only called for ours, so the
        // guard the frame loop relies on is this comparison.
        assert_ne!(feed.key.as_deref(), Some(other.key.as_str()));
        feed.apply_update(other);
        assert_eq!(feed.rows().len(), 1, "apply_update itself is key-agnostic");
    }
}
