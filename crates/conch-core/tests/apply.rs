use std::collections::BTreeMap;

use conch_core::{
    apply::{apply, ApplyError, ApplyMode, ApplyResources},
    encoding::{cert_digest, scene_hash, sign, signed_object_digest},
    types::{
        AgentId, BlobRef, Body, Cert, ChainState, CommitProof, FloorConfig, FloorMode, GrantReason,
        Hash32, Intent, IntentKind, Mouth, NodeId, RoomId, Scene, SignatureBytes, StakePolicy,
    },
};
use ed25519_dalek::SigningKey;
use serde_json::Value;
use sha2::{Digest, Sha256};

struct Fixture {
    room_key: SigningKey,
    creator_key: SigningKey,
    other_key: SigningKey,
}

impl Fixture {
    fn new() -> Self {
        Self {
            room_key: SigningKey::from_bytes(&[1; 32]),
            creator_key: SigningKey::from_bytes(&[2; 32]),
            other_key: SigningKey::from_bytes(&[3; 32]),
        }
    }

    fn room(&self) -> RoomId {
        RoomId::from_bytes(self.room_key.verifying_key().to_bytes())
    }

    fn creator(&self) -> NodeId {
        NodeId::from_bytes(self.creator_key.verifying_key().to_bytes())
    }

    fn other(&self) -> NodeId {
        NodeId::from_bytes(self.other_key.verifying_key().to_bytes())
    }

    fn genesis(&self) -> Scene {
        Scene {
            v: 1,
            room: self.room(),
            n: 0,
            term: 1,
            parent: None,
            roster: vec![self.creator()],
            leader: self.creator(),
            ts: 1_766_700_000,
            body: Body::Genesis {
                name: "Conch test".into(),
                stake: StakePolicy::default(),
                floor: FloorConfig::stick(30),
                creator_node: self.creator(),
                parent_room: None,
                token_sha256: None,
            },
            certs: Vec::new(),
        }
    }

    fn proof(&self, scene: &Scene, leader: NodeId, signers: &[&SigningKey]) -> CommitProof {
        let hash = hash_scene(scene);
        let certs = signers
            .iter()
            .map(|key| {
                let node = NodeId::from_bytes(key.verifying_key().to_bytes());
                let digest = cert_digest(
                    &scene.room,
                    scene.n,
                    hash.as_bytes(),
                    scene.term,
                    &leader,
                    &node,
                );
                Cert::node(node, SignatureBytes::from_bytes(sign(key, &digest)))
            })
            .collect();

        CommitProof {
            rpc_term: scene.term,
            leader,
            certs,
        }
    }

    fn genesis_proof(&self, scene: &Scene) -> CommitProof {
        let mut proof = self.proof(scene, self.creator(), &[&self.creator_key]);
        let hash = hash_scene(scene);
        proof.certs.push(Cert::room(SignatureBytes::from_bytes(sign(
            &self.room_key,
            hash.as_bytes(),
        ))));
        proof
    }

    fn signed_intent(&self, id_byte: u8, ts: u64) -> Intent {
        self.signed_intent_for(&self.creator_key, "codex", id_byte, ts)
    }

    fn signed_intent_for(&self, key: &SigningKey, agent: &str, id_byte: u8, ts: u64) -> Intent {
        let mut intent = Intent {
            v: 1,
            id: Hash32::from_bytes([id_byte; 32]),
            room: self.room(),
            kind: IntentKind::Wait,
            agent: AgentId::new(agent).unwrap(),
            node: NodeId::from_bytes(key.verifying_key().to_bytes()),
            ts,
            exp: ts + 86_400,
            sig: SignatureBytes::from_bytes([0; 64]),
        };
        let value = serde_json::to_value(&intent).unwrap();
        intent.sig = SignatureBytes::from_bytes(sign(key, &signed_object_digest(&value)));
        intent
    }
}

fn hash_scene(scene: &Scene) -> Hash32 {
    Hash32::from_bytes(scene_hash(&serde_json::to_value(scene).unwrap()))
}

fn committed_genesis(fixture: &Fixture) -> ChainState {
    let scene = fixture.genesis();
    apply(
        &ChainState::empty(),
        &scene,
        Some(&fixture.genesis_proof(&scene)),
        ApplyMode::Commit(&ApplyResources::default()),
    )
    .unwrap()
}

fn grant_scene(fixture: &Fixture, state: &ChainState, intent: &Intent) -> Scene {
    Scene {
        v: 1,
        room: fixture.room(),
        n: state.head_n.unwrap() + 1,
        term: state.head_term.unwrap(),
        parent: state.head_hash,
        roster: state.roster.clone(),
        leader: fixture.creator(),
        ts: intent.ts + 1,
        body: Body::Grant {
            to: Mouth {
                agent: intent.agent.clone(),
                node: intent.node,
            },
            reason: GrantReason::Queue,
            intent_id: intent.id,
        },
        certs: Vec::new(),
    }
}

#[test]
fn genesis_commit_singleton() {
    let fixture = Fixture::new();
    let scene = fixture.genesis();
    let next = apply(
        &ChainState::empty(),
        &scene,
        Some(&fixture.genesis_proof(&scene)),
        ApplyMode::Commit(&ApplyResources::default()),
    )
    .unwrap();

    assert_eq!(next.head_n, Some(0));
    assert_eq!(next.head_hash, Some(hash_scene(&scene)));
    assert_eq!(next.roster, vec![fixture.creator()]);
    assert_eq!(next.floor_mode, Some(FloorMode::Stick));
}

#[test]
fn genesis_requires_room_signature_and_creator_majority_cert() {
    let fixture = Fixture::new();
    let scene = fixture.genesis();
    let node_only = fixture.proof(&scene, fixture.creator(), &[&fixture.creator_key]);
    let room_only = CommitProof {
        rpc_term: 1,
        leader: fixture.creator(),
        certs: vec![Cert::room(SignatureBytes::from_bytes(sign(
            &fixture.room_key,
            hash_scene(&scene).as_bytes(),
        )))],
    };

    assert!(matches!(
        apply(
            &ChainState::empty(),
            &scene,
            Some(&node_only),
            ApplyMode::Commit(&ApplyResources::default())
        ),
        Err(ApplyError::MissingRoomSignature)
    ));
    assert!(matches!(
        apply(
            &ChainState::empty(),
            &scene,
            Some(&room_only),
            ApplyMode::Commit(&ApplyResources::default())
        ),
        Err(ApplyError::InsufficientCerts { .. })
    ));
}

#[test]
fn catchup_rejects_self_appointed_roster() {
    let fixture = Fixture::new();
    let state = committed_genesis(&fixture);
    let scene = Scene {
        v: 1,
        room: fixture.room(),
        n: 1,
        term: 2,
        parent: state.head_hash,
        roster: vec![fixture.other()],
        leader: fixture.other(),
        ts: 1_766_700_001,
        body: Body::Membership {
            stake: StakePolicy::default(),
            floor: FloorConfig::stick(30),
            closes_grant: None,
        },
        certs: Vec::new(),
    };
    let proof = fixture.proof(&scene, fixture.other(), &[&fixture.other_key]);

    assert!(matches!(
        apply(
            &state,
            &scene,
            Some(&proof),
            ApplyMode::Commit(&ApplyResources::default())
        ),
        Err(ApplyError::RosterMismatch)
    ));
}

#[test]
fn precert_skips_majority_but_requires_intent_for_grant() {
    let fixture = Fixture::new();
    let state = committed_genesis(&fixture);
    let intent = fixture.signed_intent(9, 1_766_700_010);
    let scene = grant_scene(&fixture, &state, &intent);

    assert!(matches!(
        apply(
            &state,
            &scene,
            None,
            ApplyMode::Precert(&ApplyResources::default())
        ),
        Err(ApplyError::MissingIntent)
    ));

    let resources = ApplyResources {
        intents: vec![intent],
        blobs: BTreeMap::new(),
    };
    let unchanged = apply(&state, &scene, None, ApplyMode::Precert(&resources)).unwrap();
    assert_eq!(unchanged, state);
}

#[test]
fn precert_grant_requires_the_deterministic_queue_head() {
    let fixture = Fixture::new();
    let state = committed_genesis(&fixture);
    let target = fixture.signed_intent(9, 1_766_700_020);
    let earlier = fixture.signed_intent_for(&fixture.other_key, "claude", 8, 1_766_700_010);
    let scene = grant_scene(&fixture, &state, &target);

    assert!(matches!(
        apply(
            &state,
            &scene,
            None,
            ApplyMode::Precert(&ApplyResources {
                intents: vec![target, earlier],
                blobs: BTreeMap::new(),
            })
        ),
        Err(ApplyError::IntentNotQueueHead)
    ));
}

#[test]
fn commit_of_grant_does_not_require_live_intent_bytes() {
    let fixture = Fixture::new();
    let state = committed_genesis(&fixture);
    let intent = fixture.signed_intent(9, 1_766_700_010);
    let scene = grant_scene(&fixture, &state, &intent);
    let proof = fixture.proof(&scene, fixture.creator(), &[&fixture.creator_key]);

    let next = apply(
        &state,
        &scene,
        Some(&proof),
        ApplyMode::Commit(&ApplyResources::default()),
    )
    .unwrap();
    assert_eq!(next.live_grant.as_ref().unwrap().hash, hash_scene(&scene));
    assert!(next.consumed_intents.contains(&intent.id));
}

#[test]
fn staged_allows_missing_blobs_commit_does_not() {
    let fixture = Fixture::new();
    let genesis = committed_genesis(&fixture);
    let intent = fixture.signed_intent(9, 1_766_700_010);
    let grant = grant_scene(&fixture, &genesis, &intent);
    let grant_proof = fixture.proof(&grant, fixture.creator(), &[&fixture.creator_key]);
    let granted = apply(
        &genesis,
        &grant,
        Some(&grant_proof),
        ApplyMode::Commit(&ApplyResources::default()),
    )
    .unwrap();

    let bytes = b"hello blob".to_vec();
    let blob_hash = Hash32::from_bytes(Sha256::digest(&bytes).into());
    let speech = Scene {
        v: 1,
        room: fixture.room(),
        n: 2,
        term: 1,
        parent: granted.head_hash,
        roster: granted.roster.clone(),
        leader: fixture.creator(),
        ts: 1_766_700_020,
        body: Body::Speech {
            closes_grant: granted.live_grant.as_ref().unwrap().hash,
            text: "done".into(),
            blobs: vec![BlobRef {
                name: "note.txt".into(),
                sha256: blob_hash,
                bytes: bytes.len() as u64,
            }],
        },
        certs: Vec::new(),
    };
    let proof = fixture.proof(&speech, fixture.creator(), &[&fixture.creator_key]);

    let staged = apply(&granted, &speech, Some(&proof), ApplyMode::Staged).unwrap();
    assert_eq!(staged, granted);
    assert!(matches!(
        apply(
            &granted,
            &speech,
            Some(&proof),
            ApplyMode::Commit(&ApplyResources::default())
        ),
        Err(ApplyError::MissingBlob(hash)) if hash == blob_hash
    ));

    let mut blobs = BTreeMap::new();
    blobs.insert(blob_hash, bytes);
    let committed = apply(
        &granted,
        &speech,
        Some(&proof),
        ApplyMode::Commit(&ApplyResources {
            intents: Vec::new(),
            blobs,
        }),
    )
    .unwrap();
    assert_eq!(committed.head_n, Some(2));
    assert!(committed.live_grant.is_none());
}

#[test]
fn grant_while_another_grant_is_live_is_rejected() {
    let fixture = Fixture::new();
    let genesis = committed_genesis(&fixture);
    let first_intent = fixture.signed_intent(9, 1_766_700_010);
    let first = grant_scene(&fixture, &genesis, &first_intent);
    let first_proof = fixture.proof(&first, fixture.creator(), &[&fixture.creator_key]);
    let granted = apply(
        &genesis,
        &first,
        Some(&first_proof),
        ApplyMode::Commit(&ApplyResources::default()),
    )
    .unwrap();
    let second_intent = fixture.signed_intent(10, 1_766_700_011);
    let second = grant_scene(&fixture, &granted, &second_intent);

    assert!(matches!(
        apply(
            &granted,
            &second,
            None,
            ApplyMode::Precert(&ApplyResources {
                intents: vec![second_intent],
                blobs: BTreeMap::new(),
            })
        ),
        Err(ApplyError::InvalidFloorTransition)
    ));
}

#[test]
fn relabeling_commit_proof_term_invalidates_signatures() {
    let fixture = Fixture::new();
    let state = committed_genesis(&fixture);
    let intent = fixture.signed_intent(9, 1_766_700_010);
    let scene = grant_scene(&fixture, &state, &intent);
    let mut proof = fixture.proof(&scene, fixture.creator(), &[&fixture.creator_key]);
    proof.rpc_term = 2;

    assert!(matches!(
        apply(
            &state,
            &scene,
            Some(&proof),
            ApplyMode::Commit(&ApplyResources::default())
        ),
        Err(ApplyError::InvalidCertSignature(_))
    ));
}

#[test]
fn genesis_proof_is_fixed_to_rpc_term_one() {
    let fixture = Fixture::new();
    let scene = fixture.genesis();
    let hash = hash_scene(&scene);
    let node_digest = cert_digest(
        &scene.room,
        scene.n,
        hash.as_bytes(),
        2,
        &fixture.creator(),
        &fixture.creator(),
    );
    let proof = CommitProof {
        rpc_term: 2,
        leader: fixture.creator(),
        certs: vec![
            Cert::node(
                fixture.creator(),
                SignatureBytes::from_bytes(sign(&fixture.creator_key, &node_digest)),
            ),
            Cert::room(SignatureBytes::from_bytes(sign(
                &fixture.room_key,
                hash.as_bytes(),
            ))),
        ],
    };

    assert!(matches!(
        apply(
            &ChainState::empty(),
            &scene,
            Some(&proof),
            ApplyMode::Commit(&ApplyResources::default())
        ),
        Err(ApplyError::InvalidGenesis)
    ));
}

#[test]
fn view_change_is_exactly_one_add_or_remove_with_exact_arithmetic() {
    let fixture = Fixture::new();
    let state = committed_genesis(&fixture);
    let mut next_roster = vec![fixture.creator(), fixture.other()];
    next_roster.sort();
    let valid = Scene {
        v: 1,
        room: fixture.room(),
        n: 1,
        term: 1,
        parent: state.head_hash,
        roster: state.roster.clone(),
        leader: fixture.creator(),
        ts: 1_766_700_020,
        body: Body::ViewChange {
            add: vec![fixture.other()],
            remove: Vec::new(),
            next_roster,
            closes_grant: None,
        },
        certs: Vec::new(),
    };
    assert!(apply(
        &state,
        &valid,
        None,
        ApplyMode::Precert(&ApplyResources::default())
    )
    .is_ok());

    let mut invalid = valid;
    if let Body::ViewChange { remove, .. } = &mut invalid.body {
        remove.push(fixture.creator());
    }
    assert!(matches!(
        apply(
            &state,
            &invalid,
            None,
            ApplyMode::Precert(&ApplyResources::default())
        ),
        Err(ApplyError::InvalidViewChange)
    ));
}

#[test]
fn scene_deserialization_rejects_unknown_hashed_keys() {
    let fixture = Fixture::new();
    let mut value = serde_json::to_value(fixture.genesis()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("surprise".into(), Value::Bool(true));

    assert!(serde_json::from_value::<Scene>(value).is_err());
}

#[test]
fn optional_hashed_fields_reject_explicit_null() {
    let fixture = Fixture::new();
    let state = committed_genesis(&fixture);
    let value = serde_json::json!({
        "v": 1,
        "room": fixture.room(),
        "n": 1,
        "term": 1,
        "parent": state.head_hash,
        "roster": state.roster,
        "leader": fixture.creator(),
        "ts": 1_766_700_020_u64,
        "body": {
            "type": "membership",
            "stake": StakePolicy::default(),
            "floor": FloorConfig::stick(30),
            "closes_grant": null
        }
    });

    assert!(serde_json::from_value::<Scene>(value).is_err());
}
