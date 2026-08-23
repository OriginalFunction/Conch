use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use conch_core::{
    apply::{apply, ApplyMode, ApplyResources},
    client::{ClientReply, ClientRequest},
    consensus::{
        begin_campaign, tail, CloseTake, ConsensusError, GetScenes, HaveMessage, Hello, SwarmMsg,
    },
    disk::{Replay, Store, StoreError},
    encoding::{cert_digest, scene_hash, sign},
    floor::{FloorEngine, FloorError, SpeakAck, TakeBuffer, TakePhase},
    frame::{self, FrameError, MAX_FRAME_BYTES},
    types::{
        AgentId, Body, Cert, ChainState, CommitProof, CommittedScene, ConsensusRole, FloorConfig,
        GrantReason, Hash32, Intent, IntentKind, Mouth, NodeId, RoomId, Scene, SignatureBytes,
        StakePolicy,
    },
};
use ed25519_dalek::SigningKey;
use rand::random;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex as AsyncMutex, Notify},
    task::JoinHandle,
    time::{timeout, Duration},
};

const SYNC_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Apply(#[from] conch_core::apply::ApplyError),
    #[error(transparent)]
    Consensus(#[from] ConsensusError),
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    Floor(#[from] FloorError),
    #[error("JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("node key is invalid")]
    InvalidNodeKey,
    #[error("unknown room {0}")]
    UnknownRoom(RoomId),
    #[error("peer protocol violation: {0}")]
    Protocol(&'static str),
    #[error("room synchronization timed out")]
    SyncTimeout,
}

#[derive(Clone)]
pub struct Daemon {
    inner: Arc<Inner>,
}

struct Inner {
    data_dir: PathBuf,
    node_key: SigningKey,
    rooms: RwLock<BTreeMap<RoomId, Store>>,
    floors: RwLock<BTreeMap<RoomId, Arc<RoomFloor>>>,
}

struct RoomFloor {
    engine: Mutex<FloorEngine>,
    mutation: AsyncMutex<()>,
    changed: Notify,
}

pub struct RunningServer {
    addr: SocketAddr,
    task: JoinHandle<Result<(), DaemonError>>,
}

impl RunningServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn abort(&self) {
        self.task.abort();
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Daemon {
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self, DaemonError> {
        let data_dir = data_dir.into();
        fs::create_dir_all(data_dir.join("rooms"))?;
        let node_key = load_or_create_node_key(&data_dir)?;
        let node_id = NodeId::from_bytes(node_key.verifying_key().to_bytes());
        let mut rooms = BTreeMap::new();
        let mut floors = BTreeMap::new();
        for entry in fs::read_dir(data_dir.join("rooms"))? {
            let Ok(entry) = entry else {
                continue;
            };
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(room) = name.parse::<RoomId>() else {
                continue;
            };
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                let store = Store::open(entry.path())?;
                let replay = store.load_replay()?;
                let persisted = read_take(&store)?;
                let mut engine = FloorEngine::restore(node_id, persisted);
                engine.observe_committed(&replay.chain);
                for intent in store.load_intents()? {
                    if !replay.chain.consumed_intents.contains(&intent.id) {
                        engine.upsert_intent(&replay.chain, intent)?;
                    }
                }
                floors.insert(room, Arc::new(RoomFloor::new(engine)));
                rooms.insert(room, store);
            }
        }
        Ok(Self {
            inner: Arc::new(Inner {
                data_dir,
                node_key,
                rooms: RwLock::new(rooms),
                floors: RwLock::new(floors),
            }),
        })
    }

    pub fn node_id(&self) -> NodeId {
        NodeId::from_bytes(self.inner.node_key.verifying_key().to_bytes())
    }

    pub fn track_room(&self, room: RoomId) -> Result<(), DaemonError> {
        let store = Store::open(self.room_path(room))?;
        self.inner
            .rooms
            .write()
            .expect("room registry lock is not poisoned")
            .insert(room, store);
        self.inner
            .floors
            .write()
            .expect("floor registry lock is not poisoned")
            .insert(
                room,
                Arc::new(RoomFloor::new(FloorEngine::new(self.node_id()))),
            );
        Ok(())
    }

    pub fn create_genesis(&self, name: &str) -> Result<RoomId, DaemonError> {
        let room_key = SigningKey::from_bytes(&random::<[u8; 32]>());
        let room = RoomId::from_bytes(room_key.verifying_key().to_bytes());
        let node = self.node_id();
        let scene = Scene {
            v: 1,
            room,
            n: 0,
            term: 1,
            parent: None,
            roster: vec![node],
            leader: node,
            ts: unix_timestamp(),
            body: Body::Genesis {
                name: name.to_owned(),
                stake: StakePolicy::default(),
                floor: FloorConfig::stick(30),
                creator_node: node,
                parent_room: None,
                token_sha256: None,
            },
            certs: Vec::new(),
        };
        let hash = hash_scene(&scene);
        let node_digest = cert_digest(&room, 0, hash.as_bytes(), 1, &node, &node);
        let proof = CommitProof {
            rpc_term: 1,
            leader: node,
            certs: vec![
                Cert::node(
                    node,
                    SignatureBytes::from_bytes(sign(&self.inner.node_key, &node_digest)),
                ),
                Cert::room(SignatureBytes::from_bytes(sign(&room_key, hash.as_bytes()))),
            ],
        };

        let room_path = self.room_path(room);
        fs::create_dir_all(&room_path)?;
        write_secret(&room_path.join("room.key"), &room_key.to_bytes())?;
        let store = Store::open(&room_path)?;
        store.durable_commit(&scene, &proof)?;
        let chain = store.load_replay()?.chain;
        self.inner
            .rooms
            .write()
            .expect("room registry lock is not poisoned")
            .insert(room, store);
        self.inner
            .floors
            .write()
            .expect("floor registry lock is not poisoned")
            .insert(
                room,
                Arc::new(RoomFloor::from_chain(self.node_id(), &chain)),
            );
        Ok(room)
    }

    pub fn replay(&self, room: RoomId) -> Result<Replay, DaemonError> {
        self.store(room)?.load_replay().map_err(Into::into)
    }

    pub async fn start(&self, addr: SocketAddr) -> Result<RunningServer, DaemonError> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let daemon = self.clone();
        let task = tokio::spawn(async move { daemon.serve_listener(listener).await });
        Ok(RunningServer { addr, task })
    }

    pub async fn serve(&self, addr: SocketAddr) -> Result<(), DaemonError> {
        let listener = TcpListener::bind(addr).await?;
        self.clone().serve_listener(listener).await
    }

    pub async fn sync_room_from(
        &self,
        addr: SocketAddr,
        room: RoomId,
    ) -> Result<ChainState, DaemonError> {
        timeout(SYNC_TIMEOUT, self.sync_room_from_inner(addr, room))
            .await
            .map_err(|_| DaemonError::SyncTimeout)?
    }

    async fn sync_room_from_inner(
        &self,
        addr: SocketAddr,
        room: RoomId,
    ) -> Result<ChainState, DaemonError> {
        let mut stream = TcpStream::connect(addr).await?;
        write_message(&mut stream, &SwarmMsg::Hello(self.hello())).await?;
        let remote = read_message(&mut stream)
            .await?
            .ok_or(DaemonError::Protocol("peer closed before hello"))?;
        validate_hello(remote)?;

        loop {
            let message = read_message(&mut stream)
                .await?
                .ok_or(DaemonError::Protocol("peer closed before have"))?;
            let SwarmMsg::Have(have) = message else {
                continue;
            };
            if have.room != room {
                continue;
            }
            let local = self.replay(room)?;
            if local.chain.head_n == Some(have.n) && local.chain.head_hash == Some(have.hash) {
                return Ok(local.chain);
            }
            let from_n = local.chain.head_n.map_or(0, |head| head + 1);
            if from_n > have.n {
                return Ok(local.chain);
            }
            write_message(
                &mut stream,
                &SwarmMsg::GetScenes(GetScenes {
                    room,
                    from_n,
                    to_n: have.n,
                }),
            )
            .await?;

            for expected_n in from_n..=have.n {
                let record = loop {
                    match read_message(&mut stream).await? {
                        Some(SwarmMsg::Scene(record)) if record.scene.room == room => break record,
                        Some(_) => continue,
                        None => {
                            return Err(DaemonError::Protocol(
                                "peer closed during get_scenes response",
                            ));
                        }
                    }
                };
                if record.scene.n != expected_n {
                    return Err(DaemonError::Protocol("get_scenes returned wrong height"));
                }
                self.install_record(room, &record)?;
            }
            let replay = self.replay(room)?;
            if replay.chain.head_n == Some(have.n) && replay.chain.head_hash == Some(have.hash) {
                write_message(
                    &mut stream,
                    &SwarmMsg::Have(have_from_replay(room, &replay)?),
                )
                .await?;
                return Ok(replay.chain);
            }
            return Err(DaemonError::Protocol(
                "catch-up did not reach advertised head",
            ));
        }
    }

    async fn serve_listener(self, listener: TcpListener) -> Result<(), DaemonError> {
        loop {
            let (stream, _) = listener.accept().await?;
            let daemon = self.clone();
            tokio::spawn(async move {
                let _ = daemon.handle_connection(stream).await;
            });
        }
    }

    async fn handle_connection(&self, mut stream: TcpStream) -> Result<(), DaemonError> {
        let first: Value = read_frame(&mut stream)
            .await?
            .ok_or(DaemonError::Protocol("peer closed before hello"))?;
        match first.get("typ").and_then(Value::as_str) {
            Some("hello") => {
                let hello = serde_json::from_value(first).map_err(FrameError::from)?;
                self.handle_swarm_connection(stream, hello).await
            }
            Some("attach") => {
                let attach = serde_json::from_value(first).map_err(FrameError::from)?;
                self.handle_client_connection(stream, attach).await
            }
            _ => Err(DaemonError::Protocol(
                "hello or attach must be the first frame",
            )),
        }
    }

    async fn handle_swarm_connection(
        &self,
        mut stream: TcpStream,
        hello: SwarmMsg,
    ) -> Result<(), DaemonError> {
        validate_hello(hello)?;
        write_message(&mut stream, &SwarmMsg::Hello(self.hello())).await?;
        for have in self.haves()? {
            write_message(&mut stream, &SwarmMsg::Have(have)).await?;
        }

        while let Some(message) = read_message(&mut stream).await? {
            match message {
                SwarmMsg::GetScenes(request) => {
                    let replay = self.replay(request.room)?;
                    for record in replay.history.iter().filter(|record| {
                        record.scene.n >= request.from_n && record.scene.n <= request.to_n
                    }) {
                        write_message(&mut stream, &SwarmMsg::Scene(record.clone())).await?;
                    }
                }
                SwarmMsg::Scene(record) => {
                    self.install_record(record.scene.room, &record)?;
                }
                SwarmMsg::Commit(commit) => {
                    let proof = commit.proof();
                    let record = CommittedScene {
                        scene: commit.scene,
                        commit_proof: proof,
                    };
                    self.install_record(record.scene.room, &record)?;
                }
                SwarmMsg::Intent(intent) => self.receive_intent(intent).await?,
                SwarmMsg::Freeze(freeze) => {
                    if let Some(close) = self.receive_freeze(freeze.room, freeze.grant_hash)? {
                        write_message(&mut stream, &SwarmMsg::CloseTake(close)).await?;
                    }
                }
                SwarmMsg::Have(_) | SwarmMsg::Auth(_) => {}
                _ => {}
            }
        }
        Ok(())
    }

    async fn handle_client_connection(
        &self,
        mut stream: TcpStream,
        attach: ClientRequest,
    ) -> Result<(), DaemonError> {
        let ClientRequest::Attach { agent } = attach else {
            return Err(DaemonError::Protocol(
                "attach must be the first client frame",
            ));
        };
        write_frame(
            &mut stream,
            &ClientReply::success(json!({ "agent": agent })),
        )
        .await?;
        let Some(request) = read_frame::<_, ClientRequest>(&mut stream).await? else {
            return Ok(());
        };
        let reply = self.execute_client(agent, request).await;
        write_frame(&mut stream, &reply).await?;
        Ok(())
    }

    async fn execute_client(&self, agent: AgentId, request: ClientRequest) -> ClientReply {
        let result = match request {
            ClientRequest::Create { name } => {
                self.create_genesis(&name).map(|room| json!({ "id": room }))
            }
            ClientRequest::WaitForFloor { room, timeout_secs } => {
                self.client_wait_for_floor(agent, room, timeout_secs).await
            }
            ClientRequest::Speak {
                room,
                text,
                request_id,
            } => self.client_speak(agent, room, &text, &request_id),
            ClientRequest::Yield { room } => self.client_yield(agent, room).await,
            ClientRequest::RaiseHand { room } => self.client_raise_hand(agent, room).await,
            ClientRequest::History { room, from_n } => self.replay(room).and_then(|replay| {
                serde_json::to_value(
                    replay
                        .history
                        .into_iter()
                        .filter(|record| record.scene.n >= from_n)
                        .collect::<Vec<_>>(),
                )
                .map_err(Into::into)
            }),
            ClientRequest::Status { room } => self.client_status(room),
            ClientRequest::Attach { .. } => Err(DaemonError::Protocol("duplicate attach")),
        };

        match result {
            Ok(data) => ClientReply::success(data),
            Err(DaemonError::UnknownRoom(_)) => {
                ClientReply::failure("unknown_room", "room is not joined on this node")
            }
            Err(DaemonError::Floor(FloorError::NoGrant)) => {
                ClientReply::failure("no_grant", "no OPEN grant for this mouth")
            }
            Err(DaemonError::Floor(FloorError::NotStaker)) => {
                ClientReply::failure("unauthorized", "observer or removed node cannot speak")
            }
            Err(DaemonError::Floor(FloorError::Timeout)) => {
                ClientReply::failure("timeout", "wait-for-floor timed out")
            }
            Err(error) => ClientReply::failure("invalid", error.to_string()),
        }
    }

    async fn client_wait_for_floor(
        &self,
        agent: AgentId,
        room: RoomId,
        timeout_secs: Option<u64>,
    ) -> Result<Value, DaemonError> {
        self.queue_intent(room, agent.clone(), IntentKind::Wait)
            .await?;
        let waiting = self.wait_for_open(room, agent);
        if let Some(seconds) = timeout_secs {
            timeout(Duration::from_secs(seconds), waiting)
                .await
                .map_err(|_| DaemonError::Floor(FloorError::Timeout))?
        } else {
            waiting.await
        }
    }

    async fn wait_for_open(&self, room: RoomId, agent: AgentId) -> Result<Value, DaemonError> {
        let floor = self.floor(room)?;
        let mouth = Mouth {
            agent,
            node: self.node_id(),
        };
        loop {
            let changed = floor.changed.notified();
            let grant = floor
                .engine
                .lock()
                .expect("floor lock is not poisoned")
                .open_grant_for(&mouth);
            if let Some(grant) = grant {
                let replay = self.replay(room)?;
                let record = replay
                    .history
                    .into_iter()
                    .find(|record| hash_scene(&record.scene) == grant.grant_hash)
                    .ok_or(DaemonError::Protocol("OPEN grant is absent from history"))?;
                return Ok(serde_json::to_value(record.scene)?);
            }
            changed.await;
        }
    }

    async fn client_raise_hand(&self, agent: AgentId, room: RoomId) -> Result<Value, DaemonError> {
        let id = self.queue_intent(room, agent, IntentKind::Raise).await?;
        Ok(json!({ "intent_id": id }))
    }

    async fn queue_intent(
        &self,
        room: RoomId,
        agent: AgentId,
        kind: IntentKind,
    ) -> Result<Hash32, DaemonError> {
        let floor = self.floor(room)?;
        let _mutation = floor.mutation.lock().await;
        let replay = self.replay(room)?;
        let node = self.node_id();
        if !replay.chain.roster.contains(&node) {
            return Err(FloorError::NotStaker.into());
        }
        let now = unix_timestamp();
        let existing = floor
            .engine
            .lock()
            .expect("floor lock is not poisoned")
            .intents()
            .find(|intent| {
                intent.agent == agent
                    && intent.node == node
                    && !replay.chain.consumed_intents.contains(&intent.id)
            })
            .cloned();
        let replaced = existing.as_ref().map(|intent| intent.id);
        let (id, ts) = match (kind, existing) {
            (IntentKind::Wait, Some(intent)) => (intent.id, intent.ts),
            _ => (Hash32::from_bytes(random::<[u8; 32]>()), now),
        };
        let mut intent = Intent {
            v: 1,
            id,
            room,
            kind,
            agent,
            node,
            ts,
            exp: now.saturating_add(86_400),
            sig: SignatureBytes::from_bytes([0; 64]),
        };
        intent.sig = SignatureBytes::from_bytes(sign(
            &self.inner.node_key,
            &conch_core::encoding::signed_object_digest(&serde_json::to_value(&intent)?),
        ));
        floor
            .engine
            .lock()
            .expect("floor lock is not poisoned")
            .upsert_intent(&replay.chain, intent)?;
        let stored = floor
            .engine
            .lock()
            .expect("floor lock is not poisoned")
            .intents()
            .find(|intent| intent.id == id)
            .cloned()
            .expect("upserted intent is present");
        let store = self.store(room)?;
        store.write_intent(&stored)?;
        if replaced.is_some_and(|prior| prior != id) {
            store.remove_intent(replaced.expect("checked as some"))?;
        }
        self.maybe_grant_next_locked(room, &floor)?;
        Ok(id)
    }

    async fn receive_intent(&self, intent: Intent) -> Result<(), DaemonError> {
        let room = intent.room;
        let floor = self.floor(room)?;
        let _mutation = floor.mutation.lock().await;
        let replay = self.replay(room)?;
        let mouth = Mouth {
            agent: intent.agent.clone(),
            node: intent.node,
        };
        let replaced = floor
            .engine
            .lock()
            .expect("floor lock is not poisoned")
            .intents()
            .find(|known| known.agent == mouth.agent && known.node == mouth.node)
            .map(|known| known.id);
        floor
            .engine
            .lock()
            .expect("floor lock is not poisoned")
            .upsert_intent(&replay.chain, intent.clone())?;
        let store = self.store(room)?;
        store.write_intent(&intent)?;
        if replaced.is_some_and(|prior| prior != intent.id) {
            store.remove_intent(replaced.expect("checked as some"))?;
        }
        self.maybe_grant_next_locked(room, &floor)?;
        Ok(())
    }

    fn receive_freeze(
        &self,
        room: RoomId,
        grant_hash: Hash32,
    ) -> Result<Option<CloseTake>, DaemonError> {
        let floor = self.floor(room)?;
        let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
        let Some(holder) = engine.take().map(|take| take.holder.clone()) else {
            return Ok(None);
        };
        if engine
            .take()
            .is_none_or(|take| take.grant_hash != grant_hash)
        {
            return Ok(None);
        }
        let frozen = engine.freeze(&holder)?;
        write_take(
            &self.store(room)?,
            engine.take().expect("freeze preserves take"),
        )?;
        Ok(Some(CloseTake {
            room,
            grant_hash,
            text: frozen.text,
            rev: frozen.rev,
            blobs: frozen.blobs,
        }))
    }

    fn client_speak(
        &self,
        agent: AgentId,
        room: RoomId,
        text: &str,
        request_id: &str,
    ) -> Result<Value, DaemonError> {
        let floor = self.floor(room)?;
        let mouth = Mouth {
            agent,
            node: self.node_id(),
        };
        let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
        let response = engine.speak(&mouth, text, request_id)?;
        if let Some(take) = engine.take() {
            write_take(&self.store(room)?, take)?;
        }
        Ok(serde_json::to_value(response)?)
    }

    async fn client_yield(&self, agent: AgentId, room: RoomId) -> Result<Value, DaemonError> {
        let floor = self.floor(room)?;
        let _mutation = floor.mutation.lock().await;
        let mouth = Mouth {
            agent,
            node: self.node_id(),
        };
        let frozen = {
            let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
            let frozen = engine.freeze(&mouth)?;
            write_take(
                &self.store(room)?,
                engine.take().expect("freeze preserves take"),
            )?;
            frozen
        };
        self.commit_singleton_body(
            room,
            Body::Speech {
                closes_grant: frozen.grant_hash,
                text: frozen.text.clone(),
                blobs: frozen.blobs.clone(),
            },
            &floor,
        )?;
        self.maybe_grant_next_locked(room, &floor)?;
        Ok(serde_json::to_value(SpeakAck {
            ok: true,
            grant_hash: frozen.grant_hash,
            rev: frozen.rev,
        })?)
    }

    fn client_status(&self, room: Option<RoomId>) -> Result<Value, DaemonError> {
        if let Some(room) = room {
            let replay = self.replay(room)?;
            return Ok(json!({
                "room": room,
                "node": self.node_id(),
                "head_n": replay.chain.head_n,
                "head_hash": replay.chain.head_hash,
                "current_term": replay.consensus.current_term,
            }));
        }
        let rooms = self
            .inner
            .rooms
            .read()
            .expect("room registry lock is not poisoned")
            .keys()
            .copied()
            .collect::<Vec<_>>();
        Ok(json!({ "node": self.node_id(), "rooms": rooms }))
    }

    fn maybe_grant_next_locked(&self, room: RoomId, floor: &RoomFloor) -> Result<(), DaemonError> {
        let replay = self.replay(room)?;
        if replay.chain.live_grant.is_some()
            || replay.chain.floor_mode != Some(conch_core::types::FloorMode::Stick)
        {
            return Ok(());
        }
        let intent = floor
            .engine
            .lock()
            .expect("floor lock is not poisoned")
            .queue_head(&replay.chain, unix_timestamp())
            .cloned();
        let Some(intent) = intent else {
            return Ok(());
        };
        self.commit_singleton_body(
            room,
            Body::Grant {
                to: Mouth {
                    agent: intent.agent,
                    node: intent.node,
                },
                reason: GrantReason::Queue,
                intent_id: intent.id,
            },
            floor,
        )?;
        Ok(())
    }

    fn commit_singleton_body(
        &self,
        room: RoomId,
        body: Body,
        floor: &RoomFloor,
    ) -> Result<CommittedScene, DaemonError> {
        let store = self.store(room)?;
        let replay = store.load_replay()?;
        let node = self.node_id();
        if replay.chain.roster.as_slice() != [node] {
            return Err(DaemonError::Protocol(
                "local client mutation currently requires a singleton roster",
            ));
        }
        let local_tail = tail(
            replay.pending.as_ref(),
            replay.chain.head_n.zip(replay.chain.head_hash),
            replay.head_proof.as_ref(),
        )?;
        let mut consensus = replay.consensus;
        let rpc_term = begin_campaign(&mut consensus, node, &replay.chain.roster, local_tail)?;
        consensus.role = ConsensusRole::Leader;
        consensus.leader_id = Some(node);
        store.write_consensus(&consensus)?;
        let scene = Scene {
            v: 1,
            room,
            n: replay.chain.head_n.expect("non-genesis scene has head") + 1,
            term: rpc_term,
            parent: replay.chain.head_hash,
            roster: replay.chain.roster.clone(),
            leader: node,
            ts: unix_timestamp(),
            body,
            certs: Vec::new(),
        };
        let resources = ApplyResources {
            intents: floor
                .engine
                .lock()
                .expect("floor lock is not poisoned")
                .intents()
                .cloned()
                .collect(),
            blobs: BTreeMap::new(),
        };
        apply(&replay.chain, &scene, None, ApplyMode::Precert(&resources))?;
        let hash = hash_scene(&scene);
        let digest = cert_digest(&room, scene.n, hash.as_bytes(), rpc_term, &node, &node);
        let cert = Cert::node(
            node,
            SignatureBytes::from_bytes(sign(&self.inner.node_key, &digest)),
        );
        store.write_pending(&conch_core::types::Pending {
            n: scene.n,
            hash,
            scene: scene.clone(),
            accepted_rpc_term: rpc_term,
            accepted_leader: node,
            cert: cert.clone(),
        })?;
        let proof = CommitProof {
            rpc_term,
            leader: node,
            certs: vec![cert],
        };
        let chain = store.durable_commit(&scene, &proof)?;
        if let Body::Grant { intent_id, .. } = &scene.body {
            store.remove_intent(*intent_id)?;
        }
        {
            let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
            engine.observe_committed(&chain);
            if let Some(take) = engine.take() {
                write_take(&store, take)?;
            } else {
                remove_take(&store)?;
            }
        }
        floor.changed.notify_waiters();
        Ok(CommittedScene {
            scene,
            commit_proof: proof,
        })
    }

    fn hello(&self) -> Hello {
        let node = self.node_id();
        Hello {
            node,
            r#pub: node,
            addrs: Vec::new(),
            decl: Vec::new(),
        }
    }

    fn haves(&self) -> Result<Vec<HaveMessage>, DaemonError> {
        let rooms = self
            .inner
            .rooms
            .read()
            .expect("room registry lock is not poisoned")
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let mut haves = Vec::new();
        for room in rooms {
            let replay = self.replay(room)?;
            if replay.chain.head_n.is_some() {
                haves.push(have_from_replay(room, &replay)?);
            }
        }
        Ok(haves)
    }

    fn install_record(&self, room: RoomId, record: &CommittedScene) -> Result<(), DaemonError> {
        if record.scene.room != room {
            return Err(DaemonError::Protocol("scene belongs to a different room"));
        }
        let store = self.store(room)?;
        let replay = store.load_replay()?;
        let hash = hash_scene(&record.scene);
        if replay
            .history
            .iter()
            .any(|known| known.scene.n == record.scene.n && hash_scene(&known.scene) == hash)
        {
            return Ok(());
        }
        let chain = store.durable_commit(&record.scene, &record.commit_proof)?;
        let floor = self.floor(room)?;
        {
            let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
            engine.observe_committed(&chain);
            if let Some(take) = engine.take() {
                write_take(&store, take)?;
            } else {
                remove_take(&store)?;
            }
        }
        floor.changed.notify_waiters();
        Ok(())
    }

    fn store(&self, room: RoomId) -> Result<Store, DaemonError> {
        self.inner
            .rooms
            .read()
            .expect("room registry lock is not poisoned")
            .get(&room)
            .cloned()
            .ok_or(DaemonError::UnknownRoom(room))
    }

    fn floor(&self, room: RoomId) -> Result<Arc<RoomFloor>, DaemonError> {
        self.inner
            .floors
            .read()
            .expect("floor registry lock is not poisoned")
            .get(&room)
            .cloned()
            .ok_or(DaemonError::UnknownRoom(room))
    }

    fn room_path(&self, room: RoomId) -> PathBuf {
        self.inner.data_dir.join("rooms").join(room.to_string())
    }
}

impl RoomFloor {
    fn new(engine: FloorEngine) -> Self {
        Self {
            engine: Mutex::new(engine),
            mutation: AsyncMutex::new(()),
            changed: Notify::new(),
        }
    }

    fn from_chain(node: NodeId, chain: &ChainState) -> Self {
        let mut engine = FloorEngine::new(node);
        engine.observe_committed(chain);
        Self::new(engine)
    }
}

pub async fn write_message<W>(writer: &mut W, message: &SwarmMsg) -> Result<(), DaemonError>
where
    W: AsyncWrite + Unpin,
{
    write_frame(writer, message).await
}

pub async fn read_message<R>(reader: &mut R) -> Result<Option<SwarmMsg>, DaemonError>
where
    R: AsyncRead + Unpin,
{
    read_frame(reader).await
}

pub async fn write_frame<W, T>(writer: &mut W, message: &T) -> Result<(), DaemonError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    writer.write_all(&frame::encode(message)?).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<R, T>(reader: &mut R) -> Result<Option<T>, DaemonError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut prefix = [0_u8; 4];
    if reader.read(&mut prefix[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut prefix[1..]).await?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge.into());
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload).await?;
    Ok(Some(frame::decode_payload(&payload)?))
}

fn validate_hello(message: SwarmMsg) -> Result<Hello, DaemonError> {
    let SwarmMsg::Hello(hello) = message else {
        return Err(DaemonError::Protocol("hello must be the first frame"));
    };
    if hello.node != hello.r#pub {
        return Err(DaemonError::Protocol("hello node and pub do not match"));
    }
    Ok(hello)
}

fn have_from_replay(room: RoomId, replay: &Replay) -> Result<HaveMessage, DaemonError> {
    Ok(HaveMessage {
        room,
        n: replay
            .chain
            .head_n
            .ok_or(DaemonError::Protocol("have requires a committed head"))?,
        hash: replay
            .chain
            .head_hash
            .ok_or(DaemonError::Protocol("have requires a committed hash"))?,
        rpc_term: replay
            .head_proof
            .as_ref()
            .ok_or(DaemonError::Protocol("have requires a committed proof"))?
            .rpc_term,
    })
}

fn hash_scene(scene: &Scene) -> Hash32 {
    Hash32::from_bytes(scene_hash(
        &serde_json::to_value(scene).expect("typed scene is serializable"),
    ))
}

fn load_or_create_node_key(data_dir: &Path) -> Result<SigningKey, DaemonError> {
    let key_path = data_dir.join("node.key");
    let key = if key_path.exists() {
        let encoded = fs::read_to_string(&key_path)?;
        let bytes = hex::decode(encoded.trim()).map_err(|_| DaemonError::InvalidNodeKey)?;
        let secret: [u8; 32] = bytes.try_into().map_err(|_| DaemonError::InvalidNodeKey)?;
        SigningKey::from_bytes(&secret)
    } else {
        let key = SigningKey::from_bytes(&random::<[u8; 32]>());
        write_secret(&key_path, &key.to_bytes())?;
        key
    };
    fs::write(
        data_dir.join("node.pub"),
        hex::encode(key.verifying_key().to_bytes()),
    )?;
    Ok(key)
}

fn write_secret(path: &Path, bytes: &[u8; 32]) -> Result<(), DaemonError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(hex::encode(bytes).as_bytes())?;
    file.sync_all()?;
    sync_dir(path.parent().expect("secret has a parent directory"))?;
    Ok(())
}

fn read_take(store: &Store) -> Result<Option<TakeBuffer>, DaemonError> {
    for name in ["close_take.json", "take.json"] {
        let path = store.root().join(name);
        match fs::read(path) {
            Ok(bytes) => return Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

fn write_take(store: &Store, take: &TakeBuffer) -> Result<(), DaemonError> {
    write_json_atomic(&store.root().join("take.json"), take)?;
    let closing = store.root().join("close_take.json");
    if take.phase == TakePhase::Closing {
        write_json_atomic(&closing, take)?;
    } else if closing.exists() {
        fs::remove_file(closing)?;
        sync_dir(store.root())?;
    }
    Ok(())
}

fn remove_take(store: &Store) -> Result<(), DaemonError> {
    let mut removed = false;
    for name in ["take.json", "close_take.json"] {
        let path = store.root().join(name);
        match fs::remove_file(path) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if removed {
        sync_dir(store.root())?;
    }
    Ok(())
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), DaemonError> {
    let temporary = path.with_extension(format!("tmp-{}", hex::encode(random::<[u8; 8]>())));
    let bytes = serde_json::to_vec(value)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    sync_dir(path.parent().expect("state file has a parent"))?;
    Ok(())
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
