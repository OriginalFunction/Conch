use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use conchd::tcp::Daemon;
use tempfile::TempDir;

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
