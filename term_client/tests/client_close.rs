//! A `Client` that is dropped must close its socket.
//!
//! Regression: the frame reader owns the read half of a `tokio::io::split`
//! socket, and tokio closes the socket only once *both* halves are gone. The
//! reader task used to stay parked in `read` forever after the `Client` was
//! dropped, so the daemon never saw EOF and every short-lived client — the
//! one-shot helpers create one per call (`account_state`, `pull_config_files`,
//! an update check, ...) — leaked an ESTABLISHED connection. On the daemon
//! side each of those keeps a connection task alive with a 16 ms frame
//! ticker, which is how a long-lived frontend ends up burning a core on idle
//! connections and starving the session that has frames to send.

use term_proto::{
    ClientMethod, ClientRequest, CoreEvent, CoreFrameType, ServerEvent, ServerOk, ServerReply,
    ServerResult,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn request(socket: &mut TcpStream) -> ClientRequest {
    let len = socket.read_u32_le().await.unwrap() as usize;
    let mut frame = vec![0; len];
    socket.read_exact(&mut frame).await.unwrap();
    assert_eq!(frame[0], CoreFrameType::ClientRequest as u8);
    rmp_serde::from_slice(&frame[1..]).unwrap()
}

fn ack(rpc_id: u64) -> Vec<u8> {
    term_proto::encode_frame(
        CoreFrameType::ServerReply,
        &rmp_serde::to_vec_named(&ServerReply {
            rpc_id,
            result: ServerResult::Ok(ServerOk::Ack),
        })
        .unwrap(),
    )
}

async fn eof(socket: &mut TcpStream) {
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), socket.read(&mut [0; 1]))
            .await
            .expect("an unused client socket must close")
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn dropping_a_client_closes_the_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let mut buf = [0u8; 64];
        // EOF (`Ok(0)`) is the accepted socket's view of the client closing.
        // A leaked client leaves this read parked until the timeout fires.
        tokio::time::timeout(std::time::Duration::from_secs(5), sock.read(&mut buf))
            .await
            .expect("the dropped client must close the connection")
            .expect("read")
    });

    let sock = TcpStream::connect(addr).await.expect("connect");
    let client = term_client::Client::from_stream(sock);
    drop(client);

    let read = server.await.expect("server task");
    assert_eq!(
        read, 0,
        "expected EOF on the server side, read {read} bytes"
    );
}

#[tokio::test]
async fn a_consumer_dropped_after_exit_does_not_discard_a_partial_close_ack() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socket = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (mut peer, _) = listener.accept().await.unwrap();
    let (writer, mut events, mut asks) = term_client::Client::from_stream(socket).into_parts();
    let (partial_tx, partial_rx) = tokio::sync::oneshot::channel();
    let (continue_tx, continue_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let close = request(&mut peer).await;
        assert!(matches!(
            close.method,
            ClientMethod::SessionClose { sid: 7 }
        ));
        let event = ServerEvent {
            sid: 7,
            event: CoreEvent::Exit { code: Some(0), reason: None },
        };
        peer.write_all(&term_proto::encode_frame(
            CoreFrameType::ServerEvent,
            &rmp_serde::to_vec_named(&event).unwrap(),
        ))
        .await
        .unwrap();
        let frame = ack(close.rpc_id);
        peer.write_all(&frame[..2]).await.unwrap();
        partial_tx.send(()).unwrap();
        continue_rx.await.unwrap();
        peer.write_all(&frame[2..]).await.unwrap();
        eof(&mut peer).await;
    });
    let caller = writer.clone();
    let closing = tokio::spawn(async move { caller.close(7).await });
    assert!(matches!(
        events.recv().await.unwrap().event,
        CoreEvent::Exit { .. }
    ));
    partial_rx.await.unwrap();
    drop(events);
    // Withhold the rest of the ACK. The event receiver is gone, but a live
    // pending RPC must keep the reader (and its ask sender) alive, without spin.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), asks.recv())
            .await
            .is_err()
    );
    continue_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), closing)
        .await
        .expect("close must receive its ACK, not fall back to the 3s close timeout")
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), asks.recv())
            .await
            .unwrap()
            .is_none()
    );
    drop(writer);
    server.await.unwrap();
}

#[tokio::test]
async fn consumer_shutdown_drains_every_pending_reply_before_retiring_the_reader() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socket = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (mut peer, _) = listener.accept().await.unwrap();
    let (writer, events, mut asks) = term_client::Client::from_stream(socket).into_parts();
    let (received_tx, received_rx) = tokio::sync::oneshot::channel();
    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
    let (second_tx, second_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let a = request(&mut peer).await;
        let b = request(&mut peer).await;
        let (a, b) = if matches!(a.method, ClientMethod::SessionClose { sid: 1 }) {
            (a, b)
        } else {
            (b, a)
        };
        received_tx.send(()).unwrap();
        first_rx.await.unwrap();
        peer.write_all(&ack(a.rpc_id)).await.unwrap();
        second_rx.await.unwrap();
        peer.write_all(&ack(b.rpc_id)).await.unwrap();
        eof(&mut peer).await;
    });
    let first = writer.clone();
    let second = writer.clone();
    let a = tokio::spawn(async move { first.call(ClientMethod::SessionClose { sid: 1 }).await });
    let b = tokio::spawn(async move { second.call(ClientMethod::SessionClose { sid: 2 }).await });
    received_rx.await.unwrap();
    drop(events);
    first_tx.send(()).unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), a)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        ServerResult::Ok(ServerOk::Ack)
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), asks.recv())
            .await
            .is_err(),
        "one pending reply still owns the reader"
    );
    second_tx.send(()).unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), b)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        ServerResult::Ok(ServerOk::Ack)
    ));
    drop((writer, asks));
    server.await.unwrap();
}

#[tokio::test]
async fn cancelled_or_expired_pending_calls_do_not_leak_a_socket_without_consumers() {
    for abort in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        let (writer, events, asks) = term_client::Client::from_stream(socket).into_parts();
        let (received_tx, received_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            request(&mut peer).await;
            received_tx.send(()).unwrap();
            // This peer intentionally never acknowledges the request.
            eof(&mut peer).await;
        });
        let caller = writer.clone();
        let call = tokio::spawn(async move {
            if abort {
                caller.call(ClientMethod::LicenseStatus).await
            } else {
                caller
                    .call_within(
                        ClientMethod::LicenseStatus,
                        std::time::Duration::from_millis(50),
                    )
                    .await
            }
        });
        received_rx.await.unwrap();
        drop((events, asks));
        if abort {
            call.abort();
            assert!(call.await.unwrap_err().is_cancelled());
        } else {
            assert!(call.await.unwrap().unwrap_err().contains("within"));
        }
        drop(writer);
        server.await.unwrap();
    }
}
