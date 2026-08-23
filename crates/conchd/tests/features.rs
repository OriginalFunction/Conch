use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use conch_core::{
    client::{ClientReply, ClientRequest},
    consensus::{Freeze, Hello, SwarmMsg},
    encoding::scene_hash,
    ticket::JoinRole,
    types::{AgentId, BlobRef, Body, FloorConfig, FloorMode, Hash32, Mouth, StakePolicy},
};
use conchd::tcp::{read_frame, write_frame, Daemon};
use tempfile::TempDir;
use tokio::{io::AsyncWriteExt, net::TcpStream, time::Duration};

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

async fn attach(addr: SocketAddr, agent: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    write_frame(
        &mut stream,
        &ClientRequest::Attach {
            agent: AgentId::new(agent).unwrap(),
        },
    )
    .await
    .unwrap();
    assert!(
        read_frame::<_, ClientReply>(&mut stream)
            .await
            .unwrap()
            .unwrap()
            .ok
    );
    stream
}

async fn request(stream: &mut TcpStream, request: ClientRequest) -> ClientReply {
    write_frame(stream, &request).await.unwrap();
    read_frame(stream).await.unwrap().unwrap()
}

#[tokio::test]
async fn non_moderator_grant_not_moderator() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let moderator = AgentId::new("human:operator").unwrap();
    let ticket = daemon
        .create_ticket(
            "moderated",
            StakePolicy::default(),
            FloorConfig {
                mode: FloorMode::Moderator,
                timeout_secs: 30,
                moderator: Some(Mouth {
                    agent: moderator.clone(),
                    node: daemon.node_id(),
                }),
            },
        )
        .unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let target = Mouth {
        agent: AgentId::new("agent:writer").unwrap(),
        node: daemon.node_id(),
    };

    let mut intruder = attach(server.addr(), "human:intruder").await;
    let denied = request(
        &mut intruder,
        ClientRequest::Grant {
            room: ticket.id,
            to: target.clone(),
        },
    )
    .await;
    assert!(!denied.ok);
    assert_eq!(denied.error.unwrap().code, "not_moderator");

    let mut operator = attach(server.addr(), moderator.as_str()).await;
    let granted = request(
        &mut operator,
        ClientRequest::Grant {
            room: ticket.id,
            to: target.clone(),
        },
    )
    .await;
    assert!(granted.ok, "{granted:?}");
    assert_eq!(
        daemon
            .replay(ticket.id)
            .unwrap()
            .chain
            .live_grant
            .unwrap()
            .to,
        target
    );
}

#[tokio::test]
async fn breakout_auto_join_shares_child_genesis() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket("parent", StakePolicy::default(), FloorConfig::stick(30))
        .unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let mut client = attach(server.addr(), "agent:builder").await;

    assert!(
        request(&mut client, ClientRequest::RaiseHand { room: ticket.id })
            .await
            .ok
    );
    let reply = request(
        &mut client,
        ClientRequest::Breakout {
            room: ticket.id,
            name: "side room".into(),
            members: None,
        },
    )
    .await;
    assert!(reply.ok, "{reply:?}");
    let child_ticket: conch_core::ticket::Ticket =
        serde_json::from_value(reply.data.unwrap()["ticket"].clone()).unwrap();

    let parent = daemon.replay(ticket.id).unwrap();
    let parent_record = parent.history.last().unwrap();
    match &parent_record.scene.body {
        Body::Breakout {
            ticket: embedded,
            auto_join,
            ..
        } => {
            assert_eq!(
                serde_json::from_value::<conch_core::ticket::Ticket>(embedded.clone()).unwrap(),
                child_ticket
            );
            assert_eq!(auto_join, &[daemon.node_id()]);
        }
        body => panic!("expected breakout, got {body:?}"),
    }

    let child = daemon.replay(child_ticket.id).unwrap();
    assert_eq!(child.history.len(), 1);
    let child_genesis = &child.history[0].scene;
    match &child_genesis.body {
        Body::Genesis { parent_room, .. } => assert_eq!(*parent_room, Some(ticket.id)),
        body => panic!("expected child genesis, got {body:?}"),
    }
    assert_eq!(
        child_ticket.genesis,
        Hash32::from_bytes(scene_hash(&serde_json::to_value(child_genesis).unwrap()))
    );
}

#[tokio::test]
async fn blob_put_is_durable_before_speech_certification() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket("blobs", StakePolicy::default(), FloorConfig::stick(30))
        .unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let mut client = attach(server.addr(), "agent:writer").await;

    assert!(
        request(&mut client, ClientRequest::RaiseHand { room: ticket.id })
            .await
            .ok
    );
    let bytes = b"durable attachment";
    write_frame(
        &mut client,
        &ClientRequest::PutBlob {
            room: ticket.id,
            name: "note.txt".into(),
            bytes: bytes.len() as u64,
        },
    )
    .await
    .unwrap();
    client.write_u32(bytes.len() as u32).await.unwrap();
    client.write_all(bytes).await.unwrap();
    client.flush().await.unwrap();
    let put: ClientReply = read_frame(&mut client).await.unwrap().unwrap();
    assert!(put.ok, "{put:?}");
    let blob: BlobRef = serde_json::from_value(put.data.unwrap()).unwrap();
    assert_eq!(
        fs::read(
            data.path()
                .join("rooms")
                .join(ticket.id.to_string())
                .join("blobs")
                .join(blob.sha256.to_string())
        )
        .unwrap(),
        bytes
    );

    assert!(
        request(
            &mut client,
            ClientRequest::Speak {
                room: ticket.id,
                text: "with an attachment".into(),
                request_id: "00000000000000000000000000000001".into(),
            }
        )
        .await
        .ok
    );
    let yielded = request(&mut client, ClientRequest::Yield { room: ticket.id }).await;
    assert!(yielded.ok, "{yielded:?}");
    match &daemon
        .replay(ticket.id)
        .unwrap()
        .history
        .last()
        .unwrap()
        .scene
        .body
    {
        Body::Speech { blobs, .. } => assert_eq!(blobs, &[blob]),
        body => panic!("expected speech, got {body:?}"),
    }
}

#[tokio::test]
async fn unsigned_hello_cannot_spoof_leader_freeze() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket(
            "freeze auth",
            StakePolicy::default(),
            FloorConfig::stick(30),
        )
        .unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let mut holder = attach(server.addr(), "agent:holder").await;
    assert!(
        request(&mut holder, ClientRequest::RaiseHand { room: ticket.id })
            .await
            .ok
    );
    assert!(
        request(
            &mut holder,
            ClientRequest::Speak {
                room: ticket.id,
                text: "private draft".into(),
                request_id: "00000000000000000000000000000002".into(),
            }
        )
        .await
        .ok
    );
    let grant_hash = daemon
        .replay(ticket.id)
        .unwrap()
        .chain
        .live_grant
        .unwrap()
        .hash;

    let mut attacker = TcpStream::connect(server.addr()).await.unwrap();
    let node = daemon.node_id();
    write_frame(
        &mut attacker,
        &SwarmMsg::Hello(Hello {
            node,
            r#pub: node,
            addrs: Vec::new(),
            decl: Vec::new(),
        }),
    )
    .await
    .unwrap();
    let _: SwarmMsg = read_frame(&mut attacker).await.unwrap().unwrap();
    let _: SwarmMsg = read_frame(&mut attacker).await.unwrap().unwrap();
    write_frame(
        &mut attacker,
        &SwarmMsg::Freeze(Freeze {
            room: ticket.id,
            grant_hash,
        }),
    )
    .await
    .unwrap();
    assert!(tokio::time::timeout(
        Duration::from_millis(100),
        read_frame::<_, SwarmMsg>(&mut attacker)
    )
    .await
    .is_err());
}

#[tokio::test]
async fn history_follow_streams_each_new_committed_batch() {
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket("follow", StakePolicy::default(), FloorConfig::stick(30))
        .unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let mut follower = attach(server.addr(), "agent:reader").await;
    write_frame(
        &mut follower,
        &ClientRequest::History {
            room: ticket.id,
            from_n: 0,
            follow: true,
        },
    )
    .await
    .unwrap();
    let initial: ClientReply = read_frame(&mut follower).await.unwrap().unwrap();
    assert_eq!(initial.data.unwrap().as_array().unwrap().len(), 1);

    let mut writer = attach(server.addr(), "agent:writer").await;
    assert!(
        request(&mut writer, ClientRequest::RaiseHand { room: ticket.id })
            .await
            .ok
    );
    let update: ClientReply =
        tokio::time::timeout(Duration::from_secs(1), read_frame(&mut follower))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    let records = update.data.unwrap();
    assert_eq!(records.as_array().unwrap().len(), 1);
    assert_eq!(records[0]["scene"]["body"]["type"], "grant");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_daemon_staker_wrap_uses_network_votes_and_certs() {
    let source_data = TempDir::new().unwrap();
    let follower_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let follower = Daemon::open(follower_data.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let follower_server = follower.start(loopback()).await.unwrap();
    let ticket = source
        .create_ticket(
            "network wrap",
            StakePolicy::default(),
            FloorConfig::stick(30),
        )
        .unwrap();
    follower
        .join_ticket(ticket.clone(), JoinRole::Stake)
        .await
        .unwrap();
    assert_eq!(source.replay(ticket.id).unwrap().chain.roster.len(), 2);
    assert_eq!(follower.replay(ticket.id).unwrap().chain.roster.len(), 2);

    let mut writer = attach(follower_server.addr(), "agent:remote").await;
    assert!(
        request(&mut writer, ClientRequest::RaiseHand { room: ticket.id })
            .await
            .ok
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if follower
                .replay(ticket.id)
                .unwrap()
                .chain
                .live_grant
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let attachment = b"network blob";
    write_frame(
        &mut writer,
        &ClientRequest::PutBlob {
            room: ticket.id,
            name: "network.txt".into(),
            bytes: attachment.len() as u64,
        },
    )
    .await
    .unwrap();
    writer.write_u32(attachment.len() as u32).await.unwrap();
    writer.write_all(attachment).await.unwrap();
    writer.flush().await.unwrap();
    let put: ClientReply = read_frame(&mut writer).await.unwrap().unwrap();
    assert!(put.ok, "{put:?}");
    let blob: BlobRef = serde_json::from_value(put.data.unwrap()).unwrap();
    let spoke = request(
        &mut writer,
        ClientRequest::Speak {
            room: ticket.id,
            text: "wrapped by two real daemons".into(),
            request_id: "00000000000000000000000000000003".into(),
        },
    )
    .await;
    assert!(spoke.ok, "{spoke:?}");
    let yielded = request(&mut writer, ClientRequest::Yield { room: ticket.id }).await;
    assert!(yielded.ok, "{yielded:?}");

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if source.replay(ticket.id).unwrap().chain.head_n
                == follower.replay(ticket.id).unwrap().chain.head_n
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let source_history = source.replay(ticket.id).unwrap().history;
    let follower_history = follower.replay(ticket.id).unwrap().history;
    assert_eq!(source_history, follower_history);
    assert_eq!(source_history.len(), 4);
    assert!(matches!(
        &source_history[3].scene.body,
        Body::Speech { text, blobs, .. }
            if text == "wrapped by two real daemons" && blobs == std::slice::from_ref(&blob)
    ));
    assert_eq!(
        fs::read(
            source_data
                .path()
                .join("rooms")
                .join(ticket.id.to_string())
                .join("blobs")
                .join(blob.sha256.to_string())
        )
        .unwrap(),
        attachment
    );
    assert_eq!(
        ticket.peers,
        vec![format!("tcp://{}", source_server.addr())]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn two_of_three_live_stakers_can_wrap() {
    let source_data = TempDir::new().unwrap();
    let second_data = TempDir::new().unwrap();
    let third_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let second = Daemon::open(second_data.path()).unwrap();
    let third = Daemon::open(third_data.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let _second_server = second.start(loopback()).await.unwrap();
    let third_server = third.start(loopback()).await.unwrap();
    let ticket = source
        .create_ticket(
            "three stakers",
            StakePolicy::default(),
            FloorConfig::stick(30),
        )
        .unwrap();
    second
        .join_ticket(ticket.clone(), JoinRole::Stake)
        .await
        .unwrap();
    third
        .join_ticket(ticket.clone(), JoinRole::Stake)
        .await
        .unwrap();
    assert_eq!(source.replay(ticket.id).unwrap().chain.roster.len(), 3);
    assert_eq!(second.replay(ticket.id).unwrap().chain.roster.len(), 3);
    assert_eq!(third.replay(ticket.id).unwrap().chain.roster.len(), 3);
    third_server.abort();
    drop(third_server);
    drop(third);

    let mut writer = attach(source_server.addr(), "agent:quorum").await;
    let raised = request(&mut writer, ClientRequest::RaiseHand { room: ticket.id }).await;
    assert!(raised.ok, "{raised:?}");
    let spoke = request(
        &mut writer,
        ClientRequest::Speak {
            room: ticket.id,
            text: "majority remains".into(),
            request_id: "00000000000000000000000000000004".into(),
        },
    )
    .await;
    assert!(spoke.ok, "{spoke:?}");
    let yielded = request(&mut writer, ClientRequest::Yield { room: ticket.id }).await;
    assert!(yielded.ok, "{yielded:?}");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if source.replay(ticket.id).unwrap().chain.head_n
                == second.replay(ticket.id).unwrap().chain.head_n
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        source.replay(ticket.id).unwrap().history,
        second.replay(ticket.id).unwrap().history
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_of_two_live_stakers_cannot_wrap() {
    let source_data = TempDir::new().unwrap();
    let follower_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let follower = Daemon::open(follower_data.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let follower_server = follower.start(loopback()).await.unwrap();
    let ticket = source
        .create_ticket(
            "two stakers",
            StakePolicy::default(),
            FloorConfig::stick(30),
        )
        .unwrap();
    follower
        .join_ticket(ticket.clone(), JoinRole::Stake)
        .await
        .unwrap();
    assert_eq!(source.replay(ticket.id).unwrap().chain.head_n, Some(1));
    follower_server.abort();
    drop(follower_server);
    drop(follower);

    let mut writer = attach(source_server.addr(), "agent:alone").await;
    let raised = request(&mut writer, ClientRequest::RaiseHand { room: ticket.id }).await;
    assert!(!raised.ok);
    assert_eq!(raised.error.unwrap().code, "unavailable");
    let replay = source.replay(ticket.id).unwrap();
    assert_eq!(replay.chain.head_n, Some(1));
    assert_eq!(replay.pending.unwrap().n, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarted_network_leader_carries_exact_pending_hash() {
    let source_data = TempDir::new().unwrap();
    let follower_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let follower = Daemon::open(follower_data.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let follower_server = follower.start(loopback()).await.unwrap();
    let ticket = source
        .create_ticket(
            "carry forward",
            StakePolicy::default(),
            FloorConfig::stick(30),
        )
        .unwrap();
    follower
        .join_ticket(ticket.clone(), JoinRole::Stake)
        .await
        .unwrap();
    follower_server.abort();
    drop(follower_server);
    drop(follower);

    let mut writer = attach(source_server.addr(), "agent:carry").await;
    let first = request(&mut writer, ClientRequest::RaiseHand { room: ticket.id }).await;
    assert!(!first.ok);
    let accepted_hash = source.replay(ticket.id).unwrap().pending.unwrap().hash;
    source_server.abort();
    drop(source_server);
    drop(source);

    let source = Daemon::open(source_data.path()).unwrap();
    let follower = Daemon::open(follower_data.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let _follower_server = follower.start(loopback()).await.unwrap();
    // Dynamic test ports changed; refresh the peer address learned by the source.
    follower
        .sync_room_from(source_server.addr(), ticket.id)
        .await
        .unwrap();
    let mut writer = attach(source_server.addr(), "agent:carry").await;
    let retried = request(&mut writer, ClientRequest::RaiseHand { room: ticket.id }).await;
    assert!(retried.ok, "{retried:?}");
    let replay = source.replay(ticket.id).unwrap();
    assert_eq!(replay.chain.head_n, Some(2));
    assert_eq!(replay.chain.head_hash, Some(accepted_hash));
    assert_eq!(replay.history.last().unwrap().commit_proof.rpc_term, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn network_breakout_autojoins_listed_peer() {
    let source_data = TempDir::new().unwrap();
    let holder_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let holder = Daemon::open(holder_data.path()).unwrap();
    let _source_server = source.start(loopback()).await.unwrap();
    let holder_server = holder.start(loopback()).await.unwrap();
    let parent = source
        .create_ticket(
            "breakout parent",
            StakePolicy::default(),
            FloorConfig::stick(30),
        )
        .unwrap();
    holder
        .join_ticket(parent.clone(), JoinRole::Stake)
        .await
        .unwrap();
    let mut client = attach(holder_server.addr(), "agent:holder").await;
    assert!(
        request(&mut client, ClientRequest::RaiseHand { room: parent.id })
            .await
            .ok
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while holder.replay(parent.id).unwrap().chain.live_grant.is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let breakout = request(
        &mut client,
        ClientRequest::Breakout {
            room: parent.id,
            name: "child".into(),
            members: None,
        },
    )
    .await;
    assert!(breakout.ok, "{breakout:?}");
    let child: conch_core::ticket::Ticket =
        serde_json::from_value(breakout.data.unwrap()["ticket"].clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if source
                .replay(child.id)
                .is_ok_and(|replay| replay.chain.roster.len() == 2)
                && holder
                    .replay(child.id)
                    .is_ok_and(|replay| replay.chain.roster.len() == 2)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        source.replay(child.id).unwrap().history,
        holder.replay(child.id).unwrap().history
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn leader_self_removal_pushes_commit_then_cannot_campaign() {
    let source_data = TempDir::new().unwrap();
    let second_data = TempDir::new().unwrap();
    let third_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let second = Daemon::open(second_data.path()).unwrap();
    let third = Daemon::open(third_data.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let _second_server = second.start(loopback()).await.unwrap();
    let _third_server = third.start(loopback()).await.unwrap();
    let ticket = source
        .create_ticket("leave", StakePolicy::default(), FloorConfig::stick(30))
        .unwrap();
    second
        .join_ticket(ticket.clone(), JoinRole::Stake)
        .await
        .unwrap();
    third
        .join_ticket(ticket.clone(), JoinRole::Stake)
        .await
        .unwrap();
    let removed = source.node_id();
    let mut client = attach(source_server.addr(), "agent:leaver").await;
    let left = request(
        &mut client,
        ClientRequest::Leave {
            room: ticket.id,
            vacate: false,
        },
    )
    .await;
    assert!(left.ok, "{left:?}");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if second
                .replay(ticket.id)
                .is_ok_and(|replay| !replay.chain.roster.contains(&removed))
                && third
                    .replay(ticket.id)
                    .is_ok_and(|replay| !replay.chain.roster.contains(&removed))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(!source.can_certify(ticket.id).unwrap());
    assert_eq!(source.replay(ticket.id).unwrap().chain.roster.len(), 2);
    assert_eq!(
        source.replay(ticket.id).unwrap().history,
        second.replay(ticket.id).unwrap().history
    );
}
