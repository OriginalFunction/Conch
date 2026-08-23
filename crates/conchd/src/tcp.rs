use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use conch_core::{
    apply::{apply, ApplyMode},
    client::{ClientReply, ClientRequest},
    consensus::{
        advance_term, begin_campaign, tail, up_to_date, AdvanceSource, Append, BlobMeta,
        CertMessage, CloseTake, ConsensusError, GetScenes, HaveMessage, Heartbeat, Hello, Nack,
        PeerInfo, Pex, RequestVote, SwarmMsg, Vote,
    },
    disk::{Replay, Store, StoreError},
    encoding::{cert_digest, scene_hash, sign, signed_object_digest, verify},
    floor::{FloorEngine, FloorError, SpeakAck, TakeBuffer, TakePhase},
    frame::{self, FrameError, MAX_FRAME_BYTES},
    ticket::{eligible, Declaration, JoinRole, Ticket, TicketError},
    types::{
        AgentId, BlobRef, Body, Cert, CertSigner, ChainState, CommitProof, CommittedScene,
        ConsensusRole, ConsensusState, FloorConfig, FloorMode, GrantReason, Hash32, Intent,
        IntentKind, Mouth, NodeId, Pending, RoomId, Scene, SignatureBytes, StakePolicy,
    },
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::random;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex as AsyncMutex, Notify},
    task::{self, JoinError, JoinHandle, JoinSet},
    time::{interval, sleep, timeout, Duration, Instant, MissedTickBehavior},
};

const SYNC_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_BLOB_BYTES: u64 = 32 * 1024 * 1024;
const MAX_OFFERED_BLOBS_BYTES: u64 = 64 * 1024 * 1024;

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
    #[error(transparent)]
    Ticket(#[from] TicketError),
    #[error("blocking store task failed: {0}")]
    Join(#[from] JoinError),
    #[error("node key is invalid")]
    InvalidNodeKey,
    #[error("unknown room {0}")]
    UnknownRoom(RoomId),
    #[error("peer protocol violation: {0}")]
    Protocol(&'static str),
    #[error("room synchronization timed out")]
    SyncTimeout,
    #[error("bad ticket: {0}")]
    BadTicket(&'static str),
    #[error("no ticket peer could provide the room")]
    JoinUnavailable,
    #[error("room mutation requires an available local leader")]
    MutationUnavailable,
    #[error("WebSocket transport failed: {0}")]
    WebSocket(String),
    #[error("request did not come from the configured moderator mouth")]
    NotModerator,
    #[error("a roster member cannot join as an observer before it is removed")]
    InvalidJoinRole,
    #[error("invalid advertised endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("a verified peer already committed a newer head")]
    RecoveredHead,
}

#[derive(Clone)]
pub struct Daemon {
    inner: Arc<Inner>,
}

struct Inner {
    data_dir: PathBuf,
    node_key: SigningKey,
    rooms: RwLock<BTreeMap<RoomId, Store>>,
    replays: RwLock<BTreeMap<RoomId, Arc<Mutex<Replay>>>>,
    floors: RwLock<BTreeMap<RoomId, Arc<RoomFloor>>>,
    joins: RwLock<BTreeMap<RoomId, LocalJoin>>,
    agents: RwLock<BTreeSet<AgentId>>,
    addrs: RwLock<Vec<String>>,
    trackers: RwLock<Vec<String>>,
    peers: RwLock<BTreeMap<NodeId, Vec<String>>>,
    election_deadlines: Mutex<BTreeMap<RoomId, Instant>>,
    syncing: RwLock<BTreeSet<RoomId>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalJoin {
    role: JoinRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<Hash32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ListenConfig {
    tcp: Vec<String>,
    swarm: Vec<String>,
}

struct RoomFloor {
    engine: Mutex<FloorEngine>,
    mutation: AsyncMutex<()>,
    changed: Notify,
}

struct SyncMarker {
    inner: Arc<Inner>,
    room: RoomId,
}

impl Drop for SyncMarker {
    fn drop(&mut self) {
        self.inner
            .syncing
            .write()
            .expect("sync registry lock is not poisoned")
            .remove(&self.room);
    }
}

enum AppendResponse {
    Cert(CertMessage),
    Nack(Nack),
    Refused,
}

pub struct RunningServer {
    addr: SocketAddr,
    task: JoinHandle<Result<(), DaemonError>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionProtocol {
    Tcp { allow_client: bool },
    Swarm,
    Client,
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
        let mut replays = BTreeMap::new();
        let mut floors = BTreeMap::new();
        let mut joins = BTreeMap::new();
        let peers = fs::read(data_dir.join("peers.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
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
                        let id = intent.id;
                        if engine.upsert_intent(&replay.chain, intent).is_err() {
                            store.remove_intent(id)?;
                            eprintln!("conchd: discarded invalid or stale intent {id}");
                        }
                    }
                }
                let local_join = read_local_join(&store)?.unwrap_or(LocalJoin {
                    role: if replay.chain.roster.contains(&node_id) {
                        JoinRole::Stake
                    } else {
                        JoinRole::Observe
                    },
                    token: None,
                });
                floors.insert(room, Arc::new(RoomFloor::new(engine)));
                replays.insert(room, Arc::new(Mutex::new(replay)));
                joins.insert(room, local_join);
                rooms.insert(room, store);
            }
        }
        let daemon = Self {
            inner: Arc::new(Inner {
                data_dir,
                node_key,
                rooms: RwLock::new(rooms),
                replays: RwLock::new(replays),
                floors: RwLock::new(floors),
                joins: RwLock::new(joins),
                agents: RwLock::new(BTreeSet::new()),
                addrs: RwLock::new(Vec::new()),
                trackers: RwLock::new(Vec::new()),
                peers: RwLock::new(peers),
                election_deadlines: Mutex::new(BTreeMap::new()),
                syncing: RwLock::new(BTreeSet::new()),
            }),
        };
        daemon.recover_staged_breakouts()?;
        Ok(daemon)
    }

    pub fn node_id(&self) -> NodeId {
        NodeId::from_bytes(self.inner.node_key.verifying_key().to_bytes())
    }

    pub fn can_certify(&self, room: RoomId) -> Result<bool, DaemonError> {
        let role = self
            .inner
            .joins
            .read()
            .expect("join registry lock is not poisoned")
            .get(&room)
            .map(|join| join.role)
            .unwrap_or(JoinRole::Observe);
        Ok(role == JoinRole::Stake && self.replay(room)?.chain.roster.contains(&self.node_id()))
    }

    pub fn track_room(&self, room: RoomId) -> Result<(), DaemonError> {
        let store = Store::open(self.room_path(room))?;
        let replay = store.load_replay()?;
        let persisted = read_take(&store)?;
        let mut engine = FloorEngine::restore(self.node_id(), persisted);
        engine.observe_committed(&replay.chain);
        for intent in store.load_intents()? {
            if !replay.chain.consumed_intents.contains(&intent.id) {
                let id = intent.id;
                if engine.upsert_intent(&replay.chain, intent).is_err() {
                    store.remove_intent(id)?;
                    eprintln!("conchd: discarded invalid or stale intent {id}");
                }
            }
        }
        let default_role = if replay.chain.roster.contains(&self.node_id()) {
            JoinRole::Stake
        } else {
            JoinRole::Observe
        };
        let local_join = read_local_join(&store)?.unwrap_or(LocalJoin {
            role: default_role,
            token: None,
        });
        self.inner
            .rooms
            .write()
            .expect("room registry lock is not poisoned")
            .insert(room, store);
        self.inner
            .replays
            .write()
            .expect("replay registry lock is not poisoned")
            .insert(room, Arc::new(Mutex::new(replay)));
        self.inner
            .floors
            .write()
            .expect("floor registry lock is not poisoned")
            .insert(room, Arc::new(RoomFloor::new(engine)));
        self.inner
            .joins
            .write()
            .expect("join registry lock is not poisoned")
            .insert(room, local_join);
        Ok(())
    }

    pub fn create_genesis(&self, name: &str) -> Result<RoomId, DaemonError> {
        Ok(self
            .create_ticket(name, StakePolicy::default(), FloorConfig::stick(30))?
            .id)
    }

    pub fn create_ticket(
        &self,
        name: &str,
        stake: StakePolicy,
        floor: FloorConfig,
    ) -> Result<Ticket, DaemonError> {
        self.create_ticket_with_token(name, stake, floor, None)
    }

    pub fn create_ticket_with_token(
        &self,
        name: &str,
        stake: StakePolicy,
        floor: FloorConfig,
        token: Option<Hash32>,
    ) -> Result<Ticket, DaemonError> {
        self.create_ticket_inner(name, stake, floor, token, None)
    }

    fn create_ticket_inner(
        &self,
        name: &str,
        stake: StakePolicy,
        floor: FloorConfig,
        token: Option<Hash32>,
        parent: Option<RoomId>,
    ) -> Result<Ticket, DaemonError> {
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
                stake: stake.clone(),
                floor: floor.clone(),
                creator_node: node,
                parent_room: parent,
                token_sha256: token
                    .map(|token| Hash32::from_bytes(Sha256::digest(token.as_bytes()).into())),
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
        let ticket = Ticket {
            v: 1,
            id: room,
            name: name.to_owned(),
            trackers: self
                .inner
                .trackers
                .read()
                .expect("tracker registry lock is not poisoned")
                .clone(),
            peers: self
                .inner
                .addrs
                .read()
                .expect("address registry lock is not poisoned")
                .clone(),
            token,
            stake,
            floor,
            parent,
            genesis: hash,
        };
        ticket.validate()?;

        let room_path = self.room_path(room);
        fs::create_dir_all(&room_path)?;
        write_secret(&room_path.join("room.key"), &room_key.to_bytes())?;
        let store = Store::open(&room_path)?;
        write_ticket(&store, &ticket)?;
        let local_join = LocalJoin {
            role: JoinRole::Stake,
            token: ticket.token,
        };
        write_local_join(&store, &local_join)?;
        store.persist_committed_scene(&ChainState::empty(), &scene, &proof)?;
        store.unlink_pending_if_stale(Some(0))?;
        let replay = store.load_replay()?;
        let chain = replay.chain.clone();
        self.inner
            .rooms
            .write()
            .expect("room registry lock is not poisoned")
            .insert(room, store);
        self.inner
            .replays
            .write()
            .expect("replay registry lock is not poisoned")
            .insert(room, Arc::new(Mutex::new(replay)));
        self.inner
            .floors
            .write()
            .expect("floor registry lock is not poisoned")
            .insert(
                room,
                Arc::new(RoomFloor::from_chain(self.node_id(), &chain)),
            );
        self.inner
            .joins
            .write()
            .expect("join registry lock is not poisoned")
            .insert(room, local_join);
        self.write_current_room(room)?;
        Ok(ticket)
    }

    fn prepare_breakout_ticket(
        &self,
        name: &str,
        stake: StakePolicy,
        floor: FloorConfig,
        token: Option<Hash32>,
        parent: RoomId,
    ) -> Result<Ticket, DaemonError> {
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
                stake: stake.clone(),
                floor: floor.clone(),
                creator_node: node,
                parent_room: Some(parent),
                token_sha256: token
                    .map(|token| Hash32::from_bytes(Sha256::digest(token.as_bytes()).into())),
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
        let ticket = Ticket {
            v: 1,
            id: room,
            name: name.to_owned(),
            trackers: self
                .inner
                .trackers
                .read()
                .expect("tracker registry lock is not poisoned")
                .clone(),
            peers: self
                .inner
                .addrs
                .read()
                .expect("address registry lock is not poisoned")
                .clone(),
            token,
            stake,
            floor,
            parent: Some(parent),
            genesis: hash,
        };
        ticket.validate()?;
        let stage = self.staged_breakout_path(room);
        fs::create_dir_all(&stage)?;
        write_secret(&stage.join("room.key"), &room_key.to_bytes())?;
        let store = Store::open(&stage)?;
        write_ticket(&store, &ticket)?;
        write_local_join(
            &store,
            &LocalJoin {
                role: JoinRole::Stake,
                token: ticket.token,
            },
        )?;
        store.persist_committed_scene(&ChainState::empty(), &scene, &proof)?;
        store.unlink_pending_if_stale(Some(0))?;
        Ok(ticket)
    }

    pub fn replay(&self, room: RoomId) -> Result<Replay, DaemonError> {
        Ok(self
            .replay_entry(room)?
            .lock()
            .expect("replay lock is not poisoned")
            .clone())
    }

    pub async fn start(&self, addr: SocketAddr) -> Result<RunningServer, DaemonError> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        self.remember_addr(addr)?;
        let daemon = self.clone();
        let task = tokio::spawn(async move { daemon.serve_listener(listener).await });
        Ok(RunningServer { addr, task })
    }

    pub async fn serve(&self, addr: SocketAddr) -> Result<(), DaemonError> {
        let listener = TcpListener::bind(addr).await?;
        self.remember_addr(listener.local_addr()?)?;
        self.clone().serve_listener(listener).await
    }

    fn remember_addr(&self, mut addr: SocketAddr) -> Result<(), DaemonError> {
        if addr.ip().is_unspecified() {
            addr.set_ip("127.0.0.1".parse().expect("loopback address is valid"));
        }
        let endpoint = format!("tcp://{addr}");
        let mut addrs = self
            .inner
            .addrs
            .write()
            .expect("address registry lock is not poisoned");
        if !addrs.contains(&endpoint) {
            addrs.push(endpoint);
        }
        drop(addrs);
        self.persist_listen()
    }

    pub(crate) fn remember_http_addr(&self, mut addr: SocketAddr) -> Result<(), DaemonError> {
        if addr.ip().is_unspecified() {
            addr.set_ip("127.0.0.1".parse().expect("loopback address is valid"));
        }
        let endpoint = format!("ws://{addr}/swarm");
        let mut trackers = self
            .inner
            .trackers
            .write()
            .expect("tracker registry lock is not poisoned");
        if !trackers.contains(&endpoint) {
            trackers.push(endpoint);
        }
        drop(trackers);
        self.persist_listen()
    }

    pub fn advertise(&self, endpoint: &str) -> Result<(), DaemonError> {
        let target = if endpoint.starts_with("tcp://") || endpoint.starts_with("tcps://") {
            &self.inner.addrs
        } else if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
            &self.inner.trackers
        } else {
            return Err(DaemonError::InvalidEndpoint(endpoint.to_owned()));
        };
        let mut endpoints = target.write().expect("endpoint lock is not poisoned");
        let endpoint = endpoint.to_owned();
        if !endpoints.contains(&endpoint) {
            endpoints.push(endpoint);
        }
        drop(endpoints);
        self.persist_listen()
    }

    fn persist_listen(&self) -> Result<(), DaemonError> {
        let listen = ListenConfig {
            tcp: self
                .inner
                .addrs
                .read()
                .expect("address registry lock is not poisoned")
                .clone(),
            swarm: self
                .inner
                .trackers
                .read()
                .expect("tracker registry lock is not poisoned")
                .clone(),
        };
        write_json_atomic(&self.inner.data_dir.join("listen.json"), &listen)
    }

    fn remember_peer(&self, hello: &Hello) -> Result<(), DaemonError> {
        if hello.node == self.node_id() || hello.addrs.is_empty() {
            return Ok(());
        }
        let mut peers = self.inner.peers.write().expect("peer lock is not poisoned");
        let endpoints = peers.entry(hello.node).or_default();
        for endpoint in &hello.addrs {
            if !endpoints.contains(endpoint) {
                endpoints.push(endpoint.clone());
            }
        }
        let snapshot = peers.clone();
        drop(peers);
        write_json_atomic(&self.inner.data_dir.join("peers.json"), &snapshot)
    }

    fn pex(&self) -> Pex {
        let mut peers = self
            .inner
            .peers
            .read()
            .expect("peer lock is not poisoned")
            .iter()
            .map(|(node, addrs)| PeerInfo {
                node: *node,
                addrs: addrs.clone(),
            })
            .collect::<Vec<_>>();
        let own_addrs = self
            .inner
            .addrs
            .read()
            .expect("address registry lock is not poisoned")
            .clone();
        if !own_addrs.is_empty() {
            peers.push(PeerInfo {
                node: self.node_id(),
                addrs: own_addrs,
            });
        }
        peers.sort_by_key(|peer| peer.node);
        peers.dedup_by_key(|peer| peer.node);
        Pex { peers }
    }

    fn remember_pex(&self, pex: &Pex) -> Result<(), DaemonError> {
        let mut peers = self.inner.peers.write().expect("peer lock is not poisoned");
        for peer in &pex.peers {
            if peer.node == self.node_id() {
                continue;
            }
            let endpoints = peers.entry(peer.node).or_default();
            for endpoint in &peer.addrs {
                if (endpoint.starts_with("tcp://") || endpoint.starts_with("tcps://"))
                    && !endpoints.contains(endpoint)
                {
                    endpoints.push(endpoint.clone());
                }
            }
        }
        let snapshot = peers.clone();
        drop(peers);
        write_json_atomic(&self.inner.data_dir.join("peers.json"), &snapshot)
    }

    fn peer_endpoints(&self, node: NodeId) -> Vec<String> {
        self.inner
            .peers
            .read()
            .expect("peer lock is not poisoned")
            .get(&node)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn sync_room_from(
        &self,
        addr: SocketAddr,
        room: RoomId,
    ) -> Result<ChainState, DaemonError> {
        timeout(
            SYNC_TIMEOUT,
            self.sync_room_from_inner(addr, room, None, None),
        )
        .await
        .map_err(|_| DaemonError::SyncTimeout)?
    }

    async fn sync_room_from_inner(
        &self,
        addr: SocketAddr,
        room: RoomId,
        token: Option<Hash32>,
        expected_genesis: Option<Hash32>,
    ) -> Result<ChainState, DaemonError> {
        let stream = TcpStream::connect(addr).await?;
        self.sync_room_stream(stream, room, token, expected_genesis)
            .await
    }

    pub(crate) async fn sync_room_stream<S>(
        &self,
        mut stream: S,
        room: RoomId,
        token: Option<Hash32>,
        expected_genesis: Option<Hash32>,
    ) -> Result<ChainState, DaemonError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let _syncing = self.mark_syncing(room);
        write_message(&mut stream, &SwarmMsg::Hello(self.hello())).await?;
        let remote = read_message(&mut stream)
            .await?
            .ok_or(DaemonError::Protocol("peer closed before hello"))?;
        let remote = validate_hello(remote)?;
        self.remember_peer(&remote)?;
        if let Some(token) = token {
            write_message(
                &mut stream,
                &SwarmMsg::Auth(conch_core::consensus::Auth { room, token }),
            )
            .await?;
        }

        loop {
            let message = read_message(&mut stream)
                .await?
                .ok_or(DaemonError::Protocol("peer closed before have"))?;
            let have = match message {
                SwarmMsg::Have(have) => have,
                SwarmMsg::Pex(pex) => {
                    self.remember_pex(&pex)?;
                    continue;
                }
                _ => continue,
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
                        Some(SwarmMsg::BlobMeta(meta)) => {
                            self.receive_blob_into(room, meta, &mut stream).await?;
                        }
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
                if expected_n == 0
                    && expected_genesis
                        .is_some_and(|expected| hash_scene(&record.scene) != expected)
                {
                    return Err(DaemonError::BadTicket("genesis hash does not match g"));
                }
                if let Err(error) = self.install_record(room, record).await {
                    if expected_n == 0 && expected_genesis.is_some() {
                        return Err(DaemonError::BadTicket(
                            "genesis failed signature or ledger validation",
                        ));
                    }
                    return Err(error);
                }
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

    async fn receive_blob_into(
        &self,
        room: RoomId,
        meta: BlobMeta,
        stream: &mut (impl AsyncRead + Unpin),
    ) -> Result<(), DaemonError> {
        if meta.bytes > MAX_BLOB_BYTES {
            return Err(DaemonError::Protocol("blob exceeds the 32 MiB limit"));
        }
        let length = stream.read_u32().await? as u64;
        if length != meta.bytes {
            return Err(DaemonError::Protocol(
                "blob frame length does not match metadata",
            ));
        }
        let mut bytes = vec![0_u8; length as usize];
        stream.read_exact(&mut bytes).await?;
        let actual = Hash32::from_bytes(Sha256::digest(&bytes).into());
        if actual != meta.sha256 {
            return Err(DaemonError::Protocol("blob digest does not match metadata"));
        }
        self.store(room)?.put_blob(&bytes)?;
        Ok(())
    }

    async fn send_scene_blobs(
        &self,
        stream: &mut (impl AsyncWrite + Unpin),
        scene: &Scene,
    ) -> Result<(), DaemonError> {
        self.send_blob_refs(stream, scene.room, scene_blobs(scene))
            .await
    }

    async fn send_blob_refs(
        &self,
        stream: &mut (impl AsyncWrite + Unpin),
        room: RoomId,
        blobs: &[BlobRef],
    ) -> Result<(), DaemonError> {
        let store = self.store(room)?;
        for blob in blobs {
            let bytes = store.read_blob(blob.sha256)?;
            if bytes.len() as u64 != blob.bytes
                || Hash32::from_bytes(Sha256::digest(&bytes).into()) != blob.sha256
            {
                return Err(DaemonError::Protocol("local blob failed verification"));
            }
            self.send_blob_bytes(stream, blob.sha256, &bytes).await?;
        }
        Ok(())
    }

    async fn send_blob_bytes(
        &self,
        stream: &mut (impl AsyncWrite + Unpin),
        sha256: Hash32,
        bytes: &[u8],
    ) -> Result<(), DaemonError> {
        write_message(
            stream,
            &SwarmMsg::BlobMeta(BlobMeta {
                sha256,
                bytes: bytes.len() as u64,
            }),
        )
        .await?;
        stream.write_u32(bytes.len() as u32).await?;
        stream.write_all(bytes).await?;
        stream.flush().await?;
        Ok(())
    }

    pub async fn join_ticket(
        &self,
        ticket: Ticket,
        role: JoinRole,
    ) -> Result<ChainState, DaemonError> {
        ticket.validate()?;
        let room = ticket.id;
        let room_existed = self.store(room).is_ok();
        let daemon = self.clone();
        let prepared_ticket = ticket.clone();
        if let Some(chain) =
            task::spawn_blocking(move || daemon.prepare_join(&prepared_ticket, role)).await??
        {
            self.write_current_room(room)?;
            return Ok(chain);
        }

        let endpoints = ticket
            .trackers
            .iter()
            .chain(&ticket.peers)
            .cloned()
            .collect::<Vec<_>>();
        let mut pinned_failure = None;
        for endpoint in endpoints {
            if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
                let attempt = timeout(
                    SYNC_TIMEOUT,
                    self.sync_room_from_ws(&endpoint, room, ticket.token, Some(ticket.genesis)),
                )
                .await;
                match attempt {
                    Ok(Ok(chain)) => {
                        let replay = self.replay(room)?;
                        if let Err(error) = verify_ticket_replay(&ticket, &replay) {
                            if !room_existed {
                                self.discard_room_async(room).await?;
                            }
                            return Err(error);
                        }
                        if role == JoinRole::Observe
                            && replay.chain.roster.contains(&self.node_id())
                        {
                            if !room_existed {
                                self.discard_room_async(room).await?;
                            }
                            return Err(DaemonError::InvalidJoinRole);
                        }
                        let store = self.store(room)?;
                        let canonical = canonical_ticket(&ticket, &replay)?;
                        task::spawn_blocking(move || write_ticket(&store, &canonical)).await??;
                        self.write_current_room(room)?;
                        return Ok(chain);
                    }
                    Ok(Err(error @ DaemonError::BadTicket(_))) => pinned_failure = Some(error),
                    _ => {}
                }
                continue;
            }
            let Some(authority) = endpoint.strip_prefix("tcp://") else {
                continue;
            };
            let Ok(addresses) = tokio::net::lookup_host(authority).await else {
                continue;
            };
            for addr in addresses {
                let attempt = timeout(
                    SYNC_TIMEOUT,
                    self.sync_room_from_inner(addr, room, ticket.token, Some(ticket.genesis)),
                )
                .await;
                let chain = match attempt {
                    Ok(Ok(chain)) => chain,
                    Ok(Err(error @ DaemonError::BadTicket(_))) => {
                        pinned_failure = Some(error);
                        continue;
                    }
                    _ => continue,
                };
                let replay = self.replay(room)?;
                if let Err(error) = verify_ticket_replay(&ticket, &replay) {
                    if !room_existed {
                        self.discard_room_async(room).await?;
                    }
                    return Err(error);
                }
                if role == JoinRole::Observe && replay.chain.roster.contains(&self.node_id()) {
                    if !room_existed {
                        self.discard_room_async(room).await?;
                    }
                    return Err(DaemonError::InvalidJoinRole);
                }
                let store = self.store(room)?;
                let canonical = canonical_ticket(&ticket, &replay)?;
                task::spawn_blocking(move || write_ticket(&store, &canonical)).await??;
                self.write_current_room(room)?;
                return Ok(chain);
            }
        }
        if !room_existed {
            self.discard_room_async(room).await?;
        }
        Err(pinned_failure.unwrap_or(DaemonError::JoinUnavailable))
    }

    fn prepare_join(
        &self,
        ticket: &Ticket,
        role: JoinRole,
    ) -> Result<Option<ChainState>, DaemonError> {
        let room = ticket.id;
        if let Ok(replay) = self.replay(room) {
            if replay.chain.head_n.is_some() {
                verify_ticket_replay(ticket, &replay)?;
                if role == JoinRole::Observe && replay.chain.roster.contains(&self.node_id()) {
                    return Err(DaemonError::InvalidJoinRole);
                }
                let store = self.store(room)?;
                write_ticket(&store, &canonical_ticket(ticket, &replay)?)?;
                let local_join = LocalJoin {
                    role,
                    token: ticket.token,
                };
                write_local_join(&store, &local_join)?;
                self.inner
                    .joins
                    .write()
                    .expect("join registry lock is not poisoned")
                    .insert(room, local_join);
                return Ok(Some(replay.chain));
            }
        }
        let store = Store::open(self.room_path(room))?;
        write_ticket(&store, ticket)?;
        let local_join = LocalJoin {
            role,
            token: ticket.token,
        };
        write_local_join(&store, &local_join)?;
        self.track_room(room)?;
        Ok(None)
    }

    fn discard_room(&self, room: RoomId) -> Result<(), DaemonError> {
        self.inner
            .rooms
            .write()
            .expect("room registry lock is not poisoned")
            .remove(&room);
        self.inner
            .replays
            .write()
            .expect("replay registry lock is not poisoned")
            .remove(&room);
        self.inner
            .floors
            .write()
            .expect("floor registry lock is not poisoned")
            .remove(&room);
        self.inner
            .joins
            .write()
            .expect("join registry lock is not poisoned")
            .remove(&room);
        let path = self.room_path(room);
        match fs::remove_dir_all(path) {
            Ok(()) => sync_dir(&self.inner.data_dir.join("rooms"))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    async fn discard_room_async(&self, room: RoomId) -> Result<(), DaemonError> {
        let daemon = self.clone();
        task::spawn_blocking(move || daemon.discard_room(room)).await?
    }

    fn write_current_room(&self, room: RoomId) -> Result<(), DaemonError> {
        write_json_atomic(&self.inner.data_dir.join("current-room"), &room)
    }

    async fn serve_listener(self, listener: TcpListener) -> Result<(), DaemonError> {
        let mut connections = JoinSet::new();
        let mut maintenance = interval(Duration::from_millis(500));
        maintenance.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut maintenance_in_flight = false;
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let daemon = self.clone();
                    connections.spawn(async move {
                        let _ = daemon.handle_connection(stream).await;
                        false
                    });
                }
                _ = maintenance.tick(), if !maintenance_in_flight => {
                    maintenance_in_flight = true;
                    let daemon = self.clone();
                    connections.spawn(async move {
                        daemon.maintain_consensus().await;
                        true
                    });
                }
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    if result.is_ok_and(|was_maintenance| was_maintenance) {
                        maintenance_in_flight = false;
                    }
                }
            }
        }
    }

    async fn handle_connection(&self, stream: TcpStream) -> Result<(), DaemonError> {
        let peer_addr = stream.peer_addr()?;
        self.handle_transport(
            stream,
            ConnectionProtocol::Tcp {
                allow_client: client_peer_allowed(peer_addr),
            },
        )
        .await
    }

    pub(crate) async fn handle_transport<S>(
        &self,
        mut stream: S,
        protocol: ConnectionProtocol,
    ) -> Result<(), DaemonError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let first: Value = read_frame(&mut stream)
            .await?
            .ok_or(DaemonError::Protocol("peer closed before hello"))?;
        match first.get("typ").and_then(Value::as_str) {
            Some("hello")
                if matches!(
                    protocol,
                    ConnectionProtocol::Tcp { .. } | ConnectionProtocol::Swarm
                ) =>
            {
                let hello = serde_json::from_value(first).map_err(FrameError::from)?;
                self.handle_swarm_connection(stream, hello).await
            }
            Some("attach")
                if matches!(protocol, ConnectionProtocol::Client)
                    || matches!(protocol, ConnectionProtocol::Tcp { allow_client: true }) =>
            {
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
        mut stream: impl AsyncRead + AsyncWrite + Unpin + Send,
        hello: SwarmMsg,
    ) -> Result<(), DaemonError> {
        let peer = validate_hello(hello)?;
        self.remember_peer(&peer)?;
        write_message(&mut stream, &SwarmMsg::Hello(self.hello())).await?;
        write_message(&mut stream, &SwarmMsg::Pex(self.pex())).await?;
        let _ = self.admit_declared_stakers(&peer).await;
        let mut authed = BTreeSet::new();
        let mut offered_blobs = BTreeMap::new();
        let mut offered_blob_bytes = 0_u64;
        for have in self.haves(&authed)? {
            write_message(&mut stream, &SwarmMsg::Have(have)).await?;
        }

        while let Some(message) = read_message(&mut stream).await? {
            match message {
                SwarmMsg::Auth(auth) if self.authenticate(auth.room, auth.token)? => {
                    authed.insert(auth.room);
                    let replay = self.replay(auth.room)?;
                    if replay.chain.head_n.is_some() {
                        write_message(
                            &mut stream,
                            &SwarmMsg::Have(have_from_replay(auth.room, &replay)?),
                        )
                        .await?;
                    }
                }
                SwarmMsg::Auth(_) => {}
                SwarmMsg::Pex(pex) => {
                    self.remember_pex(&pex)?;
                }
                SwarmMsg::GetScenes(request) => {
                    if !self.room_authorized(request.room, &authed)? {
                        continue;
                    }
                    let replay = self.replay(request.room)?;
                    for record in replay.history.iter().filter(|record| {
                        record.scene.n >= request.from_n && record.scene.n <= request.to_n
                    }) {
                        self.send_scene_blobs(&mut stream, &record.scene).await?;
                        write_message(&mut stream, &SwarmMsg::Scene(record.clone())).await?;
                    }
                }
                SwarmMsg::BlobMeta(meta) => {
                    if meta.bytes > MAX_BLOB_BYTES {
                        return Err(DaemonError::Protocol("blob exceeds the 32 MiB limit"));
                    }
                    if offered_blob_bytes.saturating_add(meta.bytes) > MAX_OFFERED_BLOBS_BYTES {
                        return Err(DaemonError::Protocol(
                            "unreferenced blob offers exceed the connection limit",
                        ));
                    }
                    let length = stream.read_u32().await? as u64;
                    if length != meta.bytes {
                        return Err(DaemonError::Protocol(
                            "blob frame length does not match metadata",
                        ));
                    }
                    let mut bytes = vec![0_u8; length as usize];
                    stream.read_exact(&mut bytes).await?;
                    let actual = Hash32::from_bytes(Sha256::digest(&bytes).into());
                    if actual != meta.sha256 {
                        return Err(DaemonError::Protocol("blob digest does not match metadata"));
                    }
                    if let Some(previous) = offered_blobs.insert(meta.sha256, bytes) {
                        offered_blob_bytes =
                            offered_blob_bytes.saturating_sub(previous.len() as u64);
                    }
                    offered_blob_bytes = offered_blob_bytes.saturating_add(meta.bytes);
                }
                SwarmMsg::GetBlob(request) => {
                    let rooms = self
                        .inner
                        .rooms
                        .read()
                        .expect("room registry lock is not poisoned")
                        .keys()
                        .copied()
                        .collect::<Vec<_>>();
                    for room in rooms {
                        if !self.room_authorized(room, &authed)? {
                            continue;
                        }
                        let Ok(bytes) = self.store(room)?.read_blob(request.sha256) else {
                            continue;
                        };
                        self.send_blob_bytes(&mut stream, request.sha256, &bytes)
                            .await?;
                        break;
                    }
                }
                SwarmMsg::RequestVote(request) => {
                    let declared = peer
                        .decl
                        .iter()
                        .any(|declaration| declaration.room == request.room);
                    if declared
                        && self.room_authorized(request.room, &authed)?
                        && request.candidate == peer.node
                    {
                        if let Some(vote) = self.accept_request_vote(&request)? {
                            write_message(&mut stream, &SwarmMsg::Vote(vote)).await?;
                        }
                    }
                }
                SwarmMsg::Append(append) => {
                    let declared = peer
                        .decl
                        .iter()
                        .any(|declaration| declaration.room == append.room);
                    if declared
                        && self.room_authorized(append.room, &authed)?
                        && append.leader == peer.node
                    {
                        let store = self.store(append.room)?;
                        for blob in scene_blobs(&append.scene) {
                            if store.read_blob(blob.sha256).is_err() {
                                if let Some(bytes) = offered_blobs.remove(&blob.sha256) {
                                    offered_blob_bytes =
                                        offered_blob_bytes.saturating_sub(bytes.len() as u64);
                                    store.put_blob(&bytes)?;
                                }
                            }
                        }
                        match self.accept_append_with_role(&append, false)? {
                            AppendResponse::Cert(cert) => {
                                write_message(&mut stream, &SwarmMsg::Cert(cert)).await?;
                            }
                            AppendResponse::Nack(nack) => {
                                write_message(&mut stream, &SwarmMsg::Nack(nack)).await?;
                            }
                            AppendResponse::Refused => {}
                        }
                    }
                }
                SwarmMsg::Scene(record) => {
                    if !self.room_authorized(record.scene.room, &authed)? {
                        continue;
                    }
                    let _ = self.install_record(record.scene.room, record).await;
                }
                SwarmMsg::Commit(commit) => {
                    if !self.room_authorized(commit.room, &authed)? {
                        continue;
                    }
                    let store = self.store(commit.room)?;
                    for blob in scene_blobs(&commit.scene) {
                        if store.read_blob(blob.sha256).is_err() {
                            if let Some(bytes) = offered_blobs.remove(&blob.sha256) {
                                offered_blob_bytes =
                                    offered_blob_bytes.saturating_sub(bytes.len() as u64);
                                store.put_blob(&bytes)?;
                            }
                        }
                    }
                    let proof = commit.proof();
                    let record = CommittedScene {
                        scene: commit.scene,
                        commit_proof: proof,
                    };
                    let _ = self.install_record(record.scene.room, record).await;
                }
                SwarmMsg::Intent(intent) if self.room_authorized(intent.room, &authed)? => {
                    let _ = self.receive_intent(intent).await;
                }
                SwarmMsg::Intent(_) => {}
                SwarmMsg::Freeze(freeze) => {
                    if !self.room_authorized(freeze.room, &authed)? {
                        continue;
                    }
                    let declared_for_room = peer
                        .decl
                        .iter()
                        .any(|declaration| declaration.room == freeze.room);
                    let leader_is_peer = declared_for_room
                        && self.replay(freeze.room).is_ok_and(|replay| {
                            replay.chain.roster.contains(&peer.node)
                                && replay.consensus.leader_id == Some(peer.node)
                        });
                    if !leader_is_peer {
                        continue;
                    }
                    if let Ok(Some(close)) =
                        self.receive_freeze(freeze.room, freeze.grant_hash).await
                    {
                        self.send_blob_refs(&mut stream, freeze.room, &close.blobs)
                            .await?;
                        write_message(&mut stream, &SwarmMsg::CloseTake(close)).await?;
                    }
                }
                SwarmMsg::Heartbeat(heartbeat) => {
                    let declared_for_room = peer
                        .decl
                        .iter()
                        .any(|declaration| declaration.room == heartbeat.room);
                    if declared_for_room
                        && heartbeat.leader == peer.node
                        && self.room_authorized(heartbeat.room, &authed)?
                        && self.accept_heartbeat(&heartbeat)?
                    {
                        let daemon = self.clone();
                        let peer = peer.node;
                        tokio::spawn(async move {
                            let _ = daemon.sync_from_known_peer(peer, heartbeat.room).await;
                        });
                    }
                }
                SwarmMsg::Have(have) => {
                    let declared_for_room = peer
                        .decl
                        .iter()
                        .any(|declaration| declaration.room == have.room);
                    if declared_for_room
                        && self.room_authorized(have.room, &authed)?
                        && self
                            .replay(have.room)
                            .is_ok_and(|replay| replay.chain.head_n.is_none_or(|n| have.n > n))
                    {
                        let daemon = self.clone();
                        let peer = peer.node;
                        tokio::spawn(async move {
                            let _ = daemon.sync_from_known_peer(peer, have.room).await;
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn accept_request_vote(&self, request: &RequestVote) -> Result<Option<Vote>, DaemonError> {
        let room = request.room;
        let stakes = self
            .inner
            .joins
            .read()
            .expect("join registry lock is not poisoned")
            .get(&room)
            .is_some_and(|join| join.role == JoinRole::Stake);
        let store = self.store(room)?;
        let entry = self.replay_entry(room)?;
        let mut replay = entry.lock().expect("replay lock is not poisoned");
        let node = self.node_id();
        if !stakes
            || !replay.chain.roster.contains(&node)
            || !replay.chain.roster.contains(&request.candidate)
            || !valid_request_vote(request)
        {
            return Ok(None);
        }
        if request.rpc_term > replay.consensus.current_term {
            let pending = replay.pending.clone();
            let head_proof = replay.head_proof.clone();
            if advance_term(
                &mut replay.consensus,
                pending.as_ref(),
                head_proof.as_ref(),
                AdvanceSource::RosterMessage(request.rpc_term),
            ) {
                store.write_consensus(&replay.consensus)?;
            }
        }
        if request.rpc_term != replay.consensus.current_term
            || replay
                .consensus
                .voted_for
                .is_some_and(|voted| voted != request.candidate)
        {
            return Ok(None);
        }
        let local_tail = tail(
            replay.pending.as_ref(),
            replay.chain.head_n.zip(replay.chain.head_hash),
            replay.head_proof.as_ref(),
        )?;
        if !up_to_date(&request.tail(), &local_tail) {
            return Ok(None);
        }
        replay.consensus.voted_for = Some(request.candidate);
        replay.consensus.role = ConsensusRole::Follower;
        replay.consensus.leader_id = None;
        store.write_consensus(&replay.consensus)?;
        let mut vote = Vote {
            room,
            rpc_term: request.rpc_term,
            voter: node,
            candidate: request.candidate,
            last_n: local_tail.last_n,
            last_hash: local_tail.last_hash,
            last_rpc: local_tail.last_rpc,
            grant: true,
            sig: SignatureBytes::from_bytes([0; 64]),
        };
        vote.sig = SignatureBytes::from_bytes(sign(
            &self.inner.node_key,
            &signed_object_digest(&serde_json::to_value(&vote)?),
        ));
        self.reset_election_timeout(room);
        Ok(Some(vote))
    }

    fn accept_heartbeat(&self, heartbeat: &Heartbeat) -> Result<bool, DaemonError> {
        let store = self.store(heartbeat.room)?;
        let entry = self.replay_entry(heartbeat.room)?;
        let mut replay = entry.lock().expect("replay lock is not poisoned");
        if !replay.chain.roster.contains(&heartbeat.leader)
            || heartbeat.rpc_term < replay.consensus.current_term
        {
            return Ok(false);
        }
        let pending = replay.pending.clone();
        let head_proof = replay.head_proof.clone();
        let advanced = advance_term(
            &mut replay.consensus,
            pending.as_ref(),
            head_proof.as_ref(),
            AdvanceSource::RosterMessage(heartbeat.rpc_term),
        );
        if heartbeat.rpc_term != replay.consensus.current_term {
            return Ok(false);
        }
        replay.consensus.role = ConsensusRole::Follower;
        replay.consensus.leader_id = Some(heartbeat.leader);
        if advanced {
            store.write_consensus(&replay.consensus)?;
        }
        let catch_up = replay
            .chain
            .head_n
            .is_none_or(|head_n| heartbeat.n > head_n);
        drop(replay);
        self.reset_election_timeout(heartbeat.room);
        Ok(catch_up)
    }

    fn accept_append_with_role(
        &self,
        append: &Append,
        preserve_leader: bool,
    ) -> Result<AppendResponse, DaemonError> {
        let room = append.room;
        let stakes = self
            .inner
            .joins
            .read()
            .expect("join registry lock is not poisoned")
            .get(&room)
            .is_some_and(|join| join.role == JoinRole::Stake);
        let store = self.store(room)?;
        let floor = self.floor(room)?;
        let entry = self.replay_entry(room)?;
        let mut replay = entry.lock().expect("replay lock is not poisoned");
        let node = self.node_id();
        if !stakes
            || !replay.chain.roster.contains(&node)
            || !replay.chain.roster.contains(&append.leader)
            || append.scene.room != room
        {
            return Ok(AppendResponse::Refused);
        }
        if append.rpc_term > replay.consensus.current_term {
            let pending = replay.pending.clone();
            let head_proof = replay.head_proof.clone();
            if advance_term(
                &mut replay.consensus,
                pending.as_ref(),
                head_proof.as_ref(),
                AdvanceSource::RosterMessage(append.rpc_term),
            ) {
                store.write_consensus(&replay.consensus)?;
            }
        }
        if append.rpc_term != replay.consensus.current_term {
            return Ok(AppendResponse::Refused);
        }
        if !preserve_leader {
            replay.consensus.role = ConsensusRole::Follower;
            replay.consensus.leader_id = Some(append.leader);
        }
        self.reset_election_timeout(room);
        if replay.chain.head_n != Some(append.prev_n)
            || replay.chain.head_hash != Some(append.prev_hash)
        {
            let have = have_from_replay(room, &replay)?;
            return Ok(AppendResponse::Nack(Nack {
                room,
                have_n: have.n,
                have_hash: have.hash,
                have_rpc: have.rpc_term,
            }));
        }

        let hash = hash_scene(&append.scene);
        if let Some(pending) = &replay.pending {
            if pending.hash == hash {
                if append.rpc_term < pending.accepted_rpc_term
                    || (append.rpc_term == pending.accepted_rpc_term
                        && append.leader != pending.accepted_leader)
                {
                    return Ok(AppendResponse::Refused);
                }
                if append.rpc_term == pending.accepted_rpc_term {
                    return Ok(AppendResponse::Cert(cert_message_from_pending(pending)?));
                }
            } else if append.rpc_term <= pending.accepted_rpc_term {
                return Ok(AppendResponse::Refused);
            }
        }
        let carry_forward = replay
            .pending
            .as_ref()
            .is_some_and(|pending| pending.hash == hash);
        if !carry_forward {
            let mut resources = store.load_blob_inventory()?;
            resources.intents = floor
                .engine
                .lock()
                .expect("floor lock is not poisoned")
                .intents()
                .cloned()
                .collect();
            apply(
                &replay.chain,
                &append.scene,
                None,
                ApplyMode::Precert(&resources),
            )?;
        }
        let digest = cert_digest(
            &room,
            append.scene.n,
            hash.as_bytes(),
            append.rpc_term,
            &append.leader,
            &node,
        );
        let cert = Cert::node(
            node,
            SignatureBytes::from_bytes(sign(&self.inner.node_key, &digest)),
        );
        let pending = Pending {
            n: append.scene.n,
            hash,
            scene: append.scene.clone(),
            accepted_rpc_term: append.rpc_term,
            accepted_leader: append.leader,
            cert,
        };
        store.write_consensus(&replay.consensus)?;
        store.write_pending(&pending)?;
        let message = cert_message_from_pending(&pending)?;
        replay.pending = Some(pending);
        Ok(AppendResponse::Cert(message))
    }

    async fn handle_client_connection(
        &self,
        mut stream: impl AsyncRead + AsyncWrite + Unpin + Send,
        attach: ClientRequest,
    ) -> Result<(), DaemonError> {
        let ClientRequest::Attach { agent } = attach else {
            return Err(DaemonError::Protocol(
                "attach must be the first client frame",
            ));
        };
        self.inner
            .agents
            .write()
            .expect("agent registry lock is not poisoned")
            .insert(agent.clone());
        write_frame(
            &mut stream,
            &ClientReply::success(json!({ "agent": agent, "node": self.node_id() })),
        )
        .await?;
        while let Some(value) = read_frame::<_, Value>(&mut stream).await? {
            let request = match serde_json::from_value::<ClientRequest>(value) {
                Ok(request) => request,
                Err(error) => {
                    write_frame(
                        &mut stream,
                        &ClientReply::failure("invalid", error.to_string()),
                    )
                    .await?;
                    continue;
                }
            };
            if let ClientRequest::History {
                room,
                from_n,
                follow: true,
            } = request
            {
                return self.stream_history(&mut stream, room, from_n).await;
            }
            let reply = if let ClientRequest::PutBlob { room, name, bytes } = request {
                if bytes > MAX_BLOB_BYTES {
                    write_frame(
                        &mut stream,
                        &ClientReply::failure("invalid", "blob exceeds the 32 MiB limit"),
                    )
                    .await?;
                    return Ok(());
                }
                let raw_length = stream.read_u32().await? as u64;
                if raw_length != bytes {
                    write_frame(
                        &mut stream,
                        &ClientReply::failure(
                            "invalid",
                            "blob length prefix does not match metadata",
                        ),
                    )
                    .await?;
                    return Ok(());
                }
                let mut raw = vec![0_u8; bytes as usize];
                stream.read_exact(&mut raw).await?;
                match self.client_put_blob(agent.clone(), room, name, raw).await {
                    Ok(data) => ClientReply::success(data),
                    Err(error) => client_error_reply(error),
                }
            } else {
                self.execute_client(agent.clone(), request).await
            };
            write_frame(&mut stream, &reply).await?;
        }
        Ok(())
    }

    async fn admit_declared_stakers(&self, peer: &Hello) -> Result<(), DaemonError> {
        for declaration in &peer.decl {
            if declaration.role != JoinRole::Stake {
                continue;
            }
            let Ok(floor) = self.floor(declaration.room) else {
                continue;
            };
            let _mutation = floor.mutation.lock().await;
            let replay = self.replay(declaration.room)?;
            let Some(policy) = replay.chain.stake.as_ref() else {
                continue;
            };
            if replay.chain.live_grant.is_some()
                || replay.chain.roster.contains(&peer.node)
                || !eligible(policy, peer.node, declaration.role, &declaration.agents)
            {
                continue;
            }
            let mut next_roster = replay.chain.roster.clone();
            next_roster.push(peer.node);
            next_roster.sort_unstable();
            self.commit_singleton_body(
                declaration.room,
                Body::ViewChange {
                    add: vec![peer.node],
                    remove: Vec::new(),
                    next_roster,
                    closes_grant: None,
                },
                &floor,
            )
            .await?;
        }
        Ok(())
    }

    async fn execute_client(&self, agent: AgentId, request: ClientRequest) -> ClientReply {
        let result = match request {
            ClientRequest::Create {
                name,
                stake,
                floor,
                token,
            } => {
                let daemon = self.clone();
                match task::spawn_blocking(move || {
                    daemon.create_ticket_with_token(&name, stake, floor, token)
                })
                .await
                {
                    Ok(result) => result.map(|ticket| {
                        json!({
                            "id": ticket.id,
                            "magnet": ticket.to_magnet(),
                            "ticket": ticket,
                        })
                    }),
                    Err(error) => Err(error.into()),
                }
            }
            ClientRequest::Join { ticket, role } => match ticket.resolve() {
                Ok(ticket) => self
                    .join_ticket(ticket, role)
                    .await
                    .map(|chain| json!({ "id": chain.room, "role": role })),
                Err(error) => Err(error.into()),
            },
            ClientRequest::WaitForFloor { room, timeout_secs } => {
                self.client_wait_for_floor(agent, room, timeout_secs).await
            }
            ClientRequest::Speak {
                room,
                text,
                request_id,
            } => self.client_speak(agent, room, text, request_id).await,
            ClientRequest::Yield { room } => self.client_yield(agent, room).await,
            ClientRequest::RaiseHand { room } => self.client_raise_hand(agent, room).await,
            ClientRequest::Grant { room, to } => self.client_grant(agent, room, to).await,
            ClientRequest::Yank { room } => self.client_yank(agent, room).await,
            ClientRequest::Breakout {
                room,
                name,
                members,
            } => self.client_breakout(agent, room, name, members).await,
            ClientRequest::Membership { room, stake, floor } => {
                self.client_membership(agent, room, stake, floor).await
            }
            ClientRequest::Leave { room, vacate } => self.client_leave(agent, room, vacate).await,
            ClientRequest::PutBlob { .. } => Err(DaemonError::Protocol(
                "put_blob requires a following raw frame",
            )),
            ClientRequest::History {
                room,
                from_n,
                follow: false,
            } => self.history_page_from(room, from_n),
            ClientRequest::Status { room } => self.client_status(room),
            ClientRequest::Attach { .. } => Err(DaemonError::Protocol("duplicate attach")),
            ClientRequest::History { follow: true, .. } => {
                Err(DaemonError::Protocol("follow must use the streaming path"))
            }
        };

        result.map_or_else(client_error_reply, ClientReply::success)
    }

    async fn stream_history(
        &self,
        stream: &mut (impl AsyncWrite + Unpin),
        room: RoomId,
        mut from_n: u64,
    ) -> Result<(), DaemonError> {
        let floor = self.floor(room)?;
        loop {
            let changed = floor.changed.notified();
            let records = self
                .replay(room)?
                .history
                .into_iter()
                .filter(|record| record.scene.n >= from_n)
                .collect::<Vec<_>>();
            if records.is_empty() {
                changed.await;
                continue;
            }
            from_n = records
                .last()
                .expect("non-empty history batch")
                .scene
                .n
                .saturating_add(1);
            write_frame(
                stream,
                &ClientReply::success(self.history_page(room, records)?),
            )
            .await?;
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

    async fn client_grant(
        &self,
        agent: AgentId,
        room: RoomId,
        to: Mouth,
    ) -> Result<Value, DaemonError> {
        let floor = self.floor(room)?;
        let _mutation = floor.mutation.lock().await;
        let replay = self.replay(room)?;
        self.require_moderator(&replay, &agent)?;
        if replay.chain.live_grant.is_some() || !replay.chain.roster.contains(&to.node) {
            return Err(DaemonError::Protocol(
                "grant target is not currently grantable",
            ));
        }
        let intent_id = floor
            .engine
            .lock()
            .expect("floor lock is not poisoned")
            .intents()
            .find(|intent| {
                intent.agent == to.agent
                    && intent.node == to.node
                    && !replay.chain.consumed_intents.contains(&intent.id)
                    && replay.chain.roster.contains(&intent.node)
            })
            .map(|intent| intent.id)
            .ok_or(conch_core::apply::ApplyError::MissingIntent)?;
        let record = self
            .commit_singleton_body(
                room,
                Body::Grant {
                    to,
                    reason: GrantReason::Moderator,
                    intent_id,
                },
                &floor,
            )
            .await?;
        Ok(serde_json::to_value(record.scene)?)
    }

    async fn client_yank(&self, agent: AgentId, room: RoomId) -> Result<Value, DaemonError> {
        let floor = self.floor(room)?;
        let _mutation = floor.mutation.lock().await;
        let replay = self.replay(room)?;
        self.require_moderator(&replay, &agent)?;
        let grant = replay.chain.live_grant.ok_or(FloorError::NoGrant)?;
        if replay.chain.roster.len() > 1 {
            self.ensure_network_leader(room).await?;
            self.broadcast_heartbeat(room).await;
        }
        let local_frozen = {
            let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
            if engine
                .take()
                .is_some_and(|take| take.grant_hash == grant.hash)
            {
                let frozen = engine.freeze(room, grant.hash)?;
                let take = engine.take().expect("freeze preserves take").clone();
                Some((frozen, take))
            } else {
                None
            }
        };
        let (text, blobs) = if let Some((frozen, take)) = local_frozen {
            let store = self.store(room)?;
            task::spawn_blocking(move || write_take(&store, &take)).await??;
            (frozen.text, frozen.blobs)
        } else if grant.to.node != self.node_id() {
            match self
                .request_remote_freeze(grant.to.node, room, grant.hash)
                .await?
            {
                Some(close) => (close.text, close.blobs),
                None => (String::new(), Vec::new()),
            }
        } else {
            return Err(DaemonError::MutationUnavailable);
        };
        loop {
            self.commit_singleton_body(
                room,
                Body::Speech {
                    closes_grant: grant.hash,
                    text: text.clone(),
                    blobs: blobs.clone(),
                },
                &floor,
            )
            .await?;
            if self
                .replay(room)?
                .chain
                .live_grant
                .as_ref()
                .is_none_or(|live| live.hash != grant.hash)
            {
                break;
            }
        }
        Ok(json!({ "ok": true, "closes_grant": grant.hash }))
    }

    async fn client_membership(
        &self,
        agent: AgentId,
        room: RoomId,
        stake: Option<StakePolicy>,
        floor_config: Option<FloorConfig>,
    ) -> Result<Value, DaemonError> {
        let floor = self.floor(room)?;
        let _mutation = floor.mutation.lock().await;
        if !self.can_certify(room)? {
            return Err(FloorError::NotStaker.into());
        }
        let replay = self.replay(room)?;
        let mouth = Mouth {
            agent,
            node: self.node_id(),
        };
        let closes_grant = match &replay.chain.live_grant {
            Some(grant) if grant.to == mouth => Some(grant.hash),
            Some(_) => return Err(FloorError::NoGrant.into()),
            None => None,
        };
        if let Some(grant_hash) = closes_grant {
            let take = {
                let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
                engine.freeze(room, grant_hash)?;
                engine.take().expect("freeze preserves take").clone()
            };
            let store = self.store(room)?;
            task::spawn_blocking(move || write_take(&store, &take)).await??;
        }
        let stake = stake.unwrap_or_else(|| {
            replay
                .chain
                .stake
                .clone()
                .expect("committed room has stake policy")
        });
        let floor_config = floor_config.unwrap_or_else(|| floor_config_from_chain(&replay.chain));
        let record = self
            .commit_singleton_body(
                room,
                Body::Membership {
                    stake,
                    floor: floor_config,
                    closes_grant,
                },
                &floor,
            )
            .await?;
        Ok(serde_json::to_value(record.scene)?)
    }

    async fn client_breakout(
        &self,
        agent: AgentId,
        room: RoomId,
        name: String,
        members: Option<Vec<NodeId>>,
    ) -> Result<Value, DaemonError> {
        let floor = self.floor(room)?;
        let _mutation = floor.mutation.lock().await;
        let replay = self.replay(room)?;
        let mouth = Mouth {
            agent,
            node: self.node_id(),
        };
        let grant = replay
            .chain
            .live_grant
            .as_ref()
            .ok_or(FloorError::NoGrant)?;
        if grant.to != mouth {
            return Err(FloorError::NoGrant.into());
        }
        let mut auto_join = members.unwrap_or_else(|| replay.chain.roster.clone());
        auto_join.sort_unstable();
        auto_join.dedup();
        if auto_join
            .iter()
            .any(|node| !replay.chain.roster.contains(node))
        {
            return Err(DaemonError::Protocol(
                "breakout members must be in the parent roster",
            ));
        }
        let take = {
            let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
            engine.freeze(room, grant.hash)?;
            engine.take().expect("freeze preserves take").clone()
        };
        let store = self.store(room)?;
        task::spawn_blocking(move || write_take(&store, &take)).await??;

        let carried = replay.pending.as_ref().and_then(|pending| {
            if let Body::Breakout {
                closes_grant,
                ticket,
                auto_join,
            } = &pending.scene.body
            {
                (*closes_grant == grant.hash).then(|| (ticket.clone(), auto_join.clone()))
            } else {
                None
            }
        });
        let (child, body) = if let Some((ticket, carried_auto_join)) = carried {
            let child = serde_json::from_value::<Ticket>(ticket.clone())?;
            (
                child,
                Body::Breakout {
                    closes_grant: grant.hash,
                    ticket,
                    auto_join: carried_auto_join,
                },
            )
        } else {
            let token = self
                .inner
                .joins
                .read()
                .expect("join registry lock is not poisoned")
                .get(&room)
                .and_then(|join| join.token)
                .map(|_| Hash32::from_bytes(random::<[u8; 32]>()));
            let daemon = self.clone();
            let child_name = name.clone();
            let child = task::spawn_blocking(move || {
                daemon.prepare_breakout_ticket(
                    &child_name,
                    StakePolicy::default(),
                    FloorConfig::stick(30),
                    token,
                    room,
                )
            })
            .await??;
            let body = Body::Breakout {
                closes_grant: grant.hash,
                ticket: serde_json::to_value(&child)?,
                auto_join,
            };
            (child, body)
        };
        let committed = self.commit_singleton_body(room, body, &floor).await;
        let record = match committed {
            Ok(record) => record,
            Err(error) => {
                let child_is_pending = self.replay(room).is_ok_and(|replay| {
                    replay.pending.as_ref().is_some_and(|pending| {
                        matches!(
                            &pending.scene.body,
                            Body::Breakout { ticket, .. }
                                if serde_json::from_value::<Ticket>(ticket.clone())
                                    .is_ok_and(|ticket| ticket.id == child.id)
                        )
                    })
                });
                if !child_is_pending {
                    let stage = self.staged_breakout_path(child.id);
                    let _ = task::spawn_blocking(move || fs::remove_dir_all(stage)).await;
                }
                return Err(error);
            }
        };
        self.write_current_room(child.id)?;
        Ok(json!({
            "id": child.id,
            "magnet": child.to_magnet(),
            "ticket": child,
            "scene": record.scene,
        }))
    }

    async fn client_put_blob(
        &self,
        agent: AgentId,
        room: RoomId,
        name: String,
        bytes: Vec<u8>,
    ) -> Result<Value, DaemonError> {
        let floor = self.floor(room)?;
        let _mutation = floor.mutation.lock().await;
        let mouth = Mouth {
            agent,
            node: self.node_id(),
        };
        if floor
            .engine
            .lock()
            .expect("floor lock is not poisoned")
            .open_grant_for(&mouth)
            .is_none()
        {
            return Err(FloorError::NoGrant.into());
        }
        let store = self.store(room)?;
        let blob_store = store.clone();
        let verified = task::spawn_blocking(move || blob_store.put_blob(&bytes)).await??;
        let blob = BlobRef {
            name,
            sha256: verified.sha256,
            bytes: verified.bytes,
        };
        let take = {
            let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
            engine.attach_blob(&mouth, blob.clone())?;
            engine.take().expect("open grant has a take").clone()
        };
        task::spawn_blocking(move || write_take(&store, &take)).await??;
        Ok(serde_json::to_value(blob)?)
    }

    async fn client_leave(
        &self,
        agent: AgentId,
        room: RoomId,
        vacate: bool,
    ) -> Result<Value, DaemonError> {
        let floor = self.floor(room)?;
        let _mutation = floor.mutation.lock().await;
        let mut replay = self.replay(room)?;
        let node = self.node_id();
        if replay.chain.roster.contains(&node) {
            if let Some(grant) = replay.chain.live_grant.clone() {
                let mouth = Mouth { agent, node };
                if !vacate || grant.to != mouth {
                    return Err(DaemonError::MutationUnavailable);
                }
                let (frozen, take) = {
                    let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
                    let frozen = engine.freeze(room, grant.hash)?;
                    let take = engine.take().expect("freeze preserves take").clone();
                    (frozen, take)
                };
                let store = self.store(room)?;
                task::spawn_blocking(move || write_take(&store, &take)).await??;
                self.commit_singleton_body(
                    room,
                    Body::Speech {
                        closes_grant: frozen.grant_hash,
                        text: frozen.text,
                        blobs: frozen.blobs,
                    },
                    &floor,
                )
                .await?;
                replay = self.replay(room)?;
            }
            let mut next_roster = replay.chain.roster.clone();
            next_roster.retain(|member| *member != node);
            if next_roster.is_empty() {
                return Err(DaemonError::MutationUnavailable);
            }
            let record = self
                .commit_singleton_body(
                    room,
                    Body::ViewChange {
                        add: Vec::new(),
                        remove: vec![node],
                        next_roster,
                        closes_grant: None,
                    },
                    &floor,
                )
                .await?;
            let join = LocalJoin {
                role: JoinRole::Observe,
                token: self.room_token(room),
            };
            let store = self.store(room)?;
            let persisted = join.clone();
            task::spawn_blocking(move || write_local_join(&store, &persisted)).await??;
            self.inner
                .joins
                .write()
                .expect("join registry lock is not poisoned")
                .insert(room, join);
            return Ok(serde_json::to_value(record.scene)?);
        }
        let join = LocalJoin {
            role: JoinRole::Observe,
            token: self.room_token(room),
        };
        let store = self.store(room)?;
        let persisted = join.clone();
        task::spawn_blocking(move || write_local_join(&store, &persisted)).await??;
        self.inner
            .joins
            .write()
            .expect("join registry lock is not poisoned")
            .insert(room, join);
        Ok(json!({ "ok": true, "role": "observe" }))
    }

    fn require_moderator(&self, replay: &Replay, agent: &AgentId) -> Result<(), DaemonError> {
        let from = Mouth {
            agent: agent.clone(),
            node: self.node_id(),
        };
        if replay.chain.floor_mode != Some(FloorMode::Moderator)
            || replay.chain.moderator.as_ref() != Some(&from)
        {
            return Err(DaemonError::NotModerator);
        }
        Ok(())
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
        if !self.can_certify(room)? {
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
        let accepted_intent =
            replay
                .pending
                .as_ref()
                .and_then(|pending| match &pending.scene.body {
                    Body::Grant { to, intent_id, .. } if to.agent == agent && to.node == node => {
                        Some(*intent_id)
                    }
                    _ => None,
                });
        let replaced = existing.as_ref().map(|intent| intent.id);
        let (id, ts, kind) = match (kind, existing.as_ref()) {
            (_, Some(intent)) if accepted_intent == Some(intent.id) => {
                (intent.id, intent.ts, intent.kind)
            }
            (IntentKind::Wait, Some(intent)) => (intent.id, intent.ts, intent.kind),
            (_, Some(intent)) => (
                Hash32::from_bytes(random::<[u8; 32]>()),
                now.max(intent.ts.saturating_add(1)),
                kind,
            ),
            _ => (Hash32::from_bytes(random::<[u8; 32]>()), now, kind),
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
        let prior = replaced.filter(|prior| *prior != id);
        let gossip = stored.clone();
        task::spawn_blocking(move || -> Result<(), StoreError> {
            store.write_intent(&stored)?;
            if let Some(prior) = prior {
                store.remove_intent(prior)?;
            }
            Ok(())
        })
        .await??;
        self.broadcast_intent(&gossip).await;
        self.maybe_grant_next_locked(room, &floor).await?;
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
        let accepted = floor
            .engine
            .lock()
            .expect("floor lock is not poisoned")
            .upsert_intent(&replay.chain, intent.clone())?;
        if !accepted {
            return Ok(());
        }
        let store = self.store(room)?;
        let prior = replaced.filter(|prior| *prior != intent.id);
        task::spawn_blocking(move || -> Result<(), StoreError> {
            store.write_intent(&intent)?;
            if let Some(prior) = prior {
                store.remove_intent(prior)?;
            }
            Ok(())
        })
        .await??;
        let replay = self.replay(room)?;
        if replay.consensus.role == ConsensusRole::Leader
            && replay.consensus.leader_id == Some(self.node_id())
        {
            self.maybe_grant_next_locked(room, &floor).await?;
        }
        Ok(())
    }

    async fn broadcast_intent(&self, intent: &Intent) {
        let Ok(replay) = self.replay(intent.room) else {
            return;
        };
        for peer in replay
            .chain
            .roster
            .iter()
            .copied()
            .filter(|peer| *peer != self.node_id())
        {
            let Ok(mut stream) = self.connect_known_peer(peer, intent.room).await else {
                continue;
            };
            let _ = write_message(&mut stream, &SwarmMsg::Intent(intent.clone())).await;
        }
    }

    async fn receive_freeze(
        &self,
        room: RoomId,
        grant_hash: Hash32,
    ) -> Result<Option<CloseTake>, DaemonError> {
        let floor = self.floor(room)?;
        let _mutation = floor.mutation.lock().await;
        let (frozen, take) = {
            let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
            if engine
                .take()
                .is_none_or(|take| take.grant_hash != grant_hash)
            {
                return Ok(None);
            }
            let frozen = engine.freeze(room, grant_hash)?;
            let take = engine.take().expect("freeze preserves take").clone();
            (frozen, take)
        };
        let store = self.store(room)?;
        task::spawn_blocking(move || write_take(&store, &take)).await??;
        Ok(Some(CloseTake {
            room,
            grant_hash,
            text: frozen.text,
            rev: frozen.rev,
            blobs: frozen.blobs,
        }))
    }

    async fn request_remote_freeze(
        &self,
        holder: NodeId,
        room: RoomId,
        grant_hash: Hash32,
    ) -> Result<Option<CloseTake>, DaemonError> {
        const FREEZE_WAIT: Duration = Duration::from_secs(5);
        let started = Instant::now();
        let mut stream = match self.connect_known_peer(holder, room).await {
            Ok(stream) => stream,
            Err(_) => {
                sleep(FREEZE_WAIT.saturating_sub(started.elapsed())).await;
                return Ok(None);
            }
        };
        if write_message(
            &mut stream,
            &SwarmMsg::Freeze(conch_core::consensus::Freeze { room, grant_hash }),
        )
        .await
        .is_err()
        {
            sleep(FREEZE_WAIT.saturating_sub(started.elapsed())).await;
            return Ok(None);
        }
        let response = async {
            loop {
                match read_message(&mut stream).await? {
                    Some(SwarmMsg::BlobMeta(meta)) => {
                        self.receive_blob_into(room, meta, &mut stream).await?;
                    }
                    Some(SwarmMsg::CloseTake(close))
                        if close.room == room && close.grant_hash == grant_hash =>
                    {
                        return Ok(Some(close));
                    }
                    Some(_) => {}
                    None => return Ok(None),
                }
            }
        };
        match timeout(FREEZE_WAIT.saturating_sub(started.elapsed()), response).await {
            Ok(result) => result,
            // The holder may be durably CLOSING. Never replace acknowledged
            // text with an empty speech merely because its reply is delayed.
            Err(_) => Err(DaemonError::MutationUnavailable),
        }
    }

    async fn client_speak(
        &self,
        agent: AgentId,
        room: RoomId,
        text: String,
        request_id: String,
    ) -> Result<Value, DaemonError> {
        if !self.can_certify(room)? {
            return Err(FloorError::NotStaker.into());
        }
        let floor = self.floor(room)?;
        let _mutation = floor.mutation.lock().await;
        let mouth = Mouth {
            agent,
            node: self.node_id(),
        };
        let (response, take) = {
            let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
            let response = engine.speak(&mouth, &text, &request_id)?;
            let take = engine.take().cloned();
            (response, take)
        };
        if let Some(take) = take {
            let store = self.store(room)?;
            task::spawn_blocking(move || write_take(&store, &take)).await??;
        }
        Ok(serde_json::to_value(response)?)
    }

    async fn client_yield(&self, agent: AgentId, room: RoomId) -> Result<Value, DaemonError> {
        if !self.can_certify(room)? {
            return Err(FloorError::NotStaker.into());
        }
        let floor = self.floor(room)?;
        let _mutation = floor.mutation.lock().await;
        let mouth = Mouth {
            agent,
            node: self.node_id(),
        };
        let (frozen, take) = {
            let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
            let take = engine.take().ok_or(FloorError::NoGrant)?;
            if take.holder != mouth {
                return Err(FloorError::NoGrant.into());
            }
            let grant_hash = take.grant_hash;
            let frozen = engine.freeze(room, grant_hash)?;
            let take = engine.take().expect("freeze preserves take").clone();
            (frozen, take)
        };
        let store = self.store(room)?;
        task::spawn_blocking(move || write_take(&store, &take)).await??;
        loop {
            self.commit_singleton_body(
                room,
                Body::Speech {
                    closes_grant: frozen.grant_hash,
                    text: frozen.text.clone(),
                    blobs: frozen.blobs.clone(),
                },
                &floor,
            )
            .await?;
            if self
                .replay(room)?
                .chain
                .live_grant
                .as_ref()
                .is_none_or(|grant| grant.hash != frozen.grant_hash)
            {
                break;
            }
            // A different already-accepted body committed first. Preserve it,
            // then close this still-live grant at the following height.
        }
        self.maybe_grant_next_locked(room, &floor).await?;
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

    async fn maybe_grant_next_locked(
        &self,
        room: RoomId,
        floor: &Arc<RoomFloor>,
    ) -> Result<(), DaemonError> {
        loop {
            let replay = self.replay(room)?;
            if replay.chain.live_grant.is_some()
                || replay.chain.floor_mode != Some(conch_core::types::FloorMode::Stick)
            {
                return Ok(());
            }
            if replay
                .consensus
                .leader_id
                .is_some_and(|leader| leader != self.node_id())
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
            )
            .await?;
            // If the call first recovered some other pending body, loop and
            // propose the grant only after that exact accepted hash commits.
        }
    }

    async fn commit_singleton_body(
        &self,
        room: RoomId,
        body: Body,
        floor: &Arc<RoomFloor>,
    ) -> Result<CommittedScene, DaemonError> {
        loop {
            if self.replay(room)?.chain.roster.len() == 1 {
                let daemon = self.clone();
                let floor = Arc::clone(floor);
                let body = body.clone();
                return task::spawn_blocking(move || {
                    daemon.commit_singleton_body_blocking(room, body, &floor)
                })
                .await?;
            }
            match self
                .commit_distributed_body(room, body.clone(), floor)
                .await
            {
                Err(DaemonError::RecoveredHead) => continue,
                result => return result,
            }
        }
    }

    async fn commit_distributed_body(
        &self,
        room: RoomId,
        body: Body,
        floor: &RoomFloor,
    ) -> Result<CommittedScene, DaemonError> {
        if !self.can_certify(room)? {
            return Err(FloorError::NotStaker.into());
        }
        let node = self.node_id();
        let rpc_term = self.ensure_network_leader(room).await?;
        let replay = self.replay(room)?;
        if replay.consensus.current_term != rpc_term
            || replay.consensus.role != ConsensusRole::Leader
            || replay.consensus.leader_id != Some(node)
        {
            return Err(DaemonError::MutationUnavailable);
        }

        let mut probes = JoinSet::new();
        for peer in replay
            .chain
            .roster
            .iter()
            .copied()
            .filter(|peer| *peer != node)
        {
            let daemon = self.clone();
            probes.spawn(async move { (peer, daemon.probe_peer(peer, room).await) });
        }
        while let Some(result) = probes.join_next().await {
            if let Ok((peer, Ok(Some(have)))) = result {
                let local_n = replay.chain.head_n.expect("committed room has a head");
                let resolves_pending = replay
                    .pending
                    .as_ref()
                    .is_some_and(|pending| have.n == pending.n && have.hash == pending.hash);
                if have.n > local_n || resolves_pending {
                    self.sync_from_known_peer(peer, room).await?;
                    return Err(DaemonError::RecoveredHead);
                }
            }
        }

        let scene = replay.pending.as_ref().map_or_else(
            || Scene {
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
            },
            |pending| pending.scene.clone(),
        );
        let append = Append {
            room,
            rpc_term,
            leader: node,
            prev_n: replay.chain.head_n.expect("committed room has a head"),
            prev_hash: replay.chain.head_hash.expect("committed room has a hash"),
            scene: scene.clone(),
        };
        let self_cert = match self.accept_append_with_role(&append, true)? {
            AppendResponse::Cert(cert) => cert,
            _ => return Err(DaemonError::MutationUnavailable),
        };
        let mut certs = BTreeMap::from([(self_cert.node, self_cert)]);
        let peers = scene
            .roster
            .iter()
            .copied()
            .filter(|peer| *peer != node)
            .collect::<Vec<_>>();
        let retry_deadline = Instant::now() + Duration::from_secs(3);
        while certs.len() < majority(scene.roster.len()) && Instant::now() < retry_deadline {
            let mut attempts = JoinSet::new();
            for peer in peers
                .iter()
                .copied()
                .filter(|peer| !certs.contains_key(peer))
            {
                let daemon = self.clone();
                let append = append.clone();
                attempts.spawn(async move { (peer, daemon.send_append(peer, &append).await) });
            }
            let mut behind = Vec::new();
            while let Some(result) = attempts.join_next().await {
                let Ok((peer, Ok(response))) = result else {
                    continue;
                };
                match response {
                    AppendResponse::Cert(cert)
                        if valid_cert_message(&cert, &scene, rpc_term, node) =>
                    {
                        certs.entry(cert.node).or_insert(cert);
                    }
                    AppendResponse::Nack(nack) if nack.have_n > append.prev_n => {
                        self.sync_from_known_peer(peer, room).await?;
                        return Err(DaemonError::RecoveredHead);
                    }
                    AppendResponse::Nack(nack) if nack.have_n < append.prev_n => {
                        behind.push((peer, nack.have_n.saturating_add(1)));
                    }
                    AppendResponse::Nack(_) | AppendResponse::Refused | AppendResponse::Cert(_) => {
                    }
                }
            }
            for (peer, from_n) in behind {
                let _ = self.push_history(peer, room, from_n, append.prev_n).await;
            }
            let latest = self.replay(room)?;
            if latest.consensus.current_term != rpc_term
                || latest.consensus.role != ConsensusRole::Leader
                || latest.consensus.leader_id != Some(node)
            {
                return Err(DaemonError::MutationUnavailable);
            }
            if certs.len() < majority(scene.roster.len()) {
                sleep(Duration::from_millis(500)).await;
            }
        }
        if certs.len() < majority(scene.roster.len()) {
            self.demote_local_leader(room)?;
            return Err(DaemonError::MutationUnavailable);
        }
        let current = self.replay(room)?;
        if current.consensus.current_term != rpc_term
            || current.consensus.role != ConsensusRole::Leader
            || current.consensus.leader_id != Some(node)
        {
            return Err(DaemonError::MutationUnavailable);
        }
        let proof = CommitProof {
            rpc_term,
            leader: node,
            certs: certs.values().map(CertMessage::as_cert).collect(),
        };
        let store = self.store(room)?;
        let chain = store.persist_committed_scene(&current.chain, &scene, &proof)?;
        store.unlink_pending_if_stale(chain.head_n)?;
        let record = CommittedScene {
            scene: scene.clone(),
            commit_proof: proof.clone(),
        };
        self.finish_singleton_commit(
            room,
            &store,
            chain.clone(),
            record.clone(),
            current.consensus,
            floor,
        )?;
        let commit = conch_core::consensus::CommitMessage {
            room,
            n: scene.n,
            hash: hash_scene(&scene),
            rpc_term,
            leader: node,
            certs: proof.certs,
            scene,
        };
        for peer in current
            .chain
            .roster
            .iter()
            .copied()
            .filter(|peer| *peer != node)
        {
            let _ = self.push_commit(peer, &commit).await;
        }
        if !chain.roster.contains(&node) {
            let _ = self
                .wait_for_commit_haves(
                    room,
                    record.scene.n,
                    hash_scene(&record.scene),
                    &chain.roster,
                )
                .await;
            self.demote_local_leader(room)?;
        }
        Ok(record)
    }

    async fn ensure_network_leader(&self, room: RoomId) -> Result<u64, DaemonError> {
        let replay = self.replay(room)?;
        let node = self.node_id();
        if replay.consensus.role == ConsensusRole::Leader
            && replay.consensus.leader_id == Some(node)
            && replay.chain.roster.contains(&node)
        {
            return Ok(replay.consensus.current_term);
        }
        let store = self.store(room)?;
        let local_tail = tail(
            replay.pending.as_ref(),
            replay.chain.head_n.zip(replay.chain.head_hash),
            replay.head_proof.as_ref(),
        )?;
        let mut consensus = replay.consensus;
        let rpc_term = begin_campaign(&mut consensus, node, &replay.chain.roster, local_tail)?;
        store.write_consensus(&consensus)?;
        {
            let entry = self.replay_entry(room)?;
            entry.lock().expect("replay lock is not poisoned").consensus = consensus;
        }
        let mut request = RequestVote {
            room,
            rpc_term,
            candidate: node,
            last_n: local_tail.last_n,
            last_hash: local_tail.last_hash,
            last_rpc: local_tail.last_rpc,
            sig: SignatureBytes::from_bytes([0; 64]),
        };
        request.sig = SignatureBytes::from_bytes(sign(
            &self.inner.node_key,
            &signed_object_digest(&serde_json::to_value(&request)?),
        ));
        let mut votes = BTreeSet::from([node]);
        for peer in replay
            .chain
            .roster
            .iter()
            .copied()
            .filter(|peer| *peer != node)
        {
            if let Ok(Some(vote)) = self.request_vote(peer, &request).await {
                if valid_vote(&vote, &replay.chain.roster, node, rpc_term) {
                    votes.insert(vote.voter);
                }
            }
        }
        if votes.len() < majority(replay.chain.roster.len()) {
            return Err(DaemonError::MutationUnavailable);
        }
        {
            let entry = self.replay_entry(room)?;
            let mut replay = entry.lock().expect("replay lock is not poisoned");
            if replay.consensus.current_term != rpc_term || replay.consensus.voted_for != Some(node)
            {
                return Err(DaemonError::MutationUnavailable);
            }
            replay.consensus.role = ConsensusRole::Leader;
            replay.consensus.leader_id = Some(node);
        }
        self.broadcast_heartbeat(room).await;
        Ok(rpc_term)
    }

    fn demote_local_leader(&self, room: RoomId) -> Result<(), DaemonError> {
        let entry = self.replay_entry(room)?;
        let mut replay = entry.lock().expect("replay lock is not poisoned");
        replay.consensus.role = ConsensusRole::Follower;
        replay.consensus.leader_id = None;
        Ok(())
    }

    async fn connect_known_peer(
        &self,
        peer: NodeId,
        room: RoomId,
    ) -> Result<TcpStream, DaemonError> {
        for endpoint in self.peer_endpoints(peer) {
            let Some(authority) = endpoint.strip_prefix("tcp://") else {
                continue;
            };
            let Ok(addresses) = tokio::net::lookup_host(authority).await else {
                continue;
            };
            for address in addresses {
                let Ok(Ok(mut stream)) = timeout(SYNC_TIMEOUT, TcpStream::connect(address)).await
                else {
                    continue;
                };
                if write_message(&mut stream, &SwarmMsg::Hello(self.hello()))
                    .await
                    .is_err()
                {
                    continue;
                }
                let Ok(Some(message)) = timeout(SYNC_TIMEOUT, read_message(&mut stream))
                    .await
                    .map_err(|_| DaemonError::SyncTimeout)?
                else {
                    continue;
                };
                let Ok(remote) = validate_hello(message) else {
                    continue;
                };
                if remote.node != peer {
                    continue;
                }
                self.remember_peer(&remote)?;
                if let Some(token) = self.room_token(room) {
                    write_message(
                        &mut stream,
                        &SwarmMsg::Auth(conch_core::consensus::Auth { room, token }),
                    )
                    .await?;
                }
                return Ok(stream);
            }
        }
        Err(DaemonError::MutationUnavailable)
    }

    fn room_token(&self, room: RoomId) -> Option<Hash32> {
        self.inner
            .joins
            .read()
            .expect("join registry lock is not poisoned")
            .get(&room)
            .and_then(|join| join.token)
    }

    async fn probe_peer(
        &self,
        peer: NodeId,
        room: RoomId,
    ) -> Result<Option<HaveMessage>, DaemonError> {
        let mut stream = self.connect_known_peer(peer, room).await?;
        let probe = async {
            while let Some(message) = read_message(&mut stream).await? {
                if let SwarmMsg::Have(have) = message {
                    if have.room == room {
                        return Ok(Some(have));
                    }
                }
            }
            Ok::<_, DaemonError>(None)
        };
        match timeout(Duration::from_millis(500), probe).await {
            Ok(result) => result,
            Err(_) => Ok(None),
        }
    }

    async fn wait_for_commit_haves(
        &self,
        room: RoomId,
        n: u64,
        hash: Hash32,
        roster: &[NodeId],
    ) -> bool {
        let wait = async {
            loop {
                let mut probes = JoinSet::new();
                for peer in roster.iter().copied() {
                    let daemon = self.clone();
                    probes.spawn(async move { daemon.probe_peer(peer, room).await.ok().flatten() });
                }
                let mut confirmed = 0;
                while let Some(result) = probes.join_next().await {
                    if result
                        .ok()
                        .flatten()
                        .is_some_and(|have| have.n == n && have.hash == hash)
                    {
                        confirmed += 1;
                    }
                }
                if confirmed >= majority(roster.len()) {
                    return true;
                }
                sleep(Duration::from_millis(500)).await;
            }
        };
        timeout(Duration::from_secs(3), wait).await.unwrap_or(false)
    }

    async fn request_vote(
        &self,
        peer: NodeId,
        request: &RequestVote,
    ) -> Result<Option<Vote>, DaemonError> {
        let mut stream = self.connect_known_peer(peer, request.room).await?;
        write_message(&mut stream, &SwarmMsg::RequestVote(request.clone())).await?;
        let response = async {
            while let Some(message) = read_message(&mut stream).await? {
                if let SwarmMsg::Vote(vote) = message {
                    if vote.room == request.room && vote.rpc_term == request.rpc_term {
                        return Ok(Some(vote));
                    }
                }
            }
            Ok::<_, DaemonError>(None)
        };
        match timeout(Duration::from_millis(500), response).await {
            Ok(result) => result,
            Err(_) => Ok(None),
        }
    }

    async fn send_append(
        &self,
        peer: NodeId,
        append: &Append,
    ) -> Result<AppendResponse, DaemonError> {
        let mut stream = self.connect_known_peer(peer, append.room).await?;
        self.send_scene_blobs(&mut stream, &append.scene).await?;
        if let Body::Grant { intent_id, .. } = &append.scene.body {
            let floor = self.floor(append.room)?;
            let intent = floor
                .engine
                .lock()
                .expect("floor lock is not poisoned")
                .intents()
                .find(|intent| intent.id == *intent_id)
                .cloned();
            if let Some(intent) = intent {
                write_message(&mut stream, &SwarmMsg::Intent(intent)).await?;
            }
        }
        write_message(&mut stream, &SwarmMsg::Append(append.clone())).await?;
        let response = async {
            while let Some(message) = read_message(&mut stream).await? {
                match message {
                    SwarmMsg::Cert(cert)
                        if cert.room == append.room && cert.n == append.scene.n =>
                    {
                        return Ok(AppendResponse::Cert(cert));
                    }
                    SwarmMsg::Nack(nack) if nack.room == append.room => {
                        return Ok(AppendResponse::Nack(nack));
                    }
                    _ => {}
                }
            }
            Ok::<_, DaemonError>(AppendResponse::Refused)
        };
        match timeout(Duration::from_millis(500), response).await {
            Ok(result) => result,
            Err(_) => Ok(AppendResponse::Refused),
        }
    }

    async fn push_commit(
        &self,
        peer: NodeId,
        commit: &conch_core::consensus::CommitMessage,
    ) -> Result<(), DaemonError> {
        let mut stream = self.connect_known_peer(peer, commit.room).await?;
        self.send_scene_blobs(&mut stream, &commit.scene).await?;
        write_message(&mut stream, &SwarmMsg::Commit(commit.clone())).await
    }

    async fn push_history(
        &self,
        peer: NodeId,
        room: RoomId,
        from_n: u64,
        to_n: u64,
    ) -> Result<(), DaemonError> {
        let records = self
            .replay(room)?
            .history
            .into_iter()
            .filter(|record| record.scene.n >= from_n && record.scene.n <= to_n)
            .collect::<Vec<_>>();
        let mut stream = self.connect_known_peer(peer, room).await?;
        for record in records {
            self.send_scene_blobs(&mut stream, &record.scene).await?;
            write_message(&mut stream, &SwarmMsg::Scene(record)).await?;
        }
        Ok(())
    }

    async fn maintain_consensus(&self) {
        self.broadcast_heartbeats().await;
        let rooms = self
            .inner
            .rooms
            .read()
            .expect("room registry lock is not poisoned")
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for room in rooms {
            if !self.election_due(room) || !self.can_certify(room).unwrap_or(false) {
                continue;
            }
            self.reset_election_timeout(room);
            let Ok(floor) = self.floor(room) else {
                continue;
            };
            let Ok(_mutation) = floor.mutation.try_lock() else {
                continue;
            };
            let _ = self.ensure_network_leader(room).await;
        }
    }

    fn election_due(&self, room: RoomId) -> bool {
        let mut deadlines = self
            .inner
            .election_deadlines
            .lock()
            .expect("election deadline lock is not poisoned");
        let deadline = deadlines
            .entry(room)
            .or_insert_with(random_election_deadline);
        Instant::now() >= *deadline
    }

    fn reset_election_timeout(&self, room: RoomId) {
        self.inner
            .election_deadlines
            .lock()
            .expect("election deadline lock is not poisoned")
            .insert(room, random_election_deadline());
    }

    async fn broadcast_heartbeats(&self) {
        let rooms = self
            .inner
            .rooms
            .read()
            .expect("room registry lock is not poisoned")
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for room in rooms {
            self.broadcast_heartbeat(room).await;
        }
    }

    async fn broadcast_heartbeat(&self, room: RoomId) {
        let Ok(replay) = self.replay(room) else {
            return;
        };
        let node = self.node_id();
        if replay.consensus.role != ConsensusRole::Leader
            || replay.consensus.leader_id != Some(node)
            || !replay.chain.roster.contains(&node)
        {
            return;
        }
        let Ok(have) = have_from_replay(room, &replay) else {
            return;
        };
        let heartbeat = Heartbeat {
            room,
            rpc_term: replay.consensus.current_term,
            leader: node,
            n: have.n,
            hash: have.hash,
            have_rpc: have.rpc_term,
        };
        let mut sends = JoinSet::new();
        for peer in replay
            .chain
            .roster
            .iter()
            .copied()
            .filter(|peer| *peer != node)
        {
            let daemon = self.clone();
            let heartbeat = heartbeat.clone();
            sends.spawn(async move {
                let Ok(mut stream) = daemon.connect_known_peer(peer, room).await else {
                    return;
                };
                let _ = write_message(&mut stream, &SwarmMsg::Heartbeat(heartbeat)).await;
            });
        }
        while sends.join_next().await.is_some() {}
    }

    async fn sync_from_known_peer(&self, peer: NodeId, room: RoomId) -> Result<(), DaemonError> {
        let token = self.room_token(room);
        for endpoint in self.peer_endpoints(peer) {
            let Some(authority) = endpoint.strip_prefix("tcp://") else {
                continue;
            };
            let Ok(addresses) = tokio::net::lookup_host(authority).await else {
                continue;
            };
            for address in addresses {
                if self
                    .sync_room_from_inner(address, room, token, None)
                    .await
                    .is_ok()
                {
                    return Ok(());
                }
            }
        }
        Err(DaemonError::MutationUnavailable)
    }

    fn commit_singleton_body_blocking(
        &self,
        room: RoomId,
        body: Body,
        floor: &RoomFloor,
    ) -> Result<CommittedScene, DaemonError> {
        let store = self.store(room)?;
        let replay = self.replay(room)?;
        let node = self.node_id();
        if !self.can_certify(room)? || replay.chain.roster.as_slice() != [node] {
            return Err(DaemonError::MutationUnavailable);
        }
        if let Some(pending) = replay.pending.clone() {
            if pending.n != replay.chain.head_n.expect("committed room has a head") + 1 {
                return Err(DaemonError::Protocol("pending is not at head + 1"));
            }
            let proof = CommitProof {
                rpc_term: pending.accepted_rpc_term,
                leader: pending.accepted_leader,
                certs: vec![pending.cert],
            };
            let chain = store.persist_committed_scene(&replay.chain, &pending.scene, &proof)?;
            store.unlink_pending_if_stale(chain.head_n)?;
            let record = CommittedScene {
                scene: pending.scene,
                commit_proof: proof,
            };
            self.finish_singleton_commit(
                room,
                &store,
                chain,
                record.clone(),
                replay.consensus,
                floor,
            )?;
            return Ok(record);
        }
        let local_tail = tail(
            replay.pending.as_ref(),
            replay.chain.head_n.zip(replay.chain.head_hash),
            replay.head_proof.as_ref(),
        )?;
        let mut consensus = replay.consensus;
        let rpc_term =
            if consensus.role == ConsensusRole::Leader && consensus.leader_id == Some(node) {
                consensus.current_term
            } else {
                let term = begin_campaign(&mut consensus, node, &replay.chain.roster, local_tail)?;
                consensus.role = ConsensusRole::Leader;
                consensus.leader_id = Some(node);
                store.write_consensus(&consensus)?;
                term
            };
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
        let mut resources = store.load_blob_inventory()?;
        resources.intents = floor
            .engine
            .lock()
            .expect("floor lock is not poisoned")
            .intents()
            .cloned()
            .collect();
        apply(&replay.chain, &scene, None, ApplyMode::Precert(&resources))?;
        let hash = hash_scene(&scene);
        let digest = cert_digest(&room, scene.n, hash.as_bytes(), rpc_term, &node, &node);
        let cert = Cert::node(
            node,
            SignatureBytes::from_bytes(sign(&self.inner.node_key, &digest)),
        );
        let pending = conch_core::types::Pending {
            n: scene.n,
            hash,
            scene: scene.clone(),
            accepted_rpc_term: rpc_term,
            accepted_leader: node,
            cert: cert.clone(),
        };
        store.write_pending(&pending)?;
        {
            let entry = self.replay_entry(room)?;
            let mut cached = entry.lock().expect("replay lock is not poisoned");
            cached.consensus = consensus.clone();
            cached.pending = Some(pending);
        }
        let proof = CommitProof {
            rpc_term,
            leader: node,
            certs: vec![cert],
        };
        let chain = store.persist_committed_scene(&replay.chain, &scene, &proof)?;
        store.unlink_pending_if_stale(chain.head_n)?;
        let record = CommittedScene {
            scene: scene.clone(),
            commit_proof: proof.clone(),
        };
        self.finish_singleton_commit(room, &store, chain, record.clone(), consensus, floor)?;
        Ok(record)
    }

    fn finish_singleton_commit(
        &self,
        room: RoomId,
        store: &Store,
        chain: ChainState,
        record: CommittedScene,
        consensus: ConsensusState,
        floor: &RoomFloor,
    ) -> Result<(), DaemonError> {
        let breakout = match &record.scene.body {
            Body::Breakout { ticket, .. } => {
                Some(serde_json::from_value::<Ticket>(ticket.clone())?)
            }
            _ => None,
        };
        if let Body::Grant { intent_id, .. } = &record.scene.body {
            store.remove_intent(*intent_id)?;
        }
        self.cache_commit(room, chain.clone(), &record, Some(consensus))?;
        {
            let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
            engine.observe_committed(&chain);
            if let Some(take) = engine.take() {
                write_take(store, take)?;
            } else {
                remove_take(store)?;
            }
        }
        if let Some(ticket) = breakout {
            self.promote_staged_breakout(&ticket)?;
        }
        floor.changed.notify_waiters();
        Ok(())
    }

    fn hello(&self) -> Hello {
        let node = self.node_id();
        let agents = self
            .inner
            .agents
            .read()
            .expect("agent registry lock is not poisoned")
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let decl = self
            .inner
            .joins
            .read()
            .expect("join registry lock is not poisoned")
            .iter()
            .map(|(room, join)| {
                Declaration::signed(
                    *room,
                    join.role,
                    agents.clone(),
                    unix_timestamp(),
                    &self.inner.node_key,
                )
            })
            .collect();
        Hello {
            node,
            r#pub: node,
            addrs: {
                let mut addrs = self
                    .inner
                    .addrs
                    .read()
                    .expect("address registry lock is not poisoned")
                    .clone();
                addrs.extend(
                    self.inner
                        .trackers
                        .read()
                        .expect("tracker registry lock is not poisoned")
                        .iter()
                        .cloned(),
                );
                addrs
            },
            decl,
        }
    }

    fn haves(&self, authed: &BTreeSet<RoomId>) -> Result<Vec<HaveMessage>, DaemonError> {
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
            if !self.room_authorized(room, authed)? {
                continue;
            }
            let replay = self.replay(room)?;
            if replay.chain.head_n.is_some() {
                haves.push(have_from_replay(room, &replay)?);
            }
        }
        Ok(haves)
    }

    pub(crate) fn authenticate(&self, room: RoomId, token: Hash32) -> Result<bool, DaemonError> {
        let expected = match self.token_sha256(room) {
            Ok(expected) => expected,
            Err(DaemonError::UnknownRoom(_)) => return Ok(false),
            Err(error) => return Err(error),
        };
        let Some(expected) = expected else {
            return Ok(true);
        };
        Ok(Hash32::from_bytes(Sha256::digest(token.as_bytes()).into()) == expected)
    }

    fn room_authorized(
        &self,
        room: RoomId,
        authed: &BTreeSet<RoomId>,
    ) -> Result<bool, DaemonError> {
        match self.token_sha256(room) {
            Ok(required) => Ok(required.is_none() || authed.contains(&room)),
            Err(DaemonError::UnknownRoom(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn token_sha256(&self, room: RoomId) -> Result<Option<Hash32>, DaemonError> {
        let replay = self.replay(room)?;
        Ok(replay
            .history
            .first()
            .and_then(|record| match &record.scene.body {
                Body::Genesis { token_sha256, .. } => *token_sha256,
                _ => None,
            }))
    }

    pub(crate) fn served_ticket(&self, room: RoomId) -> Result<Ticket, DaemonError> {
        let bytes = fs::read(self.store(room)?.root().join("ticket.conch"))?;
        Ok(Ticket::from_json_slice(&bytes)?)
    }

    pub(crate) fn history_from(
        &self,
        room: RoomId,
        from_n: u64,
    ) -> Result<Vec<CommittedScene>, DaemonError> {
        Ok(self
            .replay(room)?
            .history
            .into_iter()
            .filter(|record| record.scene.n >= from_n)
            .collect())
    }

    pub(crate) fn history_page_from(
        &self,
        room: RoomId,
        from_n: u64,
    ) -> Result<Value, DaemonError> {
        self.history_page(room, self.history_from(room, from_n)?)
    }

    fn history_page(
        &self,
        room: RoomId,
        scenes: Vec<CommittedScene>,
    ) -> Result<Value, DaemonError> {
        let syncing = self
            .inner
            .syncing
            .read()
            .expect("sync registry lock is not poisoned")
            .contains(&room);
        Ok(json!({
            "scenes": scenes,
            "syncing": syncing,
            "complete": !syncing,
        }))
    }

    fn mark_syncing(&self, room: RoomId) -> SyncMarker {
        self.inner
            .syncing
            .write()
            .expect("sync registry lock is not poisoned")
            .insert(room);
        SyncMarker {
            inner: Arc::clone(&self.inner),
            room,
        }
    }

    async fn install_record(
        &self,
        room: RoomId,
        record: CommittedScene,
    ) -> Result<(), DaemonError> {
        let floor = self.floor(room)?;
        let _mutation = floor.mutation.lock().await;
        let breakout = match &record.scene.body {
            Body::Breakout {
                ticket, auto_join, ..
            } if auto_join.contains(&self.node_id()) => {
                Some(serde_json::from_value::<Ticket>(ticket.clone())?)
            }
            _ => None,
        };
        let daemon = self.clone();
        task::spawn_blocking(move || daemon.install_record_blocking(room, &record)).await??;
        drop(_mutation);
        if let Some(ticket) = breakout {
            if self.replay(ticket.id).is_err() {
                Box::pin(self.join_ticket(ticket, JoinRole::Stake)).await?;
            }
        }
        Ok(())
    }

    fn install_record_blocking(
        &self,
        room: RoomId,
        record: &CommittedScene,
    ) -> Result<(), DaemonError> {
        if record.scene.room != room {
            return Err(DaemonError::Protocol("scene belongs to a different room"));
        }
        let store = self.store(room)?;
        let replay = self.replay(room)?;
        let hash = hash_scene(&record.scene);
        if replay
            .history
            .iter()
            .any(|known| known.scene.n == record.scene.n && hash_scene(&known.scene) == hash)
        {
            return Ok(());
        }
        if replay
            .chain
            .head_n
            .is_some_and(|head_n| record.scene.n <= head_n)
        {
            return Ok(());
        }
        let chain =
            store.persist_committed_scene(&replay.chain, &record.scene, &record.commit_proof)?;
        store.unlink_pending_if_stale(chain.head_n)?;
        self.cache_commit(room, chain.clone(), record, None)?;
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
        if let Body::Breakout { ticket, .. } = &record.scene.body {
            let ticket = serde_json::from_value::<Ticket>(ticket.clone())?;
            self.promote_staged_breakout(&ticket)?;
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

    fn replay_entry(&self, room: RoomId) -> Result<Arc<Mutex<Replay>>, DaemonError> {
        self.inner
            .replays
            .read()
            .expect("replay registry lock is not poisoned")
            .get(&room)
            .cloned()
            .ok_or(DaemonError::UnknownRoom(room))
    }

    fn cache_commit(
        &self,
        room: RoomId,
        chain: ChainState,
        record: &CommittedScene,
        consensus_override: Option<ConsensusState>,
    ) -> Result<(), DaemonError> {
        let entry = self.replay_entry(room)?;
        let mut replay = entry.lock().expect("replay lock is not poisoned");
        replay.chain = chain;
        let head_n = replay.chain.head_n;
        replay.pending = replay
            .pending
            .take()
            .filter(|pending| head_n.is_none_or(|head_n| pending.n > head_n));
        replay.head_proof = Some(record.commit_proof.clone());
        if !replay
            .history
            .iter()
            .any(|known| known.scene.n == record.scene.n)
        {
            replay.history.push(record.clone());
        }
        if let Some(consensus) = consensus_override {
            replay.consensus = consensus;
        } else {
            let pending = replay.pending.clone();
            let head_proof = replay.head_proof.clone();
            let advanced = advance_term(
                &mut replay.consensus,
                pending.as_ref(),
                head_proof.as_ref(),
                AdvanceSource::VerifiedProof(record.commit_proof.rpc_term),
            );
            if record.commit_proof.rpc_term == replay.consensus.current_term
                && replay.chain.roster.contains(&record.commit_proof.leader)
                && record.commit_proof.leader != self.node_id()
            {
                replay.consensus.role = ConsensusRole::Follower;
                replay.consensus.leader_id = Some(record.commit_proof.leader);
            }
            if advanced {
                self.store(room)?.write_consensus(&replay.consensus)?;
            }
        }
        drop(replay);
        self.reset_election_timeout(room);
        Ok(())
    }

    fn recover_staged_breakouts(&self) -> Result<(), DaemonError> {
        let tickets = self
            .inner
            .replays
            .read()
            .expect("replay registry lock is not poisoned")
            .values()
            .flat_map(|entry| {
                entry
                    .lock()
                    .expect("replay lock is not poisoned")
                    .history
                    .iter()
                    .filter_map(|record| match &record.scene.body {
                        Body::Breakout { ticket, .. } => {
                            serde_json::from_value::<Ticket>(ticket.clone()).ok()
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        for ticket in tickets {
            self.promote_staged_breakout(&ticket)?;
        }
        Ok(())
    }

    fn promote_staged_breakout(&self, ticket: &Ticket) -> Result<(), DaemonError> {
        if self.store(ticket.id).is_ok() {
            return Ok(());
        }
        let stage = self.staged_breakout_path(ticket.id);
        if !stage.is_dir() {
            return Ok(());
        }
        let destination = self.room_path(ticket.id);
        fs::rename(&stage, &destination)?;
        sync_dir(
            stage
                .parent()
                .expect("staged breakout has a parent directory"),
        )?;
        sync_dir(
            destination
                .parent()
                .expect("room destination has a parent directory"),
        )?;
        self.track_room(ticket.id)
    }

    fn staged_breakout_path(&self, room: RoomId) -> PathBuf {
        self.inner
            .data_dir
            .join("staged-breakouts")
            .join(room.to_string())
    }

    fn room_path(&self, room: RoomId) -> PathBuf {
        self.inner.data_dir.join("rooms").join(room.to_string())
    }
}

fn client_error_reply(error: DaemonError) -> ClientReply {
    match error {
        DaemonError::UnknownRoom(_) => {
            ClientReply::failure("unknown_room", "room is not joined on this node")
        }
        DaemonError::Floor(FloorError::NoGrant) => {
            ClientReply::failure("no_grant", "no OPEN grant for this mouth")
        }
        DaemonError::Floor(FloorError::NotStaker) => {
            ClientReply::failure("unauthorized", "observer or removed node cannot speak")
        }
        DaemonError::Floor(FloorError::Timeout) => {
            ClientReply::failure("timeout", "wait-for-floor timed out")
        }
        error @ (DaemonError::Ticket(_) | DaemonError::BadTicket(_)) => {
            ClientReply::failure("bad_ticket", error.to_string())
        }
        error @ (DaemonError::JoinUnavailable
        | DaemonError::SyncTimeout
        | DaemonError::MutationUnavailable) => {
            ClientReply::failure("unavailable", error.to_string())
        }
        error @ DaemonError::Store(StoreError::SickHole(_)) => {
            ClientReply::failure("sick", error.to_string())
        }
        DaemonError::NotModerator => {
            ClientReply::failure("not_moderator", "request is not from the moderator mouth")
        }
        DaemonError::InvalidJoinRole => ClientReply::failure(
            "unauthorized",
            "a roster member must be removed before joining as an observer",
        ),
        error => ClientReply::failure("invalid", error.to_string()),
    }
}

fn floor_config_from_chain(chain: &ChainState) -> FloorConfig {
    FloorConfig {
        mode: chain.floor_mode.expect("committed room has a floor mode"),
        timeout_secs: chain
            .timeout_secs
            .expect("committed room has a floor timeout"),
        moderator: chain.moderator.clone(),
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
    let mut rooms = BTreeSet::new();
    if hello
        .decl
        .iter()
        .any(|declaration| !rooms.insert(declaration.room) || !declaration.verify(hello.node))
    {
        return Err(DaemonError::Protocol(
            "hello contains an invalid or duplicate declaration",
        ));
    }
    Ok(hello)
}

fn valid_request_vote(request: &RequestVote) -> bool {
    VerifyingKey::from_bytes(request.candidate.as_bytes()).is_ok_and(|key| {
        verify(
            &key,
            &signed_object_digest(
                &serde_json::to_value(request).expect("request_vote is serializable"),
            ),
            request.sig.as_bytes(),
        )
    })
}

fn valid_vote(vote: &Vote, roster: &[NodeId], candidate: NodeId, rpc_term: u64) -> bool {
    vote.grant
        && vote.candidate == candidate
        && vote.rpc_term == rpc_term
        && roster.contains(&vote.voter)
        && VerifyingKey::from_bytes(vote.voter.as_bytes()).is_ok_and(|key| {
            verify(
                &key,
                &signed_object_digest(&serde_json::to_value(vote).expect("vote is serializable")),
                vote.sig.as_bytes(),
            )
        })
}

fn cert_message_from_pending(pending: &Pending) -> Result<CertMessage, DaemonError> {
    let CertSigner::Node(node) = pending.cert.node else {
        return Err(DaemonError::Protocol(
            "pending cert cannot use the room key",
        ));
    };
    Ok(CertMessage {
        room: pending.scene.room,
        n: pending.n,
        hash: pending.hash,
        rpc_term: pending.accepted_rpc_term,
        leader: pending.accepted_leader,
        node,
        sig: pending.cert.sig,
    })
}

fn valid_cert_message(cert: &CertMessage, scene: &Scene, rpc_term: u64, leader: NodeId) -> bool {
    cert.room == scene.room
        && cert.n == scene.n
        && cert.hash == hash_scene(scene)
        && cert.rpc_term == rpc_term
        && cert.leader == leader
        && scene.roster.contains(&cert.node)
        && VerifyingKey::from_bytes(cert.node.as_bytes()).is_ok_and(|key| {
            let digest = cert_digest(
                &cert.room,
                cert.n,
                cert.hash.as_bytes(),
                cert.rpc_term,
                &cert.leader,
                &cert.node,
            );
            verify(&key, &digest, cert.sig.as_bytes())
        })
}

fn majority(roster_len: usize) -> usize {
    roster_len / 2 + 1
}

fn random_election_deadline() -> Instant {
    Instant::now() + Duration::from_millis(3_000 + random::<u64>() % 3_001)
}

fn client_peer_allowed(peer: SocketAddr) -> bool {
    peer.ip().is_loopback()
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

fn scene_blobs(scene: &Scene) -> &[BlobRef] {
    match &scene.body {
        Body::Speech { blobs, .. } => blobs,
        _ => &[],
    }
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

fn write_ticket(store: &Store, ticket: &Ticket) -> Result<(), DaemonError> {
    let mut stripped = ticket.clone();
    stripped.token = None;
    write_json_atomic(&store.root().join("ticket.conch"), &stripped)
}

fn read_local_join(store: &Store) -> Result<Option<LocalJoin>, DaemonError> {
    match fs::read(store.root().join("join.json")) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_local_join(store: &Store, join: &LocalJoin) -> Result<(), DaemonError> {
    let path = store.root().join("join.json");
    let temporary = path.with_extension(format!("tmp-{}", hex::encode(random::<[u8; 8]>())));
    let bytes = serde_json::to_vec(join)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, &path)?;
    sync_dir(store.root())?;
    Ok(())
}

fn verify_ticket_replay(ticket: &Ticket, replay: &Replay) -> Result<(), DaemonError> {
    let genesis = replay
        .history
        .first()
        .ok_or(DaemonError::BadTicket("peer did not provide genesis"))?;
    if genesis.scene.n != 0
        || genesis.scene.room != ticket.id
        || hash_scene(&genesis.scene) != ticket.genesis
    {
        return Err(DaemonError::BadTicket(
            "room id or genesis hash does not match",
        ));
    }
    let Body::Genesis { token_sha256, .. } = &genesis.scene.body else {
        return Err(DaemonError::BadTicket("height zero is not genesis"));
    };
    let ticket_token_sha256 = ticket
        .token
        .map(|token| Hash32::from_bytes(Sha256::digest(token.as_bytes()).into()));
    if token_sha256 != &ticket_token_sha256 {
        return Err(DaemonError::BadTicket(
            "ticket token does not match genesis",
        ));
    }
    Ok(())
}

fn canonical_ticket(ticket: &Ticket, replay: &Replay) -> Result<Ticket, DaemonError> {
    let genesis = replay
        .history
        .first()
        .ok_or(DaemonError::BadTicket("peer did not provide genesis"))?;
    let Body::Genesis {
        name,
        stake,
        floor,
        parent_room,
        ..
    } = &genesis.scene.body
    else {
        return Err(DaemonError::BadTicket("height zero is not genesis"));
    };
    let mut canonical = ticket.clone();
    canonical.name = name.clone();
    canonical.stake = stake.clone();
    canonical.floor = floor.clone();
    canonical.parent = *parent_room;
    Ok(canonical)
}

fn read_take(store: &Store) -> Result<Option<TakeBuffer>, DaemonError> {
    for name in ["close_take.json", "take.json"] {
        let path = store.root().join(name);
        match fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(take) => return Ok(Some(take)),
                Err(_) => {
                    let path = store.root().join(name);
                    fs::remove_file(&path)?;
                    sync_dir(store.root())?;
                    eprintln!("conchd: discarded invalid take file {}", path.display());
                }
            },
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn restart_commits_exact_pending_hash_before_a_fresh_body() {
        let data = TempDir::new().unwrap();
        let daemon = Daemon::open(data.path()).unwrap();
        let room = daemon.create_genesis("pending recovery").unwrap();
        let replay = daemon.replay(room).unwrap();
        let node = daemon.node_id();
        let local_tail = tail(
            replay.pending.as_ref(),
            replay.chain.head_n.zip(replay.chain.head_hash),
            replay.head_proof.as_ref(),
        )
        .unwrap();
        let mut consensus = replay.consensus;
        let rpc_term =
            begin_campaign(&mut consensus, node, &replay.chain.roster, local_tail).unwrap();
        let staged = Scene {
            v: 1,
            room,
            n: replay.chain.head_n.unwrap() + 1,
            term: rpc_term,
            parent: replay.chain.head_hash,
            roster: replay.chain.roster.clone(),
            leader: node,
            ts: unix_timestamp(),
            body: Body::Membership {
                stake: replay.chain.stake.clone().unwrap(),
                floor: FloorConfig::stick(31),
                closes_grant: None,
            },
            certs: Vec::new(),
        };
        apply(
            &replay.chain,
            &staged,
            None,
            ApplyMode::Precert(&conch_core::apply::ApplyResources::default()),
        )
        .unwrap();
        let hash = hash_scene(&staged);
        let digest = cert_digest(&room, staged.n, hash.as_bytes(), rpc_term, &node, &node);
        let pending = conch_core::types::Pending {
            n: staged.n,
            hash,
            scene: staged.clone(),
            accepted_rpc_term: rpc_term,
            accepted_leader: node,
            cert: Cert::node(
                node,
                SignatureBytes::from_bytes(sign(&daemon.inner.node_key, &digest)),
            ),
        };
        let store = daemon.store(room).unwrap();
        store.write_consensus(&consensus).unwrap();
        store.write_pending(&pending).unwrap();
        drop(daemon);

        let restarted = Daemon::open(data.path()).unwrap();
        let floor = restarted.floor(room).unwrap();
        let committed = restarted
            .commit_singleton_body_blocking(
                room,
                Body::Membership {
                    stake: replay.chain.stake.unwrap(),
                    floor: FloorConfig::stick(32),
                    closes_grant: None,
                },
                &floor,
            )
            .unwrap();

        assert_eq!(committed.scene, staged);
        assert_eq!(hash_scene(&committed.scene), hash);
        assert!(restarted.replay(room).unwrap().pending.is_none());
    }

    #[test]
    fn client_protocol_is_loopback_only() {
        assert!(client_peer_allowed("127.0.0.1:7421".parse().unwrap()));
        assert!(client_peer_allowed("[::1]:7421".parse().unwrap()));
        assert!(!client_peer_allowed("192.168.1.5:7421".parse().unwrap()));
    }

    #[test]
    fn corrupt_ephemeral_floor_files_do_not_brick_restart() {
        let data = TempDir::new().unwrap();
        let daemon = Daemon::open(data.path()).unwrap();
        let room = daemon.create_genesis("corrupt floor files").unwrap();
        let root = daemon.store(room).unwrap().root().to_path_buf();
        fs::write(root.join("take.json"), b"not json").unwrap();
        fs::write(root.join("intents").join("bad.json"), b"not json").unwrap();
        drop(daemon);

        let restarted = Daemon::open(data.path()).unwrap();
        assert_eq!(restarted.replay(room).unwrap().chain.head_n, Some(0));
        assert!(!root.join("take.json").exists());
        assert!(!root.join("intents").join("bad.json").exists());
    }
}
