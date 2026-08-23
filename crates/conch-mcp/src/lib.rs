use std::{fs, net::SocketAddr, path::PathBuf, str::FromStr};

use conch_core::{
    client::{ClientReply, ClientRequest},
    frame::{self, MAX_FRAME_BYTES},
    ticket::{JoinRole, Ticket, TicketSource},
    types::{AgentId, FloorConfig, FloorMode, Mouth, NodeId, RoomId, StakePolicy},
};
use rand::random;
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use tokio::{
    io::{self, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::TcpStream,
};

const LATEST_PROTOCOL: &str = "2025-06-18";
const SUPPORTED_PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", LATEST_PROTOCOL];
const MAX_BLOB_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
pub struct Server {
    node: String,
    agent: AgentId,
    room: Option<RoomId>,
}

impl Server {
    pub fn new(node: String, agent: AgentId, room: Option<RoomId>) -> Self {
        Self { node, agent, room }
    }

    pub async fn handle_message(&self, message: Value) -> Option<Value> {
        let id = message.get("id").cloned()?;
        let method = message.get("method").and_then(Value::as_str);
        let result = match method {
            Some("initialize") => {
                let requested = message
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(LATEST_PROTOCOL);
                let protocol = if SUPPORTED_PROTOCOLS.contains(&requested) {
                    requested
                } else {
                    LATEST_PROTOCOL
                };
                Ok(json!({
                    "protocolVersion": protocol,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "conch",
                        "title": "Conch Room Server",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "instructions": "Join a room, inspect committed history, then use Conch floor tools. Do not speak without a grant."
                }))
            }
            Some("ping") => Ok(json!({})),
            Some("tools/list") => Ok(json!({ "tools": tool_definitions() })),
            Some("tools/call") => self.call_tool(message.get("params")).await,
            Some(other) => {
                return Some(json_rpc_error(
                    id,
                    -32601,
                    &format!("method not found: {other}"),
                ))
            }
            None => return Some(json_rpc_error(id, -32600, "invalid JSON-RPC request")),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json_rpc_error(id, -32602, &error),
        })
    }

    async fn call_tool(&self, params: Option<&Value>) -> Result<Value, String> {
        let params = params
            .and_then(Value::as_object)
            .ok_or("tools/call params must be an object")?;
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or("tools/call requires name")?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let arguments = Arguments::new(arguments, self.room)?;
        let prepared = self.prepare(name, &arguments).await;
        let reply = match prepared {
            Ok((request, raw)) => self.send(request, raw).await,
            Err(error) => return Ok(tool_error(&error)),
        };
        match reply {
            Ok(reply) if reply.ok => {
                let data = reply.data.unwrap_or(Value::Null);
                let structured = match &data {
                    Value::Object(object) => Value::Object(object.clone()),
                    _ => json!({ "data": data }),
                };
                Ok(json!({
                    "content": [{ "type": "text", "text": serde_json::to_string(&data).unwrap_or_else(|_| "null".into()) }],
                    "structuredContent": structured,
                    "isError": false
                }))
            }
            Ok(reply) => {
                let error = reply.error.map_or_else(
                    || "daemon returned an unspecified error".to_owned(),
                    |error| format!("{}: {}", error.code, error.message),
                );
                Ok(tool_error(&error))
            }
            Err(error) => Ok(tool_error(&error)),
        }
    }

    async fn prepare(
        &self,
        name: &str,
        arguments: &Arguments,
    ) -> Result<(ClientRequest, Option<Vec<u8>>), String> {
        let room = || arguments.room();
        let prepared = match name {
            "create" => {
                let name = arguments.string("name")?;
                let mode = arguments.optional_string("mode").unwrap_or("stick");
                let floor = match mode {
                    "stick" => FloorConfig::stick(30),
                    "moderator" => FloorConfig {
                        mode: FloorMode::Moderator,
                        timeout_secs: 30,
                        moderator: Some(Mouth {
                            agent: arguments.agent("moderator")?,
                            node: arguments.node("moderator_node")?,
                        }),
                    },
                    _ => return Err("mode must be stick or moderator".into()),
                };
                (
                    ClientRequest::Create {
                        name,
                        stake: StakePolicy::default(),
                        floor,
                        token: arguments.optional_hash("token")?,
                    },
                    None,
                )
            }
            "join" => {
                let source = TicketSource::parse(arguments.string_ref("ticket")?)
                    .map_err(|error| error.to_string())?;
                let token = arguments.optional_hash("token")?;
                let ticket = resolve_ticket(source, token).await?;
                let role = match arguments.optional_string("role").unwrap_or("stake") {
                    "stake" => JoinRole::Stake,
                    "observe" => JoinRole::Observe,
                    _ => return Err("role must be stake or observe".into()),
                };
                (
                    ClientRequest::Join {
                        ticket: ticket.into(),
                        role,
                    },
                    None,
                )
            }
            "history" => (
                ClientRequest::History {
                    room: room()?,
                    from_n: arguments.optional_u64("from").unwrap_or(0),
                },
                None,
            ),
            "wait_for_floor" => (
                ClientRequest::WaitForFloor {
                    room: room()?,
                    timeout_secs: arguments.optional_u64("timeout"),
                },
                None,
            ),
            "speak" => (
                ClientRequest::Speak {
                    room: room()?,
                    text: arguments.string("text")?,
                    request_id: arguments
                        .optional_string("request_id")
                        .map(str::to_owned)
                        .unwrap_or_else(random_request_id),
                },
                None,
            ),
            "yield" => (ClientRequest::Yield { room: room()? }, None),
            "raise_hand" => (ClientRequest::RaiseHand { room: room()? }, None),
            "grant" => (
                ClientRequest::Grant {
                    room: room()?,
                    to: Mouth {
                        agent: arguments.agent("agent")?,
                        node: arguments.node("node")?,
                    },
                },
                None,
            ),
            "yank" => (ClientRequest::Yank { room: room()? }, None),
            "config" => {
                let floor = match arguments.optional_string("mode") {
                    None => None,
                    Some("stick") => Some(FloorConfig::stick(30)),
                    Some("moderator") => Some(FloorConfig {
                        mode: FloorMode::Moderator,
                        timeout_secs: 30,
                        moderator: Some(Mouth {
                            agent: arguments.agent("moderator")?,
                            node: arguments.node("moderator_node")?,
                        }),
                    }),
                    Some(_) => return Err("mode must be stick or moderator".into()),
                };
                let stake = arguments
                    .object
                    .get("stake")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|error| format!("invalid stake: {error}"))?;
                if floor.is_none() && stake.is_none() {
                    return Err("config requires mode or stake".into());
                }
                (
                    ClientRequest::Membership {
                        room: room()?,
                        stake,
                        floor,
                    },
                    None,
                )
            }
            "breakout" => {
                let members = arguments
                    .object
                    .get("members")
                    .cloned()
                    .map(serde_json::from_value::<Vec<NodeId>>)
                    .transpose()
                    .map_err(|error| format!("invalid members: {error}"))?;
                (
                    ClientRequest::Breakout {
                        room: room()?,
                        name: arguments.string("name")?,
                        members,
                    },
                    None,
                )
            }
            "blob_put" => {
                let path = PathBuf::from(arguments.string_ref("path")?);
                let bytes = fs::read(&path).map_err(|error| error.to_string())?;
                if bytes.len() > MAX_BLOB_BYTES {
                    return Err("blob exceeds the 32 MiB limit".into());
                }
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or("blob filename must be UTF-8")?
                    .to_owned();
                (
                    ClientRequest::PutBlob {
                        room: room()?,
                        name,
                        bytes: bytes.len() as u64,
                    },
                    Some(bytes),
                )
            }
            "leave" => (
                ClientRequest::Leave {
                    room: room()?,
                    vacate: arguments.optional_bool("vacate").unwrap_or(false),
                },
                None,
            ),
            "status" => (
                ClientRequest::Status {
                    room: arguments.optional_room()?,
                },
                None,
            ),
            _ => return Err(format!("unknown Conch tool: {name}")),
        };
        Ok(prepared)
    }

    async fn send(
        &self,
        request: ClientRequest,
        raw: Option<Vec<u8>>,
    ) -> Result<ClientReply, String> {
        let mut stream = TcpStream::connect(parse_node_addr(&self.node)?)
            .await
            .map_err(|error| error.to_string())?;
        write_frame(
            &mut stream,
            &ClientRequest::Attach {
                agent: self.agent.clone(),
            },
        )
        .await?;
        let attached: ClientReply = read_frame(&mut stream).await?;
        if !attached.ok {
            return Ok(attached);
        }
        write_frame(&mut stream, &request).await?;
        if let Some(raw) = raw {
            stream
                .write_u32(raw.len() as u32)
                .await
                .map_err(|error| error.to_string())?;
            stream
                .write_all(&raw)
                .await
                .map_err(|error| error.to_string())?;
            stream.flush().await.map_err(|error| error.to_string())?;
        }
        read_frame(&mut stream).await
    }
}

pub async fn run(node: String, agent: AgentId, room: Option<RoomId>) -> Result<(), String> {
    let server = Server::new(node, agent, room);
    let input = BufReader::new(io::stdin());
    let mut lines = input.lines();
    let mut output = BufWriter::new(io::stdout());
    while let Some(line) = lines.next_line().await.map_err(|error| error.to_string())? {
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => server.handle_message(message).await,
            Err(error) => Some(json_rpc_error(Value::Null, -32700, &error.to_string())),
        };
        if let Some(response) = response {
            let encoded = serde_json::to_vec(&response).map_err(|error| error.to_string())?;
            output
                .write_all(&encoded)
                .await
                .map_err(|error| error.to_string())?;
            output
                .write_all(b"\n")
                .await
                .map_err(|error| error.to_string())?;
            output.flush().await.map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

struct Arguments {
    object: Map<String, Value>,
    default_room: Option<RoomId>,
}

impl Arguments {
    fn new(value: Value, default_room: Option<RoomId>) -> Result<Self, String> {
        let object = value
            .as_object()
            .cloned()
            .ok_or("tool arguments must be an object")?;
        Ok(Self {
            object,
            default_room,
        })
    }

    fn string_ref(&self, key: &str) -> Result<&str, String> {
        self.object
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{key} is required and must be a string"))
    }

    fn string(&self, key: &str) -> Result<String, String> {
        self.string_ref(key).map(str::to_owned)
    }

    fn optional_string(&self, key: &str) -> Option<&str> {
        self.object.get(key).and_then(Value::as_str)
    }

    fn optional_u64(&self, key: &str) -> Option<u64> {
        self.object.get(key).and_then(Value::as_u64)
    }

    fn optional_bool(&self, key: &str) -> Option<bool> {
        self.object.get(key).and_then(Value::as_bool)
    }

    fn room(&self) -> Result<RoomId, String> {
        self.optional_room()?
            .ok_or_else(|| "room is required (tool argument or --room)".into())
    }

    fn optional_room(&self) -> Result<Option<RoomId>, String> {
        self.optional_string("room")
            .map(RoomId::from_str)
            .transpose()
            .map_err(|error| error.to_string())
            .map(|room| room.or(self.default_room))
    }

    fn agent(&self, key: &str) -> Result<AgentId, String> {
        AgentId::new(self.string(key)?).map_err(|error| error.to_string())
    }

    fn node(&self, key: &str) -> Result<NodeId, String> {
        self.string_ref(key)?
            .parse()
            .map_err(|error: conch_core::types::IdError| error.to_string())
    }

    fn optional_hash(&self, key: &str) -> Result<Option<conch_core::types::Hash32>, String> {
        self.optional_string(key)
            .map(str::parse)
            .transpose()
            .map_err(|error: conch_core::types::IdError| error.to_string())
    }
}

async fn resolve_ticket(
    source: TicketSource,
    token: Option<conch_core::types::Hash32>,
) -> Result<Ticket, String> {
    let mut ticket = match source {
        TicketSource::Inline(ticket) => Ok(*ticket),
        TicketSource::File(path) => {
            Ticket::from_json_slice(&fs::read(path).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())
        }
        TicketSource::Http(url) => {
            let client = reqwest::Client::new();
            let mut request = client.get(url);
            if let Some(token) = token {
                request = request.bearer_auth(token.to_string());
            }
            let bytes = request
                .send()
                .await
                .map_err(|error| error.to_string())?
                .error_for_status()
                .map_err(|error| error.to_string())?
                .bytes()
                .await
                .map_err(|error| error.to_string())?;
            Ticket::from_json_slice(&bytes).map_err(|error| error.to_string())
        }
    }?;
    if let Some(token) = token {
        if ticket.token.is_some_and(|known| known != token) {
            return Err("provided token conflicts with the ticket token".into());
        }
        ticket.token = Some(token);
    }
    Ok(ticket)
}

async fn write_frame<T: serde::Serialize>(stream: &mut TcpStream, value: &T) -> Result<(), String> {
    stream
        .write_all(&frame::encode(value).map_err(|error| error.to_string())?)
        .await
        .map_err(|error| error.to_string())?;
    stream.flush().await.map_err(|error| error.to_string())
}

async fn read_frame<T: DeserializeOwned>(stream: &mut TcpStream) -> Result<T, String> {
    let length = stream.read_u32().await.map_err(|error| error.to_string())? as usize;
    if length > MAX_FRAME_BYTES {
        return Err("frame exceeds 64 MiB".into());
    }
    let mut payload = vec![0_u8; length];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(|error| error.to_string())?;
    frame::decode_payload(&payload).map_err(|error| error.to_string())
}

fn parse_node_addr(node: &str) -> Result<SocketAddr, String> {
    node.strip_prefix("tcp://")
        .ok_or_else(|| "MCP currently requires a tcp:// Conch node".to_owned())?
        .parse()
        .map_err(|error| format!("invalid node address: {error}"))
}

fn random_request_id() -> String {
    random::<[u8; 16]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn json_rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true
    })
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn room_properties(extra: Value) -> Value {
    let mut properties = Map::new();
    properties.insert(
        "room".into(),
        json!({ "type": "string", "description": "Room id; defaults to --room or CONCH_ROOM" }),
    );
    if let Some(extra) = extra.as_object() {
        properties.extend(extra.clone());
    }
    Value::Object(properties)
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "create",
            "Create a Conch room",
            object_schema(
                json!({
                    "name": { "type": "string" },
                    "mode": { "enum": ["stick", "moderator"] },
                    "moderator": { "type": "string" },
                    "moderator_node": { "type": "string" },
                    "token": { "type": "string", "description": "Optional 64-character hex room capability" }
                }),
                &["name"],
            ),
        ),
        tool(
            "join",
            "Join from a .conch file, magnet, or HTTP ticket",
            object_schema(
                json!({
                    "ticket": { "type": "string" }, "role": { "enum": ["stake", "observe"] },
                    "token": { "type": "string", "description": "Capability for a token-protected HTTP ticket" }
                }),
                &["ticket"],
            ),
        ),
        tool(
            "history",
            "Read committed room scenes",
            object_schema(
                room_properties(json!({
                    "from": { "type": "integer", "minimum": 0 }
                })),
                &[],
            ),
        ),
        tool(
            "wait_for_floor",
            "Wait until this agent holds an OPEN grant",
            object_schema(
                room_properties(json!({
                    "timeout": { "type": "integer", "minimum": 0 }
                })),
                &[],
            ),
        ),
        tool(
            "speak",
            "Append text to this agent's current take",
            object_schema(
                room_properties(json!({
                    "text": { "type": "string" }, "request_id": { "type": "string" }
                })),
                &["text"],
            ),
        ),
        tool(
            "yield",
            "Freeze and commit this agent's current take",
            object_schema(room_properties(json!({})), &[]),
        ),
        tool(
            "raise_hand",
            "Queue this agent for the talking stick",
            object_schema(room_properties(json!({})), &[]),
        ),
        tool(
            "grant",
            "Moderator: grant the floor to a mouth",
            object_schema(
                room_properties(json!({
                    "agent": { "type": "string" }, "node": { "type": "string" }
                })),
                &["agent", "node"],
            ),
        ),
        tool(
            "yank",
            "Moderator: freeze and close the live grant",
            object_schema(room_properties(json!({})), &[]),
        ),
        tool(
            "config",
            "Commit room floor or stake configuration",
            object_schema(
                room_properties(json!({
                    "mode": { "enum": ["stick", "moderator"] }, "moderator": { "type": "string" },
                    "moderator_node": { "type": "string" }, "stake": { "type": "object" }
                })),
                &[],
            ),
        ),
        tool(
            "breakout",
            "Create a child room while holding the floor",
            object_schema(
                room_properties(json!({
                    "name": { "type": "string" }, "members": { "type": "array", "items": { "type": "string" } }
                })),
                &["name"],
            ),
        ),
        tool(
            "blob_put",
            "Attach a local file to this agent's current take",
            object_schema(
                room_properties(json!({
                    "path": { "type": "string" }
                })),
                &["path"],
            ),
        ),
        tool(
            "leave",
            "Leave a room",
            object_schema(
                room_properties(json!({
                    "vacate": { "type": "boolean" }
                })),
                &[],
            ),
        ),
        tool(
            "status",
            "Show local Conch status",
            object_schema(room_properties(json!({})), &[]),
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}
