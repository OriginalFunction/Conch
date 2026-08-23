use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use conch_core::{
    client::ClientReply,
    frame,
    ticket::JoinRole,
    types::{AgentId, FloorConfig, Hash32, StakePolicy},
};
use conchd::tcp::{read_frame, Daemon};
use tempfile::TempDir;
use tokio::{io::AsyncWriteExt, net::TcpStream};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_daemons_replicate_genesis() {
    let source_dir = TempDir::new().unwrap();
    let follower_dir = TempDir::new().unwrap();
    let source = Daemon::open(source_dir.path()).unwrap();
    let follower = Daemon::open(follower_dir.path()).unwrap();
    let room = source.create_genesis("transport test").unwrap();
    follower.track_room(room).unwrap();

    let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let source_server = source.start(bind).await.unwrap();
    let _follower_server = follower.start(bind).await.unwrap();
    let source_replay = source.replay(room).unwrap();

    let caught_up = follower
        .sync_room_from(source_server.addr(), room)
        .await
        .unwrap();
    let follower_replay = follower.replay(room).unwrap();

    assert_eq!(caught_up, source_replay.chain);
    assert_eq!(follower_replay.chain, source_replay.chain);
    assert_eq!(follower_replay.history, source_replay.history);
    assert_eq!(follower_replay.consensus.current_term, 1);
}

#[tokio::test]
async fn roster_member_cannot_rejoin_as_observer_before_removal() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket(
            "observer test",
            StakePolicy::default(),
            FloorConfig::stick(30),
        )
        .unwrap();
    assert!(daemon.can_certify(ticket.id).unwrap());

    let error = daemon
        .join_ticket(ticket.clone(), JoinRole::Observe)
        .await
        .unwrap_err();
    assert!(matches!(error, conchd::tcp::DaemonError::InvalidJoinRole));
    assert!(daemon.can_certify(ticket.id).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_genesis_pin_is_bad_ticket_and_leaves_no_half_room() {
    let source_dir = TempDir::new().unwrap();
    let follower_dir = TempDir::new().unwrap();
    let source = Daemon::open(source_dir.path()).unwrap();
    let follower = Daemon::open(follower_dir.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let mut ticket = source
        .create_ticket("bad pin", StakePolicy::default(), FloorConfig::stick(30))
        .unwrap();
    ticket.genesis = Hash32::from_bytes([0x99; 32]);
    assert_eq!(
        ticket.peers,
        vec![format!("tcp://{}", source_server.addr())]
    );

    let error = follower
        .join_ticket(ticket.clone(), JoinRole::Stake)
        .await
        .unwrap_err();
    assert!(matches!(error, conchd::tcp::DaemonError::BadTicket(_)));
    assert!(!follower_dir
        .path()
        .join("rooms")
        .join(ticket.id.to_string())
        .exists());
    assert!(matches!(
        follower.replay(ticket.id),
        Err(conchd::tcp::DaemonError::UnknownRoom(_))
    ));
}

#[tokio::test]
async fn malformed_client_request_gets_an_invalid_reply() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let mut stream = TcpStream::connect(server.addr()).await.unwrap();
    stream
        .write_all(
            &frame::encode(&serde_json::json!({
                "typ": "attach",
                "agent": AgentId::new("local").unwrap(),
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let attached = read_frame::<_, ClientReply>(&mut stream)
        .await
        .unwrap()
        .unwrap();
    assert!(attached.ok);

    stream
        .write_all(
            &frame::encode(&serde_json::json!({
                "typ": "status",
                "room": null,
                "unexpected": true,
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let reply = read_frame::<_, ClientReply>(&mut stream)
        .await
        .unwrap()
        .unwrap();
    assert!(!reply.ok);
    assert_eq!(reply.error.unwrap().code, "invalid");
}

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}
