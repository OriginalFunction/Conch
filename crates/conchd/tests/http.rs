use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use conch_core::{
    client::{ClientReply, ClientRequest},
    frame,
    ticket::Ticket,
    types::{AgentId, FloorConfig, Hash32, StakePolicy},
};
use conchd::tcp::{read_frame, write_frame, Daemon};
use futures_util::{SinkExt, StreamExt};
use rcgen::{date_time_ymd, generate_simple_self_signed, CertificateParams, CertifiedKey, KeyPair};
use serde_json::Value;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{sleep, timeout, Duration},
};
use tokio_rustls::{
    rustls::{
        pki_types::{PrivatePkcs8KeyDer, ServerName},
        version::TLS13,
        ClientConfig, RootCertStore, ServerConfig,
    },
    TlsConnector,
};
use tokio_tungstenite::{
    connect_async, connect_async_tls_with_config,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
    Connector,
};

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

#[tokio::test]
async fn get_ticket_without_bearer_401() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let token = Hash32::from_bytes([42; 32]);
    let ticket = daemon
        .create_ticket_with_token(
            "private room",
            StakePolicy::default(),
            FloorConfig::stick(30),
            Some(token),
        )
        .unwrap();
    let server = daemon.start_http(loopback()).await.unwrap();

    let unauthenticated = http_get(server.addr(), &format!("/ticket/{}", ticket.id), None).await;
    assert_eq!(unauthenticated.0, 401);
    let wrong = format!("Bearer {}", Hash32::from_bytes([43; 32]));
    assert_eq!(
        http_get(
            server.addr(),
            &format!("/ticket/{}", ticket.id),
            Some(&wrong)
        )
        .await
        .0,
        401
    );

    let authenticated = http_get(
        server.addr(),
        &format!("/ticket/{}", ticket.id),
        Some(&format!("Bearer {token}")),
    )
    .await;
    assert_eq!(authenticated.0, 200);
    assert!(authenticated
        .2
        .to_ascii_lowercase()
        .contains("cache-control: no-store"));
    let served = Ticket::from_json_slice(&authenticated.1).unwrap();
    assert_eq!(served.id, ticket.id);
    assert_eq!(served.genesis, ticket.genesis);
    assert_eq!(served.token, None, "HTTP never reflects the capability");

    assert_eq!(
        http_get(
            server.addr(),
            &format!("/history/{}?from=0", ticket.id),
            None
        )
        .await
        .0,
        401
    );
    let history = http_get(
        server.addr(),
        &format!("/history/{}?from=0", ticket.id),
        Some(&format!("Bearer {token}")),
    )
    .await;
    assert_eq!(history.0, 200);
    assert!(history
        .2
        .to_ascii_lowercase()
        .contains("cache-control: no-store"));
}

#[tokio::test]
async fn http_connection_limit_is_acquired_before_request_headers() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start_http(loopback()).await.unwrap();
    let mut held = Vec::new();
    for _ in 0..64 {
        held.push(TcpStream::connect(server.addr()).await.unwrap());
    }
    sleep(Duration::from_millis(100)).await;

    let mut excess = TcpStream::connect(server.addr()).await.unwrap();
    let _ = excess.write_all(b"GET /").await;
    let mut byte = [0_u8; 1];
    let result = timeout(Duration::from_secs(1), excess.read(&mut byte))
        .await
        .expect("the over-limit connection is rejected promptly");
    assert!(result.is_err() || result.is_ok_and(|read| read == 0));
    drop(held);
}

#[tokio::test]
async fn swarm_websocket_rejects_a_pre_auth_message_over_64_kib() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start_http(loopback()).await.unwrap();
    let (mut socket, _) = connect_async(format!("ws://{}/swarm", server.addr()))
        .await
        .unwrap();
    let sent = socket
        .send(Message::Binary(vec![0_u8; 64 * 1024 + 1].into()))
        .await;
    if sent.is_ok() {
        let response = timeout(Duration::from_secs(1), socket.next())
            .await
            .expect("the oversized pre-auth message closes the WebSocket");
        assert!(!matches!(
            response,
            Some(Ok(Message::Text(_) | Message::Binary(_)))
        ));
    }
}

#[tokio::test]
async fn invalid_websocket_and_delete_session_credentials_share_the_auth_bucket() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let token = Hash32::from_bytes([55; 32]);
    let ticket = daemon
        .create_ticket_with_token(
            "browser auth bucket",
            StakePolicy::default(),
            FloorConfig::stick(30),
            Some(token),
        )
        .unwrap();
    let server = daemon.start_http(loopback()).await.unwrap();
    let origin = format!("http://{}", server.addr());

    for _ in 0..10 {
        let mut request = format!("ws://{}/client", server.addr())
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Origin", HeaderValue::from_str(&origin).unwrap());
        request
            .headers_mut()
            .insert("Cookie", HeaderValue::from_static("conch_session=invalid"));
        assert!(connect_async(request).await.is_err());
    }
    for _ in 0..10 {
        assert_eq!(
            http_delete_session(server.addr(), ticket.id, &origin, "conch_session=invalid").await,
            401
        );
    }
    assert_eq!(
        http_post_session(server.addr(), ticket.id, Some(token), &origin)
            .await
            .0,
        401,
        "the shared browser-auth bucket throttles a subsequent valid attempt"
    );
}

#[tokio::test]
async fn https_and_wss_require_trusted_matching_tls_and_use_secure_session_cookies() {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let key = PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into();
    let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let mut server_config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)
        .unwrap();
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let mut roots = RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let mut client_config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let client_config = Arc::new(client_config);

    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let token = Hash32::from_bytes([56; 32]);
    let ticket = daemon
        .create_ticket_with_token(
            "secure browser",
            StakePolicy::default(),
            FloorConfig::stick(30),
            Some(token),
        )
        .unwrap();
    let reservation = tokio::net::TcpListener::bind(loopback()).await.unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let secure_daemon = daemon.clone();
    let server = tokio::spawn(async move {
        secure_daemon
            .serve_http_tls(address, Arc::new(server_config))
            .await
    });
    sleep(Duration::from_millis(100)).await;

    let empty_roots = RootCertStore::empty();
    let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let untrusted = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])
        .unwrap()
        .with_root_certificates(empty_roots)
        .with_no_client_auth();
    let untrusted = TlsConnector::from(Arc::new(untrusted));
    assert!(untrusted
        .connect(
            ServerName::try_from("127.0.0.1".to_owned()).unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await
        .is_err());
    let trusted = TlsConnector::from(client_config.clone());
    assert!(trusted
        .connect(
            ServerName::try_from("localhost".to_owned()).unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await
        .is_err());

    let origin = format!("https://{address}");
    let response = https_request(
        address,
        client_config.clone(),
        &format!(
            "POST /session/{} HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ticket.id
        ),
    )
    .await;
    let headers = response_headers(&response);
    assert!(headers.starts_with("HTTP/1.1 201"));
    let cookie = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
                .map(|(_, value)| value.trim().to_owned())
        })
        .unwrap();
    assert!(cookie.starts_with("__Host-conch_session="));
    for required in [
        "Path=/",
        "HttpOnly",
        "SameSite=Strict",
        "Secure",
        "Max-Age=900",
    ] {
        assert!(cookie.contains(required));
    }
    assert!(!cookie.contains("Domain="));
    let cookie_pair = cookie.split(';').next().unwrap().to_owned();

    let mut request = format!("wss://{address}/client")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_str(&origin).unwrap());
    request
        .headers_mut()
        .insert("Cookie", HeaderValue::from_str(&cookie_pair).unwrap());
    let (mut socket, _) = connect_async_tls_with_config(
        request,
        None,
        false,
        Some(Connector::Rustls(client_config.clone())),
    )
    .await
    .unwrap();
    socket
        .send(json_message(&ClientRequest::Attach {
            agent: AgentId::new("agent:secure-browser").unwrap(),
        }))
        .await
        .unwrap();
    assert!(next_reply(&mut socket).await.ok);
    socket
        .send(json_message(&ClientRequest::History {
            room: ticket.id,
            from_n: 0,
            follow: false,
        }))
        .await
        .unwrap();
    assert!(next_reply(&mut socket).await.ok);
    drop(socket);

    let revoked = https_request(
        address,
        client_config.clone(),
        &format!(
            "DELETE /session/{} HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nCookie: {cookie_pair}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ticket.id
        ),
    )
    .await;
    assert!(response_headers(&revoked).starts_with("HTTP/1.1 204"));

    let mut request = format!("wss://{address}/client")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_str(&origin).unwrap());
    request
        .headers_mut()
        .insert("Cookie", HeaderValue::from_str(&cookie_pair).unwrap());
    assert!(connect_async_tls_with_config(
        request,
        None,
        false,
        Some(Connector::Rustls(client_config)),
    )
    .await
    .is_err());
    server.abort();
}

#[tokio::test]
async fn expired_tls_certificate_is_rejected_even_when_it_is_a_trusted_root() {
    let mut params = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    params.not_before = date_time_ymd(2019, 1, 1);
    params.not_after = date_time_ymd(2020, 1, 1);
    let signing_key = KeyPair::generate().unwrap();
    let cert = params.self_signed(&signing_key).unwrap();
    let key = PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into();
    let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let server_config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)
        .unwrap();
    let listener = tokio::net::TcpListener::bind(loopback()).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio_rustls::TlsAcceptor::from(Arc::new(server_config))
            .accept(stream)
            .await
    });

    let mut roots = RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let client = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let result = TlsConnector::from(Arc::new(client))
        .connect(
            ServerName::try_from("127.0.0.1".to_owned()).unwrap(),
            TcpStream::connect(address).await.unwrap(),
        )
        .await;
    assert!(result.is_err());
    assert!(server.await.unwrap().is_err());
}

#[tokio::test]
async fn get_history_matches_cli_protocol() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let room = daemon.create_genesis("history room").unwrap();
    let tcp = daemon.start(loopback()).await.unwrap();
    let http = daemon.start_http(loopback()).await.unwrap();

    let cli = tcp_request(
        tcp.addr(),
        ClientRequest::History {
            room,
            from_n: 0,
            follow: false,
        },
    )
    .await;
    assert!(cli.ok);
    let response = http_get(http.addr(), &format!("/history/{room}?from=0"), None).await;
    assert_eq!(response.0, 200);
    let http_history: Value = serde_json::from_slice(&response.1).unwrap();
    assert_eq!(Some(http_history), cli.data);
}

#[tokio::test]
async fn ws_client_and_tcp_expose_the_same_commit_hashes() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let token = Hash32::from_bytes([51; 32]);
    let ticket = daemon
        .create_ticket_with_token(
            "transport parity",
            StakePolicy::default(),
            FloorConfig::stick(30),
            Some(token),
        )
        .unwrap();
    let room = ticket.id;
    let tcp = daemon.start(loopback()).await.unwrap();
    let http = daemon.start_http(loopback()).await.unwrap();

    let tcp_history = tcp_request(
        tcp.addr(),
        ClientRequest::History {
            room,
            from_n: 0,
            follow: false,
        },
    )
    .await;
    let mut socket = session_socket(http.addr(), room, token).await;
    socket
        .send(json_message(&ClientRequest::Attach {
            agent: AgentId::new("human:operator").unwrap(),
        }))
        .await
        .unwrap();
    let attached = next_reply(&mut socket).await;
    assert!(attached.ok);
    assert_eq!(attached.data.unwrap()["agent"], "human:operator");
    socket
        .send(json_message(&ClientRequest::History {
            room,
            from_n: 0,
            follow: false,
        }))
        .await
        .unwrap();
    let ws_history = next_reply(&mut socket).await;
    assert!(ws_history.ok);
    assert_eq!(ws_history.data, tcp_history.data);
}

#[tokio::test]
async fn ws_client_accepts_blob_raw_frame_as_binary() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let token = Hash32::from_bytes([52; 32]);
    let ticket = daemon
        .create_ticket_with_token(
            "ws blob",
            StakePolicy::default(),
            FloorConfig::stick(30),
            Some(token),
        )
        .unwrap();
    let room = ticket.id;
    let http = daemon.start_http(loopback()).await.unwrap();
    let mut socket = session_socket(http.addr(), room, token).await;
    socket
        .send(json_message(&ClientRequest::Attach {
            agent: AgentId::new("agent:browser").unwrap(),
        }))
        .await
        .unwrap();
    assert!(next_reply(&mut socket).await.ok);
    socket
        .send(json_message(&ClientRequest::RaiseHand { room }))
        .await
        .unwrap();
    assert!(next_reply(&mut socket).await.ok);

    let raw = b"binary websocket attachment";
    socket
        .send(json_message(&ClientRequest::PutBlob {
            room,
            name: "browser.bin".into(),
            bytes: raw.len() as u64,
        }))
        .await
        .unwrap();
    let mut frame = Vec::with_capacity(4 + raw.len());
    frame.extend_from_slice(&(raw.len() as u32).to_be_bytes());
    frame.extend_from_slice(raw);
    socket.send(Message::Binary(frame.into())).await.unwrap();
    let reply = next_reply(&mut socket).await;
    assert!(reply.ok, "{reply:?}");
    let blob: conch_core::types::BlobRef = serde_json::from_value(reply.data.unwrap()).unwrap();
    assert_eq!(blob.bytes, raw.len() as u64);
}

#[tokio::test]
async fn browser_session_room_scope_applies_to_binary_client_frames() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let token = Hash32::from_bytes([54; 32]);
    let allowed = daemon
        .create_ticket_with_token(
            "allowed browser room",
            StakePolicy::default(),
            FloorConfig::stick(30),
            Some(token),
        )
        .unwrap();
    let other = daemon.create_genesis("other browser room").unwrap();
    let http = daemon.start_http(loopback()).await.unwrap();
    let mut socket = session_socket(http.addr(), allowed.id, token).await;

    let attach = frame::encode(&ClientRequest::Attach {
        agent: AgentId::new("agent:binary-browser").unwrap(),
    })
    .unwrap();
    socket.send(Message::Binary(attach.into())).await.unwrap();
    assert!(next_reply(&mut socket).await.ok);

    let request = frame::encode(&ClientRequest::History {
        room: other,
        from_n: 0,
        follow: false,
    })
    .unwrap();
    socket.send(Message::Binary(request.into())).await.unwrap();
    let reply = next_reply(&mut socket).await;
    assert!(!reply.ok);
    assert_eq!(reply.error.unwrap().code, "unauthorized");
}

#[tokio::test]
async fn ui_html_is_embedded_and_served_at_root_and_ui() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start_http(loopback()).await.unwrap();

    for path in ["/", "/ui/"] {
        let response = http_get(server.addr(), path, None).await;
        assert_eq!(response.0, 200);
        let html = String::from_utf8(response.1).unwrap();
        assert!(html.contains("Conch"));
        assert!(html.contains("id=\"transcript\""));
        assert!(html.contains("id=\"speech\""));
    }
}

#[tokio::test]
async fn ws_and_tcp_same_commit_hashes() {
    let source_data = TempDir::new().unwrap();
    let follower_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let follower = Daemon::open(follower_data.path()).unwrap();
    let room = source.create_genesis("swarm parity").unwrap();
    follower.track_room(room).unwrap();
    let source_http = source.start_http(loopback()).await.unwrap();
    let follower_tcp = follower.start(loopback()).await.unwrap();

    follower
        .sync_room_from_ws(
            &format!("ws://{}/swarm", source_http.addr()),
            room,
            None,
            source.replay(room).unwrap().chain.head_hash,
        )
        .await
        .unwrap();

    let through_tcp = tcp_request(
        follower_tcp.addr(),
        ClientRequest::History {
            room,
            from_n: 0,
            follow: false,
        },
    )
    .await;
    assert_eq!(
        through_tcp.data.unwrap(),
        serde_json::json!({
            "scenes": source.replay(room).unwrap().history,
            "syncing": false,
            "complete": true,
        })
    );
}

#[tokio::test]
async fn websocket_swarm_streams_post_auth_blob_larger_than_64_kib_in_bounded_chunks() {
    let source_data = TempDir::new().unwrap();
    let follower_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let follower = Daemon::open(follower_data.path()).unwrap();
    let source_tcp = source.start(loopback()).await.unwrap();
    let source_http = source.start_http(loopback()).await.unwrap();
    let room = source.create_genesis("large WSS blob").unwrap();

    assert!(
        tcp_request(source_tcp.addr(), ClientRequest::RaiseHand { room })
            .await
            .ok
    );
    let raw = vec![0x5a_u8; 128 * 1024];
    let mut client = TcpStream::connect(source_tcp.addr()).await.unwrap();
    write_frame(
        &mut client,
        &ClientRequest::Attach {
            agent: AgentId::new("local").unwrap(),
        },
    )
    .await
    .unwrap();
    assert!(
        read_frame::<_, ClientReply>(&mut client)
            .await
            .unwrap()
            .unwrap()
            .ok
    );
    write_frame(
        &mut client,
        &ClientRequest::PutBlob {
            room,
            name: "large.bin".into(),
            bytes: raw.len() as u64,
        },
    )
    .await
    .unwrap();
    client.write_u32(raw.len() as u32).await.unwrap();
    client.write_all(&raw).await.unwrap();
    client.flush().await.unwrap();
    let put = read_frame::<_, ClientReply>(&mut client)
        .await
        .unwrap()
        .unwrap();
    assert!(put.ok, "{put:?}");
    let blob: conch_core::types::BlobRef = serde_json::from_value(put.data.unwrap()).unwrap();
    let spoke = tcp_request(
        source_tcp.addr(),
        ClientRequest::Speak {
            room,
            text: "speech with a large blob".into(),
            request_id: "11111111111111111111111111111111".into(),
        },
    )
    .await;
    assert!(spoke.ok, "{spoke:?}");
    assert!(
        tcp_request(source_tcp.addr(), ClientRequest::Yield { room })
            .await
            .ok
    );

    follower.track_room(room).unwrap();
    let chain = follower
        .sync_room_from_ws(
            &format!("ws://{}/swarm", source_http.addr()),
            room,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(chain.head_n, Some(2));
    assert_eq!(
        std::fs::read(
            follower_data
                .path()
                .join("rooms")
                .join(room.to_string())
                .join("blobs")
                .join(blob.sha256.to_string())
        )
        .unwrap(),
        raw
    );
}

#[tokio::test]
async fn browser_session_requires_private_capability_and_exact_origin() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let private_token = Hash32::from_bytes([53; 32]);
    let private = daemon
        .create_ticket_with_token(
            "private browser",
            StakePolicy::default(),
            FloorConfig::stick(30),
            Some(private_token),
        )
        .unwrap();
    let open = daemon.create_genesis("open browser").unwrap();
    let server = daemon.start_http(loopback()).await.unwrap();
    let origin = format!("http://{}", server.addr());

    assert_eq!(
        http_post_session(server.addr(), private.id, None, &origin)
            .await
            .0,
        401
    );
    assert_eq!(
        http_post_session(
            server.addr(),
            private.id,
            Some(private_token),
            "http://evil.invalid"
        )
        .await
        .0,
        401
    );
    assert_eq!(
        http_post_session(server.addr(), open, Some(private_token), &origin)
            .await
            .0,
        401,
        "tokenless rooms never receive browser write sessions"
    );

    let (status, cookie) =
        http_post_session(server.addr(), private.id, Some(private_token), &origin).await;
    assert_eq!(status, 201);
    let cookie = cookie.expect("successful session sets a cookie");
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));
    assert!(!cookie.contains(&private_token.to_string()));
}

async fn tcp_request(addr: SocketAddr, request: ClientRequest) -> ClientReply {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    write_frame(
        &mut stream,
        &ClientRequest::Attach {
            agent: AgentId::new("local").unwrap(),
        },
    )
    .await
    .unwrap();
    assert!(
        read_frame::<_, ClientReply>(&mut stream)
            .await
            .unwrap()
            .unwrap()
            .ok
    );
    write_frame(&mut stream, &request).await.unwrap();
    read_frame(&mut stream).await.unwrap().unwrap()
}

fn json_message(value: &ClientRequest) -> Message {
    Message::Text(serde_json::to_string(value).unwrap().into())
}

async fn next_reply(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
) -> ClientReply {
    let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
        panic!("expected a JSON text frame");
    };
    serde_json::from_str(text.as_str()).unwrap()
}

async fn session_socket(
    addr: SocketAddr,
    room: conch_core::types::RoomId,
    token: Hash32,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>> {
    let origin = format!("http://{addr}");
    let (status, cookie) = http_post_session(addr, room, Some(token), &origin).await;
    assert_eq!(status, 201);
    let cookie = cookie.unwrap();
    let cookie = cookie.split(';').next().unwrap();
    let mut request = format!("ws://{addr}/client").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_str(&origin).unwrap());
    request
        .headers_mut()
        .insert("Cookie", HeaderValue::from_str(cookie).unwrap());
    connect_async(request).await.unwrap().0
}

async fn http_post_session(
    addr: SocketAddr,
    room: conch_core::types::RoomId,
    token: Option<Hash32>,
    origin: &str,
) -> (u16, Option<String>) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let authorization = token
        .map(|value| format!("Authorization: Bearer {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST /session/{room} HTTP/1.1\r\nHost: {addr}\r\nOrigin: {origin}\r\n{authorization}Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let headers = std::str::from_utf8(&response[..split]).unwrap();
    let status = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let cookie = headers.lines().find_map(|line| {
        line.split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, value)| value.trim().to_owned())
    });
    (status, cookie)
}

async fn http_delete_session(
    addr: SocketAddr,
    room: conch_core::types::RoomId,
    origin: &str,
    cookie: &str,
) -> u16 {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "DELETE /session/{room} HTTP/1.1\r\nHost: {addr}\r\nOrigin: {origin}\r\nCookie: {cookie}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    std::str::from_utf8(&response)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

async fn http_get(
    addr: SocketAddr,
    path: &str,
    authorization: Option<&str>,
) -> (u16, Vec<u8>, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let authorization = authorization
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{authorization}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let headers = std::str::from_utf8(&response[..split]).unwrap();
    let status = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, response[split + 4..].to_vec(), headers.to_owned())
}

async fn https_request(addr: SocketAddr, config: Arc<ClientConfig>, request: &str) -> Vec<u8> {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let mut stream = TlsConnector::from(config)
        .connect(ServerName::try_from("127.0.0.1".to_owned()).unwrap(), tcp)
        .await
        .unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    response
}

fn response_headers(response: &[u8]) -> &str {
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    std::str::from_utf8(&response[..split]).unwrap()
}
