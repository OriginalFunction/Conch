use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use conch_core::types::{AgentId, FloorConfig, StakePolicy};
use conch_mcp::{fetch_ticket, Server};
use conchd::tcp::Daemon;
use rcgen::{generate_simple_self_signed, CertifiedKey};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::{timeout, Duration},
};
use tokio_rustls::{
    rustls::{pki_types::PrivatePkcs8KeyDer, version::TLS13, ServerConfig},
    TlsAcceptor,
};

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

async fn read_http_request(stream: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.unwrap();
        request.push(byte[0]);
    }
    request
}

#[tokio::test]
async fn initialize_and_tools_list_use_current_mcp_envelopes() {
    let server = Server::new(
        "tcp://127.0.0.1:1".into(),
        AgentId::new("agent:test").unwrap(),
        None,
    );
    let initialized = server
        .handle_message(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } }
        }))
        .await
        .unwrap();
    assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
    assert!(initialized["result"]["capabilities"]["tools"].is_object());

    let listed = server
        .handle_message(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
        .await
        .unwrap();
    let names = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(names.contains(&"wait_for_floor"));
    assert!(names.contains(&"wait_for_history"));
    assert!(names.contains(&"raise_hand"));
    assert!(names.contains(&"blob_put"));
    assert!(names.contains(&"breakout"));
}

#[tokio::test]
async fn https_ticket_fetch_uses_the_configured_custom_ca() {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let key = PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into();
    let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let server_config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)
        .unwrap();
    let listener = TcpListener::bind(loopback()).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = TlsAcceptor::from(Arc::new(server_config))
            .accept(stream)
            .await
            .unwrap();
        let request = read_http_request(&mut stream).await;
        assert!(request.starts_with(b"GET /ticket HTTP/1.1\r\n"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
            .await
            .unwrap();
    });
    let directory = TempDir::new().unwrap();
    let ca = directory.path().join("ca.pem");
    std::fs::write(&ca, cert.pem()).unwrap();

    assert_eq!(
        fetch_ticket(&format!("https://{address}/ticket"), None, Some(&ca))
            .await
            .unwrap(),
        b"{}"
    );
    server.await.unwrap();
}

async fn mcp_redirect_chain(redirects: usize) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(loopback()).await.unwrap();
    let address = listener.local_addr().unwrap();
    let attempts = (redirects + 1).min(6);
    let server = tokio::spawn(async move {
        for index in 0..attempts {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(String::from_utf8_lossy(&request).contains("authorization: Bearer "));
            if index < redirects {
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: /hop{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    index + 1
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            } else {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await
                    .unwrap();
            }
        }
    });
    (address, server)
}

#[tokio::test]
async fn mcp_ticket_fetch_accepts_five_redirects_and_rejects_the_sixth() {
    let token = conch_core::types::Hash32::from_bytes([12; 32]);
    let (allowed, allowed_server) = mcp_redirect_chain(5).await;
    assert_eq!(
        fetch_ticket(&format!("http://{allowed}/start"), Some(token), None)
            .await
            .unwrap(),
        b"{}"
    );
    allowed_server.await.unwrap();

    let (rejected, rejected_server) = mcp_redirect_chain(6).await;
    let error = fetch_ticket(&format!("http://{rejected}/start"), Some(token), None)
        .await
        .unwrap_err();
    assert!(error.contains("redirect limit exceeded"));
    rejected_server.await.unwrap();
}

#[tokio::test]
async fn secure_ticket_redirect_to_plaintext_is_rejected_without_forwarding_capability() {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let key = PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into();
    let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let server_config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)
        .unwrap();
    let plaintext = TcpListener::bind(loopback()).await.unwrap();
    let plaintext_address = plaintext.local_addr().unwrap();
    let listener = TcpListener::bind(loopback()).await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = TlsAcceptor::from(Arc::new(server_config))
            .accept(stream)
            .await
            .unwrap();
        let request = read_http_request(&mut stream).await;
        assert!(String::from_utf8_lossy(&request).contains("authorization: Bearer "));
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{plaintext_address}/ticket\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let directory = TempDir::new().unwrap();
    let ca = directory.path().join("ca.pem");
    std::fs::write(&ca, cert.pem()).unwrap();
    let token = conch_core::types::Hash32::from_bytes([13; 32]);

    let error = fetch_ticket(&format!("https://{address}/start"), Some(token), Some(&ca))
        .await
        .unwrap_err();
    assert!(error.contains("changed origin"));
    server.await.unwrap();
    assert!(timeout(Duration::from_millis(100), plaintext.accept())
        .await
        .is_err());
}

#[tokio::test]
async fn mcp_calls_the_same_daemon_floor_protocol() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket("mcp", StakePolicy::default(), FloorConfig::stick(30))
        .unwrap();
    let daemon_server = daemon.start(loopback()).await.unwrap();
    let server = Server::new(
        format!("tcp://{}", daemon_server.addr()),
        AgentId::new("agent:mcp").unwrap(),
        Some(ticket.id),
    );

    let raised = server
        .handle_message(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "raise_hand", "arguments": {} }
        }))
        .await
        .unwrap();
    assert_eq!(raised["result"]["isError"], false);

    let spoke = server
        .handle_message(json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "speak", "arguments": { "text": "hello from MCP" } }
        }))
        .await
        .unwrap();
    assert_eq!(spoke["result"]["isError"], false);

    let yielded = server
        .handle_message(json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "yield", "arguments": {} }
        }))
        .await
        .unwrap();
    assert_eq!(yielded["result"]["isError"], false);

    let history = server
        .handle_message(json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "history", "arguments": {} }
        }))
        .await
        .unwrap();
    assert_eq!(history["result"]["isError"], false);
    let page: Value =
        serde_json::from_str(history["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(page["scenes"].as_array().unwrap().len(), 3);
    assert_eq!(page["scenes"][2]["scene"]["body"]["type"], "speech");
    assert_eq!(page["complete"], true);

    let waiting_server = server.clone();
    let waiting = tokio::spawn(async move {
        waiting_server
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": { "name": "wait_for_history", "arguments": { "after": 2, "timeout": 3 } }
            }))
            .await
            .unwrap()
    });
    tokio::task::yield_now().await;
    let raised_again = server
        .handle_message(json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": { "name": "raise_hand", "arguments": {} }
        }))
        .await
        .unwrap();
    assert_eq!(raised_again["result"]["isError"], false);
    let waited = waiting.await.unwrap();
    assert_eq!(waited["result"]["isError"], false);
    let page: Value =
        serde_json::from_str(waited["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(page["timed_out"], false);
    assert_eq!(page["scenes"][0]["scene"]["n"], 3);

    let timed_out = server
        .handle_message(json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": { "name": "wait_for_history", "arguments": { "after": 3, "timeout": 0 } }
        }))
        .await
        .unwrap();
    assert_eq!(timed_out["result"]["structuredContent"]["timed_out"], true);
}

#[tokio::test]
async fn tool_errors_keep_the_daemon_code_in_structured_content() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket("mcp errors", StakePolicy::default(), FloorConfig::stick(30))
        .unwrap();
    let daemon_server = daemon.start(loopback()).await.unwrap();
    let server = Server::new(
        format!("tcp://{}", daemon_server.addr()),
        AgentId::new("agent:mcp").unwrap(),
        Some(ticket.id),
    );

    let response = server
        .handle_message(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "speak", "arguments": { "text": "no floor" } }
        }))
        .await
        .unwrap();
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(response["result"]["structuredContent"]["code"], "no_grant");
}
