use conch_core::{
    consensus::{
        advance_term, begin_campaign, observe_have, tail, up_to_date, AdvanceSource, ConsensusRole,
        Have, Tail,
    },
    types::{
        Body, Cert, CommitProof, ConsensusState, FloorConfig, Hash32, NodeId, Pending, RoomId,
        Scene, SignatureBytes, StakePolicy,
    },
};

fn node(byte: u8) -> NodeId {
    NodeId::from_bytes([byte; 32])
}

fn proof(rpc_term: u64) -> CommitProof {
    CommitProof {
        rpc_term,
        leader: node(1),
        certs: Vec::new(),
    }
}

fn pending(rpc_term: u64, n: u64, hash_byte: u8) -> Pending {
    let leader = node(1);
    let scene = Scene {
        v: 1,
        room: RoomId::from_bytes([9; 32]),
        n,
        term: rpc_term,
        parent: (n > 0).then(|| Hash32::from_bytes([7; 32])),
        roster: vec![leader],
        leader,
        ts: 1,
        body: Body::Membership {
            stake: StakePolicy::default(),
            floor: FloorConfig::stick(30),
            closes_grant: None,
        },
        certs: Vec::new(),
    };
    Pending {
        n,
        hash: Hash32::from_bytes([hash_byte; 32]),
        scene,
        accepted_rpc_term: rpc_term,
        accepted_leader: leader,
        cert: Cert::node(leader, SignatureBytes::from_bytes([0; 64])),
    }
}

#[test]
fn proof_100_raises_current_term_before_campaign() {
    let mut state = ConsensusState {
        current_term: 1,
        voted_for: Some(node(2)),
        leader_id: Some(node(2)),
        role: ConsensusRole::Leader,
    };
    let head_proof = proof(100);

    let raised = advance_term(
        &mut state,
        None,
        Some(&head_proof),
        AdvanceSource::VerifiedProof(100),
    );

    assert!(raised);
    assert_eq!(state.current_term, 100);
    assert_eq!(state.voted_for, None);
    assert_eq!(state.leader_id, None);
    assert_eq!(state.role, ConsensusRole::Follower);
    assert_eq!(
        begin_campaign(
            &mut state,
            node(1),
            &[node(1)],
            Tail {
                last_rpc: 100,
                last_n: 7,
                last_hash: Hash32::from_bytes([7; 32]),
            }
        )
        .unwrap(),
        101
    );
}

#[test]
fn bare_have_rpc_does_not_advance_term() {
    let state = ConsensusState {
        current_term: 4,
        ..ConsensusState::default()
    };
    let have = Have {
        n: 10,
        hash: Hash32::from_bytes([10; 32]),
        rpc_term: 1_000_000_000,
    };

    let observation = observe_have(Some(4), &have);

    assert!(observation.needs_catch_up);
    assert_eq!(observation.advertised_rpc, 1_000_000_000);
    assert_eq!(state.current_term, 4);
}

#[test]
fn bare_have_at_genesis_starts_catchup_for_an_empty_node() {
    let have = Have {
        n: 0,
        hash: Hash32::from_bytes([10; 32]),
        rpc_term: 1,
    };

    assert!(observe_have(None, &have).needs_catch_up);
}

#[test]
fn higher_last_rpc_wins_without_hash_compare() {
    let candidate = Tail {
        last_rpc: 5,
        last_n: 7,
        last_hash: Hash32::from_bytes([1; 32]),
    };
    let voter = Tail {
        last_rpc: 4,
        last_n: 7,
        last_hash: Hash32::from_bytes([2; 32]),
    };

    assert!(up_to_date(&candidate, &voter));
}

#[test]
fn equal_rpc_equal_n_different_hash_refuses_vote() {
    let candidate = Tail {
        last_rpc: 5,
        last_n: 7,
        last_hash: Hash32::from_bytes([1; 32]),
    };
    let voter = Tail {
        last_rpc: 5,
        last_n: 7,
        last_hash: Hash32::from_bytes([2; 32]),
    };

    assert!(!up_to_date(&candidate, &voter));
}

#[test]
fn pending_is_the_election_tail_instead_of_committed_head() {
    let pending = pending(8, 4, 8);
    let head = (3, Hash32::from_bytes([3; 32]));
    let head_proof = proof(7);

    assert_eq!(
        tail(Some(&pending), Some(head), Some(&head_proof)).unwrap(),
        Tail {
            last_rpc: 8,
            last_n: 4,
            last_hash: Hash32::from_bytes([8; 32]),
        }
    );
}

#[test]
fn campaign_is_strictly_above_current_term_and_tail() {
    let self_node = node(1);
    let mut state = ConsensusState {
        current_term: 12,
        voted_for: Some(node(2)),
        leader_id: Some(node(2)),
        role: ConsensusRole::Follower,
    };
    let term = begin_campaign(
        &mut state,
        self_node,
        &[self_node],
        Tail {
            last_rpc: 20,
            last_n: 7,
            last_hash: Hash32::from_bytes([7; 32]),
        },
    )
    .unwrap();

    assert_eq!(term, 21);
    assert_eq!(state.current_term, 21);
    assert_eq!(state.voted_for, Some(self_node));
    assert_eq!(state.leader_id, None);
    assert_eq!(state.role, ConsensusRole::Candidate);
}

#[test]
fn removed_node_cannot_begin_campaign() {
    let self_node = node(1);
    let mut state = ConsensusState::default();
    let before = state.clone();

    assert!(begin_campaign(
        &mut state,
        self_node,
        &[node(2)],
        Tail {
            last_rpc: 1,
            last_n: 0,
            last_hash: Hash32::from_bytes([7; 32]),
        },
    )
    .is_err());
    assert_eq!(state, before);
}
