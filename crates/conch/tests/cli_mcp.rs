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
    assert!(replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "wait_for_history"));
    let names: Vec<&str> = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in ["say", "listen", "who"] {
        assert!(names.contains(&expected), "{names:?}");
    }
    assert!(!names.contains(&"raise_hand"), "{names:?}");
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

    participant
        .send(tool_call(
            15,
            "wait_for_history",
            json!({ "after": 4, "timeout": 3 }),
        ))
        .await;
    participant
        .send(json!({ "jsonrpc": "2.0", "id": 16, "method": "ping" }))
        .await;
    let pong = participant.receive().await;
    assert_eq!(pong["id"], 16, "history wait returned before ping: {pong}");
    assert_eq!(pong["result"], json!({}));
    holder
        .send(tool_call(
            17,
            "say",
            json!({ "text": "holder speaks again\n", "timeout": 5 }),
        ))
        .await;
    let committed = participant.receive().await;
    assert_eq!(committed["id"], 15);
    assert_eq!(committed["result"]["isError"], false);
    let page: Value =
        serde_json::from_str(committed["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(page["timed_out"], false);
    assert_eq!(page["scenes"][0]["scene"]["n"], 5);
    let said = holder.receive().await;
    assert_eq!(said["id"], 17);
    assert_eq!(said["result"]["isError"], false, "{said}");
    assert_eq!(said["result"]["structuredContent"]["n"], 6);
    assert_eq!(
        said["result"]["structuredContent"]["author"]["agent"],
        "agent:holder"
    );

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_address_each_other_with_mentions_through_listen_and_say() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket("Mentions", StakePolicy::default(), FloorConfig::stick(300))
        .unwrap();
    let daemon_server = daemon
        .start(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    let node = format!("tcp://{}", daemon_server.addr());
    let room = ticket.id.to_string();
    let binary = env!("CARGO_BIN_EXE_conch");
    let mut claude = McpProcess::spawn(binary, &node, &room, "agent:claude", data.path()).await;
    let mut codex = McpProcess::spawn(binary, &node, &room, "agent:codex", data.path()).await;

    let who = claude.call(tool_call(1, "who", json!({}))).await;
    assert_eq!(who["result"]["isError"], false, "{who}");
    let who = &who["result"]["structuredContent"];
    assert_eq!(who["name"], "Mentions");
    assert_eq!(who["holder"], Value::Null);
    assert_eq!(who["you"]["agent"], "agent:claude");
    assert_eq!(who["head"], 0);

    // claude speaks first. `say` only returns once the closing speech has
    // committed, so codex's `listen` afterward sees a settled page: both the
    // grant and the take are already in history, deterministically.
    let said = claude
        .call(tool_call(
            2,
            "say",
            json!({ "text": "@codex ping", "timeout": 5 }),
        ))
        .await;
    assert_eq!(said["result"]["isError"], false, "{said}");
    assert_eq!(
        said["result"]["structuredContent"]["author"]["agent"],
        "agent:claude"
    );

    let heard = codex
        .call(tool_call(
            10,
            "listen",
            json!({ "after": 0, "timeout": 10 }),
        ))
        .await;
    assert_eq!(heard["result"]["isError"], false, "{heard}");
    let page = &heard["result"]["structuredContent"];
    assert_eq!(page["timed_out"], false);
    let events = page["events"].as_array().unwrap();
    // The grant to claude is a floor event; the take is a mention of codex.
    assert!(
        events
            .iter()
            .any(|e| e["kind"] == "floor" && e["holder"]["agent"] == "agent:claude"),
        "{events:?}"
    );
    let mentions: Vec<&Value> = events.iter().filter(|e| e["kind"] == "mention").collect();
    assert_eq!(mentions.len(), 1, "{events:?}");
    let mention = mentions[0];
    assert_eq!(mention["author"]["agent"], "agent:claude");
    assert_eq!(mention["text"], "@codex ping");
    assert_eq!(page["height"], events.last().unwrap()["n"]);

    // codex answers; claude hears it as a mention.
    let height = page["height"].as_u64().unwrap();
    let answered = codex
        .call(tool_call(
            11,
            "say",
            json!({ "text": "@claude pong", "timeout": 5 }),
        ))
        .await;
    assert_eq!(answered["result"]["isError"], false, "{answered}");
    let heard = claude
        .call(tool_call(
            3,
            "listen",
            json!({ "after": height, "timeout": 5 }),
        ))
        .await;
    let events = heard["result"]["structuredContent"]["events"]
        .as_array()
        .unwrap()
        .clone();
    let mention = events
        .iter()
        .find(|e| e["kind"] == "mention")
        .expect("mention event");
    assert_eq!(mention["author"]["agent"], "agent:codex");
    assert_eq!(mention["text"], "@claude pong");

    // codex loops: listening again from its own last height sees its own
    // grant as `granted` and its own take as `speech`, never a `mention` of
    // itself, pinning the own-speech rule end to end.
    let own = codex
        .call(tool_call(
            12,
            "listen",
            json!({ "after": height, "timeout": 5 }),
        ))
        .await;
    assert_eq!(own["result"]["isError"], false, "{own}");
    let own_events = own["result"]["structuredContent"]["events"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        own_events.iter().any(|e| e["kind"] == "granted"),
        "{own_events:?}"
    );
    let own_speech = own_events
        .iter()
        .find(|e| e["kind"] == "speech")
        .expect("own speech event, not a mention");
    assert_eq!(own_speech["author"]["agent"], "agent:codex");
    assert_eq!(own_speech["text"], "@claude pong");
    assert!(
        !own_events.iter().any(|e| e["kind"] == "mention"),
        "own take must never appear as a mention: {own_events:?}"
    );

    // A quiet room: listen times out with no events and the height it examined.
    let quiet = claude
        .call(tool_call(
            4,
            "listen",
            json!({ "after": height + 2, "timeout": 1 }),
        ))
        .await;
    let page = &quiet["result"]["structuredContent"];
    assert_eq!(page["timed_out"], true);
    assert_eq!(page["events"], json!([]));
    assert_eq!(page["height"], height + 2);

    claude.shutdown().await;
    codex.shutdown().await;
}
