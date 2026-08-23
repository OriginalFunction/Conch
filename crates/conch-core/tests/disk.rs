use std::fs;

use conch_core::{
    disk::Store,
    encoding::{cert_digest, scene_hash, sign},
    types::{
        AgentId, Body, Cert, ChainState, CommitProof, ConsensusState, FloorConfig, Hash32, Intent,
        IntentKind, NodeId, Pending, RoomId, Scene, SignatureBytes, StakePolicy,
    },
};
use ed25519_dalek::SigningKey;
use tempfile::TempDir;

struct Fixture {
    room_key: SigningKey,
    node_key: SigningKey,
}

impl Fixture {
    fn new() -> Self {
        Self {
            room_key: SigningKey::from_bytes(&[11; 32]),
            node_key: SigningKey::from_bytes(&[12; 32]),
        }
    }

    fn room(&self) -> RoomId {
        RoomId::from_bytes(self.room_key.verifying_key().to_bytes())
    }

    fn node(&self) -> NodeId {
        NodeId::from_bytes(self.node_key.verifying_key().to_bytes())
    }

    fn genesis(&self) -> Scene {
        Scene {
            v: 1,
            room: self.room(),
            n: 0,
            term: 1,
            parent: None,
            roster: vec![self.node()],
            leader: self.node(),
            ts: 1_766_700_000,
            body: Body::Genesis {
                name: "disk test".into(),
                stake: StakePolicy::default(),
                floor: FloorConfig::stick(30),
                creator_node: self.node(),
                parent_room: None,
                token_sha256: None,
            },
            certs: Vec::new(),
        }
    }

    fn hash(&self, scene: &Scene) -> Hash32 {
        Hash32::from_bytes(scene_hash(&serde_json::to_value(scene).unwrap()))
    }

    fn proof(&self, scene: &Scene) -> CommitProof {
        let hash = self.hash(scene);
        let digest = cert_digest(
            &scene.room,
            scene.n,
            hash.as_bytes(),
            1,
            &self.node(),
            &self.node(),
        );
        CommitProof {
            rpc_term: 1,
            leader: self.node(),
            certs: vec![
                Cert::node(
                    self.node(),
                    SignatureBytes::from_bytes(sign(&self.node_key, &digest)),
                ),
                Cert::room(SignatureBytes::from_bytes(sign(
                    &self.room_key,
                    hash.as_bytes(),
                ))),
            ],
        }
    }

    fn pending(&self, scene: &Scene, proof: &CommitProof) -> Pending {
        Pending {
            n: scene.n,
            hash: self.hash(scene),
            scene: scene.clone(),
            accepted_rpc_term: proof.rpc_term,
            accepted_leader: proof.leader,
            cert: proof.certs[0].clone(),
        }
    }
}

fn room_store(temp: &TempDir, fixture: &Fixture) -> Store {
    Store::open(temp.path().join("rooms").join(fixture.room().to_string())).unwrap()
}

#[test]
fn durable_commit_scene_is_recoverable_before_pending_is_unlinked() {
    let temp = TempDir::new().unwrap();
    let fixture = Fixture::new();
    let store = room_store(&temp, &fixture);
    let scene = fixture.genesis();
    let proof = fixture.proof(&scene);
    store
        .write_pending(&fixture.pending(&scene, &proof))
        .unwrap();

    store
        .persist_committed_scene(&ChainState::empty(), &scene, &proof)
        .unwrap();
    assert!(store.scene_path(scene.n, fixture.hash(&scene)).exists());
    assert!(store.root().join("pending.json").exists());

    // Crash after the durable scene/proof but before pending unlink. The head
    // cache is deliberately lost too; scenes/ remains the source of truth.
    fs::remove_file(store.root().join("head")).unwrap();
    drop(store);

    let reopened = room_store(&temp, &fixture);
    let replay = reopened.load_replay().unwrap();
    assert_eq!(replay.chain.head_n, Some(0));
    assert!(replay.pending.is_none());
    assert!(!reopened.root().join("pending.json").exists());
}

#[test]
fn torn_scene_file_is_treated_as_absent() {
    let temp = TempDir::new().unwrap();
    let fixture = Fixture::new();
    let store = room_store(&temp, &fixture);
    let torn = store
        .root()
        .join("scenes")
        .join(format!("0-{}.json", "00".repeat(32)));
    fs::File::create(torn).unwrap();

    let replay = store.load_replay().unwrap();
    assert!(replay.chain.is_empty());
}

#[test]
fn pending_reloads_after_cert_before_commit() {
    let temp = TempDir::new().unwrap();
    let fixture = Fixture::new();
    let store = room_store(&temp, &fixture);
    let scene = fixture.genesis();
    let proof = fixture.proof(&scene);
    let pending = fixture.pending(&scene, &proof);
    store.write_pending(&pending).unwrap();
    drop(store);

    let replay = room_store(&temp, &fixture).load_replay().unwrap();
    assert_eq!(replay.pending, Some(pending));
    assert_eq!(replay.consensus.current_term, 1);
}

#[test]
fn pending_with_a_relabelled_accept_term_is_rejected() {
    let temp = TempDir::new().unwrap();
    let fixture = Fixture::new();
    let store = room_store(&temp, &fixture);
    let scene = fixture.genesis();
    let proof = fixture.proof(&scene);
    let mut pending = fixture.pending(&scene, &proof);
    pending.accepted_rpc_term = 2;

    assert!(store.write_pending(&pending).is_err());
}

#[test]
fn durable_commit_writes_scene_head_and_clears_pending() {
    let temp = TempDir::new().unwrap();
    let fixture = Fixture::new();
    let store = room_store(&temp, &fixture);
    let scene = fixture.genesis();
    let proof = fixture.proof(&scene);
    store.write_consensus(&ConsensusState::default()).unwrap();
    store
        .write_pending(&fixture.pending(&scene, &proof))
        .unwrap();

    let next = store.durable_commit(&scene, &proof).unwrap();

    assert_eq!(next.head_hash, Some(fixture.hash(&scene)));
    assert!(!store.root().join("pending.json").exists());
    let replay = store.load_replay().unwrap();
    assert_eq!(replay.chain, next);
    assert_eq!(replay.head_proof, Some(proof));
}

#[test]
fn intents_are_durable_objects_not_process_local_waiters() {
    let temp = TempDir::new().unwrap();
    let fixture = Fixture::new();
    let store = room_store(&temp, &fixture);
    let intent = Intent {
        v: 1,
        id: Hash32::from_bytes([8; 32]),
        room: fixture.room(),
        kind: IntentKind::Wait,
        agent: AgentId::new("codex").unwrap(),
        node: fixture.node(),
        ts: 10,
        exp: 20,
        sig: SignatureBytes::from_bytes([9; 64]),
    };

    store.write_intent(&intent).unwrap();
    assert_eq!(store.load_intents().unwrap(), vec![intent.clone()]);
    store.remove_intent(intent.id).unwrap();
    assert!(store.load_intents().unwrap().is_empty());
}
