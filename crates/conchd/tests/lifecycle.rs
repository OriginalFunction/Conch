use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::PathBuf,
    time::Duration,
};

use conch_core::{
    client::{ClientReply, ClientRequest},
    frame,
    types::AgentId,
};
use conch_launch::{spawn_detached, PidFile, SpawnOptions};
use conchd::tcp::Daemon;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

async fn request(addr: SocketAddr, request: &ClientRequest) -> ClientReply {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    for message in [
        &ClientRequest::Attach {
            agent: AgentId::new("agent:test").unwrap(),
        },
        request,
    ] {
        stream
            .write_all(&frame::encode(message).unwrap())
            .await
            .unwrap();
        let length = stream.read_u32().await.unwrap() as usize;
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload).await.unwrap();
        let reply: ClientReply = frame::decode_payload(&payload).unwrap();
        if matches!(message, ClientRequest::Attach { .. }) {
            assert!(reply.ok);
        } else {
            return reply;
        }
    }
    unreachable!()
}

#[tokio::test]
async fn version_request_reports_daemon_version() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon
        .start(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .unwrap();
    let reply = request(server.addr(), &ClientRequest::Version).await;
    assert!(reply.ok);
    assert_eq!(reply.data.unwrap()["version"], env!("CARGO_PKG_VERSION"));
    server.abort();
}

#[test]
fn daemon_binary_writes_pid_file_and_removes_it_on_sigterm() {
    let data = TempDir::new().unwrap();
    let options = SpawnOptions {
        conchd: PathBuf::from(env!("CARGO_BIN_EXE_conchd")),
        data_dir: data.path().to_path_buf(),
        tcp: free_port(),
        http: free_port(),
    };
    let pid = spawn_detached(&options).unwrap();
    let file = PidFile::read(data.path()).expect("pid file written at startup");
    assert_eq!(file.pid, pid);
    assert_eq!(file.tcp, options.tcp);
    assert!(file.is_alive());
    file.stop(Duration::from_secs(5)).unwrap();
    assert!(
        PidFile::read(data.path()).is_none(),
        "pid file removed on clean shutdown"
    );
}
