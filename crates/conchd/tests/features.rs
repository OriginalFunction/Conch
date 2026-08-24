use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, OnceLock},
};

use conch_core::{
    client::{ClientReply, ClientRequest},
    consensus::{Freeze, HelloI, SwarmMsg},
    encoding::scene_hash,
    ticket::JoinRole,
    types::{
        AgentId, BlobRef, Body, FloorConfig, FloorMode, Hash32, Mouth, SignatureBytes, StakePolicy,
    },
};
use conchd::tcp::{read_frame, write_frame, Daemon, TransportMode};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use tempfile::TempDir;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Duration, Instant},
};
use tokio_rustls::rustls::{
    pki_types::PrivatePkcs8KeyDer, version::TLS13, ClientConfig, RootCertStore, ServerConfig,
};

fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

async fn network_test_guard() -> OwnedSemaphorePermit {
    static NETWORK_TESTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(NETWORK_TESTS.get_or_init(|| Arc::new(Semaphore::new(1))))
        .acquire_owned()
        .await
        .expect("network test semaphore remains open")
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
    let _network_test = network_test_guard().await;
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
    let missing_intent = request(
        &mut operator,
        ClientRequest::Grant {
            room: ticket.id,
            to: target.clone(),
        },
    )
    .await;
    assert!(!missing_intent.ok);
    assert_eq!(missing_intent.error.unwrap().code, "invalid");

    let mut writer = attach(server.addr(), target.agent.as_str()).await;
    let raised = request(&mut writer, ClientRequest::RaiseHand { room: ticket.id }).await;
    assert!(raised.ok, "{raised:?}");
    let intent_id: Hash32 =
        serde_json::from_value(raised.data.unwrap()["intent_id"].clone()).unwrap();

    let granted = request(
        &mut operator,
        ClientRequest::Grant {
            room: ticket.id,
            to: target.clone(),
        },
    )
    .await;
    assert!(granted.ok, "{granted:?}");
    let replay = daemon.replay(ticket.id).unwrap();
    assert_eq!(replay.chain.live_grant.unwrap().to, target);
    assert!(replay.chain.consumed_intents.contains(&intent_id));
}

#[tokio::test]
async fn breakout_auto_join_shares_child_genesis() {
    let _network_test = network_test_guard().await;
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
    let _network_test = network_test_guard().await;
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
async fn oversized_blob_declaration_replies_then_closes_transport() {
    let _network_test = network_test_guard().await;
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let ticket = daemon
        .create_ticket(
            "blob framing",
            StakePolicy::default(),
            FloorConfig::stick(30),
        )
        .unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let mut client = attach(server.addr(), "agent:writer").await;
    write_frame(
        &mut client,
        &ClientRequest::PutBlob {
            room: ticket.id,
            name: "too-large.bin".into(),
            bytes: 32 * 1024 * 1024 + 1,
        },
    )
    .await
    .unwrap();
    let reply: ClientReply = read_frame(&mut client).await.unwrap().unwrap();
    assert!(!reply.ok);
    assert_eq!(reply.error.unwrap().code, "invalid");
    assert!(read_frame::<_, ClientReply>(&mut client)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn unsigned_hello_cannot_spoof_leader_freeze() {
    let _network_test = network_test_guard().await;
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
        &SwarmMsg::HelloI(HelloI {
            label: "conch-swarm-v1".into(),
            kind: "hello_i".into(),
            v: 1,
            node,
            r#pub: node,
            nonce_i: Hash32::from_bytes([7; 32]),
            sig: SignatureBytes::from_bytes([0; 64]),
        }),
    )
    .await
    .unwrap();
    write_frame(
        &mut attacker,
        &SwarmMsg::Freeze(Freeze {
            room: ticket.id,
            grant_hash,
        }),
    )
    .await
    .unwrap();
    let response = tokio::time::timeout(
        Duration::from_millis(100),
        read_frame::<_, SwarmMsg>(&mut attacker),
    )
    .await;
    assert!(!matches!(response, Ok(Ok(Some(_)))));
}

#[tokio::test]
async fn reflected_live_hello_is_rejected_before_any_response_or_state_change() {
    let _network_test = network_test_guard().await;
    let data = TempDir::new().unwrap();
    let daemon = Daemon::open(data.path()).unwrap();
    let room = daemon.create_genesis("reflection").unwrap();
    let server = daemon.start(loopback()).await.unwrap();
    let attacker = TcpListener::bind(loopback()).await.unwrap();
    let attacker_addr = attacker.local_addr().unwrap();
    let before = daemon.replay(room).unwrap();

    let outbound_daemon = daemon.clone();
    let outbound = tokio::spawn(async move {
        let _ = outbound_daemon.sync_room_from(attacker_addr, room).await;
    });
    let (mut attacker_side, _) = attacker.accept().await.unwrap();
    let hello = read_frame::<_, SwarmMsg>(&mut attacker_side)
        .await
        .unwrap()
        .expect("the initiator sends hello_i");
    assert!(matches!(hello, SwarmMsg::HelloI(_)));

    let mut reflected = TcpStream::connect(server.addr()).await.unwrap();
    write_frame(&mut reflected, &hello).await.unwrap();
    let response = tokio::time::timeout(
        Duration::from_millis(250),
        read_frame::<_, SwarmMsg>(&mut reflected),
    )
    .await;
    assert!(
        matches!(response, Ok(Ok(None))),
        "a self-directed hello must be closed before hello_r"
    );
    drop(attacker_side);
    outbound.await.unwrap();

    let after = daemon.replay(room).unwrap();
    assert_eq!(after.chain.head_hash, before.chain.head_hash);
    assert_eq!(after.consensus, before.consensus);
    assert_eq!(after.pending, before.pending);
}

#[tokio::test]
async fn inbound_first_frame_and_node_handshake_share_one_five_second_deadline() {
    let _network_test = network_test_guard().await;
    let initiator_data = TempDir::new().unwrap();
    let target_data = TempDir::new().unwrap();
    let initiator = Daemon::open(initiator_data.path()).unwrap();
    let target = Daemon::open(target_data.path()).unwrap();
    let room = initiator.create_genesis("handshake deadline").unwrap();
    let capture = TcpListener::bind(loopback()).await.unwrap();
    let capture_addr = capture.local_addr().unwrap();
    let outbound = tokio::spawn(async move {
        let _ = initiator.sync_room_from(capture_addr, room).await;
    });
    let (mut capture_stream, _) = capture.accept().await.unwrap();
    let hello = read_frame::<_, SwarmMsg>(&mut capture_stream)
        .await
        .unwrap()
        .expect("initiator emits hello_i");

    let server = target.start(loopback()).await.unwrap();
    let started = Instant::now();
    let mut slow = TcpStream::connect(server.addr()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    write_frame(&mut slow, &hello).await.unwrap();
    assert!(matches!(
        tokio::time::timeout(
            Duration::from_millis(500),
            read_frame::<_, SwarmMsg>(&mut slow)
        )
        .await,
        Ok(Ok(Some(SwarmMsg::HelloR(_))))
    ));
    let closed =
        tokio::time::timeout(Duration::from_secs(2), read_frame::<_, SwarmMsg>(&mut slow)).await;
    assert!(matches!(closed, Ok(Ok(None)) | Ok(Err(_))));
    assert!(started.elapsed() < Duration::from_secs(6));
    outbound.abort();
}

#[tokio::test]
async fn local_client_daemon_uses_custom_ca_tcps_and_never_downgrades_to_tcp() {
    let _network_test = network_test_guard().await;
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let key = PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into();
    let provider = std::sync::Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let server_config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key)
        .unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let provider = std::sync::Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let client_config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let source_data = TempDir::new().unwrap();
    let follower_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let follower = Daemon::open(follower_data.path()).unwrap();
    let client_config = std::sync::Arc::new(client_config);
    source
        .configure_transport(TransportMode::Public, Some(client_config.clone()))
        .unwrap();
    follower
        .configure_transport(TransportMode::Local, Some(client_config))
        .unwrap();
    let server = source
        .start_tls(loopback(), std::sync::Arc::new(server_config))
        .await
        .unwrap();
    let token = Hash32::from_bytes([7; 32]);
    let ticket = source
        .create_ticket_with_token(
            "public tls",
            StakePolicy::default(),
            FloorConfig::stick(30),
            Some(token),
        )
        .unwrap();
    assert!(ticket.peers.iter().any(|peer| peer.starts_with("tcps://")));

    let mut plaintext_only = ticket.clone();
    plaintext_only.peers = vec![format!("tcp://{}", server.addr())];
    assert!(follower
        .join_ticket(plaintext_only, JoinRole::Observe)
        .await
        .is_err());

    let chain = follower
        .join_ticket(ticket.clone(), JoinRole::Observe)
        .await
        .unwrap();
    assert_eq!(chain.head_hash, Some(ticket.genesis));
}

#[tokio::test]
async fn history_follow_streams_each_new_committed_batch() {
    let _network_test = network_test_guard().await;
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
    let initial = initial.data.unwrap();
    assert_eq!(initial["scenes"].as_array().unwrap().len(), 1);
    assert_eq!(initial["syncing"], false);
    assert_eq!(initial["complete"], true);

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
    assert_eq!(records["scenes"].as_array().unwrap().len(), 1);
    assert_eq!(records["scenes"][0]["scene"]["body"]["type"], "grant");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_daemon_staker_wrap_uses_network_votes_and_certs() {
    let _network_test = network_test_guard().await;
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
    let granted = tokio::time::timeout(Duration::from_secs(8), async {
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
    .await;
    assert!(
        granted.is_ok(),
        "source={:?}; follower={:?}",
        source.replay(ticket.id).unwrap(),
        follower.replay(ticket.id).unwrap()
    );
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

    tokio::time::timeout(Duration::from_secs(10), async {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moderator_yank_freezes_remote_holder_before_commit() {
    let _network_test = network_test_guard().await;
    let source_data = TempDir::new().unwrap();
    let holder_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let holder = Daemon::open(holder_data.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let holder_server = holder.start(loopback()).await.unwrap();
    let moderator = AgentId::new("human:moderator").unwrap();
    let ticket = source
        .create_ticket(
            "remote yank",
            StakePolicy::default(),
            FloorConfig {
                mode: FloorMode::Moderator,
                timeout_secs: 30,
                moderator: Some(Mouth {
                    agent: moderator.clone(),
                    node: source.node_id(),
                }),
            },
        )
        .unwrap();
    holder
        .join_ticket(ticket.clone(), JoinRole::Stake)
        .await
        .unwrap();

    let mut writer = attach(holder_server.addr(), "agent:remote-holder").await;
    let raised = request(&mut writer, ClientRequest::RaiseHand { room: ticket.id }).await;
    assert!(raised.ok, "{raised:?}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut operator = attach(source_server.addr(), moderator.as_str()).await;
    let granted = request(
        &mut operator,
        ClientRequest::Grant {
            room: ticket.id,
            to: Mouth {
                agent: AgentId::new("agent:remote-holder").unwrap(),
                node: holder.node_id(),
            },
        },
    )
    .await;
    assert!(granted.ok, "{granted:?}");
    tokio::time::timeout(Duration::from_secs(10), async {
        while holder.replay(ticket.id).unwrap().chain.live_grant.is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let spoke = request(
        &mut writer,
        ClientRequest::Speak {
            room: ticket.id,
            text: "preserve this acknowledged text".into(),
            request_id: "00000000000000000000000000000009".into(),
        },
    )
    .await;
    assert!(spoke.ok, "{spoke:?}");

    let yanked = request(&mut operator, ClientRequest::Yank { room: ticket.id }).await;
    assert!(yanked.ok, "{yanked:?}");
    let source_history = source.replay(ticket.id).unwrap().history;
    assert!(matches!(
        &source_history.last().unwrap().scene.body,
        Body::Speech { text, .. } if text == "preserve this acknowledged text"
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while holder.replay(ticket.id).unwrap().history != source_history {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn follower_staker_moderator_forwards_grant_and_yank_without_blocking_append() {
    let _network_test = network_test_guard().await;
    let source_data = TempDir::new().unwrap();
    let moderator_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let moderator_node = Daemon::open(moderator_data.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let moderator_server = moderator_node.start(loopback()).await.unwrap();
    let moderator = AgentId::new("human:remote-moderator").unwrap();
    let ticket = source
        .create_ticket(
            "observer moderator",
            StakePolicy::default(),
            FloorConfig {
                mode: FloorMode::Moderator,
                timeout_secs: 30,
                moderator: Some(Mouth {
                    agent: moderator.clone(),
                    node: moderator_node.node_id(),
                }),
            },
        )
        .unwrap();
    moderator_node
        .join_ticket(ticket.clone(), JoinRole::Stake)
        .await
        .unwrap();
    assert_eq!(source.replay(ticket.id).unwrap().chain.roster.len(), 2);

    let mut writer = attach(source_server.addr(), "agent:writer").await;
    assert!(
        request(&mut writer, ClientRequest::RaiseHand { room: ticket.id })
            .await
            .ok
    );
    let mut operator = attach(moderator_server.addr(), moderator.as_str()).await;
    let granted = request(
        &mut operator,
        ClientRequest::Grant {
            room: ticket.id,
            to: Mouth {
                agent: AgentId::new("agent:writer").unwrap(),
                node: source.node_id(),
            },
        },
    )
    .await;
    assert!(granted.ok, "{granted:?}");
    assert!(source.replay(ticket.id).unwrap().chain.live_grant.is_some());

    let yanked = request(&mut operator, ClientRequest::Yank { room: ticket.id }).await;
    assert!(yanked.ok, "{yanked:?}");
    assert!(source.replay(ticket.id).unwrap().chain.live_grant.is_none());
    assert_eq!(
        source.replay(ticket.id).unwrap().history,
        moderator_node.replay(ticket.id).unwrap().history
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn two_of_three_live_stakers_can_wrap() {
    let _network_test = network_test_guard().await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn learned_pex_mesh_wraps_after_initial_seeder_exits() {
    let _network_test = network_test_guard().await;
    let source_data = TempDir::new().unwrap();
    let second_data = TempDir::new().unwrap();
    let third_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let second = Daemon::open(second_data.path()).unwrap();
    let third = Daemon::open(third_data.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let second_server = second.start(loopback()).await.unwrap();
    let _third_server = third.start(loopback()).await.unwrap();
    let ticket = source
        .create_ticket(
            "kill seeder",
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

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let learned = fs::read(second_data.path().join("peers.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .and_then(|peers| {
                    peers
                        .get(ticket.id.to_string())?
                        .get(third.node_id().to_string())
                        .cloned()
                })
                .is_some();
            if learned {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the surviving stakers must learn one another through PEX");
    source_server.abort();
    drop(source_server);
    drop(source);

    let mut client = attach(second_server.addr(), "agent:survivor").await;
    assert!(
        request(&mut client, ClientRequest::RaiseHand { room: ticket.id })
            .await
            .ok
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if second.replay(ticket.id).unwrap().chain.live_grant.is_some()
                && third.replay(ticket.id).unwrap().chain.live_grant.is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the PEX mesh must elect and wrap without the seeder");
    assert_eq!(
        second.replay(ticket.id).unwrap().chain.head_hash,
        third.replay(ticket.id).unwrap().chain.head_hash
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn remaining_majority_elects_successor_after_leader_stops() {
    let _network_test = network_test_guard().await;
    let source_data = TempDir::new().unwrap();
    let second_data = TempDir::new().unwrap();
    let third_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let second = Daemon::open(second_data.path()).unwrap();
    let third = Daemon::open(third_data.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let second_server = second.start(loopback()).await.unwrap();
    let third_server = third.start(loopback()).await.unwrap();
    let ticket = source
        .create_ticket(
            "automatic election",
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

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let knows_peer = |root: &std::path::Path, peer: conch_core::types::NodeId| {
                fs::read(root.join("peers.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                    .and_then(|peers| {
                        peers
                            .get(ticket.id.to_string())?
                            .get(peer.to_string())
                            .cloned()
                    })
                    .is_some()
            };
            if knows_peer(second_data.path(), third.node_id())
                && knows_peer(third_data.path(), second.node_id())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("surviving stakers must durably learn each other before the seeder stops");
    source_server.abort();
    drop(source_server);
    drop(source);

    let leader_addr = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let second_state = second.replay(ticket.id).unwrap().consensus;
            if second_state.role == conch_core::types::ConsensusRole::Leader
                && second_state.leader_id == Some(second.node_id())
            {
                break second_server.addr();
            }
            let third_state = third.replay(ticket.id).unwrap().consensus;
            if third_state.role == conch_core::types::ConsensusRole::Leader
                && third_state.leader_id == Some(third.node_id())
            {
                break third_server.addr();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the surviving quorum must elect a successor after the seeder stops");
    let mut writer = attach(leader_addr, "agent:successor").await;
    let raised = request(&mut writer, ClientRequest::RaiseHand { room: ticket.id }).await;
    assert!(raised.ok, "{raised:?}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while second.replay(ticket.id).unwrap().chain.live_grant.is_none()
            || third.replay(ticket.id).unwrap().chain.live_grant.is_none()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the successor grant must reach both surviving stakers");
    assert!(second.replay(ticket.id).unwrap().chain.live_grant.is_some());
    assert!(third.replay(ticket.id).unwrap().chain.live_grant.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_of_two_live_stakers_cannot_wrap() {
    let _network_test = network_test_guard().await;
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
    let _network_test = network_test_guard().await;
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
    tokio::time::timeout(Duration::from_secs(15), async {
        while source.replay(ticket.id).unwrap().chain.head_n != Some(2) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the new leader must carry the accepted entry without a client nudge");
    let replay = source.replay(ticket.id).unwrap();
    assert_eq!(replay.chain.head_n, Some(2));
    assert_eq!(replay.chain.head_hash, Some(accepted_hash));
    assert!(
        replay.history.last().unwrap().commit_proof.rpc_term >= 3,
        "retries may advance the election term, but must commit the accepted hash"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn network_breakout_autojoins_listed_peer() {
    let _network_test = network_test_guard().await;
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
    tokio::time::timeout(Duration::from_secs(10), async {
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
    tokio::time::timeout(Duration::from_secs(10), async {
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
async fn uncommitted_breakout_child_stays_staged_across_restart() {
    let _network_test = network_test_guard().await;
    let source_data = TempDir::new().unwrap();
    let follower_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let follower = Daemon::open(follower_data.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let follower_server = follower.start(loopback()).await.unwrap();
    let parent = source
        .create_ticket(
            "staged child",
            StakePolicy::default(),
            FloorConfig::stick(30),
        )
        .unwrap();
    follower
        .join_ticket(parent.clone(), JoinRole::Stake)
        .await
        .unwrap();
    let mut client = attach(source_server.addr(), "agent:holder").await;
    assert!(
        request(&mut client, ClientRequest::RaiseHand { room: parent.id })
            .await
            .ok
    );
    follower_server.abort();
    drop(follower_server);
    drop(follower);

    let failed = request(
        &mut client,
        ClientRequest::Breakout {
            room: parent.id,
            name: "original child".into(),
            members: None,
        },
    )
    .await;
    assert!(!failed.ok);
    let pending = source.replay(parent.id).unwrap().pending.unwrap();
    let Body::Breakout { ticket, .. } = pending.scene.body else {
        panic!("expected staged breakout pending");
    };
    let child: conch_core::ticket::Ticket = serde_json::from_value(ticket).unwrap();
    assert!(source.replay(child.id).is_err());
    assert!(source_data
        .path()
        .join("staged-breakouts")
        .join(child.id.to_string())
        .is_dir());
    source_server.abort();
    drop(source_server);
    drop(source);

    let source = Daemon::open(source_data.path()).unwrap();
    assert!(source.replay(child.id).is_err());
    let follower = Daemon::open(follower_data.path()).unwrap();
    let source_server = source.start(loopback()).await.unwrap();
    let _follower_server = follower.start(loopback()).await.unwrap();
    follower
        .sync_room_from(source_server.addr(), parent.id)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while source.replay(child.id).is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the new leader must carry the staged breakout without a client nudge");
    let committed = source.replay(parent.id).unwrap();
    assert!(matches!(
        &committed.history.last().unwrap().scene.body,
        Body::Breakout { ticket, .. }
            if serde_json::from_value::<conch_core::ticket::Ticket>(ticket.clone())
                .is_ok_and(|ticket| ticket.id == child.id)
    ));
    assert!(source.replay(child.id).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn leader_self_removal_pushes_commit_then_cannot_campaign() {
    let _network_test = network_test_guard().await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn follower_leave_is_signed_and_forwarded_to_the_leader() {
    let _network_test = network_test_guard().await;
    let source_data = TempDir::new().unwrap();
    let second_data = TempDir::new().unwrap();
    let third_data = TempDir::new().unwrap();
    let source = Daemon::open(source_data.path()).unwrap();
    let second = Daemon::open(second_data.path()).unwrap();
    let third = Daemon::open(third_data.path()).unwrap();
    let _source_server = source.start(loopback()).await.unwrap();
    let second_server = second.start(loopback()).await.unwrap();
    let _third_server = third.start(loopback()).await.unwrap();
    let ticket = source
        .create_ticket(
            "forward leave",
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
    assert_eq!(
        second.replay(ticket.id).unwrap().consensus.leader_id,
        Some(source.node_id())
    );

    let mut client = attach(second_server.addr(), "agent:leaver").await;
    let left = request(
        &mut client,
        ClientRequest::Leave {
            room: ticket.id,
            vacate: false,
        },
    )
    .await;
    assert!(left.ok, "{left:?}");
    let record = source.replay(ticket.id).unwrap().history.pop().unwrap();
    assert_eq!(record.commit_proof.leader, source.node_id());
    assert!(matches!(
        record.scene.body,
        Body::ViewChange { remove, .. } if remove == vec![second.node_id()]
    ));
}
