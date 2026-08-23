use conch_core::{
    cluster::{
        AppendDelivery, AppendOutcome, CampaignOutcome, Cluster, DeliveryResult, WinOutcome,
    },
    consensus::CommitMessage,
    types::{Body, Cert, ConsensusRole, FloorConfig, Hash32, Scene},
};
use tempfile::TempDir;

#[test]
fn three_node_commit() {
    let temp = TempDir::new().unwrap();
    let mut cluster = Cluster::bootstrap(temp.path(), 3).unwrap();
    let before = cluster.node(0).chain.head_n.unwrap();

    assert_eq!(
        cluster.campaign(0).unwrap(),
        CampaignOutcome::Won { term: 2, votes: 3 }
    );
    let scene = cluster.fresh_membership_scene(0).unwrap();
    let hash = cluster.hash_scene(&scene);
    let outcome = cluster
        .append_scene(0, scene, AppendDelivery::AllReachable)
        .unwrap();

    assert!(
        matches!(outcome, AppendOutcome::Committed { hash: committed, .. } if committed == hash)
    );
    for node in cluster.nodes() {
        assert_eq!(node.chain.head_n, Some(before + 1));
        assert_eq!(node.chain.head_hash, Some(hash));
    }
}

#[test]
fn split_2_2_of_4_accepts_but_commits_nothing() {
    let temp = TempDir::new().unwrap();
    let mut cluster = Cluster::bootstrap(temp.path(), 4).unwrap();
    let before = cluster.node(0).chain.head_n.unwrap();
    assert!(cluster.campaign(0).unwrap().won());
    cluster.partition(&[0, 1], &[2, 3]);

    let scene = cluster.fresh_membership_scene(0).unwrap();
    let outcome = cluster
        .append_scene(0, scene, AppendDelivery::AllReachable)
        .unwrap();

    assert_eq!(
        outcome,
        AppendOutcome::Accepted {
            certs: 2,
            required: 3
        }
    );
    assert_eq!(cluster.node(0).chain.head_n, Some(before));
    assert_eq!(cluster.node(1).chain.head_n, Some(before));
    assert!(cluster.node(0).pending.is_some());
    assert!(cluster.node(1).pending.is_some());
    assert!(!cluster.campaign(2).unwrap().won());
}

#[test]
fn example_h_carries_forward_exact_hash_with_fresh_term_four_certs() {
    let temp = TempDir::new().unwrap();
    let mut cluster = Cluster::bootstrap(temp.path(), 5).unwrap();
    assert_eq!(cluster.campaign(0).unwrap().term(), 2);
    cluster.disconnect(0, 4);

    let h = cluster.fresh_membership_scene(0).unwrap();
    let h_hash = cluster.hash_scene(&h);
    let first = cluster
        .append_scene(0, h.clone(), AppendDelivery::LeaderOnly)
        .unwrap();
    assert!(matches!(
        first,
        AppendOutcome::Committed { rpc_term: 2, .. }
    ));
    for index in 1..=3 {
        assert_eq!(cluster.node(index).pending.as_ref().unwrap().hash, h_hash);
    }

    cluster.crash(0);
    assert_eq!(
        cluster.campaign(4).unwrap(),
        CampaignOutcome::Lost { term: 3, votes: 1 }
    );
    assert_eq!(
        cluster.campaign(1).unwrap(),
        CampaignOutcome::Won { term: 4, votes: 4 }
    );

    let result = cluster.win_probe(1, AppendDelivery::AllReachable).unwrap();
    assert!(matches!(result, WinOutcome::CommittedCarried { hash, rpc_term: 4 } if hash == h_hash));
    let committed = cluster.node(1).history.last().unwrap();
    assert_eq!(
        committed.scene, h,
        "carry-forward must preserve hashed scene bytes"
    );
    assert_eq!(committed.commit_proof.rpc_term, 4);
    assert_eq!(committed.commit_proof.leader, cluster.node(1).id);
}

#[test]
fn example_h2_installs_existing_proof_and_does_not_append() {
    let temp = TempDir::new().unwrap();
    let mut cluster = Cluster::bootstrap(temp.path(), 5).unwrap();
    assert!(cluster.campaign(0).unwrap().won());
    let h = cluster.fresh_membership_scene(0).unwrap();
    let h_hash = cluster.hash_scene(&h);
    cluster
        .append_scene(0, h, AppendDelivery::Nodes(vec![2, 3]))
        .unwrap();
    assert_eq!(cluster.node(1).pending.as_ref().unwrap().hash, h_hash);
    assert_eq!(cluster.node(2).chain.head_hash, Some(h_hash));
    assert_eq!(cluster.node(3).chain.head_hash, Some(h_hash));

    cluster.crash(0);
    assert!(cluster.campaign(1).unwrap().won());
    let appends_before = cluster.node(1).sent_appends.len();
    let result = cluster.win_probe(1, AppendDelivery::AllReachable).unwrap();

    assert_eq!(result, WinOutcome::InstalledExisting { hash: h_hash });
    assert_eq!(cluster.node(1).sent_appends.len(), appends_before);
    assert!(cluster.node(1).pending.is_none());
}

#[test]
fn win_abort_sends_no_append_after_verified_proof_demotes_winner() {
    let temp = TempDir::new().unwrap();
    let mut cluster = Cluster::bootstrap(temp.path(), 3).unwrap();
    assert!(cluster.campaign(0).unwrap().won());
    cluster.inject_verified_commit_for_probe(1, 100).unwrap();
    let appends_before = cluster.node(0).sent_appends.len();

    let result = cluster.win_probe(0, AppendDelivery::AllReachable).unwrap();

    assert_eq!(result, WinOutcome::AbortedDemoted { current_term: 100 });
    assert_eq!(cluster.node(0).sent_appends.len(), appends_before);
    assert_eq!(cluster.node(0).consensus.role, ConsensusRole::Follower);
    assert_eq!(cluster.node(0).consensus.leader_id, None);
}

#[test]
fn same_term_different_hash_is_refused() {
    let temp = TempDir::new().unwrap();
    let mut cluster = Cluster::bootstrap(temp.path(), 4).unwrap();
    assert!(cluster.campaign(0).unwrap().won());
    cluster.partition(&[0, 1], &[2, 3]);
    let h = cluster.fresh_membership_scene(0).unwrap();
    cluster
        .append_scene(0, h.clone(), AppendDelivery::AllReachable)
        .unwrap();
    let mut rival = h;
    rival.ts += 1;
    let rival_hash: Hash32 = cluster.hash_scene(&rival);

    let result = cluster.deliver_append(0, 1, rival).unwrap();

    assert_eq!(result, DeliveryResult::RefusedSameTermConflict);
    assert_ne!(cluster.node(1).pending.as_ref().unwrap().hash, rival_hash);
}

#[test]
fn removed_node_cannot_campaign() {
    let temp = TempDir::new().unwrap();
    let mut cluster = Cluster::bootstrap(temp.path(), 3).unwrap();
    assert!(cluster.campaign(0).unwrap().won());
    let removed = cluster.node(2).id;
    let scene = cluster
        .fresh_view_change_scene(0, Vec::new(), vec![removed])
        .unwrap();
    assert!(matches!(
        cluster
            .append_scene(0, scene, AppendDelivery::AllReachable)
            .unwrap(),
        AppendOutcome::Committed { .. }
    ));

    assert!(cluster.campaign(2).is_err());
}

#[test]
fn self_appointed_catchup_roster_is_rejected() {
    let temp = TempDir::new().unwrap();
    let mut cluster = Cluster::bootstrap(temp.path(), 3).unwrap();
    let follower = cluster.node(1);
    let impostor = cluster.node(2).id;
    let scene = Scene {
        v: 1,
        room: follower.chain.room.unwrap(),
        n: follower.chain.head_n.unwrap() + 1,
        term: 99,
        parent: follower.chain.head_hash,
        roster: vec![impostor],
        leader: impostor,
        ts: 1_766_999_999,
        body: Body::Membership {
            stake: follower.chain.stake.clone().unwrap(),
            floor: FloorConfig::stick(30),
            closes_grant: None,
        },
        certs: Vec::new(),
    };
    let message = CommitMessage {
        room: scene.room,
        n: scene.n,
        hash: cluster.hash_scene(&scene),
        rpc_term: 99,
        leader: impostor,
        certs: Vec::<Cert>::new(),
        scene,
    };

    assert!(cluster.deliver_commit(1, &message).is_err());
    assert_ne!(cluster.node(1).chain.head_n, Some(message.n));
}

#[test]
fn example_i_cannot_win_ballot_101_against_s_voters() {
    let temp = TempDir::new().unwrap();
    let mut cluster = Cluster::bootstrap(temp.path(), 5).unwrap();
    cluster
        .inject_verified_commit(0, &[0, 1, 2, 3, 4], 100)
        .unwrap();

    assert_eq!(
        cluster.campaign(4).unwrap(),
        CampaignOutcome::Won {
            term: 101,
            votes: 5
        }
    );
    assert_eq!(cluster.node(0).consensus.current_term, 101);
    assert_eq!(
        cluster.node(0).consensus.voted_for,
        Some(cluster.node(4).id)
    );

    let retry = cluster.campaign(0).unwrap();
    assert_eq!(retry.term(), 102, "A cannot campaign again in ballot 101");
}

#[test]
fn crash_after_cert_reloads_and_carries_the_same_hash() {
    let temp = TempDir::new().unwrap();
    let mut cluster = Cluster::bootstrap(temp.path(), 4).unwrap();
    assert!(cluster.campaign(0).unwrap().won());
    cluster.partition(&[0, 1], &[2, 3]);
    let scene = cluster.fresh_membership_scene(0).unwrap();
    let hash = cluster.hash_scene(&scene);
    assert!(matches!(
        cluster
            .append_scene(0, scene.clone(), AppendDelivery::AllReachable)
            .unwrap(),
        AppendOutcome::Accepted { certs: 2, .. }
    ));
    let durable_cert = cluster.node(1).pending.as_ref().unwrap().cert.clone();

    cluster.crash(1);
    cluster.restart(1).unwrap();
    let reloaded = cluster.node(1).pending.as_ref().unwrap();
    assert_eq!(reloaded.hash, hash);
    assert_eq!(reloaded.cert, durable_cert);

    cluster.crash(0);
    cluster.heal();
    assert!(cluster.campaign(1).unwrap().won());
    assert!(matches!(
        cluster
            .win_probe(1, AppendDelivery::AllReachable)
            .unwrap(),
        WinOutcome::CommittedCarried { hash: committed, .. } if committed == hash
    ));
    assert_eq!(cluster.node(1).history.last().unwrap().scene, scene);
}

#[test]
fn view_change_is_exactly_one_add_or_remove() {
    let temp = TempDir::new().unwrap();
    let mut cluster = Cluster::bootstrap(temp.path(), 3).unwrap();
    assert!(cluster.campaign(0).unwrap().won());
    let one = cluster.node(1).id;
    let two = cluster.node(2).id;

    let invalid = cluster
        .fresh_view_change_scene(0, Vec::new(), vec![one, two])
        .unwrap();
    assert!(cluster
        .append_scene(0, invalid, AppendDelivery::AllReachable)
        .is_err());

    let remove = cluster
        .fresh_view_change_scene(0, Vec::new(), vec![two])
        .unwrap();
    cluster
        .append_scene(0, remove, AppendDelivery::AllReachable)
        .unwrap();
    assert!(!cluster.node(0).chain.roster.contains(&two));

    let add = cluster
        .fresh_view_change_scene(0, vec![two], Vec::new())
        .unwrap();
    cluster
        .append_scene(0, add, AppendDelivery::AllReachable)
        .unwrap();
    assert!(cluster.node(0).chain.roster.contains(&two));
}

#[test]
fn self_remove_pushes_then_steps_down() {
    let temp = TempDir::new().unwrap();
    let mut cluster = Cluster::bootstrap(temp.path(), 3).unwrap();
    assert!(cluster.campaign(0).unwrap().won());
    let leader = cluster.node(0).id;
    let scene = cluster
        .fresh_view_change_scene(0, Vec::new(), vec![leader])
        .unwrap();
    let hash = cluster.hash_scene(&scene);

    cluster
        .append_scene(0, scene, AppendDelivery::AllReachable)
        .unwrap();

    assert_eq!(cluster.node(0).consensus.role, ConsensusRole::Follower);
    assert_eq!(cluster.node(0).consensus.leader_id, None);
    assert_eq!(cluster.node(1).chain.head_hash, Some(hash));
    assert_eq!(cluster.node(2).chain.head_hash, Some(hash));
}

#[test]
fn heartbeat_and_append_nack_catch_up_a_lagging_follower() {
    let temp = TempDir::new().unwrap();
    let mut cluster = Cluster::bootstrap(temp.path(), 3).unwrap();
    assert!(cluster.campaign(0).unwrap().won());

    let first = cluster.fresh_membership_scene(0).unwrap();
    cluster
        .append_scene(0, first, AppendDelivery::Nodes(vec![1]))
        .unwrap();
    assert!(cluster.node(2).chain.head_n < cluster.node(0).chain.head_n);
    cluster.tick().unwrap();
    assert_eq!(cluster.node(2).chain.head_n, cluster.node(0).chain.head_n);

    let second = cluster.fresh_membership_scene(0).unwrap();
    cluster
        .append_scene(0, second, AppendDelivery::Nodes(vec![1]))
        .unwrap();
    assert!(cluster.node(2).chain.head_n < cluster.node(0).chain.head_n);
    let third = cluster.fresh_membership_scene(0).unwrap();
    cluster
        .append_scene(0, third, AppendDelivery::AllReachable)
        .unwrap();
    assert_eq!(cluster.node(2).chain.head_n, cluster.node(0).chain.head_n);
    assert_eq!(
        cluster.node(2).chain.head_hash,
        cluster.node(0).chain.head_hash
    );
}
