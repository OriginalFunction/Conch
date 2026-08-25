use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, RwLock},
    task::{Context as TaskContext, Poll},
    time::{SystemTime, UNIX_EPOCH},
};

use conch_core::{
    apply::{apply, ApplyMode},
    client::{ClientReply, ClientRequest},
    consensus::{
        advance_term, begin_campaign, tail, up_to_date, AdvanceSource, Append, Auth, Authed,
        BlobMeta, BreakoutReq, CertMessage, CloseTake, ConsensusError, GetScenes, GrantReq,
        HaveMessage, Heartbeat, HelloAck, HelloI, HelloR, Leave, MembershipReq, Nack, PeerInfo,
        Pex, RequestVote, SwarmMsg, Vote, YankReq,
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
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore},
    task::{self, JoinError, JoinHandle, JoinSet},
    time::{interval, sleep, timeout, Duration, Instant, MissedTickBehavior},
};
use tokio_rustls::{
    rustls::{pki_types::ServerName, ClientConfig, ServerConfig},
    TlsAcceptor, TlsConnector,
};

const SYNC_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const PREAUTH_FRAME_BYTES: usize = 64 * 1024;
const HANDSHAKE_LABEL: &str = "conch-swarm-v1";
const MAX_BLOB_BYTES: u64 = 32 * 1024 * 1024;
const MAX_OFFERED_BLOBS_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 1024;
const MAX_CONNECTIONS_PER_SOURCE: usize = 64;
const AUTH_FAILURE_BURST: u32 = 20;
const AUTH_FAILURE_DECAY_INTERVAL: Duration = Duration::from_secs(6);
const INTENT_FORWARD_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const REMOVE_AFTER_SECONDS: u64 = 300;
const MAX_HISTORY_WAIT_SECS: u64 = 300;

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
    #[error("open rooms are local/LAN only")]
    OpenRoomInPublic,
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
    room_agents: RwLock<BTreeMap<RoomId, BTreeSet<AgentId>>>,
    addrs: RwLock<Vec<String>>,
    trackers: RwLock<Vec<String>>,
    peers: RwLock<BTreeMap<RoomId, BTreeMap<NodeId, Vec<String>>>>,
    last_seen: RwLock<BTreeMap<RoomId, BTreeMap<NodeId, u64>>>,
    declarations: RwLock<BTreeMap<RoomId, BTreeMap<NodeId, Declaration>>>,
    election_deadlines: Mutex<BTreeMap<RoomId, Instant>>,
    intent_forwards: Mutex<BTreeMap<RoomId, IntentForwardState>>,
    syncing: RwLock<BTreeSet<RoomId>>,
    transport: RwLock<TransportConfig>,
    browser_sessions: Mutex<Vec<BrowserSession>>,
    operator_sessions: Mutex<Vec<OperatorSession>>,
    connection_slots: Arc<Semaphore>,
    source_connections: Mutex<BTreeMap<IpAddr, usize>>,
    auth_failures: Mutex<BTreeMap<IpAddr, AuthFailureBucket>>,
    outbound_slots: Arc<Semaphore>,
    room_dials: Mutex<BTreeMap<RoomId, usize>>,
}

struct AuthFailureBucket {
    failures: u32,
    updated: Instant,
}

struct IntentForwardState {
    leader: NodeId,
    rpc_term: u64,
    ids: BTreeSet<Hash32>,
    retry_after: Instant,
}

pub(crate) struct ConnectionGuard {
    _permit: OwnedSemaphorePermit,
    inner: Arc<Inner>,
    source: IpAddr,
}

pub(crate) struct DialGuard {
    _permit: OwnedSemaphorePermit,
    inner: Arc<Inner>,
    room: RoomId,
}

impl Drop for DialGuard {
    fn drop(&mut self) {
        let mut rooms = self
            .inner
            .room_dials
            .lock()
            .expect("room dial lock is not poisoned");
        if let Some(count) = rooms.get_mut(&self.room) {
            *count -= 1;
            if *count == 0 {
                rooms.remove(&self.room);
            }
        }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let mut sources = self
            .inner
            .source_connections
            .lock()
            .expect("source connection lock is not poisoned");
        if let Some(count) = sources.get_mut(&self.source) {
            *count -= 1;
            if *count == 0 {
                sources.remove(&self.source);
            }
        }
    }
}

struct BrowserSession {
    digest: [u8; 32],
    room: RoomId,
    origin: String,
    created: u64,
    expires: u64,
}

struct OperatorSession {
    digest: [u8; 32],
    origin: String,
    created: u64,
    expires: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    Local,
    Lan,
    Public,
}

#[derive(Clone)]
struct TransportConfig {
    mode: TransportMode,
    tls_client: Option<Arc<ClientConfig>>,
}

trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}
type BoxedStream = Box<dyn IoStream>;

struct GuardedStream {
    stream: BoxedStream,
    _dial_guard: DialGuard,
}

impl AsyncRead for GuardedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for GuardedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.stream).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.stream).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.stream).poll_shutdown(context)
    }
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

struct AuthenticatedPeer {
    node: NodeId,
    decl: Vec<Declaration>,
}

pub struct RunningServer {
    addr: SocketAddr,
    task: JoinHandle<Result<(), DaemonError>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionProtocol {
    Tcp { allow_client: bool },
    Swarm,
    Client { allowed_room: RoomId },
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
        fs::create_dir_all(data_dir.join("staged-breakouts"))?;
        harden_data_dir(&data_dir)?;
        let node_key = load_or_create_node_key(&data_dir)?;
        let node_id = NodeId::from_bytes(node_key.verifying_key().to_bytes());
        let mut rooms = BTreeMap::new();
        let mut replays = BTreeMap::new();
        let mut floors = BTreeMap::new();
        let mut joins = BTreeMap::new();
        let peers: BTreeMap<RoomId, BTreeMap<NodeId, Vec<String>>> =
            fs::read(data_dir.join("peers.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
        let mut last_seen: BTreeMap<RoomId, BTreeMap<NodeId, u64>> =
            fs::read(data_dir.join("last-seen.json"))
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
        let now = unix_timestamp();
        for (room, replay) in &replays {
            let replay = replay.lock().expect("replay lock is not poisoned");
            let room_seen = last_seen.entry(*room).or_default();
            for peer in &replay.chain.roster {
                room_seen.entry(*peer).or_insert(now);
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
                room_agents: RwLock::new(BTreeMap::new()),
                addrs: RwLock::new(Vec::new()),
                trackers: RwLock::new(Vec::new()),
                peers: RwLock::new(peers),
                last_seen: RwLock::new(last_seen),
                declarations: RwLock::new(BTreeMap::new()),
                election_deadlines: Mutex::new(BTreeMap::new()),
                intent_forwards: Mutex::new(BTreeMap::new()),
                syncing: RwLock::new(BTreeSet::new()),
                transport: RwLock::new(TransportConfig {
                    mode: TransportMode::Local,
                    tls_client: None,
                }),
                browser_sessions: Mutex::new(Vec::new()),
                operator_sessions: Mutex::new(Vec::new()),
                connection_slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
                source_connections: Mutex::new(BTreeMap::new()),
                auth_failures: Mutex::new(BTreeMap::new()),
                outbound_slots: Arc::new(Semaphore::new(64)),
                room_dials: Mutex::new(BTreeMap::new()),
            }),
        };
        daemon.recover_staged_breakouts()?;
        Ok(daemon)
    }

    pub fn node_id(&self) -> NodeId {
        NodeId::from_bytes(self.inner.node_key.verifying_key().to_bytes())
    }

    pub fn configure_transport(
        &self,
        mode: TransportMode,
        tls_client: Option<Arc<ClientConfig>>,
    ) -> Result<(), DaemonError> {
        if mode == TransportMode::Public && tls_client.is_none() {
            return Err(DaemonError::Protocol(
                "public mode requires TLS client trust",
            ));
        }
        if mode == TransportMode::Public {
            let rooms: Vec<RoomId> = self
                .inner
                .rooms
                .read()
                .expect("room registry lock is not poisoned")
                .keys()
                .copied()
                .collect();
            for room in rooms {
                if self.token_sha256(room)?.is_none() {
                    return Err(DaemonError::OpenRoomInPublic);
                }
            }
        }
        *self
            .inner
            .transport
            .write()
            .expect("transport lock is not poisoned") = TransportConfig { mode, tls_client };
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_transport_for_test(
        &self,
        mode: TransportMode,
        tls_client: Option<Arc<ClientConfig>>,
    ) {
        *self
            .inner
            .transport
            .write()
            .expect("transport lock is not poisoned") = TransportConfig { mode, tls_client };
    }

    pub(crate) fn transport_mode(&self) -> TransportMode {
        self.inner
            .transport
            .read()
            .expect("transport lock is not poisoned")
            .mode
    }

    pub(crate) fn tls_client_config(&self) -> Option<Arc<ClientConfig>> {
        self.inner
            .transport
            .read()
            .expect("transport lock is not poisoned")
            .tls_client
            .clone()
    }

    pub(crate) fn connection_guard(&self, source: IpAddr) -> Option<ConnectionGuard> {
        let permit = Arc::clone(&self.inner.connection_slots)
            .try_acquire_owned()
            .ok()?;
        let mut sources = self
            .inner
            .source_connections
            .lock()
            .expect("source connection lock is not poisoned");
        let count = sources.entry(source).or_default();
        if *count >= MAX_CONNECTIONS_PER_SOURCE {
            return None;
        }
        *count += 1;
        drop(sources);
        Some(ConnectionGuard {
            _permit: permit,
            inner: Arc::clone(&self.inner),
            source,
        })
    }

    pub(crate) fn auth_allowed(&self, source: IpAddr) -> bool {
        let mut buckets = self
            .inner
            .auth_failures
            .lock()
            .expect("auth failure lock is not poisoned");
        let Some(bucket) = buckets.get_mut(&source) else {
            return true;
        };
        decay_auth_bucket(bucket);
        bucket.failures < AUTH_FAILURE_BURST
    }

    pub(crate) fn dial_guard(&self, room: RoomId) -> Option<DialGuard> {
        let permit = Arc::clone(&self.inner.outbound_slots)
            .try_acquire_owned()
            .ok()?;
        let mut rooms = self
            .inner
            .room_dials
            .lock()
            .expect("room dial lock is not poisoned");
        let count = rooms.entry(room).or_default();
        if *count >= 8 {
            return None;
        }
        *count += 1;
        drop(rooms);
        Some(DialGuard {
            _permit: permit,
            inner: Arc::clone(&self.inner),
            room,
        })
    }

    pub(crate) fn record_auth_failure(&self, source: IpAddr) {
        let mut buckets = self
            .inner
            .auth_failures
            .lock()
            .expect("auth failure lock is not poisoned");
        let bucket = buckets.entry(source).or_insert(AuthFailureBucket {
            failures: 0,
            updated: Instant::now(),
        });
        decay_auth_bucket(bucket);
        bucket.failures = bucket.failures.saturating_add(1).min(AUTH_FAILURE_BURST);
    }

    pub(crate) fn create_browser_session(
        &self,
        room: RoomId,
        origin: String,
        token: Hash32,
    ) -> Result<String, DaemonError> {
        if self.token_sha256(room)?.is_none() || !self.authenticate(room, token)? {
            return Err(DaemonError::Protocol("room capability is invalid"));
        }
        let raw = random::<[u8; 32]>();
        let digest: [u8; 32] = Sha256::digest(raw).into();
        let now = unix_timestamp();
        let mut sessions = self
            .inner
            .browser_sessions
            .lock()
            .expect("browser session lock is not poisoned");
        sessions.retain(|session| session.expires > now);
        if sessions.len() >= 4096 {
            let oldest = sessions
                .iter()
                .enumerate()
                .min_by_key(|(_, session)| session.created)
                .map(|(index, _)| index)
                .expect("a full session table is non-empty");
            sessions.swap_remove(oldest);
        }
        sessions.push(BrowserSession {
            digest,
            room,
            origin,
            created: now,
            expires: now.saturating_add(15 * 60),
        });
        Ok(hex::encode(raw))
    }

    pub(crate) fn validate_browser_session(
        &self,
        raw: &str,
        room: Option<RoomId>,
        origin: &str,
    ) -> Option<RoomId> {
        let bytes = hex::decode(raw).ok()?;
        let raw: [u8; 32] = bytes.try_into().ok()?;
        let digest: [u8; 32] = Sha256::digest(raw).into();
        let now = unix_timestamp();
        let mut sessions = self.inner.browser_sessions.lock().ok()?;
        sessions.retain(|session| session.expires > now);
        sessions
            .iter()
            .find(|session| {
                bool::from(session.digest.ct_eq(&digest))
                    && room.is_none_or(|room| room == session.room)
                    && session.origin == origin
            })
            .map(|session| session.room)
    }

    pub(crate) fn revoke_browser_session(&self, raw: &str) {
        let Some(raw) = hex::decode(raw)
            .ok()
            .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
        else {
            return;
        };
        let digest: [u8; 32] = Sha256::digest(raw).into();
        if let Ok(mut sessions) = self.inner.browser_sessions.lock() {
            sessions.retain(|session| !bool::from(session.digest.ct_eq(&digest)));
        }
    }

    pub(crate) fn create_operator_session(&self, origin: String) -> String {
        let raw = random::<[u8; 32]>();
        let digest: [u8; 32] = Sha256::digest(raw).into();
        let now = unix_timestamp();
        let mut sessions = self
            .inner
            .operator_sessions
            .lock()
            .expect("operator session lock is not poisoned");
        sessions.retain(|session| session.expires > now);
        if sessions.len() >= 4096 {
            let oldest = sessions
                .iter()
                .enumerate()
                .min_by_key(|(_, session)| session.created)
                .map(|(index, _)| index)
                .expect("a full session table is non-empty");
            sessions.swap_remove(oldest);
        }
        sessions.push(OperatorSession {
            digest,
            origin,
            created: now,
            expires: now.saturating_add(15 * 60),
        });
        hex::encode(raw)
    }

    pub(crate) fn validate_operator_session(&self, raw: &str, origin: &str) -> bool {
        let Some(raw) = hex::decode(raw)
            .ok()
            .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
        else {
            return false;
        };
        let digest: [u8; 32] = Sha256::digest(raw).into();
        let now = unix_timestamp();
        let Ok(mut sessions) = self.inner.operator_sessions.lock() else {
            return false;
        };
        sessions.retain(|session| session.expires > now);
        sessions
            .iter()
            .any(|session| bool::from(session.digest.ct_eq(&digest)) && session.origin == origin)
    }

    pub(crate) fn revoke_operator_session(&self, raw: &str) {
        let Some(raw) = hex::decode(raw)
            .ok()
            .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
        else {
            return;
        };
        let digest: [u8; 32] = Sha256::digest(raw).into();
        if let Ok(mut sessions) = self.inner.operator_sessions.lock() {
            sessions.retain(|session| !bool::from(session.digest.ct_eq(&digest)));
        }
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
        self.reject_public_open_from_replay(&replay)?;
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
        self.reject_public_open_token(token)?;
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
        set_private_directory(&room_path)?;
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
        self.reject_public_open_token(token)?;
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
        set_private_directory(&stage)?;
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

    pub async fn serve_tls(
        &self,
        addr: SocketAddr,
        config: Arc<ServerConfig>,
    ) -> Result<(), DaemonError> {
        let listener = TcpListener::bind(addr).await?;
        self.remember_secure_addr(listener.local_addr()?)?;
        self.clone()
            .serve_tls_listener(listener, TlsAcceptor::from(config))
            .await
    }

    pub async fn start_tls(
        &self,
        addr: SocketAddr,
        config: Arc<ServerConfig>,
    ) -> Result<RunningServer, DaemonError> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        self.remember_secure_addr(addr)?;
        let daemon = self.clone();
        let task = tokio::spawn(async move {
            daemon
                .serve_tls_listener(listener, TlsAcceptor::from(config))
                .await
        });
        Ok(RunningServer { addr, task })
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

    fn remember_secure_addr(&self, addr: SocketAddr) -> Result<(), DaemonError> {
        if addr.ip().is_unspecified() {
            return Ok(());
        }
        let endpoint = format!("tcps://{addr}");
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

    pub(crate) fn remember_secure_http_addr(&self, addr: SocketAddr) -> Result<(), DaemonError> {
        if addr.ip().is_unspecified() {
            return Ok(());
        }
        let endpoint = format!("wss://{addr}/swarm");
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
        if !advertised_endpoint_allowed(self.transport_mode(), endpoint) {
            return Err(DaemonError::InvalidEndpoint(endpoint.to_owned()));
        }
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

    fn pex(&self, room: RoomId) -> Pex {
        let mut peers = self
            .inner
            .peers
            .read()
            .expect("peer lock is not poisoned")
            .get(&room)
            .into_iter()
            .flat_map(|peers| peers.iter())
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
        Pex { room, peers }
    }

    fn remember_pex(&self, pex: &Pex) -> Result<(), DaemonError> {
        if pex.peers.len() > 256
            || pex
                .peers
                .iter()
                .any(|peer| peer.addrs.len() > 8 || peer.addrs.iter().any(|addr| addr.len() > 2048))
        {
            return Err(DaemonError::Protocol(
                "PEX exceeds the authorized room limits",
            ));
        }
        let mut peers = self.inner.peers.write().expect("peer lock is not poisoned");
        let mut candidate = peers.clone();
        let room_peers = candidate.entry(pex.room).or_default();
        let mode = self.transport_mode();
        for peer in &pex.peers {
            let mut incoming = Vec::with_capacity(peer.addrs.len());
            for endpoint in &peer.addrs {
                let Some(endpoint) = canonical_peer_endpoint(mode, endpoint) else {
                    return Err(DaemonError::Protocol("PEX contains an invalid endpoint"));
                };
                if !incoming.contains(&endpoint) {
                    incoming.push(endpoint);
                }
            }
            if peer.node == self.node_id() {
                continue;
            }
            if incoming.is_empty() {
                return Err(DaemonError::Protocol("PEX peer has no endpoint"));
            }
            if !room_peers.contains_key(&peer.node) && room_peers.len() >= 256 {
                return Err(DaemonError::Protocol(
                    "PEX exceeds the authorized room limits",
                ));
            }
            let endpoints = room_peers.entry(peer.node).or_default();
            for endpoint in incoming {
                if !endpoints.contains(&endpoint) && endpoints.len() < 8 {
                    endpoints.push(endpoint);
                } else if !endpoints.contains(&endpoint) {
                    return Err(DaemonError::Protocol(
                        "PEX exceeds the authorized room limits",
                    ));
                }
            }
        }
        write_json_atomic(&self.inner.data_dir.join("peers.json"), &candidate)?;
        *peers = candidate;
        Ok(())
    }

    fn peer_endpoints(&self, room: RoomId, node: NodeId) -> Vec<String> {
        self.inner
            .peers
            .read()
            .expect("peer lock is not poisoned")
            .get(&room)
            .and_then(|peers| peers.get(&node))
            .cloned()
            .unwrap_or_default()
    }

    fn declaration(&self, room: RoomId) -> Result<Declaration, DaemonError> {
        let role = self
            .inner
            .joins
            .read()
            .expect("join registry lock is not poisoned")
            .get(&room)
            .map(|join| join.role)
            .ok_or(DaemonError::UnknownRoom(room))?;
        let agents = self
            .inner
            .room_agents
            .read()
            .expect("room-agent registry lock is not poisoned")
            .get(&room)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        Ok(Declaration::signed(
            room,
            role,
            agents,
            unix_timestamp(),
            &self.inner.node_key,
        ))
    }

    fn hello_i(&self) -> HelloI {
        let node = self.node_id();
        let mut hello = HelloI {
            label: HANDSHAKE_LABEL.to_owned(),
            kind: "hello_i".to_owned(),
            v: 1,
            node,
            r#pub: node,
            nonce_i: Hash32::from_bytes(random::<[u8; 32]>()),
            sig: SignatureBytes::from_bytes([0; 64]),
        };
        hello.sig = SignatureBytes::from_bytes(sign(
            &self.inner.node_key,
            &signed_object_digest(&serde_json::to_value(&hello).expect("hello_i is serializable")),
        ));
        hello
    }

    fn hello_r(&self, hello_i: &HelloI) -> HelloR {
        let node = self.node_id();
        let mut hello = HelloR {
            label: HANDSHAKE_LABEL.to_owned(),
            kind: "hello_r".to_owned(),
            v: 1,
            node,
            r#pub: node,
            peer: hello_i.node,
            nonce_i: hello_i.nonce_i,
            nonce_r: Hash32::from_bytes(random::<[u8; 32]>()),
            hello_i_hash: signed_hash(hello_i),
            sig: SignatureBytes::from_bytes([0; 64]),
        };
        hello.sig = SignatureBytes::from_bytes(sign(
            &self.inner.node_key,
            &signed_object_digest(&serde_json::to_value(&hello).expect("hello_r is serializable")),
        ));
        hello
    }

    fn hello_ack(&self, hello_i: &HelloI, hello_r: &HelloR) -> HelloAck {
        let mut hello = HelloAck {
            label: HANDSHAKE_LABEL.to_owned(),
            kind: "hello_ack".to_owned(),
            v: 1,
            node: self.node_id(),
            peer: hello_r.node,
            nonce_i: hello_i.nonce_i,
            nonce_r: hello_r.nonce_r,
            hello_i_hash: signed_hash(hello_i),
            hello_r_hash: signed_hash(hello_r),
            sig: SignatureBytes::from_bytes([0; 64]),
        };
        hello.sig = SignatureBytes::from_bytes(sign(
            &self.inner.node_key,
            &signed_object_digest(
                &serde_json::to_value(&hello).expect("hello_ack is serializable"),
            ),
        ));
        hello
    }

    async fn initiate_handshake<S>(
        &self,
        stream: &mut S,
        expected: Option<NodeId>,
    ) -> Result<AuthenticatedPeer, DaemonError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        timeout(HANDSHAKE_TIMEOUT, async {
            let hello_i = self.hello_i();
            write_message(stream, &SwarmMsg::HelloI(hello_i.clone())).await?;
            let response = read_pre_auth_message(stream)
                .await?
                .ok_or(DaemonError::Protocol("peer closed during node handshake"))?;
            let hello_r = match response {
                SwarmMsg::HelloR(hello_r) => hello_r,
                SwarmMsg::HelloI(peer_hello) => {
                    validate_hello_i(&peer_hello)?;
                    if peer_hello.node == self.node_id() {
                        return Err(DaemonError::Protocol("self-directed node handshake"));
                    }
                    if expected.is_some_and(|expected| expected != peer_hello.node) {
                        return Err(DaemonError::Protocol("unexpected handshake peer"));
                    }
                    if self.node_id() < peer_hello.node {
                        return self.accept_handshake(stream, peer_hello).await;
                    }
                    let response = read_pre_auth_message(stream)
                        .await?
                        .ok_or(DaemonError::Protocol("peer closed during node handshake"))?;
                    let SwarmMsg::HelloR(hello_r) = response else {
                        return Err(DaemonError::Protocol("hello_r is required"));
                    };
                    hello_r
                }
                _ => return Err(DaemonError::Protocol("hello_r is required")),
            };
            if hello_r.node == self.node_id() {
                return Err(DaemonError::Protocol("self-directed node handshake"));
            }
            validate_hello_r(&hello_i, &hello_r, expected)?;
            let ack = self.hello_ack(&hello_i, &hello_r);
            write_message(stream, &SwarmMsg::HelloAck(ack)).await?;
            Ok(AuthenticatedPeer {
                node: hello_r.node,
                decl: Vec::new(),
            })
        })
        .await
        .map_err(|_| DaemonError::Protocol("node handshake timed out"))?
    }

    async fn accept_handshake<S>(
        &self,
        stream: &mut S,
        hello_i: HelloI,
    ) -> Result<AuthenticatedPeer, DaemonError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        validate_hello_i(&hello_i)?;
        if hello_i.node == self.node_id() {
            return Err(DaemonError::Protocol("self-directed node handshake"));
        }
        let hello_r = self.hello_r(&hello_i);
        write_message(stream, &SwarmMsg::HelloR(hello_r.clone())).await?;
        let response = read_pre_auth_message(stream)
            .await?
            .ok_or(DaemonError::Protocol("peer closed during node handshake"))?;
        let SwarmMsg::HelloAck(ack) = response else {
            return Err(DaemonError::Protocol("hello_ack is required"));
        };
        validate_hello_ack(&hello_i, &hello_r, &ack)?;
        Ok(AuthenticatedPeer {
            node: hello_i.node,
            decl: Vec::new(),
        })
    }

    async fn authorize_outbound<S>(
        &self,
        stream: &mut S,
        peer: &mut AuthenticatedPeer,
        room: RoomId,
        token: Option<Hash32>,
    ) -> Result<(), DaemonError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        write_message(
            stream,
            &SwarmMsg::Auth(Auth {
                room,
                token,
                declaration: self.declaration(room)?,
            }),
        )
        .await?;
        let message = timeout(HANDSHAKE_TIMEOUT, read_message(stream))
            .await
            .map_err(|_| DaemonError::Protocol("room authorization timed out"))??
            .ok_or(DaemonError::Protocol(
                "peer closed before room authorization",
            ))?;
        let SwarmMsg::Authed(authed) = message else {
            return Err(DaemonError::Protocol(
                "unexpected message before room authorization",
            ));
        };
        if authed.room != room
            || authed.declaration.room != room
            || !authed.declaration.verify(peer.node)
        {
            return Err(DaemonError::Protocol("invalid room authorization response"));
        }
        self.mark_peer_seen(room, peer.node)?;
        self.remember_declaration(peer.node, &authed.declaration);
        peer.decl.retain(|known| known.room != room);
        peer.decl.push(authed.declaration);
        write_message(stream, &SwarmMsg::Pex(self.pex(room))).await?;
        Ok(())
    }

    pub async fn sync_room_from(
        &self,
        addr: SocketAddr,
        room: RoomId,
    ) -> Result<ChainState, DaemonError> {
        match self.transport_mode() {
            TransportMode::Local if !addr.ip().is_loopback() => {
                return Err(DaemonError::Protocol(
                    "local mode refuses non-loopback sync endpoints",
                ));
            }
            TransportMode::Public => {
                return Err(DaemonError::Protocol(
                    "public mode requires an authenticated tcps or wss endpoint",
                ));
            }
            TransportMode::Local | TransportMode::Lan => {}
        }
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
        let Some(_syncing) = self.mark_syncing(room) else {
            return self
                .replay(room)
                .map(|replay| replay.chain)
                .map_err(|_| DaemonError::MutationUnavailable);
        };
        let mut remote = self.initiate_handshake(&mut stream, None).await?;
        self.authorize_outbound(&mut stream, &mut remote, room, token)
            .await?;
        self.sync_authorized_stream(&mut stream, room, expected_genesis)
            .await
    }

    async fn sync_authorized_stream<S>(
        &self,
        stream: &mut S,
        room: RoomId,
        expected_genesis: Option<Hash32>,
    ) -> Result<ChainState, DaemonError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        loop {
            let message = read_message(stream)
                .await?
                .ok_or(DaemonError::Protocol("peer closed before have"))?;
            let have = match message {
                SwarmMsg::Have(have) => have,
                SwarmMsg::Pex(pex) if pex.room == room => {
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
                stream,
                &SwarmMsg::GetScenes(GetScenes {
                    room,
                    from_n,
                    to_n: have.n,
                }),
            )
            .await?;

            for expected_n in from_n..=have.n {
                let record = loop {
                    match read_message(stream).await? {
                        Some(SwarmMsg::Scene(record)) if record.scene.room == room => break record,
                        Some(SwarmMsg::BlobMeta(meta)) => {
                            self.receive_blob_into(room, meta, stream).await?;
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
                write_message(stream, &SwarmMsg::Have(have_from_replay(room, &replay)?)).await?;
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
        let length = read_raw_length(stream).await? as u64;
        if length != meta.bytes {
            return Err(DaemonError::Protocol(
                "blob frame length does not match metadata",
            ));
        }
        let mut bytes = vec![0_u8; length as usize];
        read_raw_bytes(stream, &mut bytes).await?;
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
        write_raw_bytes(stream, bytes).await?;
        Ok(())
    }

    pub async fn join_ticket(
        &self,
        ticket: Ticket,
        role: JoinRole,
    ) -> Result<ChainState, DaemonError> {
        ticket.validate()?;
        self.reject_public_open_token(ticket.token)?;
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

        let ticket_endpoints = ticket
            .trackers
            .iter()
            .chain(&ticket.peers)
            .cloned()
            .collect::<Vec<_>>();
        let mut pinned_failure = None;
        let mut attempted = BTreeSet::new();
        for discovery_round in 0..2 {
            let endpoints = if discovery_round == 0 {
                ticket_endpoints.clone()
            } else {
                self.inner
                    .peers
                    .read()
                    .expect("peer lock is not poisoned")
                    .get(&room)
                    .into_iter()
                    .flat_map(|peers| peers.values())
                    .flatten()
                    .cloned()
                    .collect()
            };
            for endpoint in endpoints {
                if !attempted.insert(endpoint.clone()) {
                    continue;
                }
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
                            task::spawn_blocking(move || write_ticket(&store, &canonical))
                                .await??;
                            self.write_current_room(room)?;
                            return Ok(chain);
                        }
                        Ok(Err(error @ DaemonError::BadTicket(_))) => pinned_failure = Some(error),
                        _ => {}
                    }
                    continue;
                }
                let (authority, secure) = if let Some(authority) = endpoint.strip_prefix("tcp://") {
                    if self.transport_mode() == TransportMode::Public {
                        continue;
                    }
                    (authority, false)
                } else if let Some(authority) = endpoint.strip_prefix("tcps://") {
                    if self.tls_client_config().is_none() {
                        continue;
                    }
                    (authority, true)
                } else {
                    continue;
                };
                if !secure
                    && self.transport_mode() == TransportMode::Local
                    && !literal_loopback_authority(authority)
                {
                    continue;
                }
                let Ok(addresses) = tokio::net::lookup_host(authority).await else {
                    continue;
                };
                for addr in addresses {
                    if !secure
                        && self.transport_mode() == TransportMode::Local
                        && !addr.ip().is_loopback()
                    {
                        continue;
                    }
                    let Some(_dial_guard) = self.dial_guard(room) else {
                        continue;
                    };
                    let attempt = timeout(SYNC_TIMEOUT, async {
                        let stream = TcpStream::connect(addr).await?;
                        if secure {
                            let config = self
                                .tls_client_config()
                                .ok_or(DaemonError::Protocol("public mode requires TLS trust"))?;
                            let host = tls_authority_host(authority)?;
                            let server_name = ServerName::try_from(host)
                                .map_err(|_| DaemonError::InvalidEndpoint(endpoint.clone()))?;
                            let stream = TlsConnector::from(config)
                                .connect(server_name, stream)
                                .await?;
                            self.sync_room_stream(stream, room, ticket.token, Some(ticket.genesis))
                                .await
                        } else {
                            self.sync_room_stream(stream, room, ticket.token, Some(ticket.genesis))
                                .await
                        }
                    })
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
        self.inner
            .intent_forwards
            .lock()
            .expect("intent-forward lock is not poisoned")
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
        let listener_addr = listener.local_addr()?;
        let mut connections = JoinSet::new();
        let mut maintenance = interval(Duration::from_millis(500));
        maintenance.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut maintenance_in_flight = false;
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, peer) = accepted?;
                    let daemon = self.clone();
                    let Some(connection_guard) = daemon.connection_guard(peer.ip()) else {
                        continue;
                    };
                    connections.spawn(async move {
                        let _connection_guard = connection_guard;
                        let _ = daemon.handle_connection(stream, listener_addr).await;
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

    async fn serve_tls_listener(
        self,
        listener: TcpListener,
        acceptor: TlsAcceptor,
    ) -> Result<(), DaemonError> {
        let mut connections = JoinSet::new();
        let mut maintenance = interval(Duration::from_millis(500));
        maintenance.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut maintenance_in_flight = false;
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, peer) = accepted?;
                    let daemon = self.clone();
                    let acceptor = acceptor.clone();
                    let Some(connection_guard) = daemon.connection_guard(peer.ip()) else {
                        continue;
                    };
                    connections.spawn(async move {
                        let _connection_guard = connection_guard;
                        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
                        if let Ok(Ok(stream)) = timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                            let _ = daemon
                                .handle_transport_before(
                                    stream,
                                    ConnectionProtocol::Swarm,
                                    Some(peer.ip()),
                                    deadline,
                                )
                                .await;
                        }
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

    async fn handle_connection(
        &self,
        stream: TcpStream,
        listener_addr: SocketAddr,
    ) -> Result<(), DaemonError> {
        let peer_addr = stream.peer_addr()?;
        self.handle_transport_from(
            stream,
            ConnectionProtocol::Tcp {
                allow_client: client_peer_allowed(listener_addr, peer_addr),
            },
            Some(peer_addr.ip()),
        )
        .await
    }

    pub(crate) async fn handle_transport_with_source<S>(
        &self,
        stream: S,
        protocol: ConnectionProtocol,
        source: IpAddr,
    ) -> Result<(), DaemonError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        self.handle_transport_from(stream, protocol, Some(source))
            .await
    }

    async fn handle_transport_from<S>(
        &self,
        stream: S,
        protocol: ConnectionProtocol,
        source: Option<IpAddr>,
    ) -> Result<(), DaemonError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        self.handle_transport_before(stream, protocol, source, Instant::now() + HANDSHAKE_TIMEOUT)
            .await
    }

    async fn handle_transport_before<S>(
        &self,
        mut stream: S,
        protocol: ConnectionProtocol,
        source: Option<IpAddr>,
        deadline: Instant,
    ) -> Result<(), DaemonError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        if source.is_some_and(|source| !self.auth_allowed(source)) {
            return Err(DaemonError::Protocol("source authentication is throttled"));
        }
        enum Established {
            Swarm(AuthenticatedPeer),
            Client(ClientRequest, Option<RoomId>),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let established = timeout(remaining, async {
            let first: Value = read_frame_limited(&mut stream, PREAUTH_FRAME_BYTES)
                .await?
                .ok_or(DaemonError::Protocol("peer closed before hello"))?;
            match first.get("typ").and_then(Value::as_str) {
                Some("hello_i")
                    if matches!(
                        protocol,
                        ConnectionProtocol::Tcp { .. } | ConnectionProtocol::Swarm
                    ) =>
                {
                    let hello = serde_json::from_value(first).map_err(FrameError::from)?;
                    let SwarmMsg::HelloI(hello_i) = hello else {
                        return Err(DaemonError::Protocol("hello_i must be the first frame"));
                    };
                    Ok(Established::Swarm(
                        self.accept_handshake(&mut stream, hello_i).await?,
                    ))
                }
                Some("attach")
                    if matches!(protocol, ConnectionProtocol::Client { .. })
                        || matches!(protocol, ConnectionProtocol::Tcp { allow_client: true }) =>
                {
                    let attach = serde_json::from_value(first).map_err(FrameError::from)?;
                    let allowed_room = match protocol {
                        ConnectionProtocol::Client { allowed_room } => Some(allowed_room),
                        ConnectionProtocol::Tcp { .. } | ConnectionProtocol::Swarm => None,
                    };
                    Ok(Established::Client(attach, allowed_room))
                }
                _ => Err(DaemonError::Protocol(
                    "hello or attach must be the first frame",
                )),
            }
        })
        .await
        .map_err(|_| DaemonError::Protocol("node handshake timed out"))?;
        match established {
            Ok(Established::Swarm(peer)) => {
                self.handle_swarm_connection(stream, peer, source).await
            }
            Ok(Established::Client(attach, allowed_room)) => {
                self.handle_client_connection(stream, attach, allowed_room)
                    .await
            }
            Err(error) => {
                if let Some(source) = source {
                    self.record_auth_failure(source);
                }
                Err(error)
            }
        }
    }

    async fn handle_swarm_connection(
        &self,
        mut stream: impl AsyncRead + AsyncWrite + Unpin + Send,
        mut peer: AuthenticatedPeer,
        source: Option<IpAddr>,
    ) -> Result<(), DaemonError> {
        let mut authed = BTreeSet::new();
        let mut accepted_auth = BTreeMap::<RoomId, Auth>::new();
        let mut offered_blobs = BTreeMap::new();
        let mut offered_blob_bytes = 0_u64;

        while let Some(message) = read_message(&mut stream).await? {
            match &message {
                SwarmMsg::HelloI(_)
                | SwarmMsg::HelloR(_)
                | SwarmMsg::HelloAck(_)
                | SwarmMsg::Authed(_)
                | SwarmMsg::Vote(_)
                | SwarmMsg::Cert(_)
                | SwarmMsg::Nack(_) => {
                    if authed.is_empty() {
                        if let Some(source) = source {
                            self.record_auth_failure(source);
                        }
                    }
                    return Err(DaemonError::Protocol(
                        "unexpected message in authenticated responder state",
                    ));
                }
                SwarmMsg::BlobMeta(_) if authed.is_empty() => {
                    if let Some(source) = source {
                        self.record_auth_failure(source);
                    }
                    return Err(DaemonError::Protocol(
                        "room message arrived before room authorization",
                    ));
                }
                SwarmMsg::Auth(_) | SwarmMsg::BlobMeta(_) => {}
                other => {
                    if swarm_message_room(other).is_some_and(|room| !authed.contains(&room)) {
                        if let Some(source) = source {
                            self.record_auth_failure(source);
                        }
                        return Err(DaemonError::Protocol(
                            "room message arrived before room authorization",
                        ));
                    }
                }
            }
            if let Some(room) = swarm_message_room(&message) {
                if authed.contains(&room) {
                    self.mark_peer_seen(room, peer.node)?;
                }
            }
            match message {
                SwarmMsg::Auth(auth) => {
                    let token_authorized = match self.authorize_room_token(auth.room, auth.token) {
                        Ok(authorized) => authorized,
                        Err(error) => {
                            if let Some(source) = source {
                                self.record_auth_failure(source);
                            }
                            return Err(error);
                        }
                    };
                    if auth.declaration.room != auth.room
                        || !auth.declaration.verify(peer.node)
                        || !token_authorized
                    {
                        if let Some(source) = source {
                            self.record_auth_failure(source);
                        }
                        return Err(DaemonError::Protocol("room authorization failed"));
                    }
                    if let Some(accepted) = accepted_auth.get(&auth.room) {
                        if accepted != &auth {
                            if let Some(source) = source {
                                self.record_auth_failure(source);
                            }
                            return Err(DaemonError::Protocol(
                                "conflicting duplicate room authorization",
                            ));
                        }
                        write_message(
                            &mut stream,
                            &SwarmMsg::Authed(Authed {
                                room: auth.room,
                                declaration: self.declaration(auth.room)?,
                            }),
                        )
                        .await?;
                        continue;
                    }
                    accepted_auth.insert(auth.room, auth.clone());
                    authed.insert(auth.room);
                    self.mark_peer_seen(auth.room, peer.node)?;
                    self.remember_declaration(peer.node, &auth.declaration);
                    peer.decl.retain(|known| known.room != auth.room);
                    peer.decl.push(auth.declaration.clone());
                    write_message(
                        &mut stream,
                        &SwarmMsg::Authed(Authed {
                            room: auth.room,
                            declaration: self.declaration(auth.room)?,
                        }),
                    )
                    .await?;
                    write_message(&mut stream, &SwarmMsg::Pex(self.pex(auth.room))).await?;
                    // Existing roster peers take this lock-free path. A genuinely
                    // new eligible staker is admitted before its initial history.
                    let _ = self
                        .admit_declared_staker(peer.node, &auth.declaration)
                        .await;
                    let replay = self.replay(auth.room)?;
                    if replay.chain.head_n.is_some() {
                        write_message(
                            &mut stream,
                            &SwarmMsg::Have(have_from_replay(auth.room, &replay)?),
                        )
                        .await?;
                    }
                }
                SwarmMsg::Pex(pex) if authed.contains(&pex.room) => {
                    self.remember_pex(&pex)?;
                }
                SwarmMsg::Pex(_) => {}
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
                    let length = read_raw_length(&mut stream).await? as u64;
                    if length != meta.bytes {
                        return Err(DaemonError::Protocol(
                            "blob frame length does not match metadata",
                        ));
                    }
                    let mut bytes = vec![0_u8; length as usize];
                    read_raw_bytes(&mut stream, &mut bytes).await?;
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
                SwarmMsg::GetBlob(request) if self.room_authorized(request.room, &authed)? => {
                    let Ok(bytes) = self.store(request.room)?.read_blob(request.sha256) else {
                        continue;
                    };
                    self.send_blob_bytes(&mut stream, request.sha256, &bytes)
                        .await?;
                }
                SwarmMsg::GetBlob(_) => {}
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
                    {
                        let catch_up = self.accept_heartbeat(&heartbeat)?;
                        let daemon = self.clone();
                        let leader = peer.node;
                        let room = heartbeat.room;
                        let rpc_term = heartbeat.rpc_term;
                        tokio::spawn(async move {
                            if catch_up {
                                let _ = daemon.sync_from_known_peer(leader, room).await;
                            }
                            daemon
                                .forward_local_intents_to_leader(room, leader, rpc_term)
                                .await;
                        });
                    }
                }
                SwarmMsg::Leave(leave) => {
                    let declared_for_room = peer
                        .decl
                        .iter()
                        .any(|declaration| declaration.room == leave.room);
                    if declared_for_room
                        && leave.node == peer.node
                        && valid_leave(&leave)
                        && self.room_authorized(leave.room, &authed)?
                    {
                        if let Ok(Some(record)) = self.receive_leave(&leave).await {
                            write_message(&mut stream, &SwarmMsg::Scene(record)).await?;
                        }
                    }
                }
                SwarmMsg::GrantReq(request) => {
                    let declared = peer.decl.iter().any(|declaration| {
                        declaration.room == request.room
                            && request.from.node == peer.node
                            && declaration.agents.contains(&request.from.agent)
                    });
                    if declared && self.room_authorized(request.room, &authed)? {
                        if let Ok(scene) = self
                            .client_grant_from(request.from, request.room, request.to)
                            .await
                        {
                            if let Ok(scene) = serde_json::from_value::<Scene>(scene) {
                                if let Some(record) = self
                                    .replay(request.room)?
                                    .history
                                    .into_iter()
                                    .find(|record| hash_scene(&record.scene) == hash_scene(&scene))
                                {
                                    write_message(&mut stream, &SwarmMsg::Scene(record)).await?;
                                }
                            }
                        }
                    }
                }
                SwarmMsg::YankReq(request) => {
                    let declared = peer.decl.iter().any(|declaration| {
                        declaration.room == request.room
                            && request.from.node == peer.node
                            && declaration.agents.contains(&request.from.agent)
                    });
                    if declared
                        && self.room_authorized(request.room, &authed)?
                        && self
                            .client_yank_from(request.from, request.room)
                            .await
                            .is_ok()
                    {
                        if let Some(record) = self.replay(request.room)?.history.last().cloned() {
                            write_message(&mut stream, &SwarmMsg::Scene(record)).await?;
                        }
                    }
                }
                SwarmMsg::BreakoutReq(request) => {
                    let declared = peer.decl.iter().any(|declaration| {
                        declaration.room == request.room
                            && request.from.node == peer.node
                            && declaration.agents.contains(&request.from.agent)
                    });
                    if declared && self.room_authorized(request.room, &authed)? {
                        let prepared = match (request.ticket, request.genesis) {
                            (Some(ticket), Some(genesis)) => {
                                serde_json::from_value::<Ticket>(ticket)
                                    .ok()
                                    .map(|ticket| (ticket, genesis))
                            }
                            (None, None) => None,
                            _ => None,
                        };
                        if let Ok(value) = self
                            .client_breakout_from(
                                request.from,
                                request.room,
                                request.name,
                                request.members,
                                prepared,
                            )
                            .await
                        {
                            if let Some(scene) = value
                                .get("scene")
                                .cloned()
                                .and_then(|scene| serde_json::from_value::<Scene>(scene).ok())
                            {
                                if let Some(record) = self
                                    .replay(request.room)?
                                    .history
                                    .into_iter()
                                    .find(|record| hash_scene(&record.scene) == hash_scene(&scene))
                                {
                                    write_message(&mut stream, &SwarmMsg::Scene(record)).await?;
                                }
                            }
                        }
                    }
                }
                SwarmMsg::MembershipReq(request) => {
                    let declared = peer.decl.iter().any(|declaration| {
                        declaration.room == request.room
                            && request.from.node == peer.node
                            && declaration.agents.contains(&request.from.agent)
                    });
                    if declared && self.room_authorized(request.room, &authed)? {
                        if let Ok(scene) = self
                            .client_membership_from(
                                request.from,
                                request.room,
                                request.stake,
                                request.floor,
                            )
                            .await
                        {
                            if let Ok(scene) = serde_json::from_value::<Scene>(scene) {
                                if let Some(record) = self
                                    .replay(request.room)?
                                    .history
                                    .into_iter()
                                    .find(|record| hash_scene(&record.scene) == hash_scene(&scene))
                                {
                                    write_message(&mut stream, &SwarmMsg::Scene(record)).await?;
                                }
                            }
                        }
                    }
                }
                SwarmMsg::CloseTake(close) => {
                    let declared_for_room = peer
                        .decl
                        .iter()
                        .any(|declaration| declaration.room == close.room);
                    if declared_for_room
                        && self.room_authorized(close.room, &authed)?
                        && self.replay(close.room).is_ok_and(|replay| {
                            replay.chain.live_grant.as_ref().is_some_and(|grant| {
                                grant.hash == close.grant_hash && grant.to.node == peer.node
                            })
                        })
                    {
                        let store = self.store(close.room)?;
                        for blob in &close.blobs {
                            if store.read_blob(blob.sha256).is_err() {
                                if let Some(bytes) = offered_blobs.remove(&blob.sha256) {
                                    offered_blob_bytes =
                                        offered_blob_bytes.saturating_sub(bytes.len() as u64);
                                    store.put_blob(&bytes)?;
                                }
                            }
                        }
                        if let Ok(Some(record)) = self.receive_close_take(close).await {
                            write_message(&mut stream, &SwarmMsg::Scene(record)).await?;
                        }
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
                SwarmMsg::HelloI(_)
                | SwarmMsg::HelloR(_)
                | SwarmMsg::HelloAck(_)
                | SwarmMsg::Authed(_)
                | SwarmMsg::Vote(_)
                | SwarmMsg::Cert(_)
                | SwarmMsg::Nack(_) => unreachable!("rejected before message dispatch"),
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
        allowed_room: Option<RoomId>,
    ) -> Result<(), DaemonError> {
        let ClientRequest::Attach { agent } = attach else {
            return Err(DaemonError::Protocol(
                "attach must be the first client frame",
            ));
        };
        if let Some(room) = allowed_room {
            self.remember_room_agent(room, agent.clone());
        }
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
            if allowed_room.is_some_and(|allowed| request_room(&request) != Some(allowed)) {
                write_frame(
                    &mut stream,
                    &ClientReply::failure(
                        "unauthorized",
                        "client session is authorized for a different room",
                    ),
                )
                .await?;
                return Ok(());
            }
            if let Some(room) = request_room(&request) {
                self.remember_room_agent(room, agent.clone());
            }
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
                let raw_length = read_raw_length(&mut stream).await? as u64;
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
                read_raw_bytes(&mut stream, &mut raw).await?;
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

    async fn admit_declared_staker(
        &self,
        peer: NodeId,
        declaration: &Declaration,
    ) -> Result<(), DaemonError> {
        if declaration.role != JoinRole::Stake {
            return Ok(());
        }
        let Ok(floor) = self.floor(declaration.room) else {
            return Ok(());
        };
        // Heartbeats, appends, and forwarded requests all establish fresh
        // connections. Roster peers must never queue behind the mutation lock
        // merely because their hello repeats an already-settled declaration.
        let initial = self.replay(declaration.room)?;
        let Some(policy) = initial.chain.stake.as_ref() else {
            return Ok(());
        };
        if self.transport_mode() == TransportMode::Public
            && self.token_sha256(declaration.room)?.is_none()
            && !initial.chain.roster.contains(&peer)
        {
            // Public mode must not authorize open rooms at all. Keep this
            // lock-free no-op so a race with configure_transport cannot mint
            // a roster seat from a tokenless declaration.
            return Ok(());
        }
        let local_can_admit = initial.consensus.leader_id == Some(self.node_id())
            || initial.chain.roster.as_slice() == [self.node_id()];
        if !local_can_admit
            || initial.chain.live_grant.is_some()
            || initial.chain.roster.contains(&peer)
            || !eligible(policy, peer, declaration.role, &declaration.agents)
        {
            return Ok(());
        }
        let _mutation = floor.mutation.lock().await;
        let replay = self.replay(declaration.room)?;
        let Some(policy) = replay.chain.stake.as_ref() else {
            return Ok(());
        };
        if self.transport_mode() == TransportMode::Public
            && self.token_sha256(declaration.room)?.is_none()
            && !replay.chain.roster.contains(&peer)
        {
            return Ok(());
        }
        let local_can_admit = replay.consensus.leader_id == Some(self.node_id())
            || replay.chain.roster.as_slice() == [self.node_id()];
        if !local_can_admit
            || replay.chain.live_grant.is_some()
            || replay.chain.roster.contains(&peer)
            || !eligible(policy, peer, declaration.role, &declaration.agents)
        {
            return Ok(());
        }
        let mut next_roster = replay.chain.roster.clone();
        next_roster.push(peer);
        next_roster.sort_unstable();
        self.commit_singleton_body(
            declaration.room,
            Body::ViewChange {
                add: vec![peer],
                remove: Vec::new(),
                next_roster,
                closes_grant: None,
            },
            &floor,
        )
        .await?;
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
                        self.remember_room_agent(ticket.id, agent.clone());
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
                Ok(ticket) => self.join_ticket(ticket, role).await.map(|chain| {
                    let room = chain.room.expect("joined chain has a room id");
                    self.remember_room_agent(room, agent.clone());
                    json!({ "id": room, "role": role })
                }),
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
            ClientRequest::WaitForHistory {
                room,
                after_n,
                timeout_secs,
            } => {
                self.client_wait_for_history(room, after_n, timeout_secs)
                    .await
            }
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

    async fn client_wait_for_history(
        &self,
        room: RoomId,
        after_n: u64,
        timeout_secs: Option<u64>,
    ) -> Result<Value, DaemonError> {
        let floor = self.floor(room)?;
        let wait = Duration::from_secs(timeout_secs.unwrap_or(60).min(MAX_HISTORY_WAIT_SECS));
        let deadline = Instant::now() + wait;
        loop {
            // Register before reading so a commit between the read and await
            // cannot be missed.
            let changed = floor.changed.notified();
            let records = match after_n.checked_add(1) {
                Some(from_n) => self.history_from(room, from_n)?,
                None => Vec::new(),
            };
            if !records.is_empty() {
                let mut page = self.history_page(room, records)?;
                page["timed_out"] = json!(false);
                return Ok(page);
            }
            if Instant::now() >= deadline {
                let mut page = self.history_page(room, Vec::new())?;
                page["timed_out"] = json!(true);
                return Ok(page);
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                let mut page = self.history_page(room, Vec::new())?;
                page["timed_out"] = json!(true);
                return Ok(page);
            }
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
        let replay = self.replay(room)?;
        let mouth = Mouth {
            agent: agent.clone(),
            node: self.node_id(),
        };
        if let Some(grant) = replay
            .chain
            .live_grant
            .as_ref()
            .filter(|grant| grant.to == mouth)
        {
            let intent_id = replay
                .history
                .iter()
                .find(|record| hash_scene(&record.scene) == grant.hash)
                .and_then(|record| match &record.scene.body {
                    Body::Grant { intent_id, .. } => Some(*intent_id),
                    _ => None,
                })
                .ok_or(DaemonError::Protocol(
                    "live grant is missing its committed intent",
                ))?;
            return Ok(json!({ "intent_id": intent_id }));
        }
        let id = self.queue_intent(room, agent, IntentKind::Raise).await?;
        Ok(json!({ "intent_id": id }))
    }

    async fn client_grant(
        &self,
        agent: AgentId,
        room: RoomId,
        to: Mouth,
    ) -> Result<Value, DaemonError> {
        self.client_grant_from(
            Mouth {
                agent,
                node: self.node_id(),
            },
            room,
            to,
        )
        .await
    }

    async fn client_grant_from(
        &self,
        from: Mouth,
        room: RoomId,
        to: Mouth,
    ) -> Result<Value, DaemonError> {
        let floor = self.floor(room)?;
        let replay = self.replay(room)?;
        self.require_moderator_mouth(&replay, &from)?;
        if let Some(leader) = replay
            .consensus
            .leader_id
            .filter(|leader| *leader != self.node_id())
        {
            let record = self
                .request_grant(leader, &GrantReq { room, to, from })
                .await?
                .ok_or(DaemonError::MutationUnavailable)?;
            let scene = record.scene.clone();
            self.install_record(room, record).await?;
            return Ok(serde_json::to_value(scene)?);
        }
        if replay.chain.roster.len() > 1
            && (replay.consensus.role != ConsensusRole::Leader
                || replay.consensus.leader_id != Some(self.node_id()))
        {
            return Err(DaemonError::MutationUnavailable);
        }
        let _mutation = floor.mutation.lock().await;
        let replay = self.replay(room)?;
        self.require_moderator_mouth(&replay, &from)?;
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
        self.client_yank_from(
            Mouth {
                agent,
                node: self.node_id(),
            },
            room,
        )
        .await
    }

    async fn client_yank_from(&self, from: Mouth, room: RoomId) -> Result<Value, DaemonError> {
        let floor = self.floor(room)?;
        let replay = self.replay(room)?;
        self.require_moderator_mouth(&replay, &from)?;
        if let Some(leader) = replay
            .consensus
            .leader_id
            .filter(|leader| *leader != self.node_id())
        {
            let record = self
                .request_yank(leader, &YankReq { room, from })
                .await?
                .ok_or(DaemonError::MutationUnavailable)?;
            let scene = record.scene.clone();
            self.install_record(room, record).await?;
            return Ok(serde_json::to_value(scene)?);
        }
        if replay.chain.roster.len() > 1
            && (replay.consensus.role != ConsensusRole::Leader
                || replay.consensus.leader_id != Some(self.node_id()))
        {
            return Err(DaemonError::MutationUnavailable);
        }
        let _mutation = floor.mutation.lock().await;
        let replay = self.replay(room)?;
        self.require_moderator_mouth(&replay, &from)?;
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
        self.client_membership_from(
            Mouth {
                agent,
                node: self.node_id(),
            },
            room,
            stake,
            floor_config,
        )
        .await
    }

    async fn client_membership_from(
        &self,
        from: Mouth,
        room: RoomId,
        stake: Option<StakePolicy>,
        floor_config: Option<FloorConfig>,
    ) -> Result<Value, DaemonError> {
        let floor = self.floor(room)?;
        if !self.can_certify(room)? {
            return Err(FloorError::NotStaker.into());
        }
        let replay = self.replay(room)?;
        if let Some(leader) = replay
            .consensus
            .leader_id
            .filter(|leader| *leader != self.node_id())
        {
            let record = self
                .request_membership(
                    leader,
                    &MembershipReq {
                        room,
                        stake,
                        floor: floor_config,
                        from,
                    },
                )
                .await?
                .ok_or(DaemonError::MutationUnavailable)?;
            let scene = record.scene.clone();
            let daemon = self.clone();
            task::spawn_blocking(move || daemon.install_record_blocking(room, &record)).await??;
            return Ok(serde_json::to_value(scene)?);
        }
        if replay.chain.roster.len() > 1
            && (replay.consensus.role != ConsensusRole::Leader
                || replay.consensus.leader_id != Some(self.node_id()))
        {
            return Err(DaemonError::MutationUnavailable);
        }
        let _mutation = floor.mutation.lock().await;
        let replay = self.replay(room)?;
        let closes_grant = match &replay.chain.live_grant {
            Some(grant) if grant.to == from => Some(grant.hash),
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
        self.client_breakout_from(
            Mouth {
                agent,
                node: self.node_id(),
            },
            room,
            name,
            members,
            None,
        )
        .await
    }

    async fn client_breakout_from(
        &self,
        from: Mouth,
        room: RoomId,
        name: String,
        members: Option<Vec<NodeId>>,
        mut prepared: Option<(Ticket, CommittedScene)>,
    ) -> Result<Value, DaemonError> {
        let floor = self.floor(room)?;
        let replay = self.replay(room)?;
        if let Some(leader) = replay
            .consensus
            .leader_id
            .filter(|leader| *leader != self.node_id())
        {
            let mut forwarded_members = members;
            if prepared.is_none() {
                let _mutation = floor.mutation.lock().await;
                let replay = self.replay(room)?;
                let grant = replay
                    .chain
                    .live_grant
                    .as_ref()
                    .ok_or(FloorError::NoGrant)?;
                if grant.to != from {
                    return Err(FloorError::NoGrant.into());
                }
                let mut auto_join = forwarded_members
                    .clone()
                    .unwrap_or_else(|| replay.chain.roster.clone());
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
                forwarded_members = Some(auto_join);
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
                prepared =
                    Some(
                        task::spawn_blocking(move || {
                            let ticket = daemon.prepare_breakout_ticket(
                                &child_name,
                                StakePolicy::default(),
                                FloorConfig::stick(30),
                                token,
                                room,
                            )?;
                            let stage = Store::open(daemon.staged_breakout_path(ticket.id))?;
                            let genesis =
                                stage.load_replay()?.history.into_iter().next().ok_or(
                                    DaemonError::Protocol("prepared breakout has no genesis"),
                                )?;
                            Ok::<_, DaemonError>((ticket, genesis))
                        })
                        .await??,
                    );
                drop(_mutation);
            }
            let (child, genesis) = prepared
                .as_ref()
                .expect("follower prepares breakout before forwarding");
            let record = self
                .request_breakout(
                    leader,
                    &BreakoutReq {
                        room,
                        name,
                        members: forwarded_members,
                        from,
                        ticket: Some(serde_json::to_value(child)?),
                        genesis: Some(genesis.clone()),
                    },
                )
                .await?
                .ok_or(DaemonError::MutationUnavailable)?;
            let Body::Breakout { ticket, .. } = &record.scene.body else {
                return Err(DaemonError::Protocol(
                    "leader returned a non-breakout scene",
                ));
            };
            let child = serde_json::from_value::<Ticket>(ticket.clone())?;
            let scene = record.scene.clone();
            self.install_record(room, record).await?;
            self.write_current_room(child.id)?;
            return Ok(json!({
                "id": child.id,
                "magnet": child.to_magnet(),
                "ticket": child,
                "scene": scene,
            }));
        }
        if replay.chain.roster.len() > 1
            && (replay.consensus.role != ConsensusRole::Leader
                || replay.consensus.leader_id != Some(self.node_id()))
        {
            return Err(DaemonError::MutationUnavailable);
        }
        let _mutation = floor.mutation.lock().await;
        let replay = self.replay(room)?;
        let grant = replay
            .chain
            .live_grant
            .as_ref()
            .ok_or(FloorError::NoGrant)?;
        if grant.to != from {
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
        if prepared.is_none() {
            let take = {
                let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
                engine.freeze(room, grant.hash)?;
                engine.take().expect("freeze preserves take").clone()
            };
            let store = self.store(room)?;
            task::spawn_blocking(move || write_take(&store, &take)).await??;
        }

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
        } else if let Some((child, genesis)) = prepared {
            validate_prepared_breakout(room, &from, &name, &child, &genesis)?;
            let body = Body::Breakout {
                closes_grant: grant.hash,
                ticket: serde_json::to_value(&child)?,
                auto_join,
            };
            (child, body)
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
        let should_auto_join = matches!(
            &record.scene.body,
            Body::Breakout { auto_join, .. } if auto_join.contains(&self.node_id())
        );
        drop(_mutation);
        if should_auto_join && self.replay(child.id).is_err() {
            self.spawn_child_join(child.clone());
        }
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
        let mut replay = self.replay(room)?;
        let node = self.node_id();
        if replay.chain.roster.contains(&node) && replay.chain.live_grant.is_none() {
            if let Some(leader) = replay.consensus.leader_id.filter(|leader| *leader != node) {
                let mut leave = Leave {
                    room,
                    node,
                    sig: SignatureBytes::from_bytes([0; 64]),
                };
                leave.sig = SignatureBytes::from_bytes(sign(
                    &self.inner.node_key,
                    &signed_object_digest(&serde_json::to_value(&leave)?),
                ));
                let record = self
                    .request_leave(leader, &leave)
                    .await?
                    .ok_or(DaemonError::MutationUnavailable)?;
                let installed = record.clone();
                self.install_record(room, installed).await?;
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
        }
        let _mutation = floor.mutation.lock().await;
        replay = self.replay(room)?;
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

    fn require_moderator_mouth(&self, replay: &Replay, from: &Mouth) -> Result<(), DaemonError> {
        if replay.chain.floor_mode != Some(FloorMode::Moderator)
            || replay.chain.moderator.as_ref() != Some(from)
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
        let mouth = Mouth {
            agent: agent.clone(),
            node,
        };
        // `wait-for-floor` is commonly issued immediately after `raise-hand`.
        // Once that intent has committed as this mouth's live grant it is
        // consumed, but waiting for the grant must not enqueue a second turn.
        if kind == IntentKind::Wait {
            if let Some(grant) = replay
                .chain
                .live_grant
                .as_ref()
                .filter(|grant| grant.to == mouth)
            {
                return replay
                    .history
                    .iter()
                    .find(|record| hash_scene(&record.scene) == grant.hash)
                    .and_then(|record| match &record.scene.body {
                        Body::Grant { intent_id, .. } => Some(*intent_id),
                        _ => None,
                    })
                    .ok_or(DaemonError::Protocol(
                        "live grant is missing its committed intent",
                    ));
            }
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
        let stored = {
            let mut engine = floor.engine.lock().expect("floor lock is not poisoned");
            engine.upsert_intent(&replay.chain, intent)?;
            let stored = engine
                .intents()
                .find(|intent| intent.agent == mouth.agent && intent.node == mouth.node)
                .cloned()
                .ok_or(DaemonError::Protocol("queued mouth is missing its intent"))?;
            stored
        };
        let id = stored.id;
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

    /// Repair the best-effort initial intent gossip after a leader change.
    ///
    /// A follower repeats its own still-live intents to the verified leader at
    /// a bounded rate. This closes the race where the old leader disappears
    /// after accepting the client request but before the eventual winner has
    /// durably received it. Replays are harmless because intent ids are
    /// signed and `receive_intent` is idempotent.
    async fn forward_local_intents_to_leader(&self, room: RoomId, leader: NodeId, rpc_term: u64) {
        let Ok(replay) = self.replay(room) else {
            return;
        };
        if replay.consensus.role != ConsensusRole::Follower
            || replay.consensus.leader_id != Some(leader)
            || replay.consensus.current_term != rpc_term
        {
            return;
        }
        let Ok(floor) = self.floor(room) else {
            return;
        };
        let node = self.node_id();
        let now = unix_timestamp();
        let intents = floor
            .engine
            .lock()
            .expect("floor lock is not poisoned")
            .intents()
            .filter(|intent| {
                intent.node == node
                    && !replay.chain.consumed_intents.contains(&intent.id)
                    && now < intent.exp
            })
            .cloned()
            .collect::<Vec<_>>();
        let ids = intents
            .iter()
            .map(|intent| intent.id)
            .collect::<BTreeSet<_>>();
        if ids.is_empty() {
            self.inner
                .intent_forwards
                .lock()
                .expect("intent-forward lock is not poisoned")
                .remove(&room);
            return;
        }

        let instant = Instant::now();
        {
            let mut forwards = self
                .inner
                .intent_forwards
                .lock()
                .expect("intent-forward lock is not poisoned");
            if forwards.get(&room).is_some_and(|state| {
                state.leader == leader
                    && state.rpc_term == rpc_term
                    && state.ids == ids
                    && instant < state.retry_after
            }) {
                return;
            }
            forwards.insert(
                room,
                IntentForwardState {
                    leader,
                    rpc_term,
                    ids: ids.clone(),
                    retry_after: instant + Duration::from_millis(500),
                },
            );
        }

        let forwarded = async {
            let mut stream = self.connect_known_peer(leader, room).await?;
            for intent in intents {
                write_message(&mut stream, &SwarmMsg::Intent(intent)).await?;
            }
            Ok::<(), DaemonError>(())
        }
        .await
        .is_ok();

        let mut forwards = self
            .inner
            .intent_forwards
            .lock()
            .expect("intent-forward lock is not poisoned");
        if let Some(state) = forwards.get_mut(&room) {
            if state.leader == leader && state.rpc_term == rpc_term && state.ids == ids {
                state.retry_after = Instant::now()
                    + if forwarded {
                        INTENT_FORWARD_RETRY_INTERVAL
                    } else {
                        Duration::from_millis(500)
                    };
            }
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

    async fn receive_close_take(
        &self,
        close: CloseTake,
    ) -> Result<Option<CommittedScene>, DaemonError> {
        let floor = self.floor(close.room)?;
        let _mutation = floor.mutation.lock().await;
        let replay = self.replay(close.room)?;
        if replay.consensus.role != ConsensusRole::Leader
            || replay.consensus.leader_id != Some(self.node_id())
            || replay.chain.live_grant.as_ref().is_none_or(|grant| {
                grant.hash != close.grant_hash || grant.to.node == self.node_id()
            })
        {
            return Ok(None);
        }
        let record = self
            .commit_singleton_body(
                close.room,
                Body::Speech {
                    closes_grant: close.grant_hash,
                    text: close.text,
                    blobs: close.blobs,
                },
                &floor,
            )
            .await?;
        self.maybe_grant_next_locked(close.room, &floor).await?;
        Ok(Some(record))
    }

    async fn receive_leave(&self, leave: &Leave) -> Result<Option<CommittedScene>, DaemonError> {
        let floor = self.floor(leave.room)?;
        let _mutation = floor.mutation.lock().await;
        let replay = self.replay(leave.room)?;
        if replay.consensus.role != ConsensusRole::Leader
            || replay.consensus.leader_id != Some(self.node_id())
            || replay.chain.live_grant.is_some()
            || !replay.chain.roster.contains(&leave.node)
            || replay.chain.roster.len() <= 1
        {
            return Ok(None);
        }
        let mut next_roster = replay.chain.roster.clone();
        next_roster.retain(|node| *node != leave.node);
        let record = self
            .commit_singleton_body(
                leave.room,
                Body::ViewChange {
                    add: Vec::new(),
                    remove: vec![leave.node],
                    next_roster,
                    closes_grant: None,
                },
                &floor,
            )
            .await?;
        Ok(Some(record))
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
        let replay = self.replay(room)?;
        if let Some(leader) = replay
            .consensus
            .leader_id
            .filter(|leader| *leader != self.node_id())
        {
            drop(_mutation);
            let close = CloseTake {
                room,
                grant_hash: frozen.grant_hash,
                text: frozen.text.clone(),
                rev: frozen.rev,
                blobs: frozen.blobs.clone(),
            };
            let record = self
                .request_close_take(leader, &close)
                .await?
                .ok_or(DaemonError::MutationUnavailable)?;
            self.install_record(room, record).await?;
            return Ok(serde_json::to_value(SpeakAck {
                ok: true,
                grant_hash: frozen.grant_hash,
                rev: frozen.rev,
            })?);
        }
        if replay.chain.roster.len() > 1
            && (replay.consensus.role != ConsensusRole::Leader
                || replay.consensus.leader_id != Some(self.node_id()))
        {
            return Err(DaemonError::MutationUnavailable);
        }
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
            if replay.chain.roster.len() > 1
                && (replay.consensus.role != ConsensusRole::Leader
                    || replay.consensus.leader_id != Some(self.node_id()))
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
            let committed = if self.replay(room)?.chain.roster.len() == 1 {
                let daemon = self.clone();
                let floor = Arc::clone(floor);
                let proposed = body.clone();
                task::spawn_blocking(move || {
                    daemon.commit_singleton_body_blocking(room, proposed, &floor)
                })
                .await?
            } else {
                match self
                    .commit_distributed_body(room, body.clone(), floor)
                    .await
                {
                    Err(DaemonError::RecoveredHead) => continue,
                    result => result,
                }
            }?;
            if committed.scene.body == body {
                return Ok(committed);
            }
            // A carried pending entry is a successful recovery, not success for
            // this caller. Re-evaluate this body against the new committed head.
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
        let node = self.node_id();
        let store = self.store(room)?;
        // Campaign from the live cached state while holding the same lock used by
        // inbound votes, appends, heartbeats, and installed proofs.  Starting from
        // a detached Replay clone can otherwise overwrite a vote or a higher term
        // that arrived between the clone and the cache write (§11.3).
        let (rpc_term, local_tail, roster) = {
            let entry = self.replay_entry(room)?;
            let mut replay = entry.lock().expect("replay lock is not poisoned");
            if replay.consensus.role == ConsensusRole::Leader
                && replay.consensus.leader_id == Some(node)
                && replay.chain.roster.contains(&node)
            {
                return Ok(replay.consensus.current_term);
            }
            if !replay.chain.roster.contains(&node) {
                return Err(DaemonError::MutationUnavailable);
            }
            let local_tail = tail(
                replay.pending.as_ref(),
                replay.chain.head_n.zip(replay.chain.head_hash),
                replay.head_proof.as_ref(),
            )?;
            let roster = replay.chain.roster.clone();
            let rpc_term = begin_campaign(&mut replay.consensus, node, &roster, local_tail)?;
            store.write_consensus(&replay.consensus)?;
            (rpc_term, local_tail, roster)
        };
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
        for peer in roster.iter().copied().filter(|peer| *peer != node) {
            if let Ok(Some(vote)) = self.request_vote(peer, &request).await {
                if valid_vote(&vote, &roster, node, rpc_term) {
                    votes.insert(vote.voter);
                }
            }
        }
        if votes.len() < majority(roster.len()) {
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
        self.run_win_step(room, rpc_term).await?;
        Ok(rpc_term)
    }

    /// Execute §11.3's bounded post-election `have` probe and carry-forward.
    /// This runs for spontaneous elections too, so an accepted pending entry does
    /// not wait for an unrelated client mutation before it can commit.
    async fn run_win_step(&self, room: RoomId, won_term: u64) -> Result<(), DaemonError> {
        let node = self.node_id();
        let before = self.replay(room)?;
        let mut probes = JoinSet::new();
        for peer in before
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
            let Ok((peer, Ok(Some(have)))) = result else {
                continue;
            };
            let local = self.replay(room)?;
            let local_n = local.chain.head_n.expect("committed room has a head");
            let resolves_pending = local
                .pending
                .as_ref()
                .is_some_and(|pending| have.n == pending.n && have.hash == pending.hash);
            if have.n > local_n || resolves_pending {
                self.sync_from_known_peer(peer, room).await?;
            }
        }

        let current = self.replay(room)?;
        if current.consensus.current_term != won_term
            || current.consensus.role != ConsensusRole::Leader
            || current.consensus.leader_id != Some(node)
        {
            return Err(DaemonError::MutationUnavailable);
        }
        let Some(body) = current
            .pending
            .as_ref()
            .map(|pending| pending.scene.body.clone())
        else {
            return Ok(());
        };
        let floor = self.floor(room)?;
        // Indirection keeps the async future finite; the runtime call does not
        // recurse because this node is already the checked leader for won_term.
        match Box::pin(self.commit_distributed_body(room, body, &floor)).await {
            Ok(_) | Err(DaemonError::RecoveredHead) => {}
            Err(error) => return Err(error),
        }
        let after = self.replay(room)?;
        if after.consensus.current_term != won_term
            || after.consensus.role != ConsensusRole::Leader
            || after.consensus.leader_id != Some(node)
        {
            return Err(DaemonError::MutationUnavailable);
        }
        Ok(())
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
    ) -> Result<BoxedStream, DaemonError> {
        let dial_guard = self
            .dial_guard(room)
            .ok_or(DaemonError::MutationUnavailable)?;
        let transport = self
            .inner
            .transport
            .read()
            .expect("transport lock is not poisoned")
            .clone();
        for endpoint in self.peer_endpoints(room, peer) {
            let (authority, secure) = if let Some(authority) = endpoint.strip_prefix("tcp://") {
                if transport.mode == TransportMode::Public {
                    continue;
                }
                (authority, false)
            } else if let Some(authority) = endpoint.strip_prefix("tcps://") {
                if transport.tls_client.is_none() {
                    continue;
                }
                (authority, true)
            } else {
                continue;
            };
            let Ok(addresses) = tokio::net::lookup_host(authority).await else {
                continue;
            };
            for address in addresses {
                if !secure && transport.mode == TransportMode::Local && !address.ip().is_loopback()
                {
                    continue;
                }
                let Ok(Ok(stream)) = timeout(SYNC_TIMEOUT, TcpStream::connect(address)).await
                else {
                    continue;
                };
                let mut stream: BoxedStream = if secure {
                    let Some(config) = transport.tls_client.clone() else {
                        continue;
                    };
                    let host = tls_authority_host(authority)?;
                    let server_name = ServerName::try_from(host)
                        .map_err(|_| DaemonError::InvalidEndpoint(endpoint.clone()))?;
                    let Ok(Ok(tls)) = timeout(
                        HANDSHAKE_TIMEOUT,
                        TlsConnector::from(config).connect(server_name, stream),
                    )
                    .await
                    else {
                        continue;
                    };
                    Box::new(tls)
                } else {
                    Box::new(stream)
                };
                let Ok(mut remote) = self.initiate_handshake(&mut stream, Some(peer)).await else {
                    continue;
                };
                if self
                    .authorize_outbound(&mut stream, &mut remote, room, self.room_token(room))
                    .await
                    .is_err()
                {
                    continue;
                }
                return Ok(Box::new(GuardedStream {
                    stream,
                    _dial_guard: dial_guard,
                }));
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
        let probe = async {
            let mut stream = self.connect_known_peer(peer, room).await?;
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
                    if vote.room == request.room
                        && vote.rpc_term == request.rpc_term
                        && vote.voter == peer
                    {
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

    async fn request_leave(
        &self,
        leader: NodeId,
        leave: &Leave,
    ) -> Result<Option<CommittedScene>, DaemonError> {
        let mut stream = self.connect_known_peer(leader, leave.room).await?;
        write_message(&mut stream, &SwarmMsg::Leave(leave.clone())).await?;
        let response = async {
            while let Some(message) = read_message(&mut stream).await? {
                if let SwarmMsg::Scene(record) = message {
                    if record.scene.room == leave.room
                        && matches!(
                            &record.scene.body,
                            Body::ViewChange { remove, .. } if remove == std::slice::from_ref(&leave.node)
                        )
                    {
                        return Ok(Some(record));
                    }
                }
            }
            Ok::<_, DaemonError>(None)
        };
        match timeout(Duration::from_secs(8), response).await {
            Ok(result) => result,
            Err(_) => Ok(None),
        }
    }

    async fn request_grant(
        &self,
        leader: NodeId,
        request: &GrantReq,
    ) -> Result<Option<CommittedScene>, DaemonError> {
        let mut stream = self.connect_known_peer(leader, request.room).await?;
        write_message(&mut stream, &SwarmMsg::GrantReq(request.clone())).await?;
        let response = async {
            while let Some(message) = read_message(&mut stream).await? {
                if let SwarmMsg::Scene(record) = message {
                    if record.scene.room == request.room
                        && matches!(&record.scene.body, Body::Grant { to, .. } if to == &request.to)
                    {
                        return Ok(Some(record));
                    }
                }
            }
            Ok::<_, DaemonError>(None)
        };
        match timeout(Duration::from_secs(8), response).await {
            Ok(result) => result,
            Err(_) => Ok(None),
        }
    }

    async fn request_yank(
        &self,
        leader: NodeId,
        request: &YankReq,
    ) -> Result<Option<CommittedScene>, DaemonError> {
        let grant_hash = self
            .replay(request.room)?
            .chain
            .live_grant
            .ok_or(FloorError::NoGrant)?
            .hash;
        let mut stream = self.connect_known_peer(leader, request.room).await?;
        write_message(&mut stream, &SwarmMsg::YankReq(request.clone())).await?;
        let response = async {
            while let Some(message) = read_message(&mut stream).await? {
                if let SwarmMsg::Scene(record) = message {
                    if record.scene.room == request.room
                        && matches!(
                            &record.scene.body,
                            Body::Speech { closes_grant, .. } if *closes_grant == grant_hash
                        )
                    {
                        return Ok(Some(record));
                    }
                }
            }
            Ok::<_, DaemonError>(None)
        };
        match timeout(Duration::from_secs(8), response).await {
            Ok(result) => result,
            Err(_) => Ok(None),
        }
    }

    async fn request_breakout(
        &self,
        leader: NodeId,
        request: &BreakoutReq,
    ) -> Result<Option<CommittedScene>, DaemonError> {
        let mut stream = self.connect_known_peer(leader, request.room).await?;
        write_message(&mut stream, &SwarmMsg::BreakoutReq(request.clone())).await?;
        let response = async {
            while let Some(message) = read_message(&mut stream).await? {
                if let SwarmMsg::Scene(record) = message {
                    if record.scene.room == request.room
                        && matches!(record.scene.body, Body::Breakout { .. })
                    {
                        return Ok(Some(record));
                    }
                }
            }
            Ok::<_, DaemonError>(None)
        };
        match timeout(Duration::from_secs(8), response).await {
            Ok(result) => result,
            Err(_) => Ok(None),
        }
    }

    async fn request_membership(
        &self,
        leader: NodeId,
        request: &MembershipReq,
    ) -> Result<Option<CommittedScene>, DaemonError> {
        let mut stream = self.connect_known_peer(leader, request.room).await?;
        write_message(&mut stream, &SwarmMsg::MembershipReq(request.clone())).await?;
        let response = async {
            while let Some(message) = read_message(&mut stream).await? {
                if let SwarmMsg::Scene(record) = message {
                    if record.scene.room == request.room
                        && matches!(record.scene.body, Body::Membership { .. })
                    {
                        return Ok(Some(record));
                    }
                }
            }
            Ok::<_, DaemonError>(None)
        };
        match timeout(Duration::from_secs(8), response).await {
            Ok(result) => result,
            Err(_) => Ok(None),
        }
    }

    async fn request_close_take(
        &self,
        leader: NodeId,
        close: &CloseTake,
    ) -> Result<Option<CommittedScene>, DaemonError> {
        let mut stream = self.connect_known_peer(leader, close.room).await?;
        self.send_blob_refs(&mut stream, close.room, &close.blobs)
            .await?;
        write_message(&mut stream, &SwarmMsg::CloseTake(close.clone())).await?;
        let response = async {
            while let Some(message) = read_message(&mut stream).await? {
                if let SwarmMsg::Scene(record) = message {
                    if record.scene.room == close.room
                        && matches!(
                            record.scene.body,
                            Body::Speech { closes_grant, .. } if closes_grant == close.grant_hash
                        )
                    {
                        return Ok(Some(record));
                    }
                }
            }
            Ok::<_, DaemonError>(None)
        };
        match timeout(Duration::from_secs(8), response).await {
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
                        if cert.room == append.room
                            && cert.n == append.scene.n
                            && cert.node == peer =>
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
            let _ = self.maybe_remove_unavailable(room).await;
            let due = self.election_due(room);
            let cert = self.can_certify(room).unwrap_or(false);
            if !due || !cert {
                continue;
            }
            self.reset_election_timeout(room);
            let Ok(floor) = self.floor(room) else {
                continue;
            };
            let Ok(_mutation) = floor.mutation.try_lock() else {
                continue;
            };
            if self.ensure_network_leader(room).await.is_ok() {
                // Intents are durable ledger inputs, not connection-local
                // requests. A successor must resume an already queued hand as
                // soon as it wins instead of waiting for the agent to retry.
                let _ = self.maybe_grant_next_locked(room, &floor).await;
            }
        }
    }

    fn mark_peer_seen(&self, room: RoomId, peer: NodeId) -> Result<(), DaemonError> {
        let now = unix_timestamp();
        let mut last_seen = self
            .inner
            .last_seen
            .write()
            .expect("last-seen lock is not poisoned");
        let seen = last_seen.entry(room).or_default().entry(peer).or_default();
        let persist = now.saturating_sub(*seen) >= 30;
        *seen = now;
        if !persist {
            return Ok(());
        }
        let snapshot = last_seen.clone();
        drop(last_seen);
        write_json_atomic(&self.inner.data_dir.join("last-seen.json"), &snapshot)
    }

    fn remember_declaration(&self, node: NodeId, declaration: &Declaration) {
        self.inner
            .declarations
            .write()
            .expect("declaration lock is not poisoned")
            .entry(declaration.room)
            .or_default()
            .insert(node, declaration.clone());
    }

    fn remember_room_agent(&self, room: RoomId, agent: AgentId) {
        self.inner
            .room_agents
            .write()
            .expect("room-agent registry lock is not poisoned")
            .entry(room)
            .or_default()
            .insert(agent);
    }

    async fn maybe_remove_unavailable(&self, room: RoomId) -> Result<(), DaemonError> {
        let replay = self.replay(room)?;
        let node = self.node_id();
        if replay.consensus.role != ConsensusRole::Leader
            || replay.consensus.leader_id != Some(node)
            || replay.chain.live_grant.is_some()
            || replay.chain.roster.len() <= 1
        {
            return Ok(());
        }
        let cutoff = unix_timestamp().saturating_sub(REMOVE_AFTER_SECONDS);
        let unavailable = self
            .inner
            .last_seen
            .read()
            .expect("last-seen lock is not poisoned")
            .get(&room)
            .and_then(|seen| {
                replay
                    .chain
                    .roster
                    .iter()
                    .copied()
                    .filter(|peer| *peer != node)
                    .filter(|peer| seen.get(peer).copied().unwrap_or(0) <= cutoff)
                    .min()
            });
        let Some(unavailable) = unavailable else {
            return Ok(());
        };
        let floor = self.floor(room)?;
        let Ok(_mutation) = floor.mutation.try_lock() else {
            return Ok(());
        };
        let current = self.replay(room)?;
        if current.consensus.role != ConsensusRole::Leader
            || current.consensus.leader_id != Some(node)
            || current.chain.live_grant.is_some()
            || !current.chain.roster.contains(&unavailable)
        {
            return Ok(());
        }
        let next_roster = current
            .chain
            .roster
            .iter()
            .copied()
            .filter(|peer| *peer != unavailable)
            .collect::<Vec<_>>();
        self.commit_singleton_body(
            room,
            Body::ViewChange {
                add: Vec::new(),
                remove: vec![unavailable],
                next_roster,
                closes_grant: None,
            },
            &floor,
        )
        .await?;
        Ok(())
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
        let Some(_syncing) = self.mark_syncing(room) else {
            return Ok(());
        };
        let mut stream = self.connect_known_peer(peer, room).await?;
        self.sync_authorized_stream(&mut stream, room, None)
            .await
            .map(|_| ())
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

    pub(crate) fn authenticate(&self, room: RoomId, token: Hash32) -> Result<bool, DaemonError> {
        let expected = match self.token_sha256(room) {
            Ok(expected) => expected,
            Err(DaemonError::UnknownRoom(_)) => return Ok(false),
            Err(error) => return Err(error),
        };
        let Some(expected) = expected else {
            return Ok(true);
        };
        let actual: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        Ok(bool::from(actual.ct_eq(expected.as_bytes())))
    }

    fn authorize_room_token(
        &self,
        room: RoomId,
        token: Option<Hash32>,
    ) -> Result<bool, DaemonError> {
        match self.token_sha256(room) {
            Ok(None) => Ok(token.is_none() && self.transport_mode() != TransportMode::Public),
            Ok(Some(_)) => {
                Ok(token.is_some_and(|token| self.authenticate(room, token).unwrap_or(false)))
            }
            Err(DaemonError::UnknownRoom(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn reject_public_open_token(&self, token: Option<Hash32>) -> Result<(), DaemonError> {
        if self.transport_mode() == TransportMode::Public && token.is_none() {
            Err(DaemonError::OpenRoomInPublic)
        } else {
            Ok(())
        }
    }

    fn reject_public_open_from_replay(&self, replay: &Replay) -> Result<(), DaemonError> {
        let Some(Body::Genesis { token_sha256, .. }) =
            replay.history.first().map(|record| &record.scene.body)
        else {
            return Ok(());
        };
        if self.transport_mode() == TransportMode::Public && token_sha256.is_none() {
            Err(DaemonError::OpenRoomInPublic)
        } else {
            Ok(())
        }
    }

    fn room_authorized(
        &self,
        room: RoomId,
        authed: &BTreeSet<RoomId>,
    ) -> Result<bool, DaemonError> {
        Ok(self.token_sha256(room).is_ok() && authed.contains(&room))
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

    pub(crate) fn operator_catalog(&self) -> Result<Value, DaemonError> {
        let rooms = self
            .inner
            .rooms
            .read()
            .expect("room registry lock is not poisoned")
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let mut summaries = rooms
            .into_iter()
            .map(|room| {
                self.operator_room_summary(room).unwrap_or_else(|_| {
                    json!({
                        "id": room,
                        "name": "Unavailable room",
                        "parent": null,
                        "role": JoinRole::Observe,
                        "head_n": null,
                        "head_hash": null,
                        "last_activity": 0,
                        "syncing": false,
                        "valid": false,
                        "browser_mutable": false,
                        "roster_size": 0,
                        "floor": { "state": "unavailable" },
                    })
                })
            })
            .collect::<Vec<_>>();
        summaries.sort_by(|left, right| {
            right["last_activity"]
                .as_u64()
                .cmp(&left["last_activity"].as_u64())
                .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
        });
        Ok(json!({
            "node": self.node_id(),
            "rooms": summaries,
        }))
    }

    fn operator_room_summary(&self, room: RoomId) -> Result<Value, DaemonError> {
        let replay = self.replay(room)?;
        let (name, parent, browser_mutable) = replay
            .history
            .first()
            .and_then(|record| match &record.scene.body {
                Body::Genesis {
                    name,
                    parent_room,
                    token_sha256,
                    ..
                } => Some((name.clone(), *parent_room, token_sha256.is_some())),
                _ => None,
            })
            .ok_or(DaemonError::Protocol("room has no genesis scene"))?;
        let role = self
            .inner
            .joins
            .read()
            .expect("join registry lock is not poisoned")
            .get(&room)
            .map(|join| join.role)
            .unwrap_or(JoinRole::Observe);
        let syncing = self
            .inner
            .syncing
            .read()
            .expect("sync registry lock is not poisoned")
            .contains(&room);
        let floor = replay
            .chain
            .live_grant
            .as_ref()
            .map(
                |grant| json!({ "state": "held", "agent": &grant.to.agent, "node": grant.to.node }),
            )
            .unwrap_or_else(|| json!({ "state": "vacant" }));
        Ok(json!({
            "id": room,
            "name": name,
            "parent": parent,
            "role": role,
            "head_n": replay.chain.head_n,
            "head_hash": replay.chain.head_hash,
            "last_activity": replay.history.last().map(|record| record.scene.ts).unwrap_or(0),
            "syncing": syncing,
            "valid": true,
            "browser_mutable": browser_mutable,
            "roster_size": replay.chain.roster.len(),
            "floor": floor,
        }))
    }

    pub(crate) fn operator_room_detail(&self, room: RoomId) -> Result<Value, DaemonError> {
        let replay = self.replay(room)?;
        let summary = self.operator_room_summary(room)?;
        let local = self.node_id();
        let now = unix_timestamp();
        let declarations = self
            .inner
            .declarations
            .read()
            .expect("declaration lock is not poisoned")
            .get(&room)
            .cloned()
            .unwrap_or_default();
        let last_seen = self
            .inner
            .last_seen
            .read()
            .expect("last-seen lock is not poisoned")
            .get(&room)
            .cloned()
            .unwrap_or_default();
        let local_agents = self
            .inner
            .room_agents
            .read()
            .expect("room-agent registry lock is not poisoned")
            .get(&room)
            .cloned()
            .unwrap_or_default();

        let mut nodes = BTreeMap::<NodeId, (JoinRole, BTreeSet<AgentId>)>::new();
        for node in &replay.chain.roster {
            nodes.insert(*node, (JoinRole::Stake, BTreeSet::new()));
        }
        nodes.entry(local).or_insert((
            if replay.chain.roster.contains(&local) {
                JoinRole::Stake
            } else {
                JoinRole::Observe
            },
            BTreeSet::new(),
        ));
        for (node, declaration) in declarations {
            let recent = node == local
                || last_seen
                    .get(&node)
                    .is_some_and(|seen| now.saturating_sub(*seen) <= 10);
            if !recent {
                continue;
            }
            let entry = nodes.entry(node).or_insert((
                if replay.chain.roster.contains(&node) {
                    JoinRole::Stake
                } else {
                    JoinRole::Observe
                },
                BTreeSet::new(),
            ));
            entry.1.extend(declaration.agents);
        }
        nodes
            .entry(local)
            .or_insert((JoinRole::Observe, BTreeSet::new()))
            .1
            .extend(local_agents);
        for record in &replay.history {
            if let Body::Grant { to, .. } = &record.scene.body {
                nodes
                    .entry(to.node)
                    .or_insert((
                        if replay.chain.roster.contains(&to.node) {
                            JoinRole::Stake
                        } else {
                            JoinRole::Observe
                        },
                        BTreeSet::new(),
                    ))
                    .1
                    .insert(to.agent.clone());
            }
        }
        if let Some(moderator) = &replay.chain.moderator {
            nodes
                .entry(moderator.node)
                .or_insert((JoinRole::Observe, BTreeSet::new()))
                .1
                .insert(moderator.agent.clone());
        }
        let mut mouths = Vec::new();
        for (node, (role, agents)) in &nodes {
            let seen = if *node == local {
                Some(now)
            } else {
                last_seen.get(node).copied()
            };
            for agent in agents {
                mouths.push(json!({
                    "agent": agent,
                    "node": node,
                    "role": role,
                    "local": *node == local,
                    "recent": seen.is_some_and(|seen| now.saturating_sub(seen) <= 10),
                    "last_seen": seen,
                    "leader": replay.consensus.leader_id == Some(*node),
                    "floor_holder": replay.chain.live_grant.as_ref().is_some_and(|grant| grant.to.node == *node && grant.to.agent == *agent),
                    "moderator": replay.chain.moderator.as_ref().is_some_and(|moderator| moderator.node == *node && moderator.agent == *agent),
                }));
            }
        }
        let participants = nodes
            .into_iter()
            .map(|(node, (role, agents))| {
                let seen = if node == local {
                    Some(now)
                } else {
                    last_seen.get(&node).copied()
                };
                json!({
                    "node": node,
                    "role": role,
                    "agents": agents,
                    "local": node == local,
                    "recent": seen.is_some_and(|seen| now.saturating_sub(seen) <= 10),
                    "last_seen": seen,
                    "leader": replay.consensus.leader_id == Some(node),
                    "floor_holder": replay.chain.live_grant.as_ref().is_some_and(|grant| grant.to.node == node),
                    "moderator": replay.chain.moderator.as_ref().is_some_and(|moderator| moderator.node == node),
                })
            })
            .collect::<Vec<_>>();
        let mut queue = self
            .floor(room)?
            .engine
            .lock()
            .expect("floor lock is not poisoned")
            .intents()
            .filter(|intent| {
                replay.chain.roster.contains(&intent.node)
                    && !replay.chain.consumed_intents.contains(&intent.id)
                    && now < intent.exp
            })
            .cloned()
            .collect::<Vec<_>>();
        queue.sort_by_key(|intent| (intent.ts, intent.id));
        let queue = queue
            .into_iter()
            .enumerate()
            .map(|(index, intent)| {
                json!({
                    "position": index + 1,
                    "intent_id": intent.id,
                    "agent": intent.agent,
                    "node": intent.node,
                    "kind": intent.kind,
                    "ts": intent.ts,
                    "exp": intent.exp,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "room": summary,
            "node": local,
            "participants": participants,
            "mouths": mouths,
            "floor": {
                "mode": replay.chain.floor_mode,
                "timeout_secs": replay.chain.timeout_secs,
                "holder": replay.chain.live_grant.as_ref().map(|grant| &grant.to),
                "leader": replay.consensus.leader_id,
                "queue": queue,
            }
        }))
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

    fn mark_syncing(&self, room: RoomId) -> Option<SyncMarker> {
        let inserted = self
            .inner
            .syncing
            .write()
            .expect("sync registry lock is not poisoned")
            .insert(room);
        inserted.then(|| SyncMarker {
            inner: Arc::clone(&self.inner),
            room,
        })
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
                self.spawn_child_join(ticket);
            }
        }
        Ok(())
    }

    fn spawn_child_join(&self, ticket: Ticket) {
        let daemon = self.clone();
        tokio::spawn(async move {
            // The parent commit push is fire-and-forget. Give the holder time to
            // promote its durable stage before another auto-join node fetches it.
            sleep(Duration::from_millis(250)).await;
            loop {
                if daemon.replay(ticket.id).is_ok() {
                    break;
                }
                if Box::pin(daemon.join_ticket(ticket.clone(), JoinRole::Stake))
                    .await
                    .is_ok()
                {
                    break;
                }
                sleep(Duration::from_millis(500)).await;
            }
        });
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

fn request_room(request: &ClientRequest) -> Option<RoomId> {
    match request {
        ClientRequest::WaitForFloor { room, .. }
        | ClientRequest::Speak { room, .. }
        | ClientRequest::Yield { room }
        | ClientRequest::RaiseHand { room }
        | ClientRequest::Grant { room, .. }
        | ClientRequest::Yank { room }
        | ClientRequest::Breakout { room, .. }
        | ClientRequest::Membership { room, .. }
        | ClientRequest::PutBlob { room, .. }
        | ClientRequest::Leave { room, .. }
        | ClientRequest::History { room, .. }
        | ClientRequest::WaitForHistory { room, .. } => Some(*room),
        ClientRequest::Status { room } => *room,
        ClientRequest::Attach { .. }
        | ClientRequest::Create { .. }
        | ClientRequest::Join { .. } => None,
    }
}

fn swarm_message_room(message: &SwarmMsg) -> Option<RoomId> {
    match message {
        SwarmMsg::Auth(message) => Some(message.room),
        SwarmMsg::Authed(message) => Some(message.room),
        SwarmMsg::Pex(message) => Some(message.room),
        SwarmMsg::Have(message) => Some(message.room),
        SwarmMsg::RequestVote(message) => Some(message.room),
        SwarmMsg::Vote(message) => Some(message.room),
        SwarmMsg::Append(message) => Some(message.room),
        SwarmMsg::Cert(message) => Some(message.room),
        SwarmMsg::Commit(message) => Some(message.room),
        SwarmMsg::Heartbeat(message) => Some(message.room),
        SwarmMsg::Nack(message) => Some(message.room),
        SwarmMsg::GetScenes(message) => Some(message.room),
        SwarmMsg::Scene(message) => Some(message.scene.room),
        SwarmMsg::Intent(message) => Some(message.room),
        SwarmMsg::Freeze(message) => Some(message.room),
        SwarmMsg::CloseTake(message) => Some(message.room),
        SwarmMsg::Leave(message) => Some(message.room),
        SwarmMsg::GrantReq(message) => Some(message.room),
        SwarmMsg::YankReq(message) => Some(message.room),
        SwarmMsg::BreakoutReq(message) => Some(message.room),
        SwarmMsg::MembershipReq(message) => Some(message.room),
        SwarmMsg::GetBlob(message) => Some(message.room),
        SwarmMsg::HelloI(_)
        | SwarmMsg::HelloR(_)
        | SwarmMsg::HelloAck(_)
        | SwarmMsg::BlobMeta(_) => None,
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

fn validate_prepared_breakout(
    parent: RoomId,
    from: &Mouth,
    requested_name: &str,
    ticket: &Ticket,
    genesis: &CommittedScene,
) -> Result<(), DaemonError> {
    ticket.validate()?;
    if ticket.parent != Some(parent)
        || ticket.name != requested_name
        || ticket.id != genesis.scene.room
        || ticket.genesis != hash_scene(&genesis.scene)
    {
        return Err(DaemonError::Protocol(
            "prepared breakout ticket does not match its genesis",
        ));
    }
    match &genesis.scene.body {
        Body::Genesis {
            name,
            stake,
            floor,
            creator_node,
            parent_room,
            ..
        } if name == requested_name
            && stake == &ticket.stake
            && floor == &ticket.floor
            && *creator_node == from.node
            && *parent_room == Some(parent) => {}
        _ => {
            return Err(DaemonError::Protocol(
                "prepared breakout genesis was not created by the holder",
            ));
        }
    }
    apply(
        &ChainState::empty(),
        &genesis.scene,
        Some(&genesis.commit_proof),
        ApplyMode::Staged,
    )?;
    Ok(())
}

fn tls_authority_host(authority: &str) -> Result<String, DaemonError> {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split_once(']')
            .map(|(host, _)| host)
            .ok_or_else(|| DaemonError::InvalidEndpoint(authority.to_owned()))?
    } else {
        authority
            .rsplit_once(':')
            .map(|(host, _)| host)
            .ok_or_else(|| DaemonError::InvalidEndpoint(authority.to_owned()))?
    };
    if host.is_empty() {
        return Err(DaemonError::InvalidEndpoint(authority.to_owned()));
    }
    Ok(host.to_owned())
}

fn canonical_peer_endpoint(mode: TransportMode, endpoint: &str) -> Option<String> {
    let url = url::Url::parse(endpoint).ok()?;
    let scheme = url.scheme();
    let scheme_allowed = match mode {
        TransportMode::Local | TransportMode::Lan => matches!(scheme, "tcp" | "tcps"),
        TransportMode::Public => scheme == "tcps",
    };
    if !scheme_allowed
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return None;
    }
    let host = url.host()?;
    if mode == TransportMode::Local && scheme == "tcp" {
        let ip = match host {
            url::Host::Ipv4(ip) => IpAddr::V4(ip),
            url::Host::Ipv6(ip) => IpAddr::V6(ip),
            // `url` treats hosts on non-special schemes such as `tcp` as
            // domains, even when their spelling is a literal IP address.
            url::Host::Domain(host) => host.parse().ok()?,
        };
        if !ip.is_loopback() {
            return None;
        }
    }
    let port = url.port()?;
    let host = match host {
        url::Host::Domain(host) => match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) => ip.to_string(),
            Ok(IpAddr::V6(ip)) => format!("[{ip}]"),
            Err(_) => host.to_ascii_lowercase(),
        },
        url::Host::Ipv4(host) => host.to_string(),
        url::Host::Ipv6(host) => format!("[{host}]"),
    };
    Some(format!("{scheme}://{host}:{port}"))
}

fn literal_loopback_authority(authority: &str) -> bool {
    tls_authority_host(authority)
        .ok()
        .and_then(|host| host.parse::<std::net::IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback())
}

fn advertised_endpoint_allowed(mode: TransportMode, endpoint: &str) -> bool {
    match mode {
        TransportMode::Local => {
            if let Some(authority) = endpoint.strip_prefix("tcp://") {
                literal_loopback_authority(authority)
            } else if endpoint.starts_with("ws://") {
                url::Url::parse(endpoint)
                    .ok()
                    .and_then(|url| url.host_str()?.parse::<IpAddr>().ok())
                    .is_some_and(|ip| ip.is_loopback())
            } else {
                false
            }
        }
        TransportMode::Lan => endpoint.starts_with("tcp://") || endpoint.starts_with("ws://"),
        TransportMode::Public => endpoint.starts_with("tcps://") || endpoint.starts_with("wss://"),
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
    let encoded = frame::encode(message)?;
    timeout(IO_TIMEOUT, async {
        writer.write_all(&encoded).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| DaemonError::Protocol("write timed out"))??;
    Ok(())
}

pub async fn read_frame<R, T>(reader: &mut R) -> Result<Option<T>, DaemonError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    timeout(IO_TIMEOUT, read_frame_limited(reader, MAX_FRAME_BYTES))
        .await
        .map_err(|_| DaemonError::Protocol("read timed out"))?
}

async fn read_raw_length<R>(reader: &mut R) -> Result<u32, DaemonError>
where
    R: AsyncRead + Unpin,
{
    timeout(IO_TIMEOUT, reader.read_u32())
        .await
        .map_err(|_| DaemonError::Protocol("raw payload length timed out"))?
        .map_err(DaemonError::Io)
}

async fn read_raw_bytes<R>(reader: &mut R, bytes: &mut [u8]) -> Result<(), DaemonError>
where
    R: AsyncRead + Unpin,
{
    timeout(IO_TIMEOUT, reader.read_exact(bytes))
        .await
        .map_err(|_| DaemonError::Protocol("raw payload read timed out"))??;
    Ok(())
}

async fn write_raw_bytes<W>(writer: &mut W, bytes: &[u8]) -> Result<(), DaemonError>
where
    W: AsyncWrite + Unpin,
{
    timeout(IO_TIMEOUT, async {
        writer.write_u32(bytes.len() as u32).await?;
        writer.write_all(bytes).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| DaemonError::Protocol("raw payload write timed out"))??;
    Ok(())
}

async fn read_frame_limited<R, T>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<T>, DaemonError>
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
    if length > max_bytes {
        return Err(FrameError::TooLarge.into());
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload).await?;
    Ok(Some(frame::decode_payload(&payload)?))
}

async fn read_pre_auth_message<R>(reader: &mut R) -> Result<Option<SwarmMsg>, DaemonError>
where
    R: AsyncRead + Unpin,
{
    read_frame_limited(reader, PREAUTH_FRAME_BYTES).await
}

fn signed_hash(value: &impl Serialize) -> Hash32 {
    Hash32::from_bytes(signed_object_digest(
        &serde_json::to_value(value).expect("signed handshake object is serializable"),
    ))
}

fn decay_auth_bucket(bucket: &mut AuthFailureBucket) {
    let now = Instant::now();
    let elapsed = now.duration_since(bucket.updated);
    let decay = (elapsed.as_secs() / AUTH_FAILURE_DECAY_INTERVAL.as_secs()) as u32;
    if decay > 0 {
        bucket.failures = bucket.failures.saturating_sub(decay);
        bucket.updated += AUTH_FAILURE_DECAY_INTERVAL * decay;
    }
}

fn valid_handshake_identity(
    label: &str,
    v: u64,
    node: NodeId,
    public: NodeId,
    value: &impl Serialize,
    signature: SignatureBytes,
) -> bool {
    label == HANDSHAKE_LABEL
        && v == 1
        && node == public
        && VerifyingKey::from_bytes(public.as_bytes()).is_ok_and(|key| {
            verify(
                &key,
                &signed_object_digest(
                    &serde_json::to_value(value).expect("handshake object is serializable"),
                ),
                signature.as_bytes(),
            )
        })
}

fn validate_hello_i(hello: &HelloI) -> Result<(), DaemonError> {
    if hello.kind == "hello_i"
        && valid_handshake_identity(
            &hello.label,
            hello.v,
            hello.node,
            hello.r#pub,
            hello,
            hello.sig,
        )
    {
        Ok(())
    } else {
        Err(DaemonError::Protocol("invalid hello_i"))
    }
}

fn validate_hello_r(
    hello_i: &HelloI,
    hello: &HelloR,
    expected: Option<NodeId>,
) -> Result<(), DaemonError> {
    if hello.peer == hello_i.node
        && hello.kind == "hello_r"
        && hello.nonce_i == hello_i.nonce_i
        && hello.hello_i_hash == signed_hash(hello_i)
        && expected.is_none_or(|expected| expected == hello.node)
        && valid_handshake_identity(
            &hello.label,
            hello.v,
            hello.node,
            hello.r#pub,
            hello,
            hello.sig,
        )
    {
        Ok(())
    } else {
        Err(DaemonError::Protocol("invalid hello_r"))
    }
}

fn validate_hello_ack(
    hello_i: &HelloI,
    hello_r: &HelloR,
    hello: &HelloAck,
) -> Result<(), DaemonError> {
    let valid = hello.label == HANDSHAKE_LABEL
        && hello.kind == "hello_ack"
        && hello.v == 1
        && hello.node == hello_i.node
        && hello.peer == hello_r.node
        && hello.nonce_i == hello_i.nonce_i
        && hello.nonce_r == hello_r.nonce_r
        && hello.hello_i_hash == signed_hash(hello_i)
        && hello.hello_r_hash == signed_hash(hello_r)
        && VerifyingKey::from_bytes(hello.node.as_bytes()).is_ok_and(|key| {
            verify(
                &key,
                &signed_object_digest(
                    &serde_json::to_value(hello).expect("hello_ack is serializable"),
                ),
                hello.sig.as_bytes(),
            )
        });
    if valid {
        Ok(())
    } else {
        Err(DaemonError::Protocol("invalid hello_ack"))
    }
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

fn valid_leave(leave: &Leave) -> bool {
    VerifyingKey::from_bytes(leave.node.as_bytes()).is_ok_and(|key| {
        verify(
            &key,
            &signed_object_digest(
                &serde_json::to_value(leave).expect("leave is JSON serializable"),
            ),
            leave.sig.as_bytes(),
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

fn client_peer_allowed(listener: SocketAddr, peer: SocketAddr) -> bool {
    listener.ip().is_loopback() && peer.ip().is_loopback()
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
    let public_path = data_dir.join("node.pub");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut public_file = options.open(&public_path)?;
    public_file.write_all(hex::encode(key.verifying_key().to_bytes()).as_bytes())?;
    public_file.sync_all()?;
    sync_dir(data_dir)?;
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
    fs::rename(&temporary, path)?;
    sync_dir(path.parent().expect("state file has a parent"))?;
    Ok(())
}

#[cfg(unix)]
fn harden_data_dir(root: &Path) -> Result<(), DaemonError> {
    use std::os::unix::fs::PermissionsExt;

    fn visit(path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Conch data paths must not be symbolic links",
            ));
        }
        if metadata.is_dir() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            for entry in fs::read_dir(path)? {
                visit(&entry?.path())?;
            }
        } else if metadata.is_file() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    visit(root)?;
    Ok(())
}

#[cfg(not(unix))]
fn harden_data_dir(_root: &Path) -> Result<(), DaemonError> {
    Ok(())
}

fn set_private_directory(path: &Path) -> Result<(), DaemonError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
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
    fn expired_operator_session_is_rejected_and_pruned() {
        let data = TempDir::new().unwrap();
        let daemon = Daemon::open(data.path()).unwrap();
        let origin = "http://127.0.0.1:7420".to_owned();
        let raw = daemon.create_operator_session(origin.clone());
        assert!(daemon.validate_operator_session(&raw, &origin));
        daemon.inner.operator_sessions.lock().unwrap()[0].expires = 0;

        assert!(!daemon.validate_operator_session(&raw, &origin));
        assert!(daemon.inner.operator_sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn operator_participant_role_comes_from_roster_and_presence_expires() {
        let data = TempDir::new().unwrap();
        let peer_data = TempDir::new().unwrap();
        let daemon = Daemon::open(data.path()).unwrap();
        let peer = Daemon::open(peer_data.path()).unwrap();
        let room = daemon.create_genesis("participant authority").unwrap();
        let declaration = Declaration::signed(
            room,
            JoinRole::Stake,
            vec![AgentId::new("agent:not-yet-a-staker").unwrap()],
            unix_timestamp(),
            &peer.inner.node_key,
        );
        daemon.remember_declaration(peer.node_id(), &declaration);
        daemon.mark_peer_seen(room, peer.node_id()).unwrap();

        let detail = daemon.operator_room_detail(room).unwrap();
        let participant = detail["participants"]
            .as_array()
            .unwrap()
            .iter()
            .find(|participant| participant["node"] == json!(peer.node_id()))
            .unwrap();
        assert_eq!(participant["role"], json!(JoinRole::Observe));

        daemon
            .inner
            .last_seen
            .write()
            .unwrap()
            .get_mut(&room)
            .unwrap()
            .insert(peer.node_id(), unix_timestamp().saturating_sub(11));
        let detail = daemon.operator_room_detail(room).unwrap();
        assert!(!detail["participants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|participant| participant["node"] == json!(peer.node_id())));
    }

    #[tokio::test]
    async fn operator_detail_lists_mouths_and_the_durable_floor_queue() {
        let data = TempDir::new().unwrap();
        let daemon = Daemon::open(data.path()).unwrap();
        let room = daemon.create_genesis("mouths and queue").unwrap();
        let holder = AgentId::new("agent:holder").unwrap();
        let operator = AgentId::new("human:operator").unwrap();
        daemon.remember_room_agent(room, holder.clone());
        daemon.remember_room_agent(room, operator.clone());
        let other_room = daemon.create_genesis("other room").unwrap();
        let outsider = AgentId::new("agent:other-room-only").unwrap();
        daemon.remember_room_agent(other_room, outsider.clone());

        daemon
            .client_raise_hand(holder.clone(), room)
            .await
            .unwrap();
        let queued = daemon
            .client_raise_hand(operator.clone(), room)
            .await
            .unwrap();
        let detail = daemon.operator_room_detail(room).unwrap();

        assert_eq!(detail["participants"].as_array().unwrap().len(), 1);
        let mouths = detail["mouths"].as_array().unwrap();
        assert_eq!(mouths.len(), 2);
        assert!(mouths.iter().any(|mouth| {
            mouth["agent"] == json!(holder) && mouth["floor_holder"] == json!(true)
        }));
        assert!(mouths.iter().any(|mouth| mouth["agent"] == json!(operator)));
        assert!(!mouths.iter().any(|mouth| mouth["agent"] == json!(outsider)));
        let queue = detail["floor"]["queue"].as_array().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0]["position"], json!(1));
        assert_eq!(queue[0]["agent"], json!(operator));
        assert_eq!(queue[0]["intent_id"], queued["intent_id"]);
    }

    #[tokio::test]
    async fn bounded_history_wait_wakes_on_commit_and_times_out_successfully() {
        let data = TempDir::new().unwrap();
        let daemon = Daemon::open(data.path()).unwrap();
        let room = daemon.create_genesis("history wait").unwrap();
        let waiting_daemon = daemon.clone();
        let waiting = tokio::spawn(async move {
            waiting_daemon
                .client_wait_for_history(room, 0, Some(2))
                .await
                .unwrap()
        });
        tokio::task::yield_now().await;

        daemon
            .client_raise_hand(AgentId::new("agent:wakes-wait").unwrap(), room)
            .await
            .unwrap();
        let page = waiting.await.unwrap();
        assert_eq!(page["timed_out"], json!(false));
        assert_eq!(page["scenes"].as_array().unwrap().len(), 1);
        assert_eq!(page["scenes"][0]["scene"]["n"], json!(1));

        let timed_out = daemon
            .client_wait_for_history(room, 1, Some(0))
            .await
            .unwrap();
        assert_eq!(timed_out["timed_out"], json!(true));
        assert!(timed_out["scenes"].as_array().unwrap().is_empty());
    }

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
        assert!(client_peer_allowed(
            "127.0.0.1:7421".parse().unwrap(),
            "127.0.0.1:50000".parse().unwrap()
        ));
        assert!(client_peer_allowed(
            "[::1]:7421".parse().unwrap(),
            "[::1]:50000".parse().unwrap()
        ));
        assert!(!client_peer_allowed(
            "0.0.0.0:7421".parse().unwrap(),
            "127.0.0.1:50000".parse().unwrap()
        ));
        assert!(!client_peer_allowed(
            "127.0.0.1:7421".parse().unwrap(),
            "192.168.1.5:50000".parse().unwrap()
        ));
        assert!(advertised_endpoint_allowed(
            TransportMode::Local,
            "tcp://127.0.0.1:7421"
        ));
        assert!(!advertised_endpoint_allowed(
            TransportMode::Local,
            "tcp://localhost:7421"
        ));
        assert!(!advertised_endpoint_allowed(
            TransportMode::Public,
            "tcp://127.0.0.1:7421"
        ));
        assert!(advertised_endpoint_allowed(
            TransportMode::Public,
            "tcps://conch.example:7421"
        ));
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

    #[cfg(unix)]
    #[test]
    fn data_tree_is_private_and_symlinks_fail_closed() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let data = TempDir::new().unwrap();
        let daemon = Daemon::open(data.path()).unwrap();
        let room = daemon.create_genesis("private disk").unwrap();
        let store = daemon.store(room).unwrap();
        let room_root = store.root().to_path_buf();
        let replay = daemon.replay(room).unwrap();
        let genesis = &replay.history[0];
        let scene = store.scene_path(genesis.scene.n, hash_scene(&genesis.scene));
        for directory in [data.path(), &data.path().join("rooms"), &room_root] {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        for file in [data.path().join("node.key"), data.path().join("node.pub")] {
            assert_eq!(
                fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(daemon);

        for directory in [data.path(), &data.path().join("rooms"), &room_root] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
        }
        for file in [data.path().join("node.key"), scene.clone()] {
            fs::set_permissions(file, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let migrated = Daemon::open(data.path()).unwrap();
        for directory in [data.path(), &data.path().join("rooms"), &room_root] {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        for file in [data.path().join("node.key"), scene] {
            assert_eq!(
                fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(migrated);

        let outside = data.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, data.path().join("rooms").join("unsafe-link")).unwrap();
        assert!(Daemon::open(data.path()).is_err());
    }

    #[test]
    fn repeated_valid_pex_batches_stay_within_cumulative_room_limits() {
        let data = TempDir::new().unwrap();
        let daemon = Daemon::open(data.path()).unwrap();
        let room = daemon.create_genesis("pex bounds").unwrap();
        let batch = |port_base| {
            (0_u16..256)
                .map(|index| {
                    let mut bytes = [0_u8; 32];
                    bytes[..2].copy_from_slice(&index.to_be_bytes());
                    bytes[31] = 1;
                    let addrs = (0_u16..8)
                        .map(|offset| format!("tcp://127.0.0.1:{}", port_base + offset))
                        .collect();
                    PeerInfo {
                        node: NodeId::from_bytes(bytes),
                        addrs,
                    }
                })
                .collect()
        };
        daemon
            .remember_pex(&Pex {
                room,
                peers: batch(10_000_u16),
            })
            .unwrap();
        let before = daemon.inner.peers.read().unwrap().clone();
        assert!(daemon
            .remember_pex(&Pex {
                room,
                peers: batch(10_008_u16),
            })
            .is_err());
        assert_eq!(*daemon.inner.peers.read().unwrap(), before);
        let peers = (256_u16..300)
            .map(|index| {
                let mut bytes = [0_u8; 32];
                bytes[..2].copy_from_slice(&index.to_be_bytes());
                bytes[31] = 1;
                PeerInfo {
                    node: NodeId::from_bytes(bytes),
                    addrs: vec![format!("tcp://127.0.0.1:{}", 20_000 + index)],
                }
            })
            .collect();
        assert!(daemon.remember_pex(&Pex { room, peers }).is_err());
        let peers = daemon.inner.peers.read().unwrap();
        let room_peers = peers.get(&room).unwrap();
        assert!(room_peers.len() <= 256);
        assert!(room_peers.values().all(|endpoints| endpoints.len() <= 8));
    }

    #[test]
    fn invalid_pex_is_rejected_without_persistence_or_dial_work() {
        let data = TempDir::new().unwrap();
        let daemon = Daemon::open(data.path()).unwrap();
        let room = daemon.create_genesis("invalid pex").unwrap();
        daemon
            .remember_pex(&Pex {
                room,
                peers: vec![PeerInfo {
                    node: NodeId::from_bytes([2; 32]),
                    addrs: vec!["tcp://127.0.0.1:7500".to_owned()],
                }],
            })
            .unwrap();
        let before_memory = daemon.inner.peers.read().unwrap().clone();
        let peers_path = data.path().join("peers.json");
        let before_disk = fs::read(&peers_path).unwrap();

        for (index, endpoint) in [
            "not a URL".to_owned(),
            "udp://127.0.0.1:7501".to_owned(),
            "tcp://198.51.100.10:7502".to_owned(),
            "x".repeat(2049),
        ]
        .into_iter()
        .enumerate()
        {
            let mut node = [0_u8; 32];
            node[0] = u8::try_from(index + 3).unwrap();
            assert!(daemon
                .remember_pex(&Pex {
                    room,
                    peers: vec![PeerInfo {
                        node: NodeId::from_bytes(node),
                        addrs: vec![endpoint],
                    }],
                })
                .is_err());
            assert_eq!(*daemon.inner.peers.read().unwrap(), before_memory);
            assert_eq!(fs::read(&peers_path).unwrap(), before_disk);
            assert!(daemon.inner.room_dials.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn connection_and_auth_failure_limits_fail_closed_per_source() {
        let data = TempDir::new().unwrap();
        let daemon = Daemon::open(data.path()).unwrap();
        let source: IpAddr = "192.0.2.10".parse().unwrap();
        let mut guards = (0..MAX_CONNECTIONS_PER_SOURCE)
            .map(|_| daemon.connection_guard(source).unwrap())
            .collect::<Vec<_>>();
        assert!(daemon.connection_guard(source).is_none());
        guards.pop();
        assert!(daemon.connection_guard(source).is_some());

        let room = daemon.create_genesis("dial bounds").unwrap();
        let mut dials = (0..8)
            .map(|_| daemon.dial_guard(room).unwrap())
            .collect::<Vec<_>>();
        assert!(daemon.dial_guard(room).is_none());
        dials.pop();
        assert!(daemon.dial_guard(room).is_some());

        for _ in 0..AUTH_FAILURE_BURST {
            daemon.record_auth_failure(source);
        }
        assert!(!daemon.auth_allowed(source));
    }

    #[test]
    fn repeated_stalled_sync_triggers_are_deduplicated_per_room() {
        let data = TempDir::new().unwrap();
        let daemon = Daemon::open(data.path()).unwrap();
        let room = daemon.create_genesis("sync dedupe").unwrap();
        let first = daemon
            .mark_syncing(room)
            .expect("first sync owns the marker");
        for _ in 0..128 {
            assert!(daemon.mark_syncing(room).is_none());
        }
        assert_eq!(daemon.inner.syncing.read().unwrap().len(), 1);
        drop(first);
        assert!(daemon.mark_syncing(room).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn steady_state_frame_read_expires_at_the_shared_io_deadline() {
        let (mut reader, _stalled_writer) = tokio::io::duplex(64);
        let read = tokio::spawn(async move { read_message(&mut reader).await });
        tokio::time::advance(IO_TIMEOUT + Duration::from_millis(1)).await;
        let error = read.await.unwrap().unwrap_err();
        assert!(matches!(error, DaemonError::Protocol("read timed out")));
    }

    #[test]
    fn signed_handshake_kind_and_identity_fields_are_mandatory() {
        let data = TempDir::new().unwrap();
        let daemon = Daemon::open(data.path()).unwrap();
        let mut hello = daemon.hello_i();
        hello.kind = "hello_r".into();
        hello.sig = SignatureBytes::from_bytes(sign(
            &daemon.inner.node_key,
            &signed_object_digest(&serde_json::to_value(&hello).unwrap()),
        ));
        assert!(validate_hello_i(&hello).is_err());

        let mut hello = daemon.hello_i();
        hello.label = "other-protocol".into();
        hello.sig = SignatureBytes::from_bytes(sign(
            &daemon.inner.node_key,
            &signed_object_digest(&serde_json::to_value(&hello).unwrap()),
        ));
        assert!(validate_hello_i(&hello).is_err());

        let mut hello = daemon.hello_i();
        hello.v = 2;
        hello.sig = SignatureBytes::from_bytes(sign(
            &daemon.inner.node_key,
            &signed_object_digest(&serde_json::to_value(&hello).unwrap()),
        ));
        assert!(validate_hello_i(&hello).is_err());
    }

    #[test]
    fn handshake_tamper_replay_and_expected_peer_matrix_is_rejected() {
        let left_data = TempDir::new().unwrap();
        let right_data = TempDir::new().unwrap();
        let substitute_data = TempDir::new().unwrap();
        let left = Daemon::open(left_data.path()).unwrap();
        let right = Daemon::open(right_data.path()).unwrap();
        let substitute = Daemon::open(substitute_data.path()).unwrap();
        let hello_i = left.hello_i();
        let hello_r = right.hello_r(&hello_i);
        let hello_ack = left.hello_ack(&hello_i, &hello_r);
        assert!(validate_hello_i(&hello_i).is_ok());
        assert!(validate_hello_r(&hello_i, &hello_r, Some(right.node_id())).is_ok());
        assert!(validate_hello_ack(&hello_i, &hello_r, &hello_ack).is_ok());

        let resign_r = |hello: &mut HelloR| {
            hello.sig = SignatureBytes::from_bytes(sign(
                &right.inner.node_key,
                &signed_object_digest(&serde_json::to_value(&*hello).unwrap()),
            ));
        };
        let mut tampered = hello_r.clone();
        tampered.peer = substitute.node_id();
        resign_r(&mut tampered);
        assert!(validate_hello_r(&hello_i, &tampered, Some(right.node_id())).is_err());
        let response_tampers: [fn(&mut HelloR); 2] = [
            |hello| hello.nonce_i = Hash32::from_bytes([1; 32]),
            |hello| hello.hello_i_hash = Hash32::from_bytes([2; 32]),
        ];
        for mutate in response_tampers {
            let mut tampered = hello_r.clone();
            mutate(&mut tampered);
            resign_r(&mut tampered);
            assert!(validate_hello_r(&hello_i, &tampered, Some(right.node_id())).is_err());
        }
        let substituted = substitute.hello_r(&hello_i);
        assert!(validate_hello_r(&hello_i, &substituted, Some(right.node_id())).is_err());

        let resign_ack = |hello: &mut HelloAck| {
            hello.sig = SignatureBytes::from_bytes(sign(
                &left.inner.node_key,
                &signed_object_digest(&serde_json::to_value(&*hello).unwrap()),
            ));
        };
        let mut tampered = hello_ack.clone();
        tampered.peer = substitute.node_id();
        resign_ack(&mut tampered);
        assert!(validate_hello_ack(&hello_i, &hello_r, &tampered).is_err());
        let ack_tampers: [fn(&mut HelloAck); 4] = [
            |hello| hello.nonce_i = Hash32::from_bytes([3; 32]),
            |hello| hello.nonce_r = Hash32::from_bytes([4; 32]),
            |hello| hello.hello_i_hash = Hash32::from_bytes([5; 32]),
            |hello| hello.hello_r_hash = Hash32::from_bytes([6; 32]),
        ];
        for mutate in ack_tampers {
            let mut tampered = hello_ack.clone();
            mutate(&mut tampered);
            resign_ack(&mut tampered);
            assert!(validate_hello_ack(&hello_i, &hello_r, &tampered).is_err());
        }

        let fresh_i = left.hello_i();
        assert!(validate_hello_r(&fresh_i, &hello_r, Some(right.node_id())).is_err());
        let fresh_r = right.hello_r(&hello_i);
        assert!(validate_hello_ack(&hello_i, &fresh_r, &hello_ack).is_err());
    }

    fn public_tls_client() -> Arc<ClientConfig> {
        let roots = tokio_rustls::rustls::RootCertStore::empty();
        let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
        Arc::new(
            ClientConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&tokio_rustls::rustls::version::TLS13])
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }

    #[tokio::test]
    async fn public_mode_refuses_open_rooms() {
        let loaded = TempDir::new().unwrap();
        let daemon = Daemon::open(loaded.path()).unwrap();
        daemon.create_genesis("public open room").unwrap();
        let error = daemon
            .configure_transport(TransportMode::Public, Some(public_tls_client()))
            .unwrap_err();
        assert!(matches!(error, DaemonError::OpenRoomInPublic));

        let fresh = TempDir::new().unwrap();
        let daemon = Daemon::open(fresh.path()).unwrap();
        daemon
            .configure_transport(TransportMode::Public, Some(public_tls_client()))
            .unwrap();
        assert!(matches!(
            daemon.create_genesis("created open").unwrap_err(),
            DaemonError::OpenRoomInPublic
        ));
        let token = Hash32::from_bytes([9; 32]);
        daemon
            .create_ticket_with_token(
                "private public room",
                StakePolicy::default(),
                FloorConfig::stick(30),
                Some(token),
            )
            .unwrap();

        let forced = TempDir::new().unwrap();
        let peer_data = TempDir::new().unwrap();
        let target = Daemon::open(forced.path()).unwrap();
        let peer = Daemon::open(peer_data.path()).unwrap();
        let room = target.create_genesis("forced open").unwrap();
        target.set_transport_for_test(TransportMode::Public, Some(public_tls_client()));
        assert!(!target.authorize_room_token(room, None).unwrap());
        let declaration = Declaration::signed(
            room,
            JoinRole::Stake,
            vec![AgentId::new("agent:public-outsider").unwrap()],
            unix_timestamp(),
            &peer.inner.node_key,
        );
        target
            .admit_declared_staker(peer.node_id(), &declaration)
            .await
            .unwrap();
        assert_eq!(
            target.replay(room).unwrap().chain.roster,
            [target.node_id()]
        );
    }

    #[tokio::test]
    async fn simultaneous_hello_i_elects_lexicographically_smaller_responder() {
        let left_data = TempDir::new().unwrap();
        let right_data = TempDir::new().unwrap();
        let left = Daemon::open(left_data.path()).unwrap();
        let right = Daemon::open(right_data.path()).unwrap();
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let left_handshake = {
            let left = left.clone();
            let expected = right.node_id();
            tokio::spawn(async move { left.initiate_handshake(&mut a, Some(expected)).await })
        };
        let right_handshake = {
            let right = right.clone();
            let expected = left.node_id();
            tokio::spawn(async move { right.initiate_handshake(&mut b, Some(expected)).await })
        };
        let left_peer = left_handshake.await.unwrap().expect("left handshake");
        let right_peer = right_handshake.await.unwrap().expect("right handshake");
        assert_eq!(left_peer.node, right.node_id());
        assert_eq!(right_peer.node, left.node_id());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn room_auth_transition_rejects_pre_auth_pex_sender_mismatch_and_conflict() {
        let target_data = TempDir::new().unwrap();
        let initiator_data = TempDir::new().unwrap();
        let substitute_data = TempDir::new().unwrap();
        let target = Daemon::open(target_data.path()).unwrap();
        let initiator = Daemon::open(initiator_data.path()).unwrap();
        let substitute = Daemon::open(substitute_data.path()).unwrap();
        let server = target.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let ticket = target
            .create_ticket(
                "room auth transitions",
                StakePolicy::default(),
                FloorConfig::stick(30),
            )
            .unwrap();
        initiator
            .join_ticket(ticket.clone(), JoinRole::Observe)
            .await
            .unwrap();
        assert_eq!(ticket.peers, [format!("tcp://{}", server.addr())]);

        let fake = PeerInfo {
            node: substitute.node_id(),
            addrs: vec!["tcp://127.0.0.1:9".into()],
        };
        let (mut outbound, inbound) = tokio::io::duplex(1024 * 1024);
        let preauth_target = target.clone();
        let preauth = tokio::spawn(async move {
            preauth_target
                .handle_transport_with_source(
                    inbound,
                    ConnectionProtocol::Swarm,
                    "192.0.2.20".parse().unwrap(),
                )
                .await
        });
        initiator
            .initiate_handshake(&mut outbound, Some(target.node_id()))
            .await
            .unwrap();
        write_message(
            &mut outbound,
            &SwarmMsg::Pex(Pex {
                room: ticket.id,
                peers: vec![fake.clone()],
            }),
        )
        .await
        .unwrap();
        drop(outbound);
        assert!(preauth.await.unwrap().is_err());
        assert!(!target
            .inner
            .peers
            .read()
            .unwrap()
            .get(&ticket.id)
            .is_some_and(|peers| peers.contains_key(&fake.node)));

        let before = target.replay(ticket.id).unwrap();
        let (mut outbound, inbound) = tokio::io::duplex(1024 * 1024);
        let auth_target = target.clone();
        let auth_task = tokio::spawn(async move {
            auth_target
                .handle_transport_with_source(
                    inbound,
                    ConnectionProtocol::Swarm,
                    "192.0.2.21".parse().unwrap(),
                )
                .await
        });
        initiator
            .initiate_handshake(&mut outbound, Some(target.node_id()))
            .await
            .unwrap();
        let auth = Auth {
            room: ticket.id,
            token: None,
            declaration: initiator.declaration(ticket.id).unwrap(),
        };
        write_message(&mut outbound, &SwarmMsg::Auth(auth.clone()))
            .await
            .unwrap();
        assert!(matches!(
            read_message(&mut outbound).await.unwrap(),
            Some(SwarmMsg::Authed(_))
        ));
        assert!(matches!(
            read_message(&mut outbound).await.unwrap(),
            Some(SwarmMsg::Pex(_))
        ));
        assert!(matches!(
            read_message(&mut outbound).await.unwrap(),
            Some(SwarmMsg::Have(_))
        ));

        write_message(
            &mut outbound,
            &SwarmMsg::RequestVote(RequestVote {
                room: ticket.id,
                rpc_term: before.consensus.current_term + 100,
                candidate: substitute.node_id(),
                last_n: before.chain.head_n.unwrap(),
                last_hash: before.chain.head_hash.unwrap(),
                last_rpc: before.head_proof.as_ref().unwrap().rpc_term,
                sig: SignatureBytes::from_bytes([0; 64]),
            }),
        )
        .await
        .unwrap();
        write_message(&mut outbound, &SwarmMsg::Auth(auth.clone()))
            .await
            .unwrap();
        assert!(matches!(
            read_message(&mut outbound).await.unwrap(),
            Some(SwarmMsg::Authed(_))
        ));

        let mut conflicting = auth;
        conflicting.declaration = Declaration::signed(
            ticket.id,
            JoinRole::Observe,
            vec![AgentId::new("agent:changed").unwrap()],
            unix_timestamp(),
            &initiator.inner.node_key,
        );
        write_message(&mut outbound, &SwarmMsg::Auth(conflicting))
            .await
            .unwrap();
        drop(outbound);
        assert!(auth_task.await.unwrap().is_err());
        let after = target.replay(ticket.id).unwrap();
        assert_eq!(after.consensus, before.consensus);
        assert_eq!(after.pending, before.pending);
        assert_eq!(after.chain.head_hash, before.chain.head_hash);
    }

    #[tokio::test]
    async fn node_handshake_discloses_no_room_data_class_before_authorization() {
        let target_data = TempDir::new().unwrap();
        let initiator_data = TempDir::new().unwrap();
        let target = Daemon::open(target_data.path()).unwrap();
        let initiator = Daemon::open(initiator_data.path()).unwrap();
        let token = Hash32::from_bytes([0xa5; 32]);
        let ticket = target
            .create_ticket_with_token(
                "preauth-sentinel-room",
                StakePolicy::default(),
                FloorConfig::stick(30),
                Some(token),
            )
            .unwrap();
        target.remember_room_agent(ticket.id, AgentId::new("agent:preauth-sentinel").unwrap());

        let (mut outbound, inbound) = tokio::io::duplex(1024 * 1024);
        let responder = target.clone();
        let task = tokio::spawn(async move {
            responder
                .handle_transport_with_source(
                    inbound,
                    ConnectionProtocol::Swarm,
                    "192.0.2.30".parse().unwrap(),
                )
                .await
        });
        initiator
            .initiate_handshake(&mut outbound, Some(target.node_id()))
            .await
            .unwrap();

        assert!(
            timeout(Duration::from_millis(100), read_message(&mut outbound))
                .await
                .is_err()
        );
        assert!(!target.inner.peers.read().unwrap().contains_key(&ticket.id));
        assert_eq!(target.replay(ticket.id).unwrap().chain.head_n, Some(0));
        drop(outbound);
        assert!(task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn authenticated_protocol_errors_do_not_poison_the_source_auth_bucket() {
        let target_data = TempDir::new().unwrap();
        let peer_data = TempDir::new().unwrap();
        let target = Daemon::open(target_data.path()).unwrap();
        let peer = Daemon::open(peer_data.path()).unwrap();
        let server = target.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let ticket = target
            .create_ticket(
                "postauth bucket",
                StakePolicy::default(),
                FloorConfig::stick(30),
            )
            .unwrap();
        peer.join_ticket(ticket.clone(), JoinRole::Observe)
            .await
            .unwrap();
        let source: IpAddr = "192.0.2.40".parse().unwrap();

        for _ in 0..AUTH_FAILURE_BURST + 2 {
            let (mut outbound, inbound) = tokio::io::duplex(1024 * 1024);
            let responder = target.clone();
            let task = tokio::spawn(async move {
                responder
                    .handle_transport_with_source(inbound, ConnectionProtocol::Swarm, source)
                    .await
            });
            peer.initiate_handshake(&mut outbound, Some(target.node_id()))
                .await
                .unwrap();
            write_message(
                &mut outbound,
                &SwarmMsg::Auth(Auth {
                    room: ticket.id,
                    token: None,
                    declaration: peer.declaration(ticket.id).unwrap(),
                }),
            )
            .await
            .unwrap();
            assert!(matches!(
                read_message(&mut outbound).await.unwrap(),
                Some(SwarmMsg::Authed(_))
            ));
            assert!(matches!(
                read_message(&mut outbound).await.unwrap(),
                Some(SwarmMsg::Pex(_))
            ));
            assert!(matches!(
                read_message(&mut outbound).await.unwrap(),
                Some(SwarmMsg::Have(_))
            ));
            write_message(&mut outbound, &SwarmMsg::HelloI(peer.hello_i()))
                .await
                .unwrap();
            drop(outbound);
            assert!(task.await.unwrap().is_err());
        }
        assert!(target.auth_allowed(source));
        drop(server);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 6)]
    async fn replayed_complete_handshake_cannot_forge_a_three_node_quorum() {
        let target_data = TempDir::new().unwrap();
        let second_data = TempDir::new().unwrap();
        let third_data = TempDir::new().unwrap();
        let target = Daemon::open(target_data.path()).unwrap();
        let second = Daemon::open(second_data.path()).unwrap();
        let third = Daemon::open(third_data.path()).unwrap();
        let _second_server = second.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let _third_server = third.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let _server = target.start("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let ticket = target
            .create_ticket(
                "replayed quorum",
                StakePolicy::default(),
                FloorConfig::stick(30),
            )
            .unwrap();
        second
            .join_ticket(ticket.clone(), JoinRole::Stake)
            .await
            .unwrap();
        target.ensure_network_leader(ticket.id).await.unwrap();
        third
            .join_ticket(ticket.clone(), JoinRole::Stake)
            .await
            .unwrap();
        target
            .admit_declared_staker(third.node_id(), &third.declaration(ticket.id).unwrap())
            .await
            .unwrap();
        target.ensure_network_leader(ticket.id).await.unwrap();
        timeout(Duration::from_secs(3), async {
            while target.replay(ticket.id).unwrap().chain.roster.len() != 3 {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(target.replay(ticket.id).unwrap().chain.roster.len(), 3);
        let before = target.replay(ticket.id).unwrap();

        let hello_i = second.hello_i();
        let (mut attacker, inbound) = tokio::io::duplex(1024 * 1024);
        let first_target = target.clone();
        let first = tokio::spawn(async move {
            first_target
                .handle_transport_with_source(
                    inbound,
                    ConnectionProtocol::Swarm,
                    "192.0.2.30".parse().unwrap(),
                )
                .await
        });
        write_message(&mut attacker, &SwarmMsg::HelloI(hello_i.clone()))
            .await
            .unwrap();
        let Some(SwarmMsg::HelloR(hello_r)) = read_message(&mut attacker).await.unwrap() else {
            panic!("responder must send hello_r");
        };
        let hello_ack = second.hello_ack(&hello_i, &hello_r);
        write_message(&mut attacker, &SwarmMsg::HelloAck(hello_ack.clone()))
            .await
            .unwrap();
        drop(attacker);
        assert!(first.await.unwrap().is_ok());

        let (mut replay, inbound) = tokio::io::duplex(1024 * 1024);
        let replay_target = target.clone();
        let replayed = tokio::spawn(async move {
            replay_target
                .handle_transport_with_source(
                    inbound,
                    ConnectionProtocol::Swarm,
                    "192.0.2.31".parse().unwrap(),
                )
                .await
        });
        write_message(&mut replay, &SwarmMsg::HelloI(hello_i))
            .await
            .unwrap();
        let Some(SwarmMsg::HelloR(fresh_r)) = read_message(&mut replay).await.unwrap() else {
            panic!("responder must send a fresh hello_r");
        };
        assert_ne!(fresh_r.nonce_r, hello_r.nonce_r);
        write_message(&mut replay, &SwarmMsg::HelloAck(hello_ack))
            .await
            .unwrap();
        drop(replay);
        assert!(replayed.await.unwrap().is_err());

        let after = target.replay(ticket.id).unwrap();
        assert_eq!(after.consensus, before.consensus);
        assert_eq!(after.pending, before.pending);
        assert_eq!(after.chain.head_hash, before.chain.head_hash);
    }

    #[test]
    fn browser_sessions_expire_revoke_restart_and_remain_bounded() {
        let data = TempDir::new().unwrap();
        let daemon = Daemon::open(data.path()).unwrap();
        let token = Hash32::from_bytes([61; 32]);
        let ticket = daemon
            .create_ticket_with_token(
                "session lifecycle",
                StakePolicy::default(),
                FloorConfig::stick(30),
                Some(token),
            )
            .unwrap();
        let origin = "http://127.0.0.1:7420".to_owned();

        let revoked = daemon
            .create_browser_session(ticket.id, origin.clone(), token)
            .unwrap();
        assert_eq!(
            daemon.validate_browser_session(&revoked, Some(ticket.id), &origin),
            Some(ticket.id)
        );
        daemon.revoke_browser_session(&revoked);
        assert!(daemon
            .validate_browser_session(&revoked, Some(ticket.id), &origin)
            .is_none());

        let expired = daemon
            .create_browser_session(ticket.id, origin.clone(), token)
            .unwrap();
        daemon.inner.browser_sessions.lock().unwrap()[0].expires = 0;
        assert!(daemon
            .validate_browser_session(&expired, Some(ticket.id), &origin)
            .is_none());

        let before_restart = daemon
            .create_browser_session(ticket.id, origin.clone(), token)
            .unwrap();
        let restarted = Daemon::open(data.path()).unwrap();
        assert!(restarted
            .validate_browser_session(&before_restart, Some(ticket.id), &origin)
            .is_none());

        for _ in 0..4_100 {
            daemon
                .create_browser_session(ticket.id, origin.clone(), token)
                .unwrap();
        }
        assert_eq!(daemon.inner.browser_sessions.lock().unwrap().len(), 4_096);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn leader_removes_only_one_peer_after_verified_last_seen_expires() {
        use tempfile::TempDir;

        let first_data = TempDir::new().unwrap();
        let second_data = TempDir::new().unwrap();
        let third_data = TempDir::new().unwrap();
        let first = Daemon::open(first_data.path()).unwrap();
        let second = Daemon::open(second_data.path()).unwrap();
        let third = Daemon::open(third_data.path()).unwrap();
        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let first_server = first.start(bind).await.unwrap();
        let second_server = second.start(bind).await.unwrap();
        let third_server = third.start(bind).await.unwrap();
        let first_endpoint = format!("tcp://{}", first_server.addr());
        first.advertise(&first_endpoint).unwrap();
        let second_endpoint = format!("tcp://{}", second_server.addr());
        second.advertise(&second_endpoint).unwrap();
        let third_endpoint = format!("tcp://{}", third_server.addr());
        third.advertise(&third_endpoint).unwrap();
        let ticket = first
            .create_ticket(
                "last seen removal",
                StakePolicy::default(),
                FloorConfig::stick(30),
            )
            .unwrap();
        second
            .join_ticket(ticket.clone(), JoinRole::Stake)
            .await
            .unwrap();
        assert_eq!(
            first.peer_endpoints(ticket.id, second.node_id()),
            vec![second_endpoint]
        );
        third
            .join_ticket(ticket.clone(), JoinRole::Stake)
            .await
            .unwrap();
        first
            .inner
            .last_seen
            .write()
            .unwrap()
            .entry(ticket.id)
            .or_default()
            .insert(third.node_id(), 0);

        first.maybe_remove_unavailable(ticket.id).await.unwrap();
        let replay = first.replay(ticket.id).unwrap();
        assert_eq!(replay.chain.roster.len(), 2);
        assert!(!replay.chain.roster.contains(&third.node_id()));
        assert!(replay.chain.roster.contains(&second.node_id()));
    }
}
