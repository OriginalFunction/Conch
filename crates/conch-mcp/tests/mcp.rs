use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use conch_core::types::{AgentId, FloorConfig, StakePolicy};
use conch_mcp::Server;
use conchd::tcp::Daemon;
use serde_json::{json, Value};
use tempfile::TempDir;

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
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
    assert!(names.contains(&"raise_hand"));
    assert!(names.contains(&"blob_put"));
    assert!(names.contains(&"breakout"));
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
