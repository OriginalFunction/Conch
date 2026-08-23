use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use conch_core::{
    client::{ClientReply, ClientRequest},
    ticket::Ticket,
    types::{AgentId, FloorConfig, Hash32, StakePolicy},
};
use conchd::tcp::{read_frame, write_frame, Daemon};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

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

    let authenticated = http_get(
        server.addr(),
        &format!("/ticket/{}", ticket.id),
        Some(&format!("Bearer {token}")),
    )
    .await;
    assert_eq!(authenticated.0, 200);
    let served = Ticket::from_json_slice(&authenticated.1).unwrap();
    assert_eq!(served.id, ticket.id);
    assert_eq!(served.genesis, ticket.genesis);
    assert_eq!(served.token, None, "HTTP never reflects the capability");
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
    let room = daemon.create_genesis("transport parity").unwrap();
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
    let (mut socket, _) = connect_async(format!("ws://{}/client", http.addr()))
        .await
        .unwrap();
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
        serde_json::to_value(source.replay(room).unwrap().history).unwrap()
    );
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

async fn http_get(addr: SocketAddr, path: &str, authorization: Option<&str>) -> (u16, Vec<u8>) {
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
    (status, response[split + 4..].to_vec())
}
