use std::{thread, time::Duration};

use conch_core::{
    encoding::{sign, signed_object_digest},
    floor::{FloorEngine, FloorError, FloorWatch, TakePhase},
    types::{
        AgentId, ChainState, Hash32, Intent, IntentKind, LiveGrant, Mouth, NodeId, RoomId,
        SignatureBytes,
    },
};
use ed25519_dalek::SigningKey;

fn mouth(key: &SigningKey, agent: &str) -> Mouth {
    Mouth {
        agent: AgentId::new(agent).unwrap(),
        node: NodeId::from_bytes(key.verifying_key().to_bytes()),
    }
}

fn state(room: RoomId, roster: Vec<NodeId>, live_grant: Option<LiveGrant>) -> ChainState {
    ChainState {
        room: Some(room),
        head_n: Some(1),
        head_hash: Some(Hash32::from_bytes([1; 32])),
        head_term: Some(1),
        roster,
        stake: None,
        floor_mode: None,
        moderator: None,
        timeout_secs: Some(30),
        live_grant,
        consumed_intents: Default::default(),
    }
}

fn grant(holder: Mouth) -> LiveGrant {
    LiveGrant {
        hash: Hash32::from_bytes([9; 32]),
        to: holder,
        term: 1,
        n: 1,
    }
}

fn intent(key: &SigningKey, room: RoomId, agent: &str, id: u8, ts: u64) -> Intent {
    let mut intent = Intent {
        v: 1,
        id: Hash32::from_bytes([id; 32]),
        room,
        kind: IntentKind::Wait,
        agent: AgentId::new(agent).unwrap(),
        node: NodeId::from_bytes(key.verifying_key().to_bytes()),
        ts,
        exp: ts + 86_400,
        sig: SignatureBytes::from_bytes([0; 64]),
    };
    intent.sig = SignatureBytes::from_bytes(sign(
        key,
        &signed_object_digest(&serde_json::to_value(&intent).unwrap()),
    ));
    intent
}

#[test]
fn speak_without_grant_errors_no_grant() {
    let key = SigningKey::from_bytes(&[1; 32]);
    let holder = mouth(&key, "codex");
    let mut floor = FloorEngine::new(holder.node);

    assert_eq!(
        floor.speak(&holder, "hello", &"01".repeat(16)),
        Err(FloorError::NoGrant)
    );
}

#[test]
fn two_waits_only_queue_head_is_grantable() {
    let first_key = SigningKey::from_bytes(&[1; 32]);
    let second_key = SigningKey::from_bytes(&[2; 32]);
    let room = RoomId::from_bytes([3; 32]);
    let chain = state(
        room,
        vec![
            mouth(&first_key, "codex").node,
            mouth(&second_key, "claude").node,
        ],
        None,
    );
    let mut floor = FloorEngine::new(mouth(&first_key, "codex").node);
    let later = intent(&first_key, room, "codex", 9, 20);
    let earlier = intent(&second_key, room, "claude", 8, 10);
    floor.upsert_intent(&chain, later).unwrap();
    floor.upsert_intent(&chain, earlier.clone()).unwrap();

    assert_eq!(floor.queue_head(&chain, 21).unwrap().id, earlier.id);
}

#[test]
fn speak_request_id_is_idempotent() {
    let key = SigningKey::from_bytes(&[1; 32]);
    let holder = mouth(&key, "codex");
    let room = RoomId::from_bytes([3; 32]);
    let mut floor = FloorEngine::new(holder.node);
    floor.observe_committed(&state(room, vec![holder.node], Some(grant(holder.clone()))));
    let request_id = "01".repeat(16);

    let first = floor.speak(&holder, "hello", &request_id).unwrap();
    let retry = floor.speak(&holder, "ignored", &request_id).unwrap();

    assert_eq!(retry, first);
    assert_eq!(floor.take().unwrap().text, "hello");
    assert_eq!(floor.take().unwrap().rev, 1);
}

#[test]
fn closing_rejects_extra_speak_but_preserves_retry_result() {
    let key = SigningKey::from_bytes(&[1; 32]);
    let holder = mouth(&key, "codex");
    let room = RoomId::from_bytes([3; 32]);
    let mut floor = FloorEngine::new(holder.node);
    floor.observe_committed(&state(room, vec![holder.node], Some(grant(holder.clone()))));
    let first_id = "01".repeat(16);
    let first = floor.speak(&holder, "hello", &first_id).unwrap();
    floor
        .freeze(room, floor.take().unwrap().grant_hash)
        .unwrap();

    assert_eq!(floor.take().unwrap().phase, TakePhase::Closing);
    assert_eq!(floor.speak(&holder, "retry", &first_id).unwrap(), first);
    assert_eq!(
        floor.speak(&holder, "late", &"02".repeat(16)),
        Err(FloorError::NoGrant)
    );
}

#[test]
fn wait_for_floor_unblocks_only_on_committed_grant() {
    let key = SigningKey::from_bytes(&[1; 32]);
    let holder = mouth(&key, "codex");
    let room = RoomId::from_bytes([3; 32]);
    let watch = FloorWatch::new(FloorEngine::new(holder.node));
    let queued = intent(&key, room, "codex", 7, 10);
    let vacant = state(room, vec![holder.node], None);
    watch.with_engine(|engine| engine.upsert_intent(&vacant, queued).unwrap());
    let waiter = watch.clone();
    let waiting_mouth = holder.clone();
    let thread =
        thread::spawn(move || waiter.wait_for_floor(&waiting_mouth, Duration::from_secs(1)));

    thread::sleep(Duration::from_millis(30));
    assert!(
        !thread.is_finished(),
        "an uncommitted intent/draft cannot unblock"
    );
    watch.observe_committed(&state(room, vec![holder.node], Some(grant(holder))));

    assert!(thread.join().unwrap().is_ok());
}

#[test]
fn stale_intent_cannot_resurrect_after_supersede() {
    let key = SigningKey::from_bytes(&[1; 32]);
    let holder = mouth(&key, "codex");
    let room = RoomId::from_bytes([3; 32]);
    let chain = state(room, vec![holder.node], None);
    let newer = intent(&key, room, "codex", 9, 20);
    let stale = intent(&key, room, "codex", 8, 10);
    let mut floor = FloorEngine::new(holder.node);

    assert!(floor.upsert_intent(&chain, newer.clone()).unwrap());
    assert!(!floor.upsert_intent(&chain, stale).unwrap());
    assert_eq!(floor.queue_head(&chain, 21).unwrap().id, newer.id);
}

#[test]
fn same_id_refreshes_expiry_without_changing_queue_identity() {
    let key = SigningKey::from_bytes(&[1; 32]);
    let holder = mouth(&key, "codex");
    let room = RoomId::from_bytes([3; 32]);
    let chain = state(room, vec![holder.node], None);
    let original = intent(&key, room, "codex", 9, 20);
    let mut refreshed = original.clone();
    refreshed.exp += 60;
    refreshed.sig = SignatureBytes::from_bytes(sign(
        &key,
        &signed_object_digest(&serde_json::to_value(&refreshed).unwrap()),
    ));
    let mut floor = FloorEngine::new(holder.node);

    floor.upsert_intent(&chain, original).unwrap();
    assert!(floor.upsert_intent(&chain, refreshed.clone()).unwrap());
    assert_eq!(floor.queue_head(&chain, 21).unwrap().exp, refreshed.exp);
}

#[test]
fn expiry_boundary_and_consumption_leave_the_next_intent_queued() {
    let first_key = SigningKey::from_bytes(&[1; 32]);
    let second_key = SigningKey::from_bytes(&[2; 32]);
    let room = RoomId::from_bytes([3; 32]);
    let first = intent(&first_key, room, "codex", 8, 10);
    let second = intent(&second_key, room, "claude", 9, 20);
    let mut chain = state(room, vec![first.node, second.node], None);
    let mut floor = FloorEngine::new(first.node);
    floor.upsert_intent(&chain, first.clone()).unwrap();
    floor.upsert_intent(&chain, second.clone()).unwrap();

    assert_eq!(floor.queue_head(&chain, first.exp).unwrap().id, second.id);
    chain.consumed_intents.insert(first.id);
    assert_eq!(floor.queue_head(&chain, 21).unwrap().id, second.id);
}

#[test]
fn old_grant_freeze_and_other_room_commit_do_not_close_current_take() {
    let key = SigningKey::from_bytes(&[1; 32]);
    let holder = mouth(&key, "codex");
    let room = RoomId::from_bytes([3; 32]);
    let mut floor = FloorEngine::new(holder.node);
    floor.observe_committed(&state(room, vec![holder.node], Some(grant(holder.clone()))));
    let current_hash = floor.take().unwrap().grant_hash;

    assert_eq!(
        floor.freeze(room, Hash32::from_bytes([4; 32])),
        Err(FloorError::NoGrant)
    );
    floor.observe_committed(&state(RoomId::from_bytes([5; 32]), vec![holder.node], None));
    assert_eq!(floor.take().unwrap().grant_hash, current_hash);
    assert_eq!(floor.take().unwrap().phase, TakePhase::Open);
}
