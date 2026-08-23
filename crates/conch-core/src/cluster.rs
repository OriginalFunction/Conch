use std::{collections::BTreeMap, path::Path};

use ed25519_dalek::{SigningKey, VerifyingKey};
use thiserror::Error;

use crate::{
    apply::{apply, ApplyError, ApplyMode, ApplyResources},
    consensus::{
        advance_term, begin_campaign, tail, up_to_date, AdvanceSource, Append, CertMessage,
        CommitMessage, ConsensusError, Heartbeat, RequestVote, Tail, Vote,
    },
    disk::{Store, StoreError},
    encoding::{cert_digest, scene_hash, sign, signed_object_digest, verify},
    types::{
        Body, Cert, CertSigner, ChainState, CommitProof, CommittedScene, ConsensusRole,
        ConsensusState, FloorConfig, Hash32, NodeId, Pending, RoomId, Scene, SignatureBytes,
    },
};

#[derive(Debug, Error)]
pub enum ClusterError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Apply(#[from] ApplyError),
    #[error(transparent)]
    Consensus(#[from] ConsensusError),
    #[error("cluster requires at least one node")]
    EmptyCluster,
    #[error("node index is out of range")]
    UnknownNode,
    #[error("node is offline")]
    Offline,
    #[error("node is not the elected leader")]
    NotLeader,
    #[error("leader refused its own proposal")]
    LeaderRefusedProposal,
    #[error("commit proof injection requires a term above zero")]
    InvalidInjectedTerm,
    #[error("commit message envelope does not match its scene")]
    InvalidCommitMessage,
    #[error("leader already has a different pending hash at this height")]
    PendingConflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CampaignOutcome {
    Won { term: u64, votes: usize },
    Lost { term: u64, votes: usize },
}

impl CampaignOutcome {
    pub fn won(&self) -> bool {
        matches!(self, Self::Won { .. })
    }

    pub fn term(&self) -> u64 {
        match self {
            Self::Won { term, .. } | Self::Lost { term, .. } => *term,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendDelivery {
    AllReachable,
    LeaderOnly,
    Nodes(Vec<usize>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendOutcome {
    Committed { hash: Hash32, rpc_term: u64 },
    Accepted { certs: usize, required: usize },
    InstalledExisting { hash: Hash32 },
    AbortedDemoted { current_term: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryResult {
    Certified(CertMessage),
    Nack(crate::consensus::Have),
    RefusedStaleTerm,
    RefusedSameTermConflict,
    RefusedInvalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WinOutcome {
    NoPending,
    InstalledExisting { hash: Hash32 },
    CommittedCarried { hash: Hash32, rpc_term: u64 },
    AcceptedCarried { certs: usize, required: usize },
    AbortedDemoted { current_term: u64 },
}

pub struct TestNode {
    pub id: NodeId,
    signing_key: SigningKey,
    pub store: Store,
    pub chain: ChainState,
    pub consensus: ConsensusState,
    pub pending: Option<Pending>,
    pub head_proof: Option<CommitProof>,
    pub history: Vec<CommittedScene>,
    pub sent_appends: Vec<(u64, Hash32)>,
    pub active: bool,
    won_term: Option<u64>,
    retirement_ticks: Option<u8>,
}

pub struct Cluster {
    room: RoomId,
    room_key: SigningKey,
    nodes: Vec<TestNode>,
    links: Vec<Vec<bool>>,
}

impl Cluster {
    pub fn bootstrap(root: &Path, node_count: usize) -> Result<Self, ClusterError> {
        if node_count == 0 {
            return Err(ClusterError::EmptyCluster);
        }

        let room_key = SigningKey::from_bytes(&[200; 32]);
        let room = RoomId::from_bytes(room_key.verifying_key().to_bytes());
        let mut nodes = Vec::with_capacity(node_count);
        for index in 0..node_count {
            let signing_key = SigningKey::from_bytes(&[(index as u8) + 20; 32]);
            let id = NodeId::from_bytes(signing_key.verifying_key().to_bytes());
            let store = Store::open(
                root.join(format!("node-{index}"))
                    .join("rooms")
                    .join(room.to_string()),
            )?;
            nodes.push(TestNode {
                id,
                signing_key,
                store,
                chain: ChainState::empty(),
                consensus: ConsensusState::default(),
                pending: None,
                head_proof: None,
                history: Vec::new(),
                sent_appends: Vec::new(),
                active: true,
                won_term: None,
                retirement_ticks: None,
            });
        }
        let links = vec![vec![true; node_count]; node_count];
        let mut cluster = Self {
            room,
            room_key,
            nodes,
            links,
        };
        cluster.bootstrap_ledger()?;
        Ok(cluster)
    }

    pub fn nodes(&self) -> &[TestNode] {
        &self.nodes
    }

    pub fn node(&self, index: usize) -> &TestNode {
        &self.nodes[index]
    }

    pub fn hash_scene(&self, scene: &Scene) -> Hash32 {
        hash_scene(scene)
    }

    pub fn disconnect(&mut self, left: usize, right: usize) {
        if left < self.nodes.len() && right < self.nodes.len() {
            self.links[left][right] = false;
            self.links[right][left] = false;
        }
    }

    pub fn partition(&mut self, left: &[usize], right: &[usize]) {
        for &left in left {
            for &right in right {
                self.disconnect(left, right);
            }
        }
    }

    pub fn heal(&mut self) {
        for row in &mut self.links {
            row.fill(true);
        }
    }

    /// Delivers one deterministic 500 ms heartbeat round. A follower behind a
    /// reachable leader immediately runs the in-memory `get_scenes` path.
    pub fn tick(&mut self) -> Result<(), ClusterError> {
        let leaders: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| {
                node.active
                    && node.consensus.role == ConsensusRole::Leader
                    && node.consensus.leader_id == Some(node.id)
                    && node.won_term == Some(node.consensus.current_term)
            })
            .map(|(index, node)| {
                (
                    index,
                    Heartbeat {
                        room: self.room,
                        rpc_term: node.consensus.current_term,
                        leader: node.id,
                        n: node.chain.head_n.expect("bootstrapped leader has head"),
                        hash: node
                            .chain
                            .head_hash
                            .expect("bootstrapped leader has head hash"),
                        have_rpc: node
                            .head_proof
                            .as_ref()
                            .expect("committed head has proof")
                            .rpc_term,
                    },
                )
            })
            .collect();

        for (leader_index, heartbeat) in leaders {
            for follower_index in 0..self.nodes.len() {
                if follower_index != leader_index && self.reachable(leader_index, follower_index) {
                    self.deliver_heartbeat(leader_index, follower_index, &heartbeat)?;
                }
            }
        }
        for index in 0..self.nodes.len() {
            if self.nodes[index].retirement_ticks.is_some() {
                self.maybe_finish_self_removal(index, true);
            }
        }
        Ok(())
    }

    pub fn crash(&mut self, index: usize) {
        if let Some(node) = self.nodes.get_mut(index) {
            node.active = false;
            node.consensus.leader_id = None;
            node.consensus.role = ConsensusRole::Follower;
            node.won_term = None;
            node.retirement_ticks = None;
        }
    }

    pub fn restart(&mut self, index: usize) -> Result<(), ClusterError> {
        let node = self.nodes.get_mut(index).ok_or(ClusterError::UnknownNode)?;
        let replay = node.store.load_replay()?;
        node.chain = replay.chain;
        node.consensus = replay.consensus;
        node.consensus.role = ConsensusRole::Follower;
        node.consensus.leader_id = None;
        node.pending = replay.pending;
        node.head_proof = replay.head_proof;
        node.history = replay.history;
        node.sent_appends.clear();
        node.active = true;
        node.won_term = None;
        node.retirement_ticks = None;
        Ok(())
    }

    pub fn campaign(&mut self, candidate_index: usize) -> Result<CampaignOutcome, ClusterError> {
        self.ensure_active(candidate_index)?;
        let candidate_id = self.nodes[candidate_index].id;
        if !self.nodes[candidate_index]
            .chain
            .roster
            .contains(&candidate_id)
        {
            return Err(ClusterError::NotLeader);
        }
        let local_tail = self.node_tail(candidate_index)?;
        let committed_roster = self.nodes[candidate_index].chain.roster.clone();
        let term = begin_campaign(
            &mut self.nodes[candidate_index].consensus,
            candidate_id,
            &committed_roster,
            local_tail,
        )?;
        self.nodes[candidate_index]
            .store
            .write_consensus(&self.nodes[candidate_index].consensus)?;

        let request = self.signed_request_vote(candidate_index, term, local_tail);
        let self_vote = self.signed_vote(candidate_index, candidate_id, term, local_tail, true);
        let mut votes = BTreeMap::from([(candidate_id, self_vote)]);
        for voter_index in 0..self.nodes.len() {
            if voter_index == candidate_index || !self.reachable(candidate_index, voter_index) {
                continue;
            }
            if let Some(vote) = self.handle_request_vote(voter_index, &request)? {
                if self.valid_vote(candidate_index, &vote, candidate_id, term) {
                    votes.entry(vote.voter).or_insert(vote);
                }
            }
        }

        let required = majority(self.nodes[candidate_index].chain.roster.len());
        let outcome = if votes.len() >= required
            && self.nodes[candidate_index].consensus.current_term == term
        {
            let node = &mut self.nodes[candidate_index];
            node.consensus.role = ConsensusRole::Leader;
            node.consensus.leader_id = Some(candidate_id);
            node.won_term = Some(term);
            CampaignOutcome::Won {
                term,
                votes: votes.len(),
            }
        } else {
            CampaignOutcome::Lost {
                term,
                votes: votes.len(),
            }
        };
        Ok(outcome)
    }

    pub fn fresh_membership_scene(&self, leader_index: usize) -> Result<Scene, ClusterError> {
        let leader = self
            .nodes
            .get(leader_index)
            .ok_or(ClusterError::UnknownNode)?;
        if leader.consensus.role != ConsensusRole::Leader
            || leader.consensus.leader_id != Some(leader.id)
            || leader.pending.is_some()
        {
            return Err(ClusterError::NotLeader);
        }
        Ok(self.membership_scene(leader_index, leader.consensus.current_term))
    }

    pub fn fresh_view_change_scene(
        &self,
        leader_index: usize,
        add: Vec<NodeId>,
        remove: Vec<NodeId>,
    ) -> Result<Scene, ClusterError> {
        self.ensure_leader(leader_index)?;
        let leader = &self.nodes[leader_index];
        if leader.pending.is_some() {
            return Err(ClusterError::PendingConflict);
        }
        let mut next_roster = leader.chain.roster.clone();
        for node in &remove {
            next_roster.retain(|member| member != node);
        }
        next_roster.extend(add.iter().copied());
        next_roster.sort();
        next_roster.dedup();
        Ok(Scene {
            v: 1,
            room: self.room,
            n: leader.chain.head_n.expect("bootstrapped leader has head") + 1,
            term: leader.consensus.current_term,
            parent: leader.chain.head_hash,
            roster: leader.chain.roster.clone(),
            leader: leader.id,
            ts: 1_766_900_000 + leader.consensus.current_term,
            body: Body::ViewChange {
                add,
                remove,
                next_roster,
                closes_grant: None,
            },
            certs: Vec::new(),
        })
    }

    pub fn append_scene(
        &mut self,
        leader_index: usize,
        scene: Scene,
        delivery: AppendDelivery,
    ) -> Result<AppendOutcome, ClusterError> {
        self.ensure_leader(leader_index)?;
        let leader_id = self.nodes[leader_index].id;
        let rpc_term = self.nodes[leader_index].consensus.current_term;
        let append = Append {
            room: self.room,
            rpc_term,
            leader: leader_id,
            prev_n: self.nodes[leader_index]
                .chain
                .head_n
                .expect("bootstrapped cluster has a committed head"),
            prev_hash: self.nodes[leader_index]
                .chain
                .head_hash
                .expect("bootstrapped cluster has a committed hash"),
            scene,
        };
        let hash = hash_scene(&append.scene);
        if self.nodes[leader_index]
            .pending
            .as_ref()
            .is_some_and(|pending| pending.n == append.scene.n && pending.hash != hash)
        {
            return Err(ClusterError::PendingConflict);
        }
        self.nodes[leader_index].sent_appends.push((rpc_term, hash));

        let self_cert = match accept_append_on_node(
            &mut self.nodes[leader_index],
            &append,
            &ApplyResources::default(),
            true,
        )? {
            DeliveryResult::Certified(cert) => cert,
            _ => return Err(ClusterError::LeaderRefusedProposal),
        };
        let mut certs = BTreeMap::from([(self_cert.node, self_cert)]);
        for follower_index in 0..self.nodes.len() {
            if follower_index == leader_index || !self.reachable(leader_index, follower_index) {
                continue;
            }
            let result = accept_append_on_node(
                &mut self.nodes[follower_index],
                &append,
                &ApplyResources::default(),
                false,
            )?;
            match result {
                DeliveryResult::Certified(cert) => {
                    if valid_cert_message(&cert, &append.scene, rpc_term, leader_id) {
                        certs.entry(cert.node).or_insert(cert);
                    }
                }
                DeliveryResult::Nack(have) if have.n > append.prev_n => {
                    self.catch_up_node(follower_index, leader_index, have.n)?;
                    let leader = &self.nodes[leader_index];
                    if leader.consensus.current_term != rpc_term
                        || leader.consensus.role != ConsensusRole::Leader
                        || leader.consensus.leader_id != Some(leader.id)
                    {
                        return Ok(AppendOutcome::AbortedDemoted {
                            current_term: leader.consensus.current_term,
                        });
                    }
                    return Ok(AppendOutcome::InstalledExisting {
                        hash: leader
                            .chain
                            .head_hash
                            .expect("catch-up installed a committed head"),
                    });
                }
                DeliveryResult::Nack(have) if have.n < append.prev_n => {
                    self.catch_up_node(leader_index, follower_index, append.prev_n)?;
                    if let DeliveryResult::Certified(cert) = accept_append_on_node(
                        &mut self.nodes[follower_index],
                        &append,
                        &ApplyResources::default(),
                        false,
                    )? {
                        if valid_cert_message(&cert, &append.scene, rpc_term, leader_id) {
                            certs.entry(cert.node).or_insert(cert);
                        }
                    }
                }
                // Equal-height/different-hash is a protocol violation. Bare
                // have/nack terms never advance current_term.
                DeliveryResult::Nack(_)
                | DeliveryResult::RefusedStaleTerm
                | DeliveryResult::RefusedSameTermConflict
                | DeliveryResult::RefusedInvalid => {}
            }
        }

        let required = majority(self.nodes[leader_index].chain.roster.len());
        if certs.len() < required {
            return Ok(AppendOutcome::Accepted {
                certs: certs.len(),
                required,
            });
        }

        let proof = CommitProof {
            rpc_term,
            leader: leader_id,
            certs: certs.values().map(CertMessage::as_cert).collect(),
        };
        self.commit_node(leader_index, &append.scene, &proof)?;

        let recipients: Vec<usize> = match delivery {
            AppendDelivery::AllReachable => (0..self.nodes.len())
                .filter(|&index| index != leader_index && self.reachable(leader_index, index))
                .collect(),
            AppendDelivery::LeaderOnly => Vec::new(),
            AppendDelivery::Nodes(nodes) => nodes
                .into_iter()
                .filter(|&index| index != leader_index && self.reachable(leader_index, index))
                .collect(),
        };
        for recipient in recipients {
            self.commit_node(recipient, &append.scene, &proof)?;
        }

        if !self.nodes[leader_index].chain.roster.contains(&leader_id) {
            self.nodes[leader_index].retirement_ticks = Some(0);
            self.maybe_finish_self_removal(leader_index, false);
        }

        Ok(AppendOutcome::Committed { hash, rpc_term })
    }

    pub fn deliver_append(
        &mut self,
        leader_index: usize,
        follower_index: usize,
        scene: Scene,
    ) -> Result<DeliveryResult, ClusterError> {
        self.ensure_leader(leader_index)?;
        let append = Append {
            room: self.room,
            rpc_term: self.nodes[leader_index].consensus.current_term,
            leader: self.nodes[leader_index].id,
            prev_n: self.nodes[leader_index]
                .chain
                .head_n
                .expect("bootstrapped cluster has a head"),
            prev_hash: self.nodes[leader_index]
                .chain
                .head_hash
                .expect("bootstrapped cluster has a hash"),
            scene,
        };
        accept_append_on_node(
            self.nodes
                .get_mut(follower_index)
                .ok_or(ClusterError::UnknownNode)?,
            &append,
            &ApplyResources::default(),
            false,
        )
    }

    /// Receives a commit push or a `scene` returned by `get_scenes`.
    pub fn deliver_commit(
        &mut self,
        recipient_index: usize,
        message: &CommitMessage,
    ) -> Result<(), ClusterError> {
        if message.room != self.room
            || message.scene.room != message.room
            || message.scene.n != message.n
            || hash_scene(&message.scene) != message.hash
        {
            return Err(ClusterError::InvalidCommitMessage);
        }
        self.commit_node(recipient_index, &message.scene, &message.proof())
    }

    pub fn win_probe(
        &mut self,
        leader_index: usize,
        delivery: AppendDelivery,
    ) -> Result<WinOutcome, ClusterError> {
        self.ensure_leader(leader_index)?;
        let won_term = self.nodes[leader_index]
            .won_term
            .ok_or(ClusterError::NotLeader)?;
        let old_head = self.nodes[leader_index].chain.head_n;
        let local_pending_n = self.nodes[leader_index]
            .pending
            .as_ref()
            .map(|pending| pending.n);

        let mut source = None;
        for peer in 0..self.nodes.len() {
            if peer == leader_index || !self.reachable(leader_index, peer) {
                continue;
            }
            let peer_head = self.nodes[peer].chain.head_n;
            let is_ahead = match (old_head, peer_head) {
                (None, Some(_)) => true,
                (Some(local), Some(remote)) => remote > local,
                _ => false,
            };
            let resolves_pending = peer_head.is_some() && peer_head == local_pending_n;
            if (is_ahead || resolves_pending)
                && source.is_none_or(|prior: usize| {
                    self.nodes[peer].chain.head_n > self.nodes[prior].chain.head_n
                })
            {
                source = Some(peer);
            }
        }

        if let Some(source) = source {
            let local_head = self.nodes[leader_index].chain.head_n;
            let records: Vec<_> = self.nodes[source]
                .history
                .iter()
                .filter(|record| local_head.is_none_or(|head| record.scene.n > head))
                .cloned()
                .collect();
            for record in records {
                self.commit_node(leader_index, &record.scene, &record.commit_proof)?;
            }
        }

        let node = &self.nodes[leader_index];
        if node.consensus.current_term != won_term
            || node.consensus.role != ConsensusRole::Leader
            || node.consensus.leader_id != Some(node.id)
        {
            return Ok(WinOutcome::AbortedDemoted {
                current_term: node.consensus.current_term,
            });
        }
        if self.nodes[leader_index].chain.head_n != old_head {
            return Ok(WinOutcome::InstalledExisting {
                hash: self.nodes[leader_index]
                    .chain
                    .head_hash
                    .expect("installed commit has a hash"),
            });
        }

        let Some(scene) = self.nodes[leader_index]
            .pending
            .as_ref()
            .map(|pending| pending.scene.clone())
        else {
            return Ok(WinOutcome::NoPending);
        };
        match self.append_scene(leader_index, scene, delivery)? {
            AppendOutcome::Committed { hash, rpc_term } => {
                Ok(WinOutcome::CommittedCarried { hash, rpc_term })
            }
            AppendOutcome::Accepted { certs, required } => {
                Ok(WinOutcome::AcceptedCarried { certs, required })
            }
            AppendOutcome::InstalledExisting { hash } => Ok(WinOutcome::InstalledExisting { hash }),
            AppendOutcome::AbortedDemoted { current_term } => {
                Ok(WinOutcome::AbortedDemoted { current_term })
            }
        }
    }

    /// Test-cluster injection for the exact Win-abort receive edge: create a
    /// cryptographically valid higher-term commit on one peer, without using
    /// the local candidate's Win path, so its probe must install then demote.
    pub fn inject_verified_commit_for_probe(
        &mut self,
        peer_index: usize,
        rpc_term: u64,
    ) -> Result<Hash32, ClusterError> {
        if rpc_term == 0 {
            return Err(ClusterError::InvalidInjectedTerm);
        }
        self.inject_verified_commit(peer_index, &[peer_index], rpc_term)
    }

    pub fn inject_verified_commit(
        &mut self,
        proposer_index: usize,
        recipients: &[usize],
        rpc_term: u64,
    ) -> Result<Hash32, ClusterError> {
        if rpc_term == 0 {
            return Err(ClusterError::InvalidInjectedTerm);
        }
        self.ensure_active(proposer_index)?;
        let scene = self.membership_scene(proposer_index, rpc_term);
        let signer_indices: Vec<_> = scene
            .roster
            .iter()
            .filter_map(|node| self.index_of(*node))
            .collect();
        let proof = self.make_proof(
            &scene,
            rpc_term,
            self.nodes[proposer_index].id,
            &signer_indices,
        );
        let hash = hash_scene(&scene);
        for &recipient in recipients {
            self.commit_node(recipient, &scene, &proof)?;
        }
        Ok(hash)
    }

    fn bootstrap_ledger(&mut self) -> Result<(), ClusterError> {
        let creator = self.nodes[0].id;
        let genesis = Scene {
            v: 1,
            room: self.room,
            n: 0,
            term: 1,
            parent: None,
            roster: vec![creator],
            leader: creator,
            ts: 1_766_700_000,
            body: Body::Genesis {
                name: "cluster".into(),
                stake: crate::types::StakePolicy::default(),
                floor: FloorConfig::stick(30),
                creator_node: creator,
                parent_room: None,
                token_sha256: None,
            },
            certs: Vec::new(),
        };
        let proof = self.make_proof(&genesis, 1, creator, &[0]);
        self.commit_everywhere(&genesis, &proof)?;

        for added_index in 1..self.nodes.len() {
            let current = self.nodes[0].chain.clone();
            let mut next_roster = current.roster.clone();
            next_roster.push(self.nodes[added_index].id);
            next_roster.sort();
            let scene = Scene {
                v: 1,
                room: self.room,
                n: current.head_n.expect("genesis committed") + 1,
                term: 1,
                parent: current.head_hash,
                roster: current.roster.clone(),
                leader: creator,
                ts: 1_766_700_000 + added_index as u64,
                body: Body::ViewChange {
                    add: vec![self.nodes[added_index].id],
                    remove: Vec::new(),
                    next_roster,
                    closes_grant: None,
                },
                certs: Vec::new(),
            };
            let signers: Vec<_> = current
                .roster
                .iter()
                .filter_map(|node| self.index_of(*node))
                .collect();
            let proof = self.make_proof(&scene, 1, creator, &signers);
            self.commit_everywhere(&scene, &proof)?;
        }
        Ok(())
    }

    fn commit_everywhere(
        &mut self,
        scene: &Scene,
        proof: &CommitProof,
    ) -> Result<(), ClusterError> {
        for index in 0..self.nodes.len() {
            self.commit_node(index, scene, proof)?;
        }
        Ok(())
    }

    fn membership_scene(&self, leader_index: usize, term: u64) -> Scene {
        let node = &self.nodes[leader_index];
        Scene {
            v: 1,
            room: self.room,
            n: node.chain.head_n.expect("bootstrapped cluster has head") + 1,
            term,
            parent: node.chain.head_hash,
            roster: node.chain.roster.clone(),
            leader: node.id,
            ts: 1_766_800_000 + term,
            body: Body::Membership {
                stake: node
                    .chain
                    .stake
                    .clone()
                    .expect("bootstrapped cluster has stake policy"),
                floor: FloorConfig {
                    mode: node
                        .chain
                        .floor_mode
                        .expect("bootstrapped cluster has floor mode"),
                    timeout_secs: node
                        .chain
                        .timeout_secs
                        .expect("bootstrapped cluster has floor timeout"),
                    moderator: node.chain.moderator.clone(),
                },
                closes_grant: None,
            },
            certs: Vec::new(),
        }
    }

    fn make_proof(
        &self,
        scene: &Scene,
        rpc_term: u64,
        leader: NodeId,
        signer_indices: &[usize],
    ) -> CommitProof {
        let hash = hash_scene(scene);
        let mut certs = signer_indices
            .iter()
            .map(|&index| {
                let node = self.nodes[index].id;
                let digest = cert_digest(
                    &scene.room,
                    scene.n,
                    hash.as_bytes(),
                    rpc_term,
                    &leader,
                    &node,
                );
                Cert::node(
                    node,
                    SignatureBytes::from_bytes(sign(&self.nodes[index].signing_key, &digest)),
                )
            })
            .collect::<Vec<_>>();
        if scene.n == 0 {
            certs.push(Cert::room(SignatureBytes::from_bytes(sign(
                &self.room_key,
                hash.as_bytes(),
            ))));
        }
        CommitProof {
            rpc_term,
            leader,
            certs,
        }
    }

    fn commit_node(
        &mut self,
        index: usize,
        scene: &Scene,
        proof: &CommitProof,
    ) -> Result<(), ClusterError> {
        let node = self.nodes.get_mut(index).ok_or(ClusterError::UnknownNode)?;
        let hash = hash_scene(scene);
        if node.chain.head_n.is_some_and(|head_n| scene.n <= head_n) {
            if node
                .history
                .iter()
                .any(|record| record.scene.n == scene.n && hash_scene(&record.scene) == hash)
            {
                return Ok(());
            }
            // A stale different hash is a protocol violation, but committed
            // history wins and the message is ignored (§10 step 9).
            return Ok(());
        }
        let already_committed =
            node.chain.head_n == Some(scene.n) && node.chain.head_hash == Some(hash);
        let next = node
            .store
            .persist_committed_scene(&node.chain, scene, proof)?;
        node.store.unlink_pending_if_stale(next.head_n)?;
        node.chain = next;
        if node
            .pending
            .as_ref()
            .is_some_and(|pending| node.chain.head_n.is_some_and(|head_n| pending.n <= head_n))
        {
            node.pending = None;
        }
        if !already_committed {
            node.history.push(CommittedScene {
                scene: scene.clone(),
                commit_proof: proof.clone(),
            });
        }
        node.head_proof = Some(proof.clone());
        if advance_term(
            &mut node.consensus,
            node.pending.as_ref(),
            node.head_proof.as_ref(),
            AdvanceSource::VerifiedProof(proof.rpc_term),
        ) {
            node.won_term = None;
            node.store.write_consensus(&node.consensus)?;
        }
        Ok(())
    }

    fn handle_request_vote(
        &mut self,
        voter_index: usize,
        request: &RequestVote,
    ) -> Result<Option<Vote>, ClusterError> {
        let voter = self
            .nodes
            .get_mut(voter_index)
            .ok_or(ClusterError::UnknownNode)?;
        if !voter.active
            || request.room != voter.chain.room.expect("bootstrapped node has room")
            || !voter.chain.roster.contains(&voter.id)
            || !voter.chain.roster.contains(&request.candidate)
            || !valid_request_vote(request)
        {
            return Ok(None);
        }

        if request.rpc_term > voter.consensus.current_term
            && advance_term(
                &mut voter.consensus,
                voter.pending.as_ref(),
                voter.head_proof.as_ref(),
                AdvanceSource::RosterMessage(request.rpc_term),
            )
        {
            voter.won_term = None;
            voter.store.write_consensus(&voter.consensus)?;
        }
        if request.rpc_term != voter.consensus.current_term
            || voter
                .consensus
                .voted_for
                .is_some_and(|voted| voted != request.candidate)
        {
            return Ok(None);
        }
        let voter_tail = node_tail(voter)?;
        if !up_to_date(&request.tail(), &voter_tail) {
            return Ok(None);
        }

        voter.consensus.voted_for = Some(request.candidate);
        voter.store.write_consensus(&voter.consensus)?;
        Ok(Some(signed_vote(
            voter,
            request.candidate,
            request.rpc_term,
            voter_tail,
            true,
        )))
    }

    fn signed_request_vote(&self, index: usize, rpc_term: u64, tail: Tail) -> RequestVote {
        let node = &self.nodes[index];
        let mut request = RequestVote {
            room: self.room,
            rpc_term,
            candidate: node.id,
            last_n: tail.last_n,
            last_hash: tail.last_hash,
            last_rpc: tail.last_rpc,
            sig: SignatureBytes::from_bytes([0; 64]),
        };
        let digest =
            signed_object_digest(&serde_json::to_value(&request).expect("request_vote serializes"));
        request.sig = SignatureBytes::from_bytes(sign(&node.signing_key, &digest));
        request
    }

    fn signed_vote(
        &self,
        index: usize,
        candidate: NodeId,
        rpc_term: u64,
        tail: Tail,
        grant: bool,
    ) -> Vote {
        signed_vote(&self.nodes[index], candidate, rpc_term, tail, grant)
    }

    fn valid_vote(
        &self,
        candidate_index: usize,
        vote: &Vote,
        candidate: NodeId,
        rpc_term: u64,
    ) -> bool {
        vote.grant
            && vote.candidate == candidate
            && vote.rpc_term == rpc_term
            && self.nodes[candidate_index]
                .chain
                .roster
                .contains(&vote.voter)
            && verifying_key(vote.voter).is_some_and(|key| {
                verify(
                    &key,
                    &signed_object_digest(&serde_json::to_value(vote).expect("vote serializes")),
                    vote.sig.as_bytes(),
                )
            })
    }

    fn node_tail(&self, index: usize) -> Result<Tail, ClusterError> {
        node_tail(self.nodes.get(index).ok_or(ClusterError::UnknownNode)?)
    }

    fn index_of(&self, id: NodeId) -> Option<usize> {
        self.nodes.iter().position(|node| node.id == id)
    }

    fn ensure_active(&self, index: usize) -> Result<(), ClusterError> {
        let node = self.nodes.get(index).ok_or(ClusterError::UnknownNode)?;
        if node.active {
            Ok(())
        } else {
            Err(ClusterError::Offline)
        }
    }

    fn ensure_leader(&self, index: usize) -> Result<(), ClusterError> {
        self.ensure_active(index)?;
        let node = &self.nodes[index];
        if node.consensus.role == ConsensusRole::Leader
            && node.consensus.leader_id == Some(node.id)
            && node.won_term == Some(node.consensus.current_term)
            && node.chain.roster.contains(&node.id)
        {
            Ok(())
        } else {
            Err(ClusterError::NotLeader)
        }
    }

    fn reachable(&self, from: usize, to: usize) -> bool {
        self.nodes
            .get(from)
            .zip(self.nodes.get(to))
            .is_some_and(|(from_node, to_node)| {
                from_node.active && to_node.active && self.links[from][to]
            })
    }

    fn deliver_heartbeat(
        &mut self,
        leader_index: usize,
        follower_index: usize,
        heartbeat: &Heartbeat,
    ) -> Result<(), ClusterError> {
        let local_n = {
            let follower = self
                .nodes
                .get_mut(follower_index)
                .ok_or(ClusterError::UnknownNode)?;
            if !follower.active
                || heartbeat.room != follower.chain.room.expect("bootstrapped node has room")
                || !follower.chain.roster.contains(&heartbeat.leader)
            {
                return Ok(());
            }
            if heartbeat.rpc_term > follower.consensus.current_term
                && advance_term(
                    &mut follower.consensus,
                    follower.pending.as_ref(),
                    follower.head_proof.as_ref(),
                    AdvanceSource::RosterMessage(heartbeat.rpc_term),
                )
            {
                follower.won_term = None;
                follower.store.write_consensus(&follower.consensus)?;
            }
            if heartbeat.rpc_term != follower.consensus.current_term {
                return Ok(());
            }
            follower.consensus.role = ConsensusRole::Follower;
            follower.consensus.leader_id = Some(heartbeat.leader);
            follower.won_term = None;
            follower
                .chain
                .head_n
                .expect("bootstrapped follower has head")
        };

        if heartbeat.n > local_n {
            let records: Vec<_> = self.nodes[leader_index]
                .history
                .iter()
                .filter(|record| record.scene.n > local_n && record.scene.n <= heartbeat.n)
                .cloned()
                .collect();
            for record in records {
                self.commit_node(follower_index, &record.scene, &record.commit_proof)?;
            }
        }
        Ok(())
    }

    fn catch_up_node(
        &mut self,
        source_index: usize,
        recipient_index: usize,
        through_n: u64,
    ) -> Result<(), ClusterError> {
        let local_n = self.nodes[recipient_index]
            .chain
            .head_n
            .expect("bootstrapped recipient has head");
        let records: Vec<_> = self.nodes[source_index]
            .history
            .iter()
            .filter(|record| record.scene.n > local_n && record.scene.n <= through_n)
            .cloned()
            .collect();
        for record in records {
            self.commit_node(recipient_index, &record.scene, &record.commit_proof)?;
        }
        Ok(())
    }

    fn maybe_finish_self_removal(&mut self, leader_index: usize, advance_tick: bool) {
        let hash = self.nodes[leader_index]
            .chain
            .head_hash
            .expect("retiring leader has committed removal");
        let next_roster = self.nodes[leader_index].chain.roster.clone();
        let acknowledgements = next_roster
            .iter()
            .filter(|node_id| {
                self.index_of(**node_id).is_some_and(|index| {
                    self.nodes[index].active && self.nodes[index].chain.head_hash == Some(hash)
                })
            })
            .count();
        let elapsed = self.nodes[leader_index]
            .retirement_ticks
            .unwrap_or_default();
        if acknowledgements >= majority(next_roster.len()) || elapsed >= 6 {
            let node = &mut self.nodes[leader_index];
            node.consensus.role = ConsensusRole::Follower;
            node.consensus.leader_id = None;
            node.won_term = None;
            node.retirement_ticks = None;
        } else if advance_tick {
            self.nodes[leader_index].retirement_ticks = Some(elapsed + 1);
        }
    }
}

fn node_tail(node: &TestNode) -> Result<Tail, ClusterError> {
    Ok(tail(
        node.pending.as_ref(),
        node.chain.head_n.zip(node.chain.head_hash),
        node.head_proof.as_ref(),
    )?)
}

fn signed_vote(
    voter: &TestNode,
    candidate: NodeId,
    rpc_term: u64,
    tail: Tail,
    grant: bool,
) -> Vote {
    let mut vote = Vote {
        room: voter.chain.room.expect("bootstrapped voter has room"),
        rpc_term,
        voter: voter.id,
        candidate,
        last_n: tail.last_n,
        last_hash: tail.last_hash,
        last_rpc: tail.last_rpc,
        grant,
        sig: SignatureBytes::from_bytes([0; 64]),
    };
    let digest = signed_object_digest(&serde_json::to_value(&vote).expect("vote is serializable"));
    vote.sig = SignatureBytes::from_bytes(sign(&voter.signing_key, &digest));
    vote
}

fn valid_request_vote(request: &RequestVote) -> bool {
    verifying_key(request.candidate).is_some_and(|key| {
        verify(
            &key,
            &signed_object_digest(&serde_json::to_value(request).expect("request_vote serializes")),
            request.sig.as_bytes(),
        )
    })
}

fn accept_append_on_node(
    node: &mut TestNode,
    append: &Append,
    resources: &ApplyResources,
    preserve_leader: bool,
) -> Result<DeliveryResult, ClusterError> {
    if !node.active
        || append.room != node.chain.room.expect("bootstrapped node has room")
        || !node.chain.roster.contains(&node.id)
        || !node.chain.roster.contains(&append.leader)
    {
        return Ok(DeliveryResult::RefusedInvalid);
    }
    if append.rpc_term > node.consensus.current_term
        && advance_term(
            &mut node.consensus,
            node.pending.as_ref(),
            node.head_proof.as_ref(),
            AdvanceSource::RosterMessage(append.rpc_term),
        )
    {
        node.won_term = None;
        node.store.write_consensus(&node.consensus)?;
    }
    if append.rpc_term != node.consensus.current_term {
        return Ok(DeliveryResult::RefusedStaleTerm);
    }
    if !preserve_leader {
        node.consensus.role = ConsensusRole::Follower;
        node.consensus.leader_id = Some(append.leader);
        node.won_term = None;
    }
    if node.chain.head_n != Some(append.prev_n) || node.chain.head_hash != Some(append.prev_hash) {
        return Ok(DeliveryResult::Nack(crate::consensus::Have {
            n: node
                .chain
                .head_n
                .expect("bootstrapped node has committed n"),
            hash: node
                .chain
                .head_hash
                .expect("bootstrapped node has committed hash"),
            rpc_term: node
                .head_proof
                .as_ref()
                .expect("committed head has proof")
                .rpc_term,
        }));
    }

    let hash = hash_scene(&append.scene);
    if let Some(pending) = &node.pending {
        if pending.hash == hash {
            if append.rpc_term < pending.accepted_rpc_term {
                return Ok(DeliveryResult::RefusedStaleTerm);
            }
            if append.rpc_term == pending.accepted_rpc_term {
                if pending.accepted_leader != append.leader {
                    return Ok(DeliveryResult::RefusedSameTermConflict);
                }
                return Ok(DeliveryResult::Certified(cert_message_from_pending(
                    pending,
                )));
            }
        } else if preserve_leader || append.rpc_term <= pending.accepted_rpc_term {
            return Ok(DeliveryResult::RefusedSameTermConflict);
        } else {
            apply(
                &node.chain,
                &append.scene,
                None,
                ApplyMode::Precert(resources),
            )?;
        }
    } else {
        apply(
            &node.chain,
            &append.scene,
            None,
            ApplyMode::Precert(resources),
        )?;
    }

    let digest = cert_digest(
        &append.room,
        append.scene.n,
        hash.as_bytes(),
        append.rpc_term,
        &append.leader,
        &node.id,
    );
    let cert = Cert::node(
        node.id,
        SignatureBytes::from_bytes(sign(&node.signing_key, &digest)),
    );
    let pending = Pending {
        n: append.scene.n,
        hash,
        scene: append.scene.clone(),
        accepted_rpc_term: append.rpc_term,
        accepted_leader: append.leader,
        cert,
    };
    node.store.write_pending(&pending)?;
    let message = cert_message_from_pending(&pending);
    node.pending = Some(pending);
    Ok(DeliveryResult::Certified(message))
}

fn cert_message_from_pending(pending: &Pending) -> CertMessage {
    let (node, sig) = match &pending.cert.node {
        CertSigner::Node(node) => (*node, pending.cert.sig),
        CertSigner::Room => unreachable!("pending cert is always a node cert"),
    };
    CertMessage {
        room: pending.scene.room,
        n: pending.n,
        hash: pending.hash,
        rpc_term: pending.accepted_rpc_term,
        leader: pending.accepted_leader,
        node,
        sig,
    }
}

fn valid_cert_message(cert: &CertMessage, scene: &Scene, rpc_term: u64, leader: NodeId) -> bool {
    if cert.room != scene.room
        || cert.n != scene.n
        || cert.hash != hash_scene(scene)
        || cert.rpc_term != rpc_term
        || cert.leader != leader
        || !scene.roster.contains(&cert.node)
    {
        return false;
    }
    verifying_key(cert.node).is_some_and(|key| {
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

fn hash_scene(scene: &Scene) -> Hash32 {
    Hash32::from_bytes(scene_hash(
        &serde_json::to_value(scene).expect("typed scene is serializable"),
    ))
}

fn verifying_key(node: NodeId) -> Option<VerifyingKey> {
    VerifyingKey::from_bytes(node.as_bytes()).ok()
}

fn majority(roster_len: usize) -> usize {
    roster_len / 2 + 1
}
