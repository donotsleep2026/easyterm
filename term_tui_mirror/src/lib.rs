//! A transport and viewport, with every actual session operation owned by the
//! separately distributed daemon. No terminal emulator or SSH backend is linked.

use std::sync::Arc;

use term_client::{Client, Writer};
use term_proto::{ClientMethod, CoreEvent, OpenSpec, ServerEvent, ServerOk, ServerResult};
use term_view::{BackendCommand, RenderableContent, TerminalView, ViewOutput};
use tokio::sync::mpsc;

pub use term_client::ask;
use ask::{AskHost, AskPump};

/// Compatibility module for callers of the former combined frontend mirror.
pub mod mirror {
    pub use crate::{MirrorSession, MirrorSessionParams};
}

pub struct MirrorSessionParams {
    pub spec: OpenSpec,
    pub cols: u16,
    pub rows: u16,
    pub ask_host: Option<Arc<dyn AskHost>>,
    pub socket_token: Option<String>,
}

impl MirrorSessionParams {
    pub fn basic(spec: OpenSpec, cols: u16, rows: u16) -> Self {
        Self { spec, cols, rows, ask_host: None, socket_token: None }
    }

    pub fn with_ask_host(spec: OpenSpec, cols: u16, rows: u16, ask_host: Arc<dyn AskHost>) -> Self {
        Self { ask_host: Some(ask_host), ..Self::basic(spec, cols, rows) }
    }

    pub fn with_socket_token(mut self, token: Option<String>) -> Self {
        self.socket_token = token;
        self
    }
}

pub struct MirrorSession {
    writer: Writer,
    events: mpsc::UnboundedReceiver<ServerEvent>,
    ask_pump: AskPump,
    backend: TerminalView,
    sid: u64,
}

impl MirrorSession {
    pub async fn open<A: tokio::net::ToSocketAddrs>(addr: A, params: MirrorSessionParams) -> Result<Self, String> {
        let client = Client::connect(addr).await.map_err(|error| error.to_string())?;
        if let Some(token) = &params.socket_token {
            client.auth(token).await?;
        }
        let (writer, events, asks) = client.into_parts();
        // The daemon may ask for SSH credentials before replying to SessionOpen.
        let ask_pump = AskPump::spawn(asks, writer.clone(), params.ask_host);
        let sid = match writer.open_session(params.spec).await {
            Ok(sid) => sid,
            Err(error) => { ask_pump.abort(); return Err(format!("open session: {error}")); }
        };
        Ok(Self { writer, events, ask_pump, backend: TerminalView::new(params.cols, params.rows), sid })
    }

    pub fn sid(&self) -> u64 { self.sid }
    pub fn writer(&self) -> &Writer { &self.writer }
    pub fn backend(&mut self) -> &mut TerminalView { &mut self.backend }
    pub fn refresh(&mut self) -> &RenderableContent { self.backend.sync() }

    pub async fn forward_output(&mut self) -> Result<(), String> {
        while let Some(output) = self.backend.next_output() {
            match output {
                ViewOutput::Bytes(bytes) => self.writer.write(self.sid, bytes).await?,
                ViewOutput::HistoryScroll(step) => {
                    match self.writer.call(ClientMethod::SessionHistoryScroll { sid: self.sid, step }).await? {
                        ServerResult::Ok(ServerOk::Ack) => {}
                        ServerResult::Err(error) => return Err(format!("{}: {}", error.code, error.message)),
                        other => return Err(format!("unexpected history scroll reply: {other:?}")),
                    }
                }
                ViewOutput::Scroll { delta, modifiers, point } => {
                    let modifiers = (modifiers.shift as u8) | (modifiers.ctrl as u8) << 1
                        | (modifiers.alt as u8) << 2 | (modifiers.logo as u8) << 3;
                    match self.writer.call(ClientMethod::SessionScroll {
                        sid: self.sid, delta, column: point.column.0 as u32, line: point.line.0, modifiers,
                    }).await? {
                        ServerResult::Ok(ServerOk::Ack) => {}
                        ServerResult::Err(error) => return Err(format!("{}: {}", error.code, error.message)),
                        other => return Err(format!("unexpected scroll reply: {other:?}")),
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn next_event(&mut self) -> Option<ServerEvent> { self.events.recv().await }
    pub fn next_event_now(&mut self) -> Option<ServerEvent> { self.events.try_recv().ok() }
    /// A lost daemon may close the transport without sending a session Exit.
    /// Drain buffered events first so the frontend still applies the last frame
    /// and the daemon's exit policy before deciding how to leave the session.
    pub fn is_disconnected(&self) -> bool { self.events.is_closed() && self.events.is_empty() }

    pub fn apply_event(&mut self, event: &ServerEvent) {
        if event.sid != self.sid { return; }
        match &event.event {
            CoreEvent::Frame(frame) => self.backend.apply_frame(frame),
            CoreEvent::Title { title } => self.backend.on_title(title.clone()),
            CoreEvent::ResetTitle => self.backend.on_reset_title(),
            CoreEvent::Bell => self.backend.on_bell(),
            CoreEvent::Progress { state, progress } => self.backend.on_progress(*state, *progress),
            CoreEvent::Exit { code, reason } => {
                self.backend.set_exit_code(*code);
                self.backend.set_exit_reason(reason.clone());
            }
            _ => {}
        }
    }

    pub fn inject_key(&mut self, bytes: Vec<u8>) { self.backend.process_command(BackendCommand::Write(bytes)); }
    pub async fn write_remote(&mut self, bytes: Vec<u8>) -> Result<(), String> { self.writer.write(self.sid, bytes).await }

    pub async fn resize_all(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        let (cols, rows) = (cols.max(1), rows.max(1));
        self.writer.resize(self.sid, cols, rows).await?;
        self.backend.resize(cols, rows);
        Ok(())
    }

    pub async fn close(self) -> Result<(), String> {
        self.ask_pump.abort();
        self.writer.close(self.sid).await
    }
}

impl Drop for MirrorSession {
    fn drop(&mut self) { self.ask_pump.abort(); }
}
