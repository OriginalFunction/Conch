use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use conch_core::{
    consensus::{GetScenes, HaveMessage, Hello, SwarmMsg},
    disk::{Replay, Store, StoreError},
    encoding::{cert_digest, scene_hash, sign},
    frame::{self, FrameError, MAX_FRAME_BYTES},
    types::{
        Body, Cert, ChainState, CommitProof, CommittedScene, FloorConfig, Hash32, NodeId, RoomId,
        Scene, SignatureBytes, StakePolicy,
    },
};
use ed25519_dalek::SigningKey;
use rand::random;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
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
    Frame(#[from] FrameError),
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
        let mut rooms = BTreeMap::new();
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
                rooms.insert(room, Store::open(entry.path())?);
            }
        }
        Ok(Self {
            inner: Arc::new(Inner {
                data_dir,
                node_key,
                rooms: RwLock::new(rooms),
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
        self.inner
            .rooms
            .write()
            .expect("room registry lock is not poisoned")
            .insert(room, store);
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
        let hello = read_message(&mut stream)
            .await?
            .ok_or(DaemonError::Protocol("peer closed before hello"))?;
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
                SwarmMsg::Have(_) | SwarmMsg::Auth(_) => {}
                _ => {}
            }
        }
        Ok(())
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
        store.durable_commit(&record.scene, &record.commit_proof)?;
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

    fn room_path(&self, room: RoomId) -> PathBuf {
        self.inner.data_dir.join("rooms").join(room.to_string())
    }
}

pub async fn write_message<W>(writer: &mut W, message: &SwarmMsg) -> Result<(), DaemonError>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(&frame::encode(message)?).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_message<R>(reader: &mut R) -> Result<Option<SwarmMsg>, DaemonError>
where
    R: AsyncRead + Unpin,
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

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
