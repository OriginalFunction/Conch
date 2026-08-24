use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    process::Stdio,
};

use conch_core::types::{FloorConfig, StakePolicy};
use conchd::tcp::Daemon;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::{timeout, Duration},
};

#[tokio::test]
async fn conch_mcp_serves_newline_delimited_json_rpc() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_conch"))
        .args(["--agent", "agent:mcp", "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
"#,
        )
        .await
        .unwrap();
    drop(stdin);
    let output = child.wait_with_output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    assert!(replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "wait_for_floor"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ping_stays_responsive_while_waiting_then_mcp_completes_the_turn() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket(
            "MCP concurrency",
            StakePolicy::default(),
            FloorConfig::stick(30),
        )
        .unwrap();
    let daemon_server = daemon
        .start(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    let node = format!("tcp://{}", daemon_server.addr());
    let room = ticket.id.to_string();
    let binary = env!("CARGO_BIN_EXE_conch");

    let mut holder = McpProcess::spawn(binary, &node, &room, "agent:holder", data.path()).await;
    let mut participant =
        McpProcess::spawn(binary, &node, &room, "agent:participant", data.path()).await;

    let holder_grant = holder
        .call(tool_call(1, "wait_for_floor", json!({ "timeout": 3 })))
        .await;
    assert_eq!(holder_grant["result"]["isError"], false);

    participant
        .send(tool_call(10, "wait_for_floor", json!({})))
        .await;
    participant
        .send(json!({ "jsonrpc": "2.0", "id": 11, "method": "ping" }))
        .await;
    let pong = participant.receive().await;
    assert_eq!(
        pong["id"], 11,
        "wait_for_floor returned before ping: {pong}"
    );
    assert_eq!(pong["result"], json!({}));

    let spoke = holder
        .call(tool_call(
            2,
            "speak",
            json!({
                "text": "holder leaves the floor\n",
                "request_id": "11111111111111111111111111111111"
            }),
        ))
        .await;
    assert_eq!(spoke["result"]["isError"], false, "{spoke}");
    let yielded = holder.call(tool_call(3, "yield", json!({}))).await;
    assert_eq!(yielded["result"]["isError"], false);

    let granted = participant.receive().await;
    assert_eq!(granted["id"], 10);
    assert_eq!(granted["result"]["isError"], false);

    let spoke = participant
        .call(tool_call(
            12,
            "speak",
            json!({
                "text": "participant completes a real MCP turn\n",
                "request_id": "22222222222222222222222222222222"
            }),
        ))
        .await;
    assert_eq!(spoke["result"]["isError"], false);
    let yielded = participant.call(tool_call(13, "yield", json!({}))).await;
    assert_eq!(yielded["result"]["isError"], false);

    let history = participant.call(tool_call(14, "history", json!({}))).await;
    assert_eq!(history["result"]["isError"], false);
    let page: Value =
        serde_json::from_str(history["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let scenes = page["scenes"].as_array().unwrap();
    assert_eq!(scenes.len(), 5);
    assert_eq!(
        scenes[2]["scene"]["body"]["text"],
        "holder leaves the floor\n"
    );
    assert_eq!(
        scenes[4]["scene"]["body"]["text"],
        "participant completes a real MCP turn\n"
    );
    assert_eq!(page["complete"], true);

    holder.shutdown().await;
    participant.shutdown().await;
}

fn tool_call(id: u64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    })
}

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl McpProcess {
    async fn spawn(binary: &str, node: &str, room: &str, agent: &str, cwd: &Path) -> Self {
        let mut child = Command::new(binary)
            .args(["--node", node, "--agent", agent, "--room", room, "mcp"])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            stdout,
        }
    }

    async fn send(&mut self, message: Value) {
        let mut encoded = serde_json::to_vec(&message).unwrap();
        encoded.push(b'\n');
        self.stdin.write_all(&encoded).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn receive(&mut self) -> Value {
        let line = timeout(Duration::from_secs(3), self.stdout.next_line())
            .await
            .expect("MCP response timed out")
            .unwrap()
            .expect("MCP process closed stdout");
        serde_json::from_str(&line).unwrap()
    }

    async fn call(&mut self, message: Value) -> Value {
        self.send(message).await;
        self.receive().await
    }

    async fn shutdown(self) {
        let Self {
            mut child,
            mut stdin,
            stdout,
        } = self;
        stdin.shutdown().await.unwrap();
        drop(stdin);
        drop(stdout);
        timeout(Duration::from_secs(3), child.wait())
            .await
            .expect("MCP process did not exit")
            .unwrap();
    }
}
