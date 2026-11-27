//! Client-side host-question corridor (design doc §2.1 ② / B2 foundation).
//!
//! When a frontend opens an SSH session, the daemon raises host questions
//! (host-key confirmation, kbdint/passphrase) as `ServerAsk`s and waits for
//! answers before the open resolves. A frontend that opens SSH must therefore
//! *answer* those asks concurrently with awaiting the open reply — otherwise
//! `open_session` would block forever.
//!
//! This module provides the UI-free half of that contract, shared by the GUI
//! and TUI frontends:
//!
//! - [`AskHost`]: a frontend-provided object that turns one `ServerAsk` into
//!   an [`AnswerResult`]. The egui GUI shows a dialog and awaits the user's
//!   click; the ratatui TUI draws a prompt; tests return a canned answer.
//! - [`AskPump`]: the background task that owns the client's exclusive ask
//!   receiver, routes every incoming `ServerAsk` through the `AskHost`, and
//!   sends the answer back over the shared writer.
//! - [`AskBridge`]: an [`AskHost`] implementation that forwards each question
//!   to the frontend's foreground loop over a channel (with a reply oneshot).
//!   [`PendingAsk`] holds one forwarded question on the foreground side and is
//!   resolved by the user's decision (accept/reject host key, type + submit a
//!   kbdint answer, cancel); [`host_key_decision`] / [`input_display`] are the
//!   pure key-to-answer and echo-mask helpers.
//!
//! When the caller supplies no `AskHost`, the pump answers `Cancel` to every
//! question (a shell session never raises one; an SSH open without a host
//! fails cleanly instead of hanging).

use std::sync::Arc;
use std::collections::HashMap;

use log::warn;
use tokio::sync::{mpsc, oneshot, watch};

use crate::Writer;
use term_proto::{AnswerResult, AskKind, ServerAsk};

/// Reader-side lifetimes preserve ordering across the separate ask/event
/// queues. Only ZMODEM path questions belong to a transfer's lifetime.
#[derive(Default)]
pub(crate) struct AskLifetimes {
    pending: std::sync::Mutex<HashMap<u64, (term_proto::Sid, watch::Sender<bool>)>>,
}

impl AskLifetimes {
    pub(crate) fn register(&self, ask: &ServerAsk) {
        if let AskKind::ZmodemPath(path) = &ask.kind {
            let (signal, _) = watch::channel(false);
            self.pending.lock().unwrap().insert(ask.ask_id, (path.sid, signal));
        }
    }

    fn listen(&self, ask_id: u64) -> Option<watch::Receiver<bool>> {
        self.pending.lock().unwrap().get(&ask_id).map(|(_, signal)| signal.subscribe())
    }

    pub(crate) fn cancel_session(&self, sid: term_proto::Sid) {
        self.pending.lock().unwrap().retain(|_, (owner, signal)| {
            if *owner == sid { signal.send_replace(true); false } else { true }
        });
    }

    pub(crate) fn finish(&self, ask_id: u64) { self.pending.lock().unwrap().remove(&ask_id); }
    pub(crate) fn close(&self) { self.pending.lock().unwrap().clear(); }
}

/// Turns one daemon host question into the answer to send back. Implemented by
/// the frontend (GUI dialog / TUI prompt / scripted test host).
#[async_trait::async_trait]
pub trait AskHost: Send + Sync + 'static {
    async fn on_ask(&self, ask: ServerAsk) -> AnswerResult;
}

/// Default host: answer `Cancel` to everything (safe for shell-only callers
/// that open SSH specs by mistake — a clean failure, never a hang).
struct CancelAll;

#[async_trait::async_trait]
impl AskHost for CancelAll {
    async fn on_ask(&self, _ask: ServerAsk) -> AnswerResult {
        AnswerResult::Cancel
    }
}

/// Background task draining a client's ask receiver through an [`AskHost`]
/// and answering over the shared writer. `abort()` stops it; dropping the
/// writer the pump clones closes the connection, ending the receiver.
pub struct AskPump {
    handle: tokio::task::JoinHandle<()>,
}

impl AskPump {
    /// Spawn the pump. `host: None` ⇒ every question is cancelled.
    pub fn spawn(
        mut asks: mpsc::UnboundedReceiver<ServerAsk>,
        writer: Writer,
        host: Option<Arc<dyn AskHost>>,
    ) -> Self {
        let host: Arc<dyn AskHost> = host.unwrap_or_else(|| Arc::new(CancelAll));
        let handle = tokio::spawn(async move {
            while let Some(ask) = asks.recv().await {
                let answer = if matches!(&ask.kind, AskKind::ZmodemPath(_)) {
                    let Some(mut lifetime) = writer.ask_lifetimes.listen(ask.ask_id) else {
                        // The terminal event was decoded before this queued ask
                        // could be dispatched. Do not flash an expired picker.
                        continue;
                    };
                    tokio::select! {
                        biased;
                        _ = lifetime.changed() => continue,
                        answer = host.on_ask(ask.clone()) => answer,
                    }
                } else {
                    host.on_ask(ask.clone()).await
                };
                if let Err(e) = writer.answer(ask.ask_id, answer).await {
                    warn!("answer ask {} failed: {e}", ask.ask_id);
                    break;
                }
            }
        });
        Self { handle }
    }

    /// Stop the pump (also implicit when its writer/session is dropped).
    pub fn abort(&self) {
        self.handle.abort();
    }
}

impl Drop for AskPump {
    fn drop(&mut self) {
        // A frontend can cancel SessionOpen while the daemon is awaiting an
        // answer. A detached pump would otherwise retain its socket writer.
        self.handle.abort();
    }
}

// ── Foreground-loop bridge (shared by the GUI dialog queue and the TUI's
//    interactive overlay) ────────────────────────────────────────────────

/// One question forwarded to the foreground loop: the question itself plus the
/// oneshot through which the user's answer travels back to the ask pump.
pub type BridgeAsk = (ServerAsk, oneshot::Sender<AnswerResult>);

/// Background side of the bridge: implements [`AskHost`] (runs inside the
/// daemon's ask pump task) and posts questions to the foreground loop.
pub struct AskBridge {
    tx: mpsc::UnboundedSender<BridgeAsk>,
    waker: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl AskBridge {
    /// Build the bridge halves: the background `AskBridge` and the foreground
    /// receiver the UI loop drains (GUI dialog queue / TUI overlay).
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<BridgeAsk>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                waker: None,
            },
            rx,
        )
    }

    /// Attach a callback invoked right after a question is queued onto `tx`
    /// (e.g. `egui_ctx.request_repaint()`), so an idle UI loop wakes up
    /// immediately to render the modal dialog.
    pub fn with_waker(mut self, waker: impl Fn() + Send + Sync + 'static) -> Self {
        self.waker = Some(std::sync::Arc::new(waker));
        self
    }
}

#[async_trait::async_trait]
impl AskHost for AskBridge {
    async fn on_ask(&self, ask: ServerAsk) -> AnswerResult {
        let (tx, rx) = oneshot::channel();
        if self.tx.send((ask, tx)).is_err() {
            return AnswerResult::Cancel; // foreground loop gone
        }
        if let Some(waker) = &self.waker {
            waker();
        }
        match rx.await {
            Ok(answer) => answer,
            Err(_) => AnswerResult::Cancel, // overlay/dialog dropped
        }
    }
}

/// A host question awaiting the user's decision in the foreground loop.
pub struct PendingAsk {
    pub ask: ServerAsk,
    /// Buffer for the kbdint answer being typed (echo masked when hidden).
    pub input: String,
    reply: Option<oneshot::Sender<AnswerResult>>,
    /// Set once the answer was sent; further key input is ignored.
    resolved: bool,
}

impl PendingAsk {
    pub fn new(ask: ServerAsk, reply: oneshot::Sender<AnswerResult>) -> Self {
        Self {
            ask,
            input: String::new(),
            reply: Some(reply),
            resolved: false,
        }
    }

    /// True for the (single) kbdint prompt where input must be echo-hidden.
    pub fn echo_hidden(&self) -> bool {
        match &self.ask.kind {
            AskKind::KbdInteractive(k) => k.prompts.first().map(|p| !p.1).unwrap_or(false),
            _ => false,
        }
    }

    /// True while the user is still being asked (answer not yet sent).
    pub fn is_pending(&self) -> bool {
        !self.resolved && self.reply.as_ref().is_some_and(|reply| !reply.is_closed())
    }

    /// Answer with the typed kbdint input (Enter / dialog OK).
    pub fn submit(&mut self) {
        self.resolve(AnswerResult::Submit(vec![self.input.clone()]));
    }

    /// Cancel the question (Esc / dialog cancel).
    pub fn cancel(&mut self) {
        self.resolve(AnswerResult::Cancel);
    }

    /// Accept an offered host key ('y').
    pub fn accept_host_key(&mut self) {
        self.resolve(AnswerResult::AcceptHostKey);
    }

    /// Reject an offered host key ('n').
    pub fn reject_host_key(&mut self) {
        self.resolve(AnswerResult::RejectHostKey);
    }

    /// Resolve a daemon ZMODEM path prompt. Paths name files on the daemon's
    /// machine; the frontend chooses them but never performs the transfer.
    pub fn submit_paths(&mut self, paths: Vec<String>) {
        self.resolve(AnswerResult::Submit(paths));
    }

    fn resolve(&mut self, result: AnswerResult) {
        if self.resolved {
            return;
        }
        self.resolved = true;
        if let Some(tx) = self.reply.take() {
            let _ = tx.send(result);
        }
    }
}

/// Map one host-key decision key to an answer. Pure: unit-testable.
pub fn host_key_decision(key: char) -> Option<AnswerResult> {
    match key.to_ascii_lowercase() {
        'y' => Some(AnswerResult::AcceptHostKey),
        'n' => Some(AnswerResult::RejectHostKey),
        _ => None,
    }
}

/// Render typed input for display: mask when hidden, else plain.
pub fn input_display(input: &str, hidden: bool) -> String {
    if hidden {
        "•".repeat(input.chars().count())
    } else {
        input.to_string()
    }
}

/// A high-level decision the user made on a forwarded question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    AcceptHostKey,
    RejectHostKey,
    Submit(String),
    Cancel,
}

/// Resolve a forwarded [`PendingAsk`] from a high-level [`Decision`]. Shared
/// by the frontend overlays/dialogs and the tests, so every path resolves a
/// question exactly once.
pub fn answer_bridge_ask(ask: &mut PendingAsk, decision: Decision) {
    match decision {
        Decision::AcceptHostKey => ask.accept_host_key(),
        Decision::RejectHostKey => ask.reject_host_key(),
        Decision::Submit(input) => {
            ask.input = input;
            ask.submit();
        }
        Decision::Cancel => ask.cancel(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn protocol_pair() -> (tokio::net::TcpStream, Writer,
        mpsc::UnboundedReceiver<term_proto::ServerEvent>, mpsc::UnboundedReceiver<ServerAsk>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (server, client) = tokio::join!(listener.accept(), tokio::net::TcpStream::connect(listener.local_addr().unwrap()));
        let (writer, events, asks) = crate::Client::from_stream(client.unwrap()).into_parts();
        (server.unwrap().0, writer, events, asks)
    }

    fn path_ask(ask_id: u64, sid: u64) -> ServerAsk {
        ServerAsk { ask_id, sid: Some(sid), kind: AskKind::ZmodemPath(term_proto::ZmodemPathAsk {
            sid, session_label: "test".into(), save: false, name: None, size: None,
        }) }
    }

    async fn send_ask(socket: &mut tokio::net::TcpStream, ask: &ServerAsk) {
        socket.write_all(&term_proto::encode_frame(term_proto::CoreFrameType::ServerAsk,
            &rmp_serde::to_vec_named(ask).unwrap())).await.unwrap();
    }

    async fn send_end(socket: &mut tokio::net::TcpStream, sid: u64, cancelled: bool) {
        let event = term_proto::ServerEvent { sid, event: term_proto::CoreEvent::Zmodem(
            if cancelled { term_proto::ZmodemEventWire::Cancelled { sid } }
            else { term_proto::ZmodemEventWire::Ended { sid } }) };
        socket.write_all(&term_proto::encode_frame(term_proto::CoreFrameType::ServerEvent,
            &rmp_serde::to_vec_named(&event).unwrap())).await.unwrap();
    }

    async fn read_answer(socket: &mut tokio::net::TcpStream) -> term_proto::ClientAnswer {
        let len = socket.read_u32_le().await.unwrap() as usize;
        let mut payload = vec![0; len];
        socket.read_exact(&mut payload).await.unwrap();
        assert_eq!(payload[0], term_proto::CoreFrameType::ClientAnswer as u8);
        rmp_serde::from_slice(&payload[1..]).unwrap()
    }

    #[tokio::test]
    async fn expired_queued_path_asks_are_skipped_before_the_host_runs() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let (mut socket, writer, mut events, asks) = protocol_pair().await;
            send_ask(&mut socket, &path_ask(10, 7)).await;
            send_end(&mut socket, 7, false).await;
            send_ask(&mut socket, &path_ask(11, 7)).await;
            // Hold back AskPump until the event queue has already seen Ended.
            events.recv().await.unwrap();
            let (bridge, mut foreground) = AskBridge::channel();
            let _pump = AskPump::spawn(asks, writer, Some(Arc::new(bridge)));
            let (ask, reply) = foreground.recv().await.unwrap();
            assert_eq!(ask.ask_id, 11, "an old queued ask must never flash a picker or cancel the next one");
            let mut pending = PendingAsk::new(ask, reply);
            assert!(pending.is_pending());
            pending.submit_paths(vec!["/daemon/new.txt".into()]);
            let answer = read_answer(&mut socket).await;
            assert_eq!(answer.ask_id, 11, "no stale answer may be emitted for ask 10");
        }).await.expect("queued expiry should not await user input");
    }

    #[tokio::test]
    async fn cancellation_closes_the_active_path_bridge_without_blocking_the_next_ask() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let (mut socket, writer, mut events, asks) = protocol_pair().await;
            let (bridge, mut foreground) = AskBridge::channel();
            let _pump = AskPump::spawn(asks, writer, Some(Arc::new(bridge)));
            send_ask(&mut socket, &path_ask(20, 7)).await;
            let (ask, reply) = foreground.recv().await.unwrap();
            let mut pending = PendingAsk::new(ask, reply);
            assert!(pending.is_pending());
            send_end(&mut socket, 7, true).await;
            events.recv().await.unwrap();
            pending.reply.as_mut().unwrap().closed().await;
            assert!(!pending.is_pending(), "the foreground sees expiry even before consuming its event queue");
            send_ask(&mut socket, &path_ask(21, 7)).await;
            let (ask, reply) = foreground.recv().await.unwrap();
            let mut next = PendingAsk::new(ask, reply);
            next.submit_paths(vec!["/daemon/next.txt".into()]);
            assert_eq!(read_answer(&mut socket).await.ask_id, 21);
        }).await.expect("cancellation must release the active host future");
    }

    #[tokio::test]
    async fn transfer_end_preserves_auth_prompts_and_other_sessions_path_questions() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut host_key = host_key_ask();
            host_key.sid = Some(7);
            let mut keyboard = kbdint_ask(true);
            keyboard.sid = Some(7);
            for ask in [host_key, keyboard, path_ask(30, 8)] {
                let (mut socket, writer, mut events, asks) = protocol_pair().await;
                let (bridge, mut foreground) = AskBridge::channel();
                let _pump = AskPump::spawn(asks, writer, Some(Arc::new(bridge)));
                send_ask(&mut socket, &ask).await;
                let (forwarded, reply) = foreground.recv().await.unwrap();
                let mut pending = PendingAsk::new(forwarded, reply);
                send_end(&mut socket, 7, true).await;
                events.recv().await.unwrap();
                tokio::task::yield_now().await;
                assert!(pending.is_pending(), "transfer 7 must not retire this unrelated question");
                pending.cancel();
                assert_eq!(read_answer(&mut socket).await.ask_id, ask.ask_id);
            }
        }).await.expect("ordinary SSH authentication must remain answerable");
    }

    fn host_key_ask() -> ServerAsk {
        ServerAsk {
            ask_id: 1,
            sid: None,
            kind: AskKind::HostKeyConfirm(term_proto::HostKeyAsk {
                host: "example.com".into(),
                key_blob: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI...".into(),
                changed: false,
            }),
        }
    }

    fn kbdint_ask(hidden: bool) -> ServerAsk {
        ServerAsk {
            ask_id: 2,
            sid: None,
            kind: AskKind::KbdInteractive(term_proto::KbdInteractiveAsk {
                session_label: "s".into(),
                name: "pam".into(),
                instructions: "enter credentials".into(),
                prompts: vec![("Password:".into(), hidden, None)],
                kind: term_proto::KbdKind::ServerChallenge,
            }),
        }
    }

    #[test]
    fn host_key_decisions_map_to_answers() {
        assert_eq!(host_key_decision('y'), Some(AnswerResult::AcceptHostKey));
        assert_eq!(host_key_decision('Y'), Some(AnswerResult::AcceptHostKey));
        assert_eq!(host_key_decision('n'), Some(AnswerResult::RejectHostKey));
        assert_eq!(host_key_decision('x'), None);
    }

    #[test]
    fn input_display_masks_hidden_prompts() {
        assert_eq!(input_display("s3cret", true), "••••••");
        assert_eq!(input_display("hi", false), "hi");
    }

    #[test]
    fn pending_ask_sends_answer_once() {
        let (tx, rx) = oneshot::channel();
        let mut p = PendingAsk::new(host_key_ask(), tx);
        p.accept_host_key();
        p.cancel(); // no-op after resolve
        assert!(!p.is_pending());
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert_eq!(rt.block_on(rx).ok(), Some(AnswerResult::AcceptHostKey));
    }

    #[test]
    fn pending_ask_submits_typed_input() {
        let (tx, rx) = oneshot::channel();
        let mut p = PendingAsk::new(kbdint_ask(true), tx);
        p.input.push_str("pw123");
        p.submit();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert_eq!(
            rt.block_on(rx).ok(),
            Some(AnswerResult::Submit(vec!["pw123".into()]))
        );
    }

    #[test]
    fn bridge_forwards_and_answers() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let (bridge, mut rx) = AskBridge::channel();
            let host: Arc<dyn AskHost> = Arc::new(bridge);
            let ask = host_key_ask();

            let pump = {
                let host2 = host.clone();
                let ask2 = ask.clone();
                tokio::spawn(async move { host2.on_ask(ask2).await })
            };
            let (forwarded_ask, reply) = rx.recv().await.expect("bridge forwards");
            assert_eq!(forwarded_ask.ask_id, ask.ask_id);
            let mut p = PendingAsk::new(forwarded_ask, reply);
            answer_bridge_ask(&mut p, Decision::AcceptHostKey);
            assert_eq!(pump.await.ok(), Some(AnswerResult::AcceptHostKey));
        });
    }

    #[test]
    fn bridge_cancels_when_foreground_gone() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let (bridge, rx) = AskBridge::channel();
            drop(rx); // foreground loop gone
            let host: Arc<dyn AskHost> = Arc::new(bridge);
            let answer = host.on_ask(host_key_ask()).await;
            assert_eq!(answer, AnswerResult::Cancel);
        });
    }
}
