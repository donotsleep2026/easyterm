//! Public contract tests: a protocol peer, never the private daemon or license code.
use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::term::cell::Flags;
use ratatui::{buffer::Buffer, layout::Rect};
use term_client::ask::AskHost;
use term_discovery::connection_auth::{proof, verify_proof, ProofRole};
use term_proto::*;
use term_tui_mirror::{MirrorSession, MirrorSessionParams};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn read_frame(socket: &mut TcpStream) -> (CoreFrameType, Vec<u8>) {
    let size = socket.read_u32_le().await.unwrap() as usize;
    assert!((1..=1024 * 1024).contains(&size));
    let mut data = vec![0; size];
    socket.read_exact(&mut data).await.unwrap();
    (CoreFrameType::try_from(data[0]).unwrap(), data[1..].to_vec())
}

async fn request(socket: &mut TcpStream) -> ClientRequest {
    let (kind, bytes) = read_frame(socket).await;
    assert_eq!(kind, CoreFrameType::ClientRequest);
    rmp_serde::from_slice(&bytes).unwrap()
}

async fn send<T: serde::Serialize>(socket: &mut TcpStream, kind: CoreFrameType, value: &T) {
    socket.write_all(&encode_frame(kind, &rmp_serde::to_vec_named(value).unwrap())).await.unwrap();
}

async fn reply(socket: &mut TcpStream, rpc_id: u64, value: ServerOk) {
    send(socket, CoreFrameType::ServerReply, &ServerReply { rpc_id, result: ServerResult::Ok(value) }).await;
}

async fn authenticate(socket: &mut TcpStream, auth_id: &str, token: &str) {
    let hello = request(socket).await;
    assert!(matches!(hello.method, ClientMethod::Hello { .. }));
    let server_nonce = "b".repeat(64);
    reply(socket, hello.rpc_id, ServerOk::Hello {
        proto_version: PROTO_VERSION, features: vec!["connection_auth".into()],
        auth_id: auth_id.into(), server_nonce: server_nonce.clone(),
    }).await;
    let auth = request(socket).await;
    let ClientMethod::ConnectionAuth { client_nonce, proof: client_proof } = auth.method else { panic!("authentication required") };
    assert!(verify_proof(token, auth_id, &server_nonce, &client_nonce, ProofRole::Client, client_proof.as_str()));
    reply(socket, auth.rpc_id, ServerOk::ConnectionAuthed {
        proof: proof(token, auth_id, &server_nonce, &client_nonce, ProofRole::Server).unwrap().into(),
    }).await;
}

struct Accept;
#[async_trait::async_trait]
impl AskHost for Accept {
    async fn on_ask(&self, _: ServerAsk) -> AnswerResult { AnswerResult::AcceptHostKey }
}

fn frame(full: bool, text: &str) -> FramePayload {
    FramePayload {
        cols: 12, screen_lines: 3, history: 100, display_offset: 7, content_scrolled: 8,
        cursor: Some((0, 3)), cursor_shape: FrameCursorShape::Beam, cursor_blinking: false,
        mode: 0, full, damage: vec![FrameRow { line: 0, cells: text.chars().map(|ch| FrameCell {
            ch, fg: FrameColor::Spec(0xabcdef), bg: FrameColor::Named(257),
            flags: if ch == '中' { Flags::WIDE_CHAR.bits() } else { 0 },
            graphic: None,
        }).collect() }], highlight: Vec::new(), highlight_total: 0, current_match: None, highlight_current: None,
        blocks: Vec::new(), graphics_added: Vec::new(), graphics_dropped: Vec::new(),
    }
}

#[tokio::test]
async fn authenticated_thin_mirror_handles_asks_frames_input_scroll_resize_and_close() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let auth_id = "0123456789abcdef";
        let token = "a".repeat(64);
        let _credential = term_discovery::connection_auth::publish(addr.port(), auth_id, &token).unwrap();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            authenticate(&mut socket, auth_id, &token).await;
            let open = request(&mut socket).await;
            assert!(matches!(open.method, ClientMethod::SessionOpen(_)));
            // An SSH-style ask must be answered while the open reply is pending.
            send(&mut socket, CoreFrameType::ServerAsk, &ServerAsk {
                ask_id: 19, sid: None, kind: AskKind::HostKeyConfirm(HostKeyAsk {
                    host: "example.test".into(), key_blob: "synthetic".into(), changed: false,
                }),
            }).await;
            let (kind, bytes) = read_frame(&mut socket).await;
            assert_eq!(kind, CoreFrameType::ClientAnswer);
            let answer: ClientAnswer = rmp_serde::from_slice(&bytes).unwrap();
            assert_eq!(answer.result, AnswerResult::AcceptHostKey);
            reply(&mut socket, open.rpc_id, ServerOk::Open { sid: 41 }).await;
            send(&mut socket, CoreFrameType::ServerEvent, &ServerEvent { sid: 41, event: CoreEvent::Frame(frame(true, "中AB")) }).await;
            let input = request(&mut socket).await;
            assert!(matches!(input.method, ClientMethod::SessionWrite { sid: 41, ref params } if params.data == b"echo hi\r"));
            reply(&mut socket, input.rpc_id, ServerOk::Ack).await;
            let scroll = request(&mut socket).await;
            assert!(matches!(scroll.method, ClientMethod::SessionScroll { sid: 41, delta: 1, modifiers: 2, .. }));
            reply(&mut socket, scroll.rpc_id, ServerOk::Ack).await;
            let top = request(&mut socket).await;
            assert!(matches!(top.method, ClientMethod::SessionHistoryScroll { sid: 41, step: HistoryScrollStep::Top }));
            reply(&mut socket, top.rpc_id, ServerOk::Ack).await;
            let resize = request(&mut socket).await;
            assert!(matches!(resize.method, ClientMethod::SessionResize { sid: 41, ref params } if params.cols == 20 && params.rows == 5));
            reply(&mut socket, resize.rpc_id, ServerOk::Ack).await;
            send(&mut socket, CoreFrameType::ServerEvent, &ServerEvent { sid: 41, event: CoreEvent::Frame(frame(false, "X")) }).await;
            let close = request(&mut socket).await;
            assert!(matches!(close.method, ClientMethod::SessionClose { sid: 41 }));
            reply(&mut socket, close.rpc_id, ServerOk::Ack).await;
        });
        let mut mirror = MirrorSession::open(addr, MirrorSessionParams::with_ask_host(
            OpenSpec::Shell(ShellOpenSpec { shell: None, args: Vec::new(), env: Vec::new(), cwd: None,
                cols: 12, rows: 3, display_encoding: None }), 12, 3, Arc::new(Accept),
        )).await.unwrap();
        let event = mirror.next_event().await.unwrap();
        mirror.apply_event(&event);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 3));
        term_tui::draw_grid(mirror.refresh(), &mut buffer, Rect::new(0, 0, 12, 3));
        assert_eq!(buffer[(0, 0)].symbol(), "中");
        assert_eq!(buffer[(2, 0)].symbol(), "A");
        mirror.inject_key(b"echo hi\r".to_vec());
        mirror.forward_output().await.unwrap();
        mirror.backend().process_command(term_view::BackendCommand::Scroll(1,
            term_view::InputModifiers { ctrl: true, ..Default::default() }, Default::default()));
        mirror.forward_output().await.unwrap();
        mirror.backend().process_command(term_view::BackendCommand::ScrollToTop);
        mirror.forward_output().await.unwrap();
        mirror.resize_all(20, 5).await.unwrap();
        let event = mirror.next_event().await.unwrap();
        mirror.apply_event(&event);
        let content = mirror.refresh();
        assert_eq!((content.terminal_size.num_cols, content.terminal_size.num_lines), (20, 5));
        assert_eq!((content.history_size, content.display_offset), (100, 7));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 5));
        term_tui::draw_grid(content, &mut buffer, Rect::new(0, 0, 20, 5));
        assert_eq!(buffer[(0, 0)].symbol(), "X");
        assert_eq!(buffer[(2, 0)].symbol(), " ", "damage clears stale text");
        mirror.close().await.unwrap();
        peer.await.unwrap();
    }).await.expect("protocol contract must not hang");
}

#[tokio::test]
async fn cancelling_an_open_while_answering_an_ask_releases_the_connection() {
    struct WaitForUser(Arc<tokio::sync::Notify>);
    #[async_trait::async_trait]
    impl AskHost for WaitForUser {
        async fn on_ask(&self, _: ServerAsk) -> AnswerResult {
            self.0.notify_one();
            std::future::pending().await
        }
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let auth_id = "fedcba9876543210";
        let token = "c".repeat(64);
        let _credential = term_discovery::connection_auth::publish(addr.port(), auth_id, &token).unwrap();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            authenticate(&mut socket, auth_id, &token).await;
            let open = request(&mut socket).await;
            assert!(matches!(open.method, ClientMethod::SessionOpen(_)));
            send(&mut socket, CoreFrameType::ServerAsk, &ServerAsk {
                ask_id: 20, sid: None,
                kind: AskKind::HostKeyConfirm(HostKeyAsk { host: "example.test".into(), key_blob: "synthetic".into(), changed: false }),
            }).await;
            assert_eq!(socket.read(&mut [0; 1]).await.unwrap(), 0, "cancelled ask must not retain a socket writer");
        });
        let notified = Arc::new(tokio::sync::Notify::new());
        let ask_host = Arc::new(WaitForUser(notified.clone()));
        let open = tokio::spawn(async move {
            MirrorSession::open(addr, MirrorSessionParams::with_ask_host(
                OpenSpec::Shell(ShellOpenSpec { shell: None, args: Vec::new(), env: Vec::new(), cwd: None,
                    cols: 12, rows: 3, display_encoding: None }), 12, 3, ask_host,
            )).await
        });
        notified.notified().await;
        open.abort();
        assert!(matches!(open.await, Err(error) if error.is_cancelled()));
        peer.await.unwrap();
    }).await.expect("cancelled open must not leave a host-question task alive");
}

#[tokio::test]
async fn transport_eof_is_reported_after_the_final_frame_without_an_exit_event() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let auth_id = "abcd1234abcd1234";
        let token = "d".repeat(64);
        let _credential = term_discovery::connection_auth::publish(addr.port(), auth_id, &token).unwrap();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            authenticate(&mut socket, auth_id, &token).await;
            let open = request(&mut socket).await;
            assert!(matches!(open.method, ClientMethod::SessionOpen(_)));
            reply(&mut socket, open.rpc_id, ServerOk::Open { sid: 42 }).await;
            send(&mut socket, CoreFrameType::ServerEvent, &ServerEvent {
                sid: 42, event: CoreEvent::Frame(frame(true, "last")),
            }).await;
            // EOF is deliberately the only termination indication.
        });
        let mut mirror = MirrorSession::open(addr, MirrorSessionParams::basic(
            OpenSpec::Shell(ShellOpenSpec { shell: None, args: Vec::new(), env: Vec::new(), cwd: None,
                cols: 12, rows: 3, display_encoding: None }), 12, 3,
        )).await.unwrap();
        peer.await.unwrap();
        assert!(!mirror.is_disconnected(), "the final frame must be consumed before reporting EOF");
        let event = mirror.next_event().await.expect("final frame");
        mirror.apply_event(&event);
        while !mirror.is_disconnected() { tokio::task::yield_now().await; }
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 3));
        term_tui::draw_grid(mirror.refresh(), &mut buffer, Rect::new(0, 0, 12, 3));
        assert_eq!(buffer[(0, 0)].symbol(), "l");
        assert_eq!(buffer[(3, 0)].symbol(), "t");
    }).await.expect("transport loss must be observable without waiting for another key");
}
