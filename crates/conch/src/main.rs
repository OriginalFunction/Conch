use std::{
    env,
    io::{self, Read},
    net::SocketAddr,
    str::FromStr,
};

use conch_core::{
    client::{ClientReply, ClientRequest},
    frame::{self, MAX_FRAME_BYTES},
    types::{AgentId, RoomId},
};
use rand::random;
use serde::{de::DeserializeOwned, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("conch: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let parsed = Arguments::parse(env::args().skip(1))?;
    let mut stream = TcpStream::connect(parse_node_addr(&parsed.node)?).await?;
    write_frame(
        &mut stream,
        &ClientRequest::Attach {
            agent: parsed.agent.clone(),
        },
    )
    .await?;
    let attached: ClientReply = read_frame(&mut stream).await?;
    if !attached.ok {
        return Err(format_reply_error(&attached).into());
    }
    write_frame(&mut stream, &parsed.request).await?;
    let reply: ClientReply = read_frame(&mut stream).await?;
    if !reply.ok {
        return Err(format_reply_error(&reply).into());
    }
    println!(
        "{}",
        serde_json::to_string(&reply.data.unwrap_or_default())?
    );
    Ok(())
}

struct Arguments {
    node: String,
    agent: AgentId,
    request: ClientRequest,
}

impl Arguments {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut arguments = arguments.peekable();
        let mut node = env::var("CONCH_NODE").unwrap_or_else(|_| "tcp://127.0.0.1:7421".into());
        let mut agent = env::var("CONCH_AGENT").unwrap_or_else(|_| "local".into());
        let mut room = env::var("CONCH_ROOM").ok();

        while let Some(argument) = arguments.peek().cloned() {
            match argument.as_str() {
                "--node" => {
                    arguments.next();
                    node = arguments.next().ok_or("--node requires a URL")?;
                }
                "--agent" => {
                    arguments.next();
                    agent = arguments.next().ok_or("--agent requires a name")?;
                }
                "--room" => {
                    arguments.next();
                    room = Some(arguments.next().ok_or("--room requires an id")?);
                }
                _ => break,
            }
        }

        let command = arguments.next().ok_or("a command is required")?;
        let resolve_room = || -> Result<RoomId, String> {
            room.as_deref()
                .ok_or("--room is required for this command")?
                .parse()
                .map_err(|error| format!("invalid room id: {error}"))
        };
        let request = match command.as_str() {
            "create" => {
                expect_flag(&mut arguments, "--name")?;
                ClientRequest::Create {
                    name: arguments.next().ok_or("--name requires a value")?,
                }
            }
            "wait-for-floor" => {
                let mut timeout_secs = None;
                if arguments.peek().is_some_and(|value| value == "--timeout") {
                    arguments.next();
                    timeout_secs = Some(
                        arguments
                            .next()
                            .ok_or("--timeout requires seconds")?
                            .parse()
                            .map_err(|_| "invalid timeout")?,
                    );
                }
                ClientRequest::WaitForFloor {
                    room: resolve_room()?,
                    timeout_secs,
                }
            }
            "speak" => {
                let mut request_id = None;
                let mut read_stdin = false;
                while let Some(flag) = arguments.next() {
                    match flag.as_str() {
                        "--request-id" => {
                            request_id =
                                Some(arguments.next().ok_or("--request-id requires a value")?);
                        }
                        "--file" => {
                            if arguments.next().as_deref() != Some("-") {
                                return Err("only --file - is implemented".into());
                            }
                            read_stdin = true;
                        }
                        _ => return Err(format!("unknown speak argument: {flag}")),
                    }
                }
                let mut text = String::new();
                if read_stdin || text.is_empty() {
                    io::stdin()
                        .read_to_string(&mut text)
                        .map_err(|error| error.to_string())?;
                }
                ClientRequest::Speak {
                    room: resolve_room()?,
                    text,
                    request_id: request_id.unwrap_or_else(|| hex_string(&random::<[u8; 16]>())),
                }
            }
            "yield" => ClientRequest::Yield {
                room: resolve_room()?,
            },
            "raise-hand" => ClientRequest::RaiseHand {
                room: resolve_room()?,
            },
            "history" => {
                let mut from_n = 0;
                if arguments.peek().is_some_and(|value| value == "--from") {
                    arguments.next();
                    from_n = arguments
                        .next()
                        .ok_or("--from requires a height")?
                        .parse()
                        .map_err(|_| "invalid history height")?;
                }
                ClientRequest::History {
                    room: resolve_room()?,
                    from_n,
                }
            }
            "status" => ClientRequest::Status {
                room: room
                    .as_deref()
                    .map(RoomId::from_str)
                    .transpose()
                    .map_err(|error| format!("invalid room id: {error}"))?,
            },
            _ => return Err(format!("unknown command: {command}")),
        };
        if arguments.next().is_some() {
            return Err("unexpected trailing arguments".into());
        }

        Ok(Self {
            node,
            agent: AgentId::new(agent).map_err(|error| error.to_string())?,
            request,
        })
    }
}

async fn write_frame<T: Serialize>(
    stream: &mut TcpStream,
    value: &T,
) -> Result<(), Box<dyn std::error::Error>> {
    stream.write_all(&frame::encode(value)?).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_frame<T: DeserializeOwned>(
    stream: &mut TcpStream,
) -> Result<T, Box<dyn std::error::Error>> {
    let length = stream.read_u32().await? as usize;
    if length > MAX_FRAME_BYTES {
        return Err("frame exceeds 64 MiB".into());
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await?;
    Ok(frame::decode_payload(&payload)?)
}

fn parse_node_addr(node: &str) -> Result<SocketAddr, Box<dyn std::error::Error>> {
    let address = node
        .strip_prefix("tcp://")
        .ok_or("only tcp:// node URLs are supported")?;
    Ok(address.parse()?)
}

fn expect_flag(arguments: &mut impl Iterator<Item = String>, expected: &str) -> Result<(), String> {
    match arguments.next() {
        Some(actual) if actual == expected => Ok(()),
        _ => Err(format!("{expected} is required")),
    }
}

fn format_reply_error(reply: &ClientReply) -> String {
    reply.error.as_ref().map_or_else(
        || "daemon returned an unspecified error".into(),
        |error| format!("{}: {}", error.code, error.message),
    )
}

fn hex_string(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}
