use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ticket::Declaration;
pub use crate::types::ConsensusRole;
use crate::types::{
    BlobRef, Cert, CommitProof, ConsensusState, Hash32, Intent, NodeId, Pending, RoomId, Scene,
    SignatureBytes,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tail {
    pub last_rpc: u64,
    pub last_n: u64,
    pub last_hash: Hash32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Have {
    pub n: u64,
    pub hash: Hash32,
    pub rpc_term: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HaveObservation {
    pub needs_catch_up: bool,
    /// Informational only. This value must not be passed to `advance_term`
    /// until a corresponding proof is fetched, verified, and installed.
    pub advertised_rpc: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub node: NodeId,
    pub r#pub: NodeId,
    pub addrs: Vec<String>,
    pub decl: Vec<Declaration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    pub room: RoomId,
    pub token: Hash32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerInfo {
    pub node: NodeId,
    pub addrs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pex {
    pub peers: Vec<PeerInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HaveMessage {
    pub room: RoomId,
    pub n: u64,
    pub hash: Hash32,
    pub rpc_term: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestVote {
    pub room: RoomId,
    pub rpc_term: u64,
    pub candidate: NodeId,
    pub last_n: u64,
    pub last_hash: Hash32,
    pub last_rpc: u64,
    pub sig: SignatureBytes,
}

impl RequestVote {
    pub fn tail(&self) -> Tail {
        Tail {
            last_rpc: self.last_rpc,
            last_n: self.last_n,
            last_hash: self.last_hash,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vote {
    pub room: RoomId,
    pub rpc_term: u64,
    pub voter: NodeId,
    pub candidate: NodeId,
    pub last_n: u64,
    pub last_hash: Hash32,
    pub last_rpc: u64,
    pub grant: bool,
    pub sig: SignatureBytes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Append {
    pub room: RoomId,
    pub rpc_term: u64,
    pub leader: NodeId,
    pub prev_n: u64,
    pub prev_hash: Hash32,
    pub scene: Scene,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertMessage {
    pub room: RoomId,
    pub n: u64,
    pub hash: Hash32,
    pub rpc_term: u64,
    pub leader: NodeId,
    pub node: NodeId,
    pub sig: SignatureBytes,
}

impl CertMessage {
    pub fn as_cert(&self) -> Cert {
        Cert::node(self.node, self.sig)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitMessage {
    pub room: RoomId,
    pub n: u64,
    pub hash: Hash32,
    pub rpc_term: u64,
    pub leader: NodeId,
    pub certs: Vec<Cert>,
    pub scene: Scene,
}

impl CommitMessage {
    pub fn proof(&self) -> CommitProof {
        CommitProof {
            rpc_term: self.rpc_term,
            leader: self.leader,
            certs: self.certs.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    pub room: RoomId,
    pub rpc_term: u64,
    pub leader: NodeId,
    pub n: u64,
    pub hash: Hash32,
    pub have_rpc: u64,
}

impl Heartbeat {
    pub fn have(&self) -> Have {
        Have {
            n: self.n,
            hash: self.hash,
            rpc_term: self.have_rpc,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Nack {
    pub room: RoomId,
    pub have_n: u64,
    pub have_hash: Hash32,
    pub have_rpc: u64,
}

impl Nack {
    pub fn have(&self) -> Have {
        Have {
            n: self.have_n,
            hash: self.have_hash,
            rpc_term: self.have_rpc,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetScenes {
    pub room: RoomId,
    pub from_n: u64,
    pub to_n: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Freeze {
    pub room: RoomId,
    pub grant_hash: Hash32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseTake {
    pub room: RoomId,
    pub grant_hash: Hash32,
    pub text: String,
    pub rev: u64,
    pub blobs: Vec<BlobRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "typ", rename_all = "snake_case")]
pub enum SwarmMsg {
    Hello(Hello),
    Auth(Auth),
    Pex(Pex),
    Have(HaveMessage),
    RequestVote(RequestVote),
    Vote(Vote),
    Append(Append),
    Cert(CertMessage),
    Commit(CommitMessage),
    Heartbeat(Heartbeat),
    Nack(Nack),
    GetScenes(GetScenes),
    Scene(crate::types::CommittedScene),
    Intent(Intent),
    Freeze(Freeze),
    CloseTake(CloseTake),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvanceSource {
    VerifiedProof(u64),
    RosterMessage(u64),
    RecoveredPersistentState(u64),
}

impl AdvanceSource {
    fn rpc_term(self) -> u64 {
        match self {
            Self::VerifiedProof(term)
            | Self::RosterMessage(term)
            | Self::RecoveredPersistentState(term) => term,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConsensusError {
    #[error("committed head and proof are required when there is no pending entry")]
    MissingTail,
    #[error("consensus term overflow")]
    TermOverflow,
    #[error("node is not in the committed roster")]
    NotInRoster,
}

/// Apply the v1.6 term floor. A `true` result is the demotion signal: callers
/// must persist this state and abort any lower-term Win/append before sending.
pub fn advance_term(
    state: &mut ConsensusState,
    pending: Option<&Pending>,
    head_proof: Option<&CommitProof>,
    source: AdvanceSource,
) -> bool {
    let floor = [
        Some(source.rpc_term()),
        pending.map(|pending| pending.accepted_rpc_term),
        head_proof.map(|proof| proof.rpc_term),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(0);

    if floor <= state.current_term {
        return false;
    }

    state.current_term = floor;
    state.voted_for = None;
    state.leader_id = None;
    state.role = ConsensusRole::Follower;
    true
}

pub fn tail(
    pending: Option<&Pending>,
    committed_head: Option<(u64, Hash32)>,
    head_proof: Option<&CommitProof>,
) -> Result<Tail, ConsensusError> {
    if let Some(pending) = pending {
        return Ok(Tail {
            last_rpc: pending.accepted_rpc_term,
            last_n: pending.n,
            last_hash: pending.hash,
        });
    }

    let ((last_n, last_hash), proof) = committed_head
        .zip(head_proof)
        .ok_or(ConsensusError::MissingTail)?;
    Ok(Tail {
        last_rpc: proof.rpc_term,
        last_n,
        last_hash,
    })
}

/// Candidate freshness comparator from §11.2. Hashes are compared only when
/// both the accepted/proof term and height are equal.
pub fn up_to_date(candidate: &Tail, voter: &Tail) -> bool {
    candidate.last_rpc > voter.last_rpc
        || (candidate.last_rpc == voter.last_rpc
            && (candidate.last_n > voter.last_n
                || (candidate.last_n == voter.last_n && candidate.last_hash == voter.last_hash)))
}

/// Begin a campaign in a term strictly above both persistent term and tail.
/// Removed nodes and observers cannot enter candidate state. The caller must
/// fsync the returned self-vote before sending request_vote.
pub fn begin_campaign(
    state: &mut ConsensusState,
    self_node: NodeId,
    committed_roster: &[NodeId],
    local_tail: Tail,
) -> Result<u64, ConsensusError> {
    if !committed_roster.contains(&self_node) {
        return Err(ConsensusError::NotInRoster);
    }
    let next = state
        .current_term
        .max(local_tail.last_rpc)
        .checked_add(1)
        .ok_or(ConsensusError::TermOverflow)?;
    state.current_term = next;
    state.voted_for = Some(self_node);
    state.leader_id = None;
    state.role = ConsensusRole::Candidate;
    Ok(next)
}

/// Process only the catch-up information in a bare `have`/nack. Deliberately
/// does not receive mutable consensus state, so an advertised term cannot
/// demote a node before its proof is installed.
pub fn observe_have(local_head_n: Option<u64>, have: &Have) -> HaveObservation {
    HaveObservation {
        needs_catch_up: local_head_n.is_none_or(|local| have.n > local),
        advertised_rpc: have.rpc_term,
    }
}
