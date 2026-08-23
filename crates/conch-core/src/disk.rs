use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
};

use ed25519_dalek::VerifyingKey;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;

use crate::{
    apply::{apply, ApplyError, ApplyMode, ApplyResources},
    consensus::{advance_term, AdvanceSource},
    encoding::{cert_digest, scene_hash, verify},
    types::{
        CertSigner, ChainState, CommitProof, CommittedScene, ConsensusState, Hash32, Pending, Scene,
    },
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("disk I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("stored JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("scene failed apply(): {0}")]
    Apply(#[from] ApplyError),
    #[error("pending.json does not match its scene or accepted cert")]
    InvalidPending,
    #[error("committed scene filename does not match its contents")]
    InvalidSceneFile,
    #[error("two committed scene files conflict at height {0}")]
    ConflictingScenes(u64),
    #[error("committed prefix has a hole at height {0}")]
    SickHole(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    pub chain: ChainState,
    pub consensus: ConsensusState,
    pub pending: Option<Pending>,
    pub head_proof: Option<CommitProof>,
    pub history: Vec<CommittedScene>,
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        fs::create_dir_all(root.join("scenes"))?;
        fs::create_dir_all(root.join("blobs"))?;
        sync_dir(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn scene_path(&self, n: u64, hash: Hash32) -> PathBuf {
        self.root.join("scenes").join(format!("{n}-{hash}.json"))
    }

    pub fn write_consensus(&self, state: &ConsensusState) -> Result<(), StoreError> {
        write_json_atomic(&self.root.join("consensus.json"), state)?;
        Ok(())
    }

    pub fn write_pending(&self, pending: &Pending) -> Result<(), StoreError> {
        if !valid_pending(pending) {
            return Err(StoreError::InvalidPending);
        }
        write_json_atomic(&self.root.join("pending.json"), pending)?;
        Ok(())
    }

    /// Durable commit steps 1-2: validate, fsync scene+proof and its directory,
    /// then fsync the head cache. `pending.json` deliberately remains intact.
    pub fn persist_committed_scene(
        &self,
        state: &ChainState,
        scene: &Scene,
        proof: &CommitProof,
    ) -> Result<ChainState, StoreError> {
        let resources = self.load_blob_inventory()?;
        let next = apply(state, scene, Some(proof), ApplyMode::Commit(&resources))?;
        let hash = next
            .head_hash
            .expect("a successful commit always produces a head hash");
        self.reject_conflicting_scene(scene.n, hash)?;

        let stored = CommittedScene {
            scene: scene.clone(),
            commit_proof: proof.clone(),
        };
        write_json_atomic(&self.scene_path(scene.n, hash), &stored)?;
        write_json_atomic(&self.root.join("head"), &HeadCache { n: scene.n, hash })?;
        Ok(next)
    }

    /// Complete the normative durable order, including stale-pending unlink.
    pub fn durable_commit(
        &self,
        scene: &Scene,
        proof: &CommitProof,
    ) -> Result<ChainState, StoreError> {
        let replay = self.load_replay()?;
        let next = self.persist_committed_scene(&replay.chain, scene, proof)?;
        self.unlink_pending_if_stale(next.head_n)?;
        Ok(next)
    }

    pub fn unlink_pending_if_stale(&self, head_n: Option<u64>) -> Result<(), StoreError> {
        let Some(head_n) = head_n else {
            return Ok(());
        };
        let path = self.root.join("pending.json");
        let pending: Option<Pending> = read_optional_json(&path)?;
        if pending.is_some_and(|pending| pending.n <= head_n) {
            fs::remove_file(path)?;
            sync_dir(&self.root)?;
        }
        Ok(())
    }

    pub fn load_replay(&self) -> Result<Replay, StoreError> {
        let mut consensus: ConsensusState =
            read_optional_json(&self.root.join("consensus.json"))?.unwrap_or_default();
        let mut pending: Option<Pending> = read_optional_json(&self.root.join("pending.json"))?;
        if pending
            .as_ref()
            .is_some_and(|pending| !valid_pending(pending))
        {
            return Err(StoreError::InvalidPending);
        }
        let head_cache: Option<HeadCache> = read_optional_json_lossy(&self.root.join("head"));
        let resources = self.load_blob_inventory()?;
        let mut candidates = self.scan_scene_files()?;

        let mut chain = ChainState::empty();
        let mut head_proof = None;
        let mut history = Vec::new();
        let mut expected_n = 0_u64;
        while let Some(at_height) = candidates.remove(&expected_n) {
            let mut accepted: Option<(ChainState, CommittedScene, Hash32)> = None;
            for stored in at_height {
                let hash = Hash32::from_bytes(scene_hash(
                    &serde_json::to_value(&stored.scene).expect("typed scene is serializable"),
                ));
                // A candidate that cannot pass deterministic replay is treated
                // as absent and re-leechable, like a torn/unreadable file.
                let Ok(next) = apply(
                    &chain,
                    &stored.scene,
                    Some(&stored.commit_proof),
                    ApplyMode::Commit(&resources),
                ) else {
                    continue;
                };
                if let Some((_, _, prior_hash)) = &accepted {
                    if *prior_hash != hash {
                        return Err(StoreError::ConflictingScenes(expected_n));
                    }
                } else {
                    accepted = Some((next, stored, hash));
                }
            }
            let Some((next, stored, _)) = accepted else {
                break;
            };
            chain = next;
            head_proof = Some(stored.commit_proof.clone());
            history.push(stored);
            expected_n += 1;
        }

        if head_cache.as_ref().is_some_and(|head| expected_n <= head.n) {
            return Err(StoreError::SickHole(expected_n));
        }

        self.unlink_pending_if_stale(chain.head_n)?;
        if chain
            .head_n
            .is_some_and(|head_n| pending.as_ref().is_some_and(|pending| pending.n <= head_n))
        {
            pending = None;
        }

        let recovered_term = [
            pending.as_ref().map(|pending| pending.accepted_rpc_term),
            head_proof.as_ref().map(|proof| proof.rpc_term),
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(0);
        if advance_term(
            &mut consensus,
            pending.as_ref(),
            head_proof.as_ref(),
            AdvanceSource::RecoveredPersistentState(recovered_term),
        ) {
            self.write_consensus(&consensus)?;
        }

        Ok(Replay {
            chain,
            consensus,
            pending,
            head_proof,
            history,
        })
    }

    fn scan_scene_files(&self) -> Result<BTreeMap<u64, Vec<CommittedScene>>, StoreError> {
        let mut scenes: BTreeMap<u64, Vec<CommittedScene>> = BTreeMap::new();
        for entry in fs::read_dir(self.root.join("scenes"))? {
            let Ok(entry) = entry else {
                continue;
            };
            let Some((filename_n, filename_hash)) = parse_scene_filename(&entry.file_name()) else {
                continue;
            };
            let Ok(bytes) = fs::read(entry.path()) else {
                continue;
            };
            if bytes.is_empty() {
                continue;
            }
            let Ok(stored) = serde_json::from_slice::<CommittedScene>(&bytes) else {
                continue;
            };
            let actual_hash = Hash32::from_bytes(scene_hash(
                &serde_json::to_value(&stored.scene).expect("typed scene is serializable"),
            ));
            if stored.scene.n != filename_n || actual_hash != filename_hash {
                continue;
            }
            scenes.entry(filename_n).or_default().push(stored);
        }
        Ok(scenes)
    }

    fn reject_conflicting_scene(&self, n: u64, hash: Hash32) -> Result<(), StoreError> {
        if self.scan_scene_files()?.get(&n).is_some_and(|scenes| {
            scenes.iter().any(|stored| {
                Hash32::from_bytes(scene_hash(
                    &serde_json::to_value(&stored.scene).expect("typed scene is serializable"),
                )) != hash
            })
        }) {
            return Err(StoreError::ConflictingScenes(n));
        }
        Ok(())
    }

    fn load_blob_inventory(&self) -> Result<ApplyResources, StoreError> {
        let mut blobs = BTreeMap::new();
        for entry in fs::read_dir(self.root.join("blobs"))? {
            let Ok(entry) = entry else {
                continue;
            };
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(hash) = Hash32::from_str(&name) else {
                continue;
            };
            let Ok(bytes) = fs::read(entry.path()) else {
                continue;
            };
            blobs.insert(hash, bytes);
        }
        Ok(ApplyResources {
            intents: Vec::new(),
            blobs,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeadCache {
    n: u64,
    hash: Hash32,
}

fn parse_scene_filename(name: &std::ffi::OsStr) -> Option<(u64, Hash32)> {
    let name = name.to_str()?.strip_suffix(".json")?;
    let (n, hash) = name.split_once('-')?;
    Some((n.parse().ok()?, hash.parse().ok()?))
}

fn valid_pending(pending: &Pending) -> bool {
    let actual_hash = Hash32::from_bytes(scene_hash(
        &serde_json::to_value(&pending.scene).expect("typed scene is serializable"),
    ));
    let CertSigner::Node(cert_node) = pending.cert.node else {
        return false;
    };
    if pending.n != pending.scene.n
        || pending.hash != actual_hash
        || pending.accepted_rpc_term == 0
        || !pending.scene.roster.contains(&pending.accepted_leader)
        || !pending.scene.roster.contains(&cert_node)
    {
        return false;
    }
    let Ok(key) = VerifyingKey::from_bytes(cert_node.as_bytes()) else {
        return false;
    };
    let digest = cert_digest(
        &pending.scene.room,
        pending.n,
        pending.hash.as_bytes(),
        pending.accepted_rpc_term,
        &pending.accepted_leader,
        &cert_node,
    );
    verify(&key, &digest, pending.cert.sig.as_bytes())
}

fn read_optional_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_optional_json_lossy<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "persistent path has no parent")
    })?;
    fs::create_dir_all(parent)?;
    let suffix = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "persistent filename is not UTF-8",
            )
        })?;
    let temporary = parent.join(format!(".{filename}.tmp-{}-{suffix}", std::process::id()));
    let bytes = serde_json::to_vec(value)?;

    let result = (|| -> Result<(), StoreError> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        sync_dir(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}
