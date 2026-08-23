use thiserror::Error;

pub use crate::types::ConsensusRole;
use crate::types::{CommitProof, ConsensusState, Hash32, NodeId, Pending};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tail {
    pub last_rpc: u64,
    pub last_n: u64,
    pub last_hash: Hash32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// The caller must fsync the returned self-vote before sending request_vote.
pub fn begin_campaign(
    state: &mut ConsensusState,
    self_node: NodeId,
    local_tail: Tail,
) -> Result<u64, ConsensusError> {
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
