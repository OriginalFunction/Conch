#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    net::SocketAddr,
    path::PathBuf,
    str::FromStr,
};

use conch_core::{
    client::{ClientReply, ClientRequest},
    frame::{self, MAX_FRAME_BYTES},
    ticket::{JoinRole, Ticket, TicketSource},
    types::{
        AgentId, FloorConfig, FloorMode, Hash32, Mouth, NodeId, RoomId, StakePolicy,
        DEFAULT_FLOOR_TIMEOUT_SECS,
    },
};
use rand::random;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
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
    let Arguments {
        node,
        agent,
        tls_ca,
        request,
        output,
        command,
        node_is_default,
        json,
        room,
        oneline,
    } = parsed;
    if let ParsedRequest::Mcp { room } = &request {
        return conch_mcp::run(node, agent, *room, tls_ca, node_is_default)
            .await
            .map_err(Into::into);
    }
    if let ParsedRequest::Local(command) = request {
        return run_local(command).await;
    }
    let ctx = conch::render::Context {
        room,
        current_room: read_current_room(),
        oneline,
        width: terminal_width(),
    };
    if let ParsedRequest::Use { query } = &request {
        let summaries = call(
            &node,
            &agent,
            node_is_default,
            "use",
            ClientRequest::Status { room: None },
        )
        .await?;
        let rooms = summaries["rooms"].as_array().cloned().unwrap_or_default();
        let chosen = resolve_room_query(&rooms, query)?;
        let id: RoomId = serde_json::from_value(chosen["id"].clone())?;
        write_current_room(&id)?;
        let data = serde_json::json!({ "id": id, "name": chosen["name"] });
        return print_reply("use", &data, json, &ctx);
    }
    if let ParsedRequest::Say {
        room,
        text,
        timeout_secs,
    } = request
    {
        let grant = match call(
            &node,
            &agent,
            node_is_default,
            "say",
            ClientRequest::WaitForFloor {
                room,
                timeout_secs: Some(timeout_secs),
            },
        )
        .await
        {
            Ok(grant) => grant,
            Err(error) if error.to_string().starts_with("timeout") => {
                let position = call(
                    &node,
                    &agent,
                    node_is_default,
                    "say",
                    ClientRequest::Status { room: Some(room) },
                )
                .await
                .ok()
                .and_then(|status| {
                    status["queue"].as_array().map(|queue| {
                        queue
                            .iter()
                            .position(|entry| entry["agent"].as_str() == Some(agent.as_str()))
                            .map_or(queue.len(), |i| i + 1)
                    })
                })
                .unwrap_or(0);
                return Err(format!(
                    "timeout: no floor within {timeout_secs} s (queue position {position})"
                )
                .into());
            }
            Err(error) => return Err(error),
        };
        let grant_n = grant["n"].as_u64().unwrap_or(0);
        let grant_hash = Hash32::from_bytes(conch_core::encoding::scene_hash(&grant));
        let request_id = conch_mcp::derived_request_id(&room, &agent, &text);
        let speak = ClientRequest::Speak {
            room,
            text: text.clone(),
            request_id,
        };
        let spoke = match call(&node, &agent, node_is_default, "speak", speak.clone()).await {
            Err(error) if error.to_string().starts_with("invalid") => {
                call(&node, &agent, node_is_default, "speak", speak).await
            }
            other => other,
        };
        let yielded = call(
            &node,
            &agent,
            node_is_default,
            "yield",
            ClientRequest::Yield { room },
        )
        .await;
        spoke?;
        yielded?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut after = grant_n;
        loop {
            let page = call(
                &node,
                &agent,
                node_is_default,
                "say",
                ClientRequest::WaitForHistory {
                    room,
                    after_n: after,
                    timeout_secs: Some(10),
                },
            )
            .await?;
            let records = page["scenes"].as_array().cloned().unwrap_or_default();
            if let Some(record) = records.iter().find(|record| {
                record["scene"]["body"]["closes_grant"] == serde_json::json!(grant_hash)
            }) {
                let data = serde_json::json!({ "n": record["scene"]["n"], "grant_hash": grant_hash, "author": record.get("author").cloned().unwrap_or(Value::Null) });
                return print_reply("say", &data, json, &ctx);
            }
            after = records
                .last()
                .and_then(|record| record["scene"]["n"].as_u64())
                .unwrap_or(after);
            if std::time::Instant::now() >= deadline {
                return Err("the take was accepted but its closing speech has not committed within 60 s; check `conch history`".into());
            }
        }
    }
    if let ParsedRequest::Tail {
        room,
        backlog,
        follow,
    } = request
    {
        let status = call(
            &node,
            &agent,
            node_is_default,
            "tail",
            ClientRequest::Status { room: Some(room) },
        )
        .await?;
        let head = status["head_n"].as_u64().unwrap_or(0);
        let from_n = head.saturating_add(1).saturating_sub(backlog);
        let page = call(
            &node,
            &agent,
            node_is_default,
            "tail",
            ClientRequest::History {
                room,
                from_n,
                follow: false,
            },
        )
        .await?;
        let mut last = head;
        print_scenes(&page, json, &ctx, &mut io::stdout())?;
        if !follow {
            return Ok(());
        }
        loop {
            let next = tokio::select! {
                _ = tokio::signal::ctrl_c() => return Ok(()),
                page = call(&node, &agent, node_is_default, "tail", ClientRequest::WaitForHistory { room, after_n: last, timeout_secs: Some(60) }) => page?,
            };
            if let Some(n) = next["scenes"]
                .as_array()
                .and_then(|scenes| scenes.last())
                .and_then(|record| record["scene"]["n"].as_u64())
            {
                last = n;
            }
            print_scenes(&next, json, &ctx, &mut io::stdout())?;
        }
    }
    let follow = matches!(
        &request,
        ParsedRequest::Ready(request)
            if matches!(request.as_ref(), ClientRequest::History { follow: true, .. })
    );
    let (request, raw) = request.resolve(tls_ca.as_deref()).await?;
    let mut stream = connect_with_spawn(&node, node_is_default).await?;
    write_frame(
        &mut stream,
        &ClientRequest::Attach {
            agent: agent.clone(),
        },
    )
    .await?;
    let attached: ClientReply = read_frame(&mut stream).await?;
    if !attached.ok {
        return Err(format_reply_error(&attached, &command).into());
    }
    write_frame(&mut stream, &request).await?;
    if let Some(raw) = raw {
        stream.write_u32(raw.len() as u32).await?;
        stream.write_all(&raw).await?;
        stream.flush().await?;
    }
    if follow {
        loop {
            let reply: ClientReply = read_frame(&mut stream).await?;
            if !reply.ok {
                return Err(format_reply_error(&reply, &command).into());
            }
            print_reply(&command, &reply.data.unwrap_or_default(), json, &ctx)?;
        }
    }
    let reply: ClientReply = read_frame(&mut stream).await?;
    if !reply.ok {
        return Err(format_reply_error(&reply, &command).into());
    }
    let data = reply.data.unwrap_or_default();
    let data = if command == "join" {
        enrich_join(&node, &agent, node_is_default, data).await
    } else {
        data
    };
    let output = match output {
        Output::Json => data,
        Output::Create {
            ticket_path,
            show_secret,
        } => {
            let ticket: Ticket = serde_json::from_value(
                data.get("ticket")
                    .cloned()
                    .ok_or("daemon create reply omitted ticket")?,
            )?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&ticket_path)?;
            serde_json::to_writer(&mut file, &ticket)?;
            file.sync_all()?;
            let magnet = if show_secret {
                ticket.to_magnet()
            } else {
                let mut public = ticket.clone();
                public.token = None;
                public.to_magnet()
            };
            serde_json::json!({
                "ticket_path": format!("./{}", ticket_path.display()),
                "magnet": magnet,
                "id": ticket.id,
                "name": ticket.name,
            })
        }
    };
    print_reply(&command, &output, json, &ctx)?;
    Ok(())
}

/// After a successful join, look up the room's name and current head so the reply
/// reads naturally; a failed status leaves the join reply exactly as the daemon sent it.
async fn enrich_join(node: &str, agent: &AgentId, node_is_default: bool, mut data: Value) -> Value {
    let Some(room) = data
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| id.parse::<RoomId>().ok())
    else {
        return data;
    };
    let Ok(status) = call(
        node,
        agent,
        node_is_default,
        "status",
        ClientRequest::Status { room: Some(room) },
    )
    .await
    else {
        return data;
    };
    if let Some(object) = data.as_object_mut() {
        if let Some(name) = status.get("name") {
            object.insert("name".to_owned(), name.clone());
        }
        if let Some(head_n) = status.get("head_n") {
            object.insert("head_n".to_owned(), head_n.clone());
        }
    }
    data
}

/// Print a successful reply: `--json` prints the wire reply verbatim; otherwise the
/// render layer's text, falling back to JSON for a command it does not know.
fn print_reply(
    command: &str,
    data: &Value,
    json: bool,
    ctx: &conch::render::Context,
) -> Result<(), Box<dyn std::error::Error>> {
    if json {
        println!("{}", serde_json::to_string(data)?);
        return Ok(());
    }
    match conch::render::render(command, data, ctx) {
        Some(text) if text.is_empty() => {}
        Some(text) => println!("{text}"),
        None => println!("{}", serde_json::to_string(data)?),
    }
    Ok(())
}

/// Print a history page as one line per record (JSON record per line with --json).
fn print_scenes(
    page: &Value,
    json: bool,
    ctx: &conch::render::Context,
    out: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    for record in page["scenes"].as_array().into_iter().flatten() {
        if json {
            writeln!(out, "{}", serde_json::to_string(record)?)?;
        } else {
            writeln!(
                out,
                "{}",
                conch::render::scene_line(record, ctx.oneline, ctx.width)
            )?;
        }
    }
    out.flush()?;
    Ok(())
}

async fn run_local(command: LocalCommand) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        LocalCommand::Setup {
            host,
            agent,
            scope,
            env,
            dry_run,
        } => {
            // A dry run reports what it would change and changes nothing — including
            // not starting a daemon.
            if !dry_run && env::var_os("CONCH_SETUP_SKIP_DAEMON").is_none() {
                ensure_daemon().await?;
            }
            let home = env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or("HOME is not set")?;
            let report = conch::setup::run(&conch::setup::SetupOptions {
                host,
                agent: agent.unwrap_or_else(|| host.default_agent()),
                scope,
                env,
                dry_run,
                home,
                cwd: env::current_dir()?,
                conch_binary: stable_binary_path()
                    .ok_or("cannot determine the path of the running conch binary")?,
                version: env!("CARGO_PKG_VERSION").into(),
            })?;
            if dry_run {
                print!("{}", report.diff);
                println!("(dry run) skill → {}", report.skill_path.display());
                return Ok(());
            }
            match (report.config_changed, report.skill_changed) {
                (false, false) => println!(
                    "{}: already configured ({})",
                    host.name(),
                    report.config_path.display()
                ),
                (config_changed, skill_changed) => {
                    if config_changed {
                        println!(
                            "{}: wrote {}{}",
                            host.name(),
                            report.config_path.display(),
                            report
                                .backup_path
                                .as_ref()
                                .map(|b| format!(" (backup {})", b.display()))
                                .unwrap_or_default()
                        );
                    } else {
                        println!(
                            "{}: config already correct ({})",
                            host.name(),
                            report.config_path.display()
                        );
                    }
                    if skill_changed {
                        println!("skill → {}", report.skill_path.display());
                    } else {
                        println!("skill up to date ({})", report.skill_path.display());
                    }
                }
            }
            println!("{}", report.next_step);
            Ok(())
        }
        LocalCommand::Up { service } => {
            let options = spawn_options()?;
            let data_dir = options.data_dir.clone();
            let (tcp, http) = (options.tcp, options.http);
            if !service {
                let pid =
                    tokio::task::spawn_blocking(move || conch_launch::spawn_detached(&options))
                        .await??;
                print_running(pid, &data_dir, http);
                return Ok(());
            }

            // Homebrew owns its own unit; say so and change nothing.
            if conch::service::is_homebrew(&options.conchd) {
                conch::service::install(&options.conchd, &data_dir)?;
                return Ok(());
            }
            // The unit starts conchd itself, with KeepAlive/Restart=always. Hand-spawning
            // one as well would leave the unit's daemon unable to bind, exiting, and being
            // restarted forever, so stop whatever is running and let the unit own it.
            if conch_launch::wait_for_port(tcp, std::time::Duration::from_millis(300)) {
                // Only a daemon the pid file names can be stopped. A listener with no
                // pid file, or one whose pid was recycled, is not ours to signal — and
                // a unit installed now would fail to bind and be restarted forever.
                let refuse = |why: String| -> Box<dyn std::error::Error> {
                    format!(
                        "something is already listening on {tcp} that `conch` did not start \
                         ({why}); stop it first, then rerun `conch up --service`"
                    )
                    .into()
                };
                // `Ok` covers both a daemon that was just stopped and a pid that was
                // already gone; either way the port should be free now.
                let named = match conch_launch::PidFile::read(&data_dir) {
                    Some(existing) => match existing.stop(std::time::Duration::from_secs(5)) {
                        Ok(()) => Some(existing.pid),
                        Err(conch_launch::LaunchError::PidMismatch { pid }) => {
                            return Err(refuse(format!(
                                "pid {pid} is not a conchd, so it was not signalled"
                            )))
                        }
                        Err(error) => return Err(error.into()),
                    },
                    None => None,
                };
                if conch_launch::wait_for_port(tcp, std::time::Duration::from_millis(300)) {
                    return Err(refuse(match named {
                        Some(pid) => format!("pid {pid} is gone but the port is still held"),
                        None => format!("no pid file in {} names it", data_dir.display()),
                    }));
                }
                conch_launch::PidFile::remove(&data_dir);
            }
            conch::service::install(&options.conchd, &data_dir)?;
            const SECS: u64 = 5;
            if !conch_launch::wait_for_port(tcp, std::time::Duration::from_secs(SECS)) {
                return Err(format!(
                    "the service unit was installed but conchd did not start listening on \
                     {tcp} within {SECS}s; last log lines:\n{}",
                    conch_launch::tail_log(&data_dir, 20)
                )
                .into());
            }
            match conch_launch::wait_for_pid_file(&data_dir, std::time::Duration::from_secs(SECS)) {
                Some(pid) => print_running(pid.pid, &data_dir, http),
                None => println!("conchd running\nui:  http://{http}/"),
            }
            Ok(())
        }
        LocalCommand::Down { service } => {
            let data_dir = conch_launch::default_data_dir();
            match conch_launch::PidFile::read(&data_dir) {
                Some(pid) if pid.is_alive() => match pid.stop(std::time::Duration::from_secs(5)) {
                    Ok(()) => {
                        // `stop` only waits for `kill -0` to fail, which can (rarely, under
                        // load) report a false negative before conchd has actually removed
                        // its own pid file on the way out. Wait for the file itself so
                        // `down` never reports success while a stale pid file remains.
                        wait_for_pid_file_gone(&data_dir, std::time::Duration::from_secs(5))
                            .await?;
                        println!("conchd stopped (pid {})", pid.pid);
                    }
                    // The pid was recycled by some other program: the file is stale,
                    // and nothing was signalled.
                    Err(conch_launch::LaunchError::PidMismatch { .. }) => {
                        conch_launch::PidFile::remove(&data_dir);
                        println!("conchd is not running (removed a stale pid file)");
                    }
                    Err(error) => return Err(error.into()),
                },
                // The process is already gone; clear what it left behind.
                Some(_) => {
                    conch_launch::PidFile::remove(&data_dir);
                    println!("conchd is not running (removed a stale pid file)");
                }
                None => println!("conchd is not running"),
            }
            if service {
                conch::service::uninstall(&data_dir)?;
            }
            Ok(())
        }
        LocalCommand::Doctor => {
            // doctor exists to report a broken install; every input it cannot get is a
            // check that fails, never a reason to print nothing.
            let node = default_tcp();
            let room_id = read_current_room();
            let mut room_head = None;
            let daemon = match node.parse::<SocketAddr>() {
                Ok(addr) => {
                    if conch_launch::wait_for_port(addr, std::time::Duration::from_millis(300)) {
                        let (version, head) = probe_daemon(addr, room_id.as_deref()).await;
                        room_head = head;
                        conch::doctor::DaemonProbe::Reachable { addr, version }
                    } else {
                        conch::doctor::DaemonProbe::Unreachable { addr }
                    }
                }
                Err(error) => conch::doctor::DaemonProbe::BadAddress {
                    node,
                    error: error.to_string(),
                },
            };
            let current_room = room_id.map(|id| {
                let (name, head) = room_head.unzip();
                conch::doctor::CurrentRoom { id, name, head }
            });
            let checks = conch::doctor::run_checks(&conch::doctor::DoctorInput {
                cli_version: env!("CARGO_PKG_VERSION").into(),
                conch_binary: stable_binary_path(),
                daemon,
                data_dir: conch_launch::default_data_dir(),
                home: env::var_os("HOME").map(PathBuf::from),
                current_room,
            });
            print!("{}", conch::doctor::render(&checks));
            if conch::doctor::failed(&checks) {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

/// The path to record for this binary, preferring the un-resolved name it was invoked
/// under so a Homebrew upgrade does not orphan every host config.
fn stable_binary_path() -> Option<PathBuf> {
    let argv0 = env::args_os().next();
    let exe = env::current_exe().ok();
    let path = env::var_os("PATH");
    let cwd = env::current_dir().ok();
    conch::setup::stable_binary_path_from(
        argv0.as_deref(),
        exe.as_deref(),
        path.as_deref(),
        cwd.as_deref(),
    )
}

fn print_running(pid: u32, data_dir: &std::path::Path, http: SocketAddr) {
    println!(
        "conchd running (pid {pid})\nlog: {}\nui:  http://{http}/",
        conch_launch::log_path(data_dir).display()
    );
}

/// Ask a reachable daemon for its version (`None` when it predates the request) and,
/// given the current room, that room's name and head height.
async fn probe_daemon(
    addr: SocketAddr,
    room: Option<&str>,
) -> (Option<String>, Option<(String, u64)>) {
    async fn probe(
        addr: SocketAddr,
        room: Option<&str>,
    ) -> Option<(Option<String>, Option<(String, u64)>)> {
        let mut stream = TcpStream::connect(addr).await.ok()?;
        write_frame(
            &mut stream,
            &ClientRequest::Attach {
                agent: AgentId::new("agent:doctor").ok()?,
            },
        )
        .await
        .ok()?;
        let attached: ClientReply = read_frame(&mut stream).await.ok()?;
        if !attached.ok {
            return None;
        }
        write_frame(&mut stream, &ClientRequest::Version)
            .await
            .ok()?;
        let reply: ClientReply = read_frame(&mut stream).await.ok()?;
        let version = reply
            .data
            .as_ref()
            .and_then(|data| data.get("version")?.as_str().map(String::from));
        let Some(room) = room.and_then(|room| room.parse::<RoomId>().ok()) else {
            return Some((version, None));
        };
        write_frame(&mut stream, &ClientRequest::Status { room: Some(room) })
            .await
            .ok()?;
        let reply: ClientReply = read_frame(&mut stream).await.ok()?;
        let head = reply.data.and_then(|data| {
            let name = data.get("name")?.as_str()?.to_string();
            let head_n = data.get("head_n")?.as_u64()?;
            Some((name, head_n))
        });
        Some((version, head))
    }
    probe(addr, room).await.unwrap_or((None, None))
}

async fn wait_for_pid_file_gone(
    data_dir: &std::path::Path,
    timeout: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + timeout;
    while conch_launch::PidFile::read(data_dir).is_some() {
        if tokio::time::Instant::now() >= deadline {
            return Err("conchd stopped but its pid file was not removed in time".into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Ok(())
}

async fn connect_with_spawn(
    node: &str,
    node_is_default: bool,
) -> Result<TcpStream, Box<dyn std::error::Error>> {
    let addr = parse_node_addr(node)?;
    match TcpStream::connect(addr).await {
        Ok(stream) => Ok(stream),
        // Auto-spawn is a convenience; when it does not work the user still needs the
        // remedy for "nothing is listening", with the underlying cause beneath it.
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused && node_is_default => {
            let spawned = ensure_daemon().await;
            let failure = match spawned {
                Ok(()) => match TcpStream::connect(addr).await {
                    Ok(stream) => return Ok(stream),
                    Err(error) => error.to_string(),
                },
                Err(error) => error.to_string(),
            };
            Err(format!(
                "{}\ncause: {failure}",
                conch::remedy::connect_error(&addr.to_string())
            )
            .into())
        }
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            Err(conch::remedy::connect_error(&addr.to_string()).into())
        }
        Err(error) => Err(error.into()),
    }
}

/// One request on its own connection: attach, send, read the reply. An error reply
/// is returned as the CLI's usual `code: message` plus remedy text.
async fn call(
    node: &str,
    agent: &AgentId,
    node_is_default: bool,
    command: &str,
    request: ClientRequest,
) -> Result<Value, Box<dyn std::error::Error>> {
    let mut stream = connect_with_spawn(node, node_is_default).await?;
    write_frame(
        &mut stream,
        &ClientRequest::Attach {
            agent: agent.clone(),
        },
    )
    .await?;
    let attached: ClientReply = read_frame(&mut stream).await?;
    if !attached.ok {
        return Err(format_reply_error(&attached, command).into());
    }
    write_frame(&mut stream, &request).await?;
    let reply: ClientReply = read_frame(&mut stream).await?;
    if !reply.ok {
        return Err(format_reply_error(&reply, command).into());
    }
    Ok(reply.data.unwrap_or_default())
}

fn default_tcp() -> String {
    conch_launch::default_tcp()
}

fn default_http() -> String {
    conch_launch::default_http()
}

fn spawn_options() -> Result<conch_launch::SpawnOptions, Box<dyn std::error::Error>> {
    Ok(conch_launch::SpawnOptions {
        conchd: conch_launch::locate_conchd()?,
        data_dir: conch_launch::default_data_dir(),
        tcp: default_tcp().parse()?,
        http: default_http().parse()?,
    })
}

/// Start conchd if nothing is listening on the default node. Prints one stderr line when it does.
async fn ensure_daemon() -> Result<(), Box<dyn std::error::Error>> {
    let tcp: SocketAddr = default_tcp().parse()?;
    let http: SocketAddr = default_http().parse()?;
    let data_dir = conch_launch::default_data_dir();
    let spawned = {
        let data_dir = data_dir.clone();
        tokio::task::spawn_blocking(move || conch_launch::ensure_daemon(tcp, http, &data_dir))
            .await??
    };
    if let Some(pid) = spawned {
        eprintln!("{}", conch_launch::started_line(pid, &data_dir));
    }
    Ok(())
}

async fn fetch_ticket(
    source: &str,
    token: Option<Hash32>,
    tls_ca: Option<&std::path::Path>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    conch_mcp::fetch_ticket(source, token, tls_ca)
        .await
        .map_err(|error| io::Error::other(error).into())
}

struct Arguments {
    node: String,
    agent: AgentId,
    tls_ca: Option<PathBuf>,
    request: ParsedRequest,
    output: Output,
    command: String,
    node_is_default: bool,
    json: bool,
    room: Option<String>,
    oneline: bool,
}

enum Output {
    Json,
    Create {
        ticket_path: PathBuf,
        show_secret: bool,
    },
}

enum ParsedRequest {
    Ready(Box<ClientRequest>),
    Join {
        source: TicketSource,
        role: JoinRole,
        token: Option<Hash32>,
    },
    BlobFile {
        room: RoomId,
        path: PathBuf,
    },
    Mcp {
        room: Option<RoomId>,
    },
    Use {
        query: String,
    },
    Say {
        room: RoomId,
        text: String,
        timeout_secs: u64,
    },
    Tail {
        room: RoomId,
        backlog: u64,
        follow: bool,
    },
    Local(LocalCommand),
}

enum LocalCommand {
    Setup {
        host: conch::hosts::Host,
        agent: Option<String>,
        scope: conch::hosts::Scope,
        env: conch::hosts::Env,
        dry_run: bool,
    },
    Up {
        service: bool,
    },
    Down {
        service: bool,
    },
    Doctor,
}

impl ParsedRequest {
    async fn resolve(
        self,
        tls_ca: Option<&std::path::Path>,
    ) -> Result<(ClientRequest, Option<Vec<u8>>), Box<dyn std::error::Error>> {
        match self {
            Self::Ready(request) => Ok((*request, None)),
            Self::Join {
                source,
                role,
                token,
            } => {
                let mut ticket = match source {
                    TicketSource::Inline(ticket) => *ticket,
                    TicketSource::File(path) => Ticket::from_json_slice(&fs::read(path)?)?,
                    TicketSource::Http(url) => {
                        let bytes = fetch_ticket(&url, token, tls_ca).await?;
                        Ticket::from_json_slice(&bytes)?
                    }
                };
                if let Some(token) = token {
                    if ticket.token.is_some_and(|known| known != token) {
                        return Err("provided token conflicts with the ticket token".into());
                    }
                    ticket.token = Some(token);
                }
                Ok((
                    ClientRequest::Join {
                        ticket: ticket.into(),
                        role,
                    },
                    None,
                ))
            }
            Self::BlobFile { room, path } => {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or("blob filename must be UTF-8")?
                    .to_owned();
                let bytes = tokio::task::spawn_blocking(move || {
                    let metadata = fs::metadata(&path)?;
                    if metadata.len() > 32 * 1024 * 1024 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "blob exceeds the 32 MiB limit",
                        ));
                    }
                    fs::read(path)
                })
                .await??;
                Ok((
                    ClientRequest::PutBlob {
                        room,
                        name,
                        bytes: bytes.len() as u64,
                    },
                    Some(bytes),
                ))
            }
            Self::Mcp { .. } => unreachable!("MCP is handled before client request resolution"),
            Self::Use { .. } => {
                unreachable!("use is handled before client request resolution")
            }
            Self::Say { .. } => {
                unreachable!("say is handled before client request resolution")
            }
            Self::Tail { .. } => {
                unreachable!("tail is handled before client request resolution")
            }
            Self::Local(_) => {
                unreachable!("local commands are handled before client request resolution")
            }
        }
    }
}

fn ready(request: ClientRequest) -> ParsedRequest {
    ParsedRequest::Ready(Box::new(request))
}

/// The identity a person gets when they do not name one: `human:<username>`.
fn default_agent() -> String {
    default_agent_from(
        env::var("USER").ok().as_deref(),
        env::var("LOGNAME").ok().as_deref(),
    )
}

fn default_agent_from(user: Option<&str>, logname: Option<&str>) -> String {
    let name = user
        .or(logname)
        .map(sanitise_username)
        .unwrap_or_else(|| "operator".into());
    format!("human:{name}")
}

/// Lower-case, and only the characters an agent id allows besides `:` and `.`.
fn sanitise_username(raw: &str) -> String {
    let cleaned: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "operator".into()
    } else {
        cleaned
    }
}

/// Global options a command may claim for itself; the parser leaves these alone.
fn owned_flags(command: &str) -> &'static [&'static str] {
    match command {
        "create" => &["--token"],
        "setup" => &["--agent"],
        _ => &[],
    }
}

struct Globals {
    node: String,
    node_explicit: bool,
    agent: String,
    room: Option<String>,
    token: Option<Hash32>,
    tls_ca: Option<PathBuf>,
    json: bool,
}

/// Parse one global flag into `globals`, reading its value (if it takes one) from
/// `next`. Returns `Ok(true)` when `flag` was a global option (fully consumed,
/// including any value), `Ok(false)` when it wasn't — the caller keeps `flag` for
/// its own use (e.g. as the command word, or a command-owned argument).
fn take_global(
    flag: &str,
    next: &mut impl FnMut() -> Option<String>,
    globals: &mut Globals,
) -> Result<bool, String> {
    match flag {
        "--json" => globals.json = true,
        "--node" => {
            globals.node = next().ok_or("--node requires a URL")?;
            globals.node_explicit = true;
        }
        "--agent" => globals.agent = next().ok_or("--agent requires a name")?,
        "--room" => globals.room = Some(next().ok_or("--room requires an id")?),
        "--token" => {
            globals.token = Some(
                next()
                    .ok_or("--token requires a 64-character hex capability")?
                    .parse::<Hash32>()
                    .map_err(|error| error.to_string())?,
            );
        }
        "--tls-ca" => {
            globals.tls_ca = Some(PathBuf::from(next().ok_or("--tls-ca requires a PEM file")?));
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// Pull global options out of the arguments that follow the command word, leaving
/// the command's own arguments in order.
fn extract_globals(
    command: &str,
    args: Vec<String>,
    globals: &mut Globals,
) -> Result<Vec<String>, String> {
    let owned = owned_flags(command);
    let mut rest = Vec::with_capacity(args.len());
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if owned.contains(&arg.as_str()) {
            rest.push(arg);
            if let Some(value) = iter.next() {
                rest.push(value);
            }
            continue;
        }
        if take_global(&arg, &mut || iter.next(), globals)? {
            continue;
        }
        rest.push(arg);
    }
    Ok(rest)
}

impl Arguments {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut arguments = arguments.peekable();
        let mut globals = Globals {
            node: env::var("CONCH_NODE").unwrap_or_else(|_| format!("tcp://{}", default_tcp())),
            node_explicit: false,
            agent: env::var("CONCH_AGENT").unwrap_or_else(|_| default_agent()),
            room: env::var("CONCH_ROOM").ok().or_else(read_current_room),
            token: env::var("CONCH_TOKEN")
                .ok()
                .map(|value| value.parse::<Hash32>())
                .transpose()
                .map_err(|error| format!("invalid CONCH_TOKEN: {error}"))?,
            tls_ca: env::var_os("CONCH_TLS_CA").map(PathBuf::from),
            json: false,
        };

        let command = loop {
            let Some(argument) = arguments.next() else {
                return Err("a command is required".into());
            };
            match argument.as_str() {
                "--version" | "-V" => {
                    println!("conch {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                _ => {
                    if take_global(&argument, &mut || arguments.next(), &mut globals)? {
                        continue;
                    }
                    break argument;
                }
            }
        };
        if command == "help" {
            if let Some(command) = arguments.next() {
                print_command_help(&command)?;
            } else {
                print_help();
            }
            std::process::exit(0);
        }

        let rest: Vec<String> = arguments.collect();
        let rest = extract_globals(&command, rest, &mut globals)?;
        let Globals {
            node,
            node_explicit,
            agent,
            room,
            token,
            tls_ca,
            json,
        } = globals;
        let room_for_output = room.clone();
        let mut arguments = rest.into_iter().peekable();

        if arguments
            .peek()
            .is_some_and(|argument| argument == "--help" || argument == "-h")
        {
            print_command_help(&command)?;
            std::process::exit(0);
        }
        let resolve_room = || -> Result<RoomId, String> {
            room.as_deref()
                .ok_or("--room is required for this command")?
                .parse()
                .map_err(|error| format!("invalid room id: {error}"))
        };
        let mut output = Output::Json;
        let mut oneline = false;
        let request = match command.as_str() {
            "create" => {
                let mut name = None;
                let mut mode = FloorMode::Stick;
                let mut moderator_agent = None;
                let mut moderator_node = None;
                let mut create_token = token;
                let mut open = false;
                let mut show_secret = false;
                let mut timeout_secs = DEFAULT_FLOOR_TIMEOUT_SECS;
                while let Some(flag) = arguments.next() {
                    match flag.as_str() {
                        "--name" => {
                            name = Some(arguments.next().ok_or("--name requires a value")?);
                        }
                        "--mode" => {
                            mode = match arguments.next().as_deref() {
                                Some("stick") => FloorMode::Stick,
                                Some("moderator") => FloorMode::Moderator,
                                _ => return Err("--mode must be stick or moderator".into()),
                            };
                        }
                        "--timeout" => {
                            timeout_secs = arguments
                                .next()
                                .ok_or("--timeout requires seconds")?
                                .parse::<u64>()
                                .map_err(|error| error.to_string())?;
                            if timeout_secs < 1 {
                                return Err("--timeout must be at least 1".into());
                            }
                        }
                        "--moderator" => {
                            moderator_agent = Some(
                                AgentId::new(
                                    arguments.next().ok_or("--moderator requires an agent")?,
                                )
                                .map_err(|error| error.to_string())?,
                            );
                        }
                        "--moderator-node" => {
                            moderator_node = Some(
                                arguments
                                    .next()
                                    .ok_or("--moderator-node requires an id")?
                                    .parse::<NodeId>()
                                    .map_err(|error| error.to_string())?,
                            );
                        }
                        "--observe" => return Err("create cannot use --observe".into()),
                        "--open" => open = true,
                        "--show-secret" => show_secret = true,
                        "--token" => {
                            create_token = Some(
                                arguments
                                    .next()
                                    .ok_or("--token requires a 64-character hex capability")?
                                    .parse::<Hash32>()
                                    .map_err(|error| error.to_string())?,
                            );
                        }
                        "--token-file" => {
                            let path = arguments.next().ok_or("--token-file requires a path")?;
                            let mut value = String::new();
                            if path == "-" {
                                io::stdin()
                                    .read_to_string(&mut value)
                                    .map_err(|error| error.to_string())?;
                            } else {
                                value =
                                    fs::read_to_string(path).map_err(|error| error.to_string())?;
                            }
                            create_token = Some(
                                value
                                    .trim()
                                    .parse::<Hash32>()
                                    .map_err(|error| error.to_string())?,
                            );
                        }
                        _ => return Err(format!("unknown create argument: {flag}")),
                    }
                }
                if open && create_token.is_some() {
                    return Err("--open conflicts with --token/--token-file".into());
                }
                if !open && create_token.is_none() {
                    create_token = Some(Hash32::from_bytes(random::<[u8; 32]>()));
                }
                let name = name.ok_or("--name is required")?;
                let moderator = match (mode, moderator_agent, moderator_node) {
                    (FloorMode::Stick, None, None) => None,
                    (FloorMode::Moderator, Some(agent), Some(node)) => Some(Mouth { agent, node }),
                    (FloorMode::Stick, _, _) => {
                        return Err("stick mode cannot name a moderator".into())
                    }
                    (FloorMode::Moderator, _, _) => {
                        return Err(
                            "moderator mode requires --moderator and --moderator-node".into()
                        )
                    }
                };
                let ticket_path = PathBuf::from(format!("{}.conch", slug(&name)));
                if ticket_path.exists() {
                    return Err(format!(
                        "ticket already exists: ./{}",
                        ticket_path.display()
                    ));
                }
                output = Output::Create {
                    ticket_path,
                    show_secret,
                };
                ready(ClientRequest::Create {
                    name,
                    stake: StakePolicy::default(),
                    floor: FloorConfig {
                        mode,
                        timeout_secs,
                        moderator,
                    },
                    token: create_token,
                })
            }
            "join" => {
                let source = arguments.next().ok_or("join requires a ticket")?;
                let source = TicketSource::parse(&source).map_err(|error| error.to_string())?;
                let mut selected_role = None;
                for flag in arguments.by_ref() {
                    let role = match flag.as_str() {
                        "--stake" => JoinRole::Stake,
                        "--observe" => JoinRole::Observe,
                        _ => return Err(format!("unknown join argument: {flag}")),
                    };
                    if selected_role.replace(role).is_some() {
                        return Err("join accepts exactly one of --stake or --observe".into());
                    }
                }
                let role = selected_role.unwrap_or_default();
                ParsedRequest::Join {
                    source,
                    role,
                    token,
                }
            }
            "wait-for-floor" => ready({
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
            }),
            "say" => {
                let mut text = None;
                let mut file = None;
                let mut timeout_secs = conch_core::types::DEFAULT_FLOOR_TIMEOUT_SECS;
                while let Some(arg) = arguments.next() {
                    match arg.as_str() {
                        "--file" => {
                            file = Some(arguments.next().ok_or("--file requires a path or -")?)
                        }
                        "--timeout" => {
                            timeout_secs = arguments
                                .next()
                                .ok_or("--timeout requires seconds")?
                                .parse()
                                .map_err(|_| "invalid timeout")?;
                        }
                        other if text.is_none() && !other.starts_with("--") => {
                            text = Some(other.to_owned())
                        }
                        other => return Err(format!("unknown say argument: {other}")),
                    }
                }
                let text = match (text, file) {
                    (Some(text), None) => text,
                    (None, Some(path)) if path == "-" => {
                        let mut s = String::new();
                        io::stdin()
                            .read_to_string(&mut s)
                            .map_err(|e| e.to_string())?;
                        s
                    }
                    (None, Some(path)) => {
                        fs::read_to_string(&path).map_err(|e| format!("cannot read {path}: {e}"))?
                    }
                    (Some(_), Some(_)) => {
                        return Err("say takes either TEXT or --file, not both".into())
                    }
                    (None, None) => {
                        return Err(
                            "say needs text: conch say \"hello\" or conch say --file -".into()
                        )
                    }
                };
                if text.trim().is_empty() {
                    return Err("say needs text: the message is empty".into());
                }
                ParsedRequest::Say {
                    room: resolve_room()?,
                    text,
                    timeout_secs,
                }
            }
            "speak" => ready({
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
            }),
            "yield" => ready(ClientRequest::Yield {
                room: resolve_room()?,
            }),
            "raise-hand" => ready(ClientRequest::RaiseHand {
                room: resolve_room()?,
            }),
            "grant" => {
                let mut to_agent = None;
                let mut to_node = None;
                while let Some(flag) = arguments.next() {
                    match flag.as_str() {
                        "--to" => {
                            to_agent = Some(
                                AgentId::new(arguments.next().ok_or("--to requires a name")?)
                                    .map_err(|error| error.to_string())?,
                            );
                        }
                        "--to-node" => {
                            to_node = Some(
                                arguments
                                    .next()
                                    .ok_or("--to-node requires an id")?
                                    .parse::<NodeId>()
                                    .map_err(|error| error.to_string())?,
                            );
                        }
                        _ => return Err(format!("unknown grant argument: {flag}")),
                    }
                }
                ready(ClientRequest::Grant {
                    room: resolve_room()?,
                    to: Mouth {
                        agent: to_agent.ok_or("grant requires --to")?,
                        node: to_node.ok_or("grant requires --to-node")?,
                    },
                })
            }
            "yank" => ready(ClientRequest::Yank {
                room: resolve_room()?,
            }),
            "config" => {
                let mut mode = None;
                let mut moderator_agent = None;
                let mut moderator_node = None;
                let mut stake = None;
                let mut timeout = None;
                while let Some(flag) = arguments.next() {
                    match flag.as_str() {
                        "--mode" => {
                            mode = Some(match arguments.next().as_deref() {
                                Some("stick") => FloorMode::Stick,
                                Some("moderator") => FloorMode::Moderator,
                                _ => return Err("--mode must be stick or moderator".into()),
                            });
                        }
                        "--moderator" => {
                            moderator_agent = Some(
                                AgentId::new(
                                    arguments.next().ok_or("--moderator requires an agent")?,
                                )
                                .map_err(|error| error.to_string())?,
                            );
                        }
                        "--moderator-node" => {
                            moderator_node = Some(
                                arguments
                                    .next()
                                    .ok_or("--moderator-node requires an id")?
                                    .parse::<NodeId>()
                                    .map_err(|error| error.to_string())?,
                            );
                        }
                        "--timeout" => {
                            let secs = arguments
                                .next()
                                .ok_or("--timeout requires seconds")?
                                .parse::<u64>()
                                .map_err(|error| error.to_string())?;
                            timeout = Some(secs);
                        }
                        "--stake-json" => {
                            stake = Some(
                                serde_json::from_str::<StakePolicy>(
                                    &arguments.next().ok_or("--stake-json requires JSON")?,
                                )
                                .map_err(|error| error.to_string())?,
                            );
                        }
                        _ => return Err(format!("unknown config argument: {flag}")),
                    }
                }
                // `timeout_secs` here is a placeholder: only `--timeout` changes
                // the timeout, and the daemon carries the committed one through
                // a mode or moderator change.
                let floor = match (mode, moderator_agent, moderator_node) {
                    (None, None, None) => None,
                    (Some(FloorMode::Stick), None, None) => {
                        Some(FloorConfig::stick(DEFAULT_FLOOR_TIMEOUT_SECS))
                    }
                    (Some(FloorMode::Moderator) | None, Some(agent), Some(node)) => {
                        Some(FloorConfig {
                            mode: FloorMode::Moderator,
                            timeout_secs: DEFAULT_FLOOR_TIMEOUT_SECS,
                            moderator: Some(Mouth { agent, node }),
                        })
                    }
                    _ => return Err("moderator config requires both moderator fields".into()),
                };
                if floor.is_none() && stake.is_none() && timeout.is_none() {
                    return Err("config requires a floor, stake, or timeout change".into());
                }
                ready(ClientRequest::Membership {
                    room: resolve_room()?,
                    stake,
                    floor,
                    timeout_secs: timeout,
                })
            }
            "breakout" => {
                let mut name = None;
                let mut members = None;
                while let Some(flag) = arguments.next() {
                    match flag.as_str() {
                        "--name" => {
                            name = Some(arguments.next().ok_or("--name requires a value")?);
                        }
                        "--members" => {
                            let value = arguments.next().ok_or("--members requires node ids")?;
                            members = Some(
                                value
                                    .split(',')
                                    .filter(|member| !member.is_empty())
                                    .map(|member| member.parse::<NodeId>())
                                    .collect::<Result<Vec<_>, _>>()
                                    .map_err(|error| error.to_string())?,
                            );
                        }
                        _ => return Err(format!("unknown breakout argument: {flag}")),
                    }
                }
                ready(ClientRequest::Breakout {
                    room: resolve_room()?,
                    name: name.ok_or("breakout requires --name")?,
                    members,
                })
            }
            "blob" => {
                if arguments.next().as_deref() != Some("put") {
                    return Err("blob requires the put subcommand".into());
                }
                ParsedRequest::BlobFile {
                    room: resolve_room()?,
                    path: PathBuf::from(arguments.next().ok_or("blob put requires a file")?),
                }
            }
            "leave" => {
                let mut vacate = false;
                for flag in arguments.by_ref() {
                    match flag.as_str() {
                        "--vacate" => vacate = true,
                        _ => return Err(format!("unknown leave argument: {flag}")),
                    }
                }
                ready(ClientRequest::Leave {
                    room: resolve_room()?,
                    vacate,
                })
            }
            "history" => ready({
                let mut from_n = 0;
                let mut follow = false;
                while let Some(flag) = arguments.next() {
                    match flag.as_str() {
                        "--from" => {
                            from_n = arguments
                                .next()
                                .ok_or("--from requires a height")?
                                .parse()
                                .map_err(|_| "invalid history height")?;
                        }
                        "--follow" => follow = true,
                        "--oneline" => oneline = true,
                        _ => return Err(format!("unknown history argument: {flag}")),
                    }
                }
                ClientRequest::History {
                    room: resolve_room()?,
                    from_n,
                    follow,
                }
            }),
            "tail" => {
                let mut backlog = 20_u64;
                let mut follow = true;
                while let Some(arg) = arguments.next() {
                    match arg.as_str() {
                        "-n" | "--lines" => {
                            backlog = arguments
                                .next()
                                .ok_or("-n requires a count")?
                                .parse()
                                .map_err(|_| "invalid count")?
                        }
                        "--no-follow" => follow = false,
                        "--oneline" => oneline = true,
                        other => return Err(format!("unknown tail argument: {other}")),
                    }
                }
                ParsedRequest::Tail {
                    room: resolve_room()?,
                    backlog,
                    follow,
                }
            }
            "status" => ready(ClientRequest::Status {
                room: room
                    .as_deref()
                    .map(RoomId::from_str)
                    .transpose()
                    .map_err(|error| format!("invalid room id: {error}"))?,
            }),
            "rooms" => ready(ClientRequest::Status { room: None }),
            "use" => ParsedRequest::Use {
                query: arguments
                    .next()
                    .ok_or("use requires a room id, id prefix, or name")?
                    .to_owned(),
            },
            "mcp" => ParsedRequest::Mcp {
                room: room
                    .as_deref()
                    .map(RoomId::from_str)
                    .transpose()
                    .map_err(|error| format!("invalid room id: {error}"))?,
            },
            "setup" => {
                let host_name = arguments.next().ok_or(
                    "setup requires a host: claude, codex, grok, cursor, gemini, opencode",
                )?;
                let host = conch::hosts::Host::parse(&host_name).ok_or_else(|| {
                    format!(
                        "unknown host {host_name}; expected one of claude, codex, grok, cursor, gemini, opencode"
                    )
                })?;
                let mut agent_override = None;
                let mut scope = conch::hosts::Scope::User;
                let mut env = conch::hosts::Env::default();
                let mut dry_run = false;
                while let Some(flag) = arguments.next() {
                    match flag.as_str() {
                        "--agent" => {
                            agent_override =
                                Some(arguments.next().ok_or("--agent requires a name")?)
                        }
                        "--scope" => {
                            scope = match arguments.next().as_deref() {
                                Some("user") => conch::hosts::Scope::User,
                                Some("project") => conch::hosts::Scope::Project,
                                _ => return Err("--scope must be user or project".into()),
                            }
                        }
                        "--env" => {
                            let pair = arguments.next().ok_or("--env requires KEY=VALUE")?;
                            let (k, v) = pair.split_once('=').ok_or("--env requires KEY=VALUE")?;
                            env.0.push((k.to_string(), v.to_string()));
                        }
                        "--dry-run" => dry_run = true,
                        _ => return Err(format!("unknown setup argument: {flag}")),
                    }
                }
                ParsedRequest::Local(LocalCommand::Setup {
                    host,
                    agent: agent_override,
                    scope,
                    env,
                    dry_run,
                })
            }
            "up" | "down" => {
                let mut service = false;
                for flag in arguments.by_ref() {
                    match flag.as_str() {
                        "--service" => service = true,
                        _ => return Err(format!("unknown {command} argument: {flag}")),
                    }
                }
                ParsedRequest::Local(if command == "up" {
                    LocalCommand::Up { service }
                } else {
                    LocalCommand::Down { service }
                })
            }
            "doctor" => {
                if arguments.next().is_some() {
                    return Err("unknown doctor argument".into());
                }
                ParsedRequest::Local(LocalCommand::Doctor)
            }
            _ => return Err(format!("unknown command: {command}")),
        };
        if arguments.next().is_some() {
            return Err("unexpected trailing arguments".into());
        }
        let node_is_default = env::var_os("CONCH_NODE").is_none() && !node_explicit;

        Ok(Self {
            node,
            agent: AgentId::new(agent).map_err(|error| error.to_string())?,
            tls_ca,
            request,
            output,
            command,
            node_is_default,
            json,
            room: room_for_output,
            oneline,
        })
    }
}

fn print_help() {
    println!(
        "conch {}\n\
         Floor-controlled rooms for people and coding agents.\n\n\
         Usage: conch <COMMAND> [ARGS] [GLOBAL OPTIONS]  (global options may also precede the command)\n\n\
         Global options:\n\
           --node URL            Local daemon [default: tcp://127.0.0.1:7421]\n\
           --agent ID            Stable mouth identity [default: human:<username>]\n\
           --room ID             Room id (or CONCH_ROOM/current-room)\n\
           --token HEX           32-byte room capability\n\
           --tls-ca FILE         Additional CA bundle for HTTPS tickets\n\
           --json                Print the wire reply as JSON\n\
           -V, --version         Print version\n\
           -h, --help            Print help\n\n\
         Commands:\n\
           create, join, status, history, rooms, use, raise-hand, wait-for-floor\n\
           say, speak, yield, grant, yank, config, breakout, blob, leave, mcp, setup\n\
           up, down, doctor\n\n\
         Run `conch help <command>` for command-specific usage.",
        env!("CARGO_PKG_VERSION")
    );
}

fn print_command_help(command: &str) -> Result<(), String> {
    let usage = match command {
        "create" => {
            "conch create --name NAME [--timeout SECS] [--open | --token HEX | --token-file FILE] [--show-secret]\n\
             Creates a private room by default and writes ./<slug>.conch mode 0600.\n\
             --open is local/LAN only; a public-mode daemon refuses tokenless rooms.\n\
             Takes longer than --timeout (default 300 s) are closed by the leader."
        }
        "join" => {
            "conch [--tls-ca CA.pem] join TICKET|MAGNET|HTTPS_URL [--stake | --observe]"
        }
        "status" => "conch [--room ID] status",
        "history" => "conch history [--from N] [--follow] [--oneline] [--room ID]",
        "tail" => "conch tail [-n N] [--no-follow] [--oneline] [--room ID]\n\
             Prints the last N takes (default 20), then streams new ones until Ctrl-C.",
        "rooms" => "conch rooms [--json]\n\
             Lists the rooms this daemon has loaded; * marks the current room.",
        "use" => "conch use ROOM\n\
             ROOM is a room id, a unique id prefix, or an exact name. Sets the current room.",
        "raise-hand" => "conch --room ID raise-hand",
        "wait-for-floor" => "conch --room ID wait-for-floor [--timeout SECONDS]",
        "say" => "conch say TEXT | conch say --file PATH [--timeout SECS] [--room ID]\n\
             Takes one turn: waits for the floor (default 300 s), speaks, yields, and prints the committed scene.",
        "speak" => "printf 'text' | conch --room ID speak --file - [--request-id ID]",
        "yield" => "conch --room ID yield",
        "grant" => "conch grant --to AGENT --to-node NODE_ID [--room ID]",
        "yank" => "conch --room ID yank",
        "config" => {
            "conch --room ID config [--mode stick|moderator] [--moderator ID --moderator-node NODE_ID] [--timeout SECS] [--stake-json JSON]"
        }
        "breakout" => "conch --room ID breakout --name NAME [--members NODE_ID,...]",
        "blob" => "conch --room ID blob put FILE",
        "leave" => "conch --room ID leave [--vacate]",
        "mcp" => "conch [--room ID] --agent ID mcp",
        "setup" => {
            "conch setup <claude|codex|grok|cursor|gemini|opencode> [--agent ID] [--scope user|project] [--env K=V ...] [--dry-run]\n\
             Example: conch setup claude"
        }
        "up" => {
            "conch up [--service]\n\
             Example: conch up --service   # start now and on login"
        }
        "down" => {
            "conch down [--service]\n\
             Example: conch down --service   # stop and remove the login service"
        }
        "doctor" => "conch doctor\nExample: conch doctor   # exit 1 if anything is red",
        _ => return Err(format!("unknown command: {command}")),
    };
    println!("{usage}");
    Ok(())
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

fn format_reply_error(reply: &ClientReply, command: &str) -> String {
    reply.error.as_ref().map_or_else(
        || "daemon returned an unspecified error".into(),
        |error| match conch::remedy::for_code(&error.code, command) {
            Some(remedy) => format!("{}: {}\n{remedy}", error.code, error.message),
            None => format!("{}: {}", error.code, error.message),
        },
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

fn slug(name: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in name.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }
    if slug.is_empty() {
        "room".into()
    } else {
        slug
    }
}

/// Where the CLI reads and writes local state (the current-room marker, pid files, logs).
fn data_dir() -> PathBuf {
    env::var_os("CONCH_DATA_DIR").map_or_else(
        || {
            env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".conch")
        },
        PathBuf::from,
    )
}

fn read_current_room() -> Option<String> {
    let bytes = fs::read(data_dir().join("current-room")).ok()?;
    serde_json::from_slice::<RoomId>(&bytes)
        .ok()
        .map(|room| room.to_string())
}

/// Resolve a `use` query against the no-room `status` summaries: full id, then
/// unique id prefix, then exact (case-sensitive) name. `Err` names the room that
/// couldn't be resolved, or lists every candidate when the query is ambiguous.
fn resolve_room_query(rooms: &[Value], query: &str) -> Result<Value, String> {
    let matches = |pick: &dyn Fn(&Value) -> bool| -> Vec<Value> {
        rooms.iter().filter(|room| pick(room)).cloned().collect()
    };
    let mut found = matches(&|room| room["id"].as_str() == Some(query));
    if found.is_empty() {
        found = matches(&|room| room["id"].as_str().is_some_and(|id| id.starts_with(query)));
    }
    if found.is_empty() {
        found = matches(&|room| room["name"].as_str() == Some(query));
    }
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(format!("no room matches \"{query}\"; run `conch rooms`")),
        many => {
            let names = many
                .iter()
                .map(|room| {
                    format!(
                        "{} \"{}\"",
                        conch::render::short(room["id"].as_str().unwrap_or("")),
                        room["name"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!("\"{query}\" is ambiguous: {names}"))
        }
    }
}

/// Write the `current-room` marker atomically, mirroring the daemon's own write.
fn write_current_room(id: &RoomId) -> io::Result<()> {
    let dir = data_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join("current-room");
    let tmp = path.with_extension(format!("tmp-{}", hex_string(&random::<[u8; 8]>())));
    fs::write(&tmp, serde_json::to_vec(id)?)?;
    fs::rename(&tmp, &path)
}

/// The terminal width used to cut `--oneline` text; 120 columns when unknown.
fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(120)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::{timeout, Duration},
    };

    async fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        String::from_utf8(request).unwrap()
    }

    #[tokio::test]
    async fn ticket_fetch_follows_only_bounded_same_origin_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let request = read_request(&mut first).await;
            assert!(request.contains("authorization: Bearer "));
            first
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: /ticket\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let (mut second, _) = listener.accept().await.unwrap();
            let request = read_request(&mut second).await;
            assert!(request.starts_with("GET /ticket HTTP/1.1\r\n"));
            assert!(request.contains("authorization: Bearer "));
            second
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
        });
        let token = Hash32::from_bytes([7; 32]);
        assert_eq!(
            fetch_ticket(&format!("http://{address}/start"), Some(token), None)
                .await
                .unwrap(),
            b"{}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn ticket_fetch_rejects_cross_origin_before_forwarding_capability() {
        let source = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_address = source.local_addr().unwrap();
        let target_address = target.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = source.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert!(request.contains("authorization: Bearer "));
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/ticket\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let token = Hash32::from_bytes([9; 32]);
        let error = fetch_ticket(&format!("http://{source_address}/start"), Some(token), None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("changed origin"));
        server.await.unwrap();
        assert!(timeout(Duration::from_millis(100), target.accept())
            .await
            .is_err());
    }

    async fn redirect_chain(redirects: usize) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = (redirects + 1).min(6);
        let server = tokio::spawn(async move {
            for index in 0..attempts {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                assert!(request.contains("authorization: Bearer "));
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
    async fn cli_ticket_fetch_accepts_five_redirects_and_rejects_the_sixth() {
        let token = Hash32::from_bytes([11; 32]);
        let (allowed, allowed_server) = redirect_chain(5).await;
        assert_eq!(
            fetch_ticket(&format!("http://{allowed}/start"), Some(token), None)
                .await
                .unwrap(),
            b"{}"
        );
        allowed_server.await.unwrap();

        let (rejected, rejected_server) = redirect_chain(6).await;
        let error = fetch_ticket(&format!("http://{rejected}/start"), Some(token), None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("redirect limit exceeded"));
        rejected_server.await.unwrap();
    }

    fn parse(args: &[&str]) -> Arguments {
        Arguments::parse(args.iter().map(|s| s.to_string())).unwrap()
    }

    #[test]
    fn globals_are_accepted_after_the_command() {
        let before = parse(&[
            "--room",
            "0101010101010101010101010101010101010101010101010101010101010101",
            "--agent",
            "agent:x",
            "history",
            "--from",
            "3",
        ]);
        let after = parse(&[
            "history",
            "--from",
            "3",
            "--room",
            "0101010101010101010101010101010101010101010101010101010101010101",
            "--agent",
            "agent:x",
        ]);
        assert_eq!(before.agent, after.agent);
        assert_eq!(before.room, after.room);
        assert!(
            matches!(after.request, ParsedRequest::Ready(ref r) if matches!(**r, ClientRequest::History { from_n: 3, .. }))
        );
        assert!(!after.json);
        assert!(parse(&["status", "--json"]).json);
        assert!(parse(&["--json", "status"]).json);
    }

    #[test]
    fn a_commands_own_flags_win_over_globals() {
        // setup owns --agent; the global agent stays the default.
        let setup = parse(&["setup", "claude", "--agent", "agent:custom"]);
        assert!(matches!(
            setup.request,
            ParsedRequest::Local(LocalCommand::Setup { ref agent, .. }) if agent.as_deref() == Some("agent:custom")
        ));
        assert!(setup.agent.as_str().starts_with("human:"));
    }

    #[test]
    fn grant_takes_to_and_to_node() {
        let node = "0202020202020202020202020202020202020202020202020202020202020202";
        let room = "0101010101010101010101010101010101010101010101010101010101010101";
        let grant = parse(&[
            "grant",
            "--to",
            "agent:codex",
            "--to-node",
            node,
            "--room",
            room,
        ]);
        match grant.request {
            ParsedRequest::Ready(request) => match *request {
                ClientRequest::Grant { to, .. } => {
                    assert_eq!(to.agent.as_str(), "agent:codex");
                    assert_eq!(to.node.to_string(), node);
                }
                other => panic!("{other:?}"),
            },
            _ => panic!("grant is a ready request"),
        }
        let old = Arguments::parse(
            ["grant", "--agent", "x", "--node", node, "--room", room]
                .iter()
                .map(|s| s.to_string()),
        );
        assert!(old.is_err(), "the old grant flags are gone");
    }

    fn room(id: &str, name: &str) -> Value {
        serde_json::json!({ "id": id, "name": name })
    }

    /// Two ids sharing the "aaaa" prefix, plus a third that doesn't, so the
    /// ambiguous and unique-prefix cases are deterministic instead of depending
    /// on randomly generated room ids happening to collide.
    fn sample_rooms() -> Vec<Value> {
        vec![
            room(&format!("aaaa1111{}", "0".repeat(56)), "Alpha"),
            room(&format!("aaaa2222{}", "0".repeat(56)), "Beta"),
            room(&format!("bbbb3333{}", "0".repeat(56)), "Gamma"),
        ]
    }

    #[test]
    fn resolve_room_query_prefers_a_full_id_match_even_when_its_prefix_is_ambiguous() {
        let rooms = sample_rooms();
        let full_id = format!("aaaa1111{}", "0".repeat(56));
        let found = resolve_room_query(&rooms, &full_id).unwrap();
        assert_eq!(found["name"], "Alpha");
    }

    #[test]
    fn resolve_room_query_resolves_a_unique_prefix() {
        let rooms = sample_rooms();
        let found = resolve_room_query(&rooms, "aaaa11").unwrap();
        assert_eq!(found["name"], "Alpha");
    }

    #[test]
    fn resolve_room_query_resolves_an_exact_name() {
        let rooms = sample_rooms();
        let found = resolve_room_query(&rooms, "Beta").unwrap();
        assert_eq!(found["name"], "Beta");
    }

    #[test]
    fn resolve_room_query_name_matching_is_case_sensitive() {
        let rooms = sample_rooms();
        let error = resolve_room_query(&rooms, "beta").unwrap_err();
        assert!(error.contains("no room matches \"beta\""), "{error}");
    }

    #[test]
    fn resolve_room_query_lists_every_candidate_when_a_prefix_is_ambiguous() {
        let rooms = sample_rooms();
        let error = resolve_room_query(&rooms, "aaaa").unwrap_err();
        assert!(error.contains("\"aaaa\" is ambiguous:"), "{error}");
        assert!(error.contains("Alpha") && error.contains("Beta"), "{error}");
        assert!(!error.contains("Gamma"), "{error}");
    }

    #[test]
    fn resolve_room_query_reports_an_unknown_query() {
        let rooms = sample_rooms();
        let error = resolve_room_query(&rooms, "Nowhere").unwrap_err();
        assert_eq!(error, "no room matches \"Nowhere\"; run `conch rooms`");
    }

    #[test]
    fn default_identity_is_a_sanitised_human_username() {
        assert_eq!(sanitise_username("Ray.Hwang"), "ray-hwang");
        assert_eq!(sanitise_username("ray_h-1"), "ray_h-1");
        assert_eq!(sanitise_username("réné"), "r-n-");
        assert_eq!(sanitise_username(""), "operator");
        assert_eq!(default_agent_from(Some("Ray"), None), "human:ray");
        assert_eq!(default_agent_from(None, Some("ops")), "human:ops");
        assert_eq!(default_agent_from(None, None), "human:operator");
    }
}
