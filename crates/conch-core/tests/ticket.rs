use conch_core::{
    ticket::{eligible, Declaration, JoinRole, Ticket, TicketError},
    types::{AgentId, FloorConfig, Hash32, NodeId, RoomId, StakePolicy},
};
use ed25519_dalek::SigningKey;

fn ticket() -> Ticket {
    Ticket {
        v: 1,
        id: RoomId::from_bytes([1; 32]),
        name: "Design room".into(),
        trackers: vec!["tcp://tracker.example:7421".into()],
        peers: vec!["tcp://10.0.0.2:7421".into()],
        token: Some(Hash32::from_bytes([2; 32])),
        stake: StakePolicy::default(),
        floor: FloorConfig::stick(30),
        parent: None,
        genesis: Hash32::from_bytes([3; 32]),
    }
}

#[test]
fn magnet_round_trip_keeps_the_genesis_pin_and_capability() {
    let original = ticket();
    let encoded = original.to_magnet();
    assert!(encoded.starts_with("conch:1:"));
    assert!(encoded.contains("&g="));

    let decoded = Ticket::from_magnet(&encoded).unwrap();
    assert_eq!(decoded.id, original.id);
    assert_eq!(decoded.name, original.name);
    assert_eq!(decoded.trackers, original.trackers);
    assert_eq!(decoded.peers, original.peers);
    assert_eq!(decoded.token, original.token);
    assert_eq!(decoded.genesis, original.genesis);
    assert_eq!(decoded.stake, StakePolicy::default());
}

#[test]
fn magnet_without_g_rejected() {
    let input = format!("conch:1:{}?dn=missing", RoomId::from_bytes([1; 32]));
    assert!(matches!(
        Ticket::from_magnet(&input),
        Err(TicketError::MissingGenesis)
    ));
}

#[test]
fn join_default_stake() {
    assert_eq!(JoinRole::default(), JoinRole::Stake);
}

#[test]
fn observer_never_in_certs() {
    let policy = StakePolicy {
        agents: true,
        explicit: true,
        allowlist: Vec::new(),
    };
    let node = NodeId::from_bytes([8; 32]);
    let agents = vec![AgentId::new("nano").unwrap()];

    assert!(!eligible(&policy, node, JoinRole::Observe, &agents));
    assert!(eligible(&policy, node, JoinRole::Stake, &[]));
}

#[test]
fn declaration_signature_binds_room_role_and_agents() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let node = NodeId::from_bytes(key.verifying_key().to_bytes());
    let declaration = Declaration::signed(
        RoomId::from_bytes([4; 32]),
        JoinRole::Observe,
        vec![AgentId::new("nano").unwrap()],
        123,
        &key,
    );
    assert!(declaration.verify(node));

    let mut relabeled = declaration;
    relabeled.role = JoinRole::Stake;
    assert!(!relabeled.verify(node));
}

#[test]
fn allowlist_and_agent_predicate_match_the_spec() {
    let node = NodeId::from_bytes([8; 32]);
    let other = NodeId::from_bytes([9; 32]);
    let agents = vec![AgentId::new("claude").unwrap()];
    let policy = StakePolicy {
        agents: true,
        explicit: false,
        allowlist: vec![node],
    };

    assert!(eligible(&policy, node, JoinRole::Stake, &agents));
    assert!(!eligible(&policy, node, JoinRole::Stake, &[]));
    assert!(!eligible(&policy, other, JoinRole::Stake, &agents));
}

#[test]
fn ticket_json_ignores_unknown_keys_but_rejects_null_optional_fields() {
    let mut value = serde_json::to_value(ticket()).unwrap();
    value["future_hint"] = serde_json::json!(true);
    let decoded = Ticket::from_json_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(decoded.id, RoomId::from_bytes([1; 32]));

    value["parent"] = serde_json::Value::Null;
    assert!(matches!(
        Ticket::from_json_slice(&serde_json::to_vec(&value).unwrap()),
        Err(TicketError::Json(_))
    ));
}
