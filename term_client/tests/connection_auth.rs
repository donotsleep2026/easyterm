use term_proto::{ClientRequest, CoreFrameType, ServerOk, ServerReply, ServerResult};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn read_request(socket: &mut tokio::net::TcpStream) -> ClientRequest {
    let len = socket.read_u32_le().await.unwrap() as usize;
    assert!((1..=16 * 1024).contains(&len));
    let mut bytes = vec![0; len];
    socket.read_exact(&mut bytes).await.unwrap();
    assert_eq!(bytes[0], CoreFrameType::ClientRequest as u8);
    rmp_serde::from_slice(&bytes[1..]).unwrap()
}

async fn reply(socket: &mut tokio::net::TcpStream, rpc_id: u64, value: ServerOk) {
    let payload = rmp_serde::to_vec_named(&ServerReply {
        rpc_id,
        result: ServerResult::Ok(value),
    })
    .unwrap();
    socket
        .write_all(&term_proto::encode_frame(
            CoreFrameType::ServerReply,
            &payload,
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_peer_without_the_secret_cannot_authenticate_by_ack_or_reflection() {
    for reflect in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let id = "0123456789abcdef";
        // Models a protected record left after an abnormal publisher exit.
        let token = "a".repeat(64);
        let _record = term_discovery::connection_auth::publish(addr.port(), id, &token).unwrap();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let hello = read_request(&mut socket).await;
            reply(
                &mut socket,
                hello.rpc_id,
                ServerOk::Hello {
                    proto_version: term_proto::PROTO_VERSION,
                    features: vec!["connection_auth".into()],
                    auth_id: id.into(),
                    server_nonce: "b".repeat(64),
                },
            )
            .await;
            let request = read_request(&mut socket).await;
            let term_proto::ClientMethod::ConnectionAuth { proof, .. } = request.method else {
                panic!("expected proof");
            };
            let response = if reflect {
                ServerOk::ConnectionAuthed { proof }
            } else {
                ServerOk::Authed
            };
            reply(&mut socket, request.rpc_id, response).await;
            // Failed mutual authentication must send no business request.
            let mut byte = [0];
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(2), socket.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        assert!(term_client::Client::connect(addr).await.is_err());
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn oversized_handshake_reply_is_rejected_before_body_allocation() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket.write_all(&16_385u32.to_le_bytes()).await.unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    });
    assert!(term_client::Client::connect(addr).await.is_err());
    peer.await.unwrap();
}

#[tokio::test]
async fn connect_never_downgrades_to_an_unauthenticated_daemon() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let len = socket.read_u32_le().await.unwrap() as usize;
        let mut bytes = vec![0; len];
        socket.read_exact(&mut bytes).await.unwrap();
        assert_eq!(bytes[0], CoreFrameType::ClientRequest as u8);
        let request: ClientRequest = rmp_serde::from_slice(&bytes[1..]).unwrap();
        let reply = ServerReply {
            rpc_id: request.rpc_id,
            result: ServerResult::Ok(ServerOk::Hello {
                proto_version: term_proto::PROTO_VERSION,
                features: vec![],
                auth_id: "0123456789abcdef".into(),
                server_nonce: "0".repeat(64),
            }),
        };
        let payload = rmp_serde::to_vec_named(&reply).unwrap();
        socket
            .write_all(&term_proto::encode_frame(
                CoreFrameType::ServerReply,
                &payload,
            ))
            .await
            .unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    });
    let result = term_client::Client::connect(addr).await;
    assert!(matches!(result, Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied));
    peer.await.unwrap();
}

#[tokio::test]
async fn pending_calls_fail_when_the_authenticated_reader_exits() {
    use term_discovery::connection_auth::{proof, verify_proof, ProofRole};

    for malformed_frame in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let token = "a".repeat(64);
        let peer_token = token.clone();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let auth_id = "0123456789abcdef";
            let server_nonce = "b".repeat(64);
            let hello = read_request(&mut socket).await;
            assert!(matches!(
                hello.method,
                term_proto::ClientMethod::Hello { .. }
            ));
            reply(
                &mut socket,
                hello.rpc_id,
                ServerOk::Hello {
                    proto_version: term_proto::PROTO_VERSION,
                    features: vec!["connection_auth".into()],
                    auth_id: auth_id.into(),
                    server_nonce: server_nonce.clone(),
                },
            )
            .await;
            let auth = read_request(&mut socket).await;
            let term_proto::ClientMethod::ConnectionAuth {
                client_nonce,
                proof: client_proof,
            } = auth.method
            else {
                panic!("expected connection authentication");
            };
            assert!(verify_proof(
                &peer_token,
                auth_id,
                &server_nonce,
                &client_nonce,
                ProofRole::Client,
                client_proof.as_str(),
            ));
            let server_proof = proof(
                &peer_token,
                auth_id,
                &server_nonce,
                &client_nonce,
                ProofRole::Server,
            )
            .unwrap();
            reply(
                &mut socket,
                auth.rpc_id,
                ServerOk::ConnectionAuthed {
                    proof: server_proof.into(),
                },
            )
            .await;

            let request = read_request(&mut socket).await;
            assert!(matches!(
                request.method,
                term_proto::ClientMethod::ConfigSave { .. }
            ));
            if malformed_frame {
                // Force a framing error rather than a clean EOF. Neither path
                // can ever produce a reply for the accepted config-save RPC.
                socket.write_all(&[0; 5]).await.unwrap();
            }
        });
        let client = term_client::Client::connect_with_token(addr, &token)
            .await
            .unwrap();
        let writer = client.writer();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            writer.call(term_proto::ClientMethod::ConfigSave {
                path: "configs/pty_settings.toml".into(),
                contents: "ptys = []".into(),
            }),
        )
        .await
        .expect("a lost reader must fail pending RPCs while Writer is still held");
        assert!(matches!(outcome, Err(error) if error.contains("connection closed")));
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            writer.call(term_proto::ClientMethod::VaultStatus),
        )
        .await
        .expect("new calls on a closed reader must fail immediately");
        assert!(matches!(outcome, Err(error) if error.contains("connection closed")));
        peer.await.unwrap();
    }
}
