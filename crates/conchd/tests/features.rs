use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use conch_core::{
    client::{ClientReply, ClientRequest},
    consensus::{Freeze, Hello, SwarmMsg},
    encoding::scene_hash,
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
