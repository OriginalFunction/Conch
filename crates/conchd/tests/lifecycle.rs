use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Command,
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

/// Two loopback ports that stay reserved until the caller drops the listeners, closing
/// the window in which another test (or anything else on the machine) could take them.
fn reserve_ports() -> (SocketAddr, SocketAddr, (TcpListener, TcpListener)) {
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let http = TcpListener::bind("127.0.0.1:0").unwrap();
    let addrs = (tcp.local_addr().unwrap(), http.local_addr().unwrap());
    (addrs.0, addrs.1, (tcp, http))
}

/// Stops whatever the pid file names when the test ends, so a failing assertion
/// never leaks a daemon.
struct DaemonGuard(PathBuf);

impl DaemonGuard {
    fn new(data_dir: &Path) -> Self {
        Self(data_dir.to_path_buf())
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(pid) = PidFile::read(&self.0) {
            let _ = pid.stop(Duration::from_secs(5));
        }
    }
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

#[tokio::test]
async fn a_daemon_that_cannot_bind_leaves_the_running_daemon_alone() {
    let data = TempDir::new().unwrap();
    let _guard = DaemonGuard::new(data.path());
    let (tcp, http, reserved) = reserve_ports();
    let options = SpawnOptions {
        conchd: PathBuf::from(env!("CARGO_BIN_EXE_conchd")),
        data_dir: data.path().to_path_buf(),
        tcp,
        http,
    };
    drop(reserved);
    let first = spawn_detached(&options).unwrap();

    // A second daemon aimed at the same data dir and ports cannot bind.
    let second = Command::new(env!("CARGO_BIN_EXE_conchd"))
        .arg("--localhost")
        .arg("--data-dir")
        .arg(data.path())
        .arg("--tcp")
        .arg(tcp.to_string())
        .arg("--http")
        .arg(http.to_string())
        .output()
        .unwrap();
    assert!(
        !second.status.success(),
        "second daemon should refuse to start: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    // It must not have taken over, or deleted, the running daemon's pid file.
    let file = PidFile::read(data.path()).expect("first daemon's pid file survives");
    assert_eq!(file.pid, first);
    assert!(file.is_alive());
    let reply = request(tcp, &ClientRequest::Version).await;
    assert!(reply.ok);
    assert_eq!(reply.data.unwrap()["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn daemon_binary_writes_pid_file_and_removes_it_on_sigterm() {
    let data = TempDir::new().unwrap();
    let _guard = DaemonGuard::new(data.path());
    let (tcp, http, reserved) = reserve_ports();
    let options = SpawnOptions {
        conchd: PathBuf::from(env!("CARGO_BIN_EXE_conchd")),
        data_dir: data.path().to_path_buf(),
        tcp,
        http,
    };
    drop(reserved);
    let pid = spawn_detached(&options).unwrap();
    let file = PidFile::read(data.path()).expect("pid file written once listeners are bound");
    assert_eq!(file.pid, pid);
    assert_eq!(file.tcp, options.tcp);
    assert!(file.is_alive());
    file.stop(Duration::from_secs(5)).unwrap();
    assert!(
        PidFile::read(data.path()).is_none(),
        "pid file removed on clean shutdown"
    );
}
