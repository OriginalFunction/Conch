use std::collections::{BTreeMap, BTreeSet};

use ed25519_dalek::VerifyingKey;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    encoding::{cert_digest, scene_hash, signed_object_digest, verify},
    floor::intent_supersedes,
    ticket::Ticket,
    types::{
        Body, CertSigner, ChainState, CommitProof, FloorConfig, FloorMode, GrantReason, Hash32,
        Intent, LiveGrant, Mouth, NodeId, Scene,
    },
};

const MAX_BLOB_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct ApplyResources {
    /// Validated gossip candidates known to this node. `apply` still verifies
    /// signatures before using one for queue ordering.
    pub intents: Vec<Intent>,
    /// Stream-verified materialized blobs keyed by their advertised digest.
    pub blobs: BTreeMap<Hash32, VerifiedBlob>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedBlob {
    pub sha256: Hash32,
    pub bytes: u64,
}

impl VerifiedBlob {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            sha256: Hash32::from_bytes(Sha256::digest(bytes).into()),
            bytes: bytes.len() as u64,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ApplyMode<'a> {
    Precert(&'a ApplyResources),
    Commit(&'a ApplyResources),
    Staged,
}

impl<'a> ApplyMode<'a> {
    fn resources(self) -> Option<&'a ApplyResources> {
        match self {
            Self::Precert(resources) | Self::Commit(resources) => Some(resources),
            Self::Staged => None,
        }
    }

    fn validates_proof(self) -> bool {
        matches!(self, Self::Commit(_) | Self::Staged)
    }

    fn advances_head(self) -> bool {
        matches!(self, Self::Commit(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ApplyError {
    #[error("scene version must be 1")]
    InvalidVersion,
    #[error("scene height is not the next chain height")]
    InvalidHeight,
    #[error("scene belongs to a different room")]
    RoomMismatch,
    #[error("scene parent does not match the committed head")]
    ParentMismatch,
    #[error("scene roster does not equal the roster derived from committed state")]
    RosterMismatch,
    #[error("scene roster must be non-empty, sorted, and unique")]
    NonCanonicalRoster,
    #[error("scene leader is not in the roster")]
    LeaderNotInRoster,
    #[error("scene term is invalid or recedes below the committed scene term")]
    InvalidSceneTerm,
    #[error("genesis envelope or body is invalid")]
    InvalidGenesis,
    #[error("floor configuration is invalid")]
    InvalidFloorConfig,
    #[error("body is not legal in the current floor state")]
    InvalidFloorTransition,
    #[error("grant target is not a roster node")]
    GrantTargetNotRoster,
    #[error("intent was already consumed")]
    ConsumedIntent,
    #[error("grant intent bytes are not available")]
    MissingIntent,
    #[error("intent signature or envelope is invalid")]
    InvalidIntent,
    #[error("grant target does not match its intent")]
    IntentTargetMismatch,
    #[error("grant scene timestamp is not before intent expiry")]
    IntentExpired,
    #[error("grant intent is not the deterministic queue head")]
    IntentNotQueueHead,
    #[error("view-change must contain exactly one valid add or remove")]
    InvalidViewChange,
    #[error("breakout auto_join must be a subset of the current roster")]
    InvalidAutoJoin,
    #[error("breakout must contain a valid child ticket whose parent is this room")]
    InvalidBreakoutTicket,
    #[error("commit or staged validation requires a commit proof")]
    MissingCommitProof,
    #[error("commit proof leader is not in the derived roster")]
    ProofLeaderNotRoster,
    #[error("commit proof contains duplicate signer {0}")]
    DuplicateCert(String),
    #[error("commit proof contains a signer outside the derived roster")]
    CertSignerNotRoster,
    #[error("invalid commit certificate signature from {0}")]
    InvalidCertSignature(NodeId),
    #[error("invalid genesis room-key signature")]
    InvalidRoomSignature,
    #[error("genesis proof is missing the room-key signature")]
    MissingRoomSignature,
    #[error("commit proof has {actual} node certs but needs {required}")]
    InsufficientCerts { actual: usize, required: usize },
    #[error("blob exceeds the 32 MiB limit")]
    BlobTooLarge,
    #[error("blob {0} is not materialized")]
    MissingBlob(Hash32),
    #[error("blob {0} length does not match the scene")]
    BlobLengthMismatch(Hash32),
    #[error("blob {0} digest does not match its bytes")]
    BlobHashMismatch(Hash32),
    #[error("stale conflicting commit at an already committed height")]
    StaleConflictingCommit,
}

/// Applies one deterministic ledger transition.
///
/// For a scene below `state.head_n`, callers resolve idempotency against their
/// committed scene store; the reducer only has the current head hash and
/// therefore reports [`ApplyError::StaleConflictingCommit`].
pub fn apply(
    state: &ChainState,
    scene: &Scene,
    proof: Option<&CommitProof>,
    mode: ApplyMode<'_>,
) -> Result<ChainState, ApplyError> {
    let hash = hash_scene(scene);

    if mode.validates_proof() {
        if let Some(head_n) = state.head_n {
            if scene.n <= head_n {
                if scene.n == head_n && state.head_hash == Some(hash) {
                    return Ok(state.clone());
                }
                return Err(ApplyError::StaleConflictingCommit);
            }
        }
    }

    validate_envelope(state, scene)?;
    validate_body(state, scene)?;

    if let ApplyMode::Precert(resources) = mode {
        validate_grant_intent(state, scene, resources)?;
    }

    if mode.validates_proof() {
        validate_proof(scene, proof.ok_or(ApplyError::MissingCommitProof)?, hash)?;
    }

    if let Some(resources) = mode.resources() {
        validate_blobs(scene, resources)?;
    }

    if !mode.advances_head() {
        return Ok(state.clone());
    }

    Ok(transition(state, scene, hash))
}

fn hash_scene(scene: &Scene) -> Hash32 {
    let value = serde_json::to_value(scene).expect("typed scene is JSON-serializable");
    Hash32::from_bytes(scene_hash(&value))
}

fn validate_envelope(state: &ChainState, scene: &Scene) -> Result<(), ApplyError> {
    if scene.v != 1 {
        return Err(ApplyError::InvalidVersion);
    }
    if !is_sorted_unique(&scene.roster) || scene.roster.is_empty() {
        return Err(ApplyError::NonCanonicalRoster);
    }

    if state.is_empty() {
        if scene.n != 0 || scene.parent.is_some() {
            return Err(ApplyError::InvalidHeight);
        }
        let creator = match &scene.body {
            Body::Genesis { creator_node, .. } => *creator_node,
            _ => return Err(ApplyError::InvalidGenesis),
        };
        if scene.term != 1 || scene.roster.as_slice() != [creator] || scene.leader != creator {
            return Err(ApplyError::InvalidGenesis);
        }
    } else {
        if state.room != Some(scene.room) {
            return Err(ApplyError::RoomMismatch);
        }
        if scene.n != state.head_n.expect("non-empty state has head") + 1 {
            return Err(ApplyError::InvalidHeight);
        }
        if scene.parent != state.head_hash {
            return Err(ApplyError::ParentMismatch);
        }
        if scene.roster != state.roster {
            return Err(ApplyError::RosterMismatch);
        }
        if matches!(scene.body, Body::Genesis { .. }) {
            return Err(ApplyError::InvalidGenesis);
        }
        if scene.term < state.head_term.expect("non-empty state has scene term") {
            return Err(ApplyError::InvalidSceneTerm);
        }
    }

    if scene.term == 0 {
        return Err(ApplyError::InvalidSceneTerm);
    }
    if !scene.roster.contains(&scene.leader) {
        return Err(ApplyError::LeaderNotInRoster);
    }
    Ok(())
}

fn validate_body(state: &ChainState, scene: &Scene) -> Result<(), ApplyError> {
    match &scene.body {
        Body::Genesis { name, floor, .. } => {
            if !state.is_empty() || name.is_empty() || name.chars().count() > 128 {
                return Err(ApplyError::InvalidGenesis);
            }
            validate_floor_config(floor)?;
        }
        Body::Grant { to, intent_id, .. } => {
            if !scene.roster.contains(&to.node) {
                return Err(ApplyError::GrantTargetNotRoster);
            }
            if state.consumed_intents.contains(intent_id) {
                return Err(ApplyError::ConsumedIntent);
            }
            // `reason` records proposal provenance. §10 does not make it a
            // reducer validity condition; the leader path chooses it per §12.
        }
        Body::Speech { blobs, .. } => validate_blob_refs(blobs)?,
        Body::Breakout {
            ticket, auto_join, ..
        } => {
            if has_duplicates(auto_join)
                || auto_join.iter().any(|node| !scene.roster.contains(node))
            {
                return Err(ApplyError::InvalidAutoJoin);
            }
            let ticket: Ticket = serde_json::from_value(ticket.clone())
                .map_err(|_| ApplyError::InvalidBreakoutTicket)?;
            ticket
                .validate()
                .map_err(|_| ApplyError::InvalidBreakoutTicket)?;
            if ticket.parent != Some(scene.room) {
                return Err(ApplyError::InvalidBreakoutTicket);
            }
        }
        Body::Membership { floor, .. } => validate_floor_config(floor)?,
        Body::ViewChange {
            add,
            remove,
            next_roster,
            ..
        } => validate_view_change(state, add, remove, next_roster)?,
    }

    if state.is_empty() {
        return if matches!(scene.body, Body::Genesis { .. }) {
            Ok(())
        } else {
            Err(ApplyError::InvalidGenesis)
        };
    }

    let closes = closes_grant(&scene.body);
    match &state.live_grant {
        Some(live) if closes != Some(live.hash) => return Err(ApplyError::InvalidFloorTransition),
        None if closes.is_some() => return Err(ApplyError::InvalidFloorTransition),
        _ => {}
    }

    let allowed = matches!(
        (&state.live_grant, &scene.body),
        (None, Body::Grant { .. })
            | (
                None,
                Body::Membership {
                    closes_grant: None,
                    ..
                }
            )
            | (
                None,
                Body::ViewChange {
                    closes_grant: None,
                    ..
                }
            )
            | (Some(_), Body::Speech { .. } | Body::Breakout { .. })
            | (
                Some(_),
                Body::Membership {
                    closes_grant: Some(_),
                    ..
                } | Body::ViewChange {
                    closes_grant: Some(_),
                    ..
                }
            )
    );
    if !allowed {
        return Err(ApplyError::InvalidFloorTransition);
    }

    Ok(())
}

fn validate_floor_config(floor: &FloorConfig) -> Result<(), ApplyError> {
    let moderator_matches_mode = match floor.mode {
        FloorMode::Stick => floor.moderator.is_none(),
        FloorMode::Moderator => floor.moderator.is_some(),
    };
    if floor.timeout_secs == 0 || !moderator_matches_mode {
        return Err(ApplyError::InvalidFloorConfig);
    }
    Ok(())
}

fn validate_view_change(
    state: &ChainState,
    add: &[NodeId],
    remove: &[NodeId],
    next_roster: &[NodeId],
) -> Result<(), ApplyError> {
    if add.len() + remove.len() != 1
        || !is_sorted_unique(add)
        || !is_sorted_unique(remove)
        || !is_sorted_unique(next_roster)
        || next_roster.is_empty()
    {
        return Err(ApplyError::InvalidViewChange);
    }

    let mut derived: BTreeSet<_> = state.roster.iter().copied().collect();
    if let Some(node) = remove.first() {
        if !derived.remove(node) {
            return Err(ApplyError::InvalidViewChange);
        }
    }
    if let Some(node) = add.first() {
        if !derived.insert(*node) {
            return Err(ApplyError::InvalidViewChange);
        }
        if let Some(stake) = &state.stake {
            if !stake.allowlist.is_empty() && !stake.allowlist.contains(node) {
                return Err(ApplyError::InvalidViewChange);
            }
        }
    }
    let expected: Vec<_> = derived.into_iter().collect();
    if next_roster != expected {
        return Err(ApplyError::InvalidViewChange);
    }
    Ok(())
}

fn validate_grant_intent(
    state: &ChainState,
    scene: &Scene,
    resources: &ApplyResources,
) -> Result<(), ApplyError> {
    let (to, reason, intent_id) = match &scene.body {
        Body::Grant {
            to,
            reason,
            intent_id,
        } => (to, reason, intent_id),
        _ => return Ok(()),
    };

    let target = resources
        .intents
        .iter()
        .find(|intent| intent.id == *intent_id)
        .ok_or(ApplyError::MissingIntent)?;
    if state.consumed_intents.contains(intent_id) {
        return Err(ApplyError::ConsumedIntent);
    }
    validate_intent(target, scene.room)?;
    if target.node != to.node || target.agent != to.agent {
        return Err(ApplyError::IntentTargetMismatch);
    }
    if scene.ts >= target.exp {
        return Err(ApplyError::IntentExpired);
    }

    let mut active_by_mouth: BTreeMap<Mouth, &Intent> = BTreeMap::new();
    for intent in &resources.intents {
        if intent.room != scene.room
            || intent.v != 1
            || !scene.roster.contains(&intent.node)
            || state.consumed_intents.contains(&intent.id)
            || scene.ts >= intent.exp
            || validate_intent(intent, scene.room).is_err()
        {
            continue;
        }
        let mouth = Mouth {
            agent: intent.agent.clone(),
            node: intent.node,
        };
        match active_by_mouth.get(&mouth) {
            Some(previous) if !intent_supersedes(intent, previous) => {}
            _ => {
                active_by_mouth.insert(mouth, intent);
            }
        }
    }

    let target_mouth = Mouth {
        agent: target.agent.clone(),
        node: target.node,
    };
    if active_by_mouth
        .get(&target_mouth)
        .is_none_or(|active| active.id != target.id)
    {
        return Err(ApplyError::IntentNotQueueHead);
    }

    // A moderator chooses a waiter, not necessarily the globally oldest
    // waiter. Every grant still names that mouth's live intent.
    if *reason == GrantReason::Moderator {
        return Ok(());
    }

    let head = active_by_mouth
        .values()
        .min_by_key(|intent| (intent.ts, intent.id))
        .ok_or(ApplyError::MissingIntent)?;
    if head.id != target.id {
        return Err(ApplyError::IntentNotQueueHead);
    }
    Ok(())
}

fn validate_intent(intent: &Intent, room: crate::types::RoomId) -> Result<(), ApplyError> {
    if intent.v != 1 || intent.room != room || intent.exp <= intent.ts {
        return Err(ApplyError::InvalidIntent);
    }
    let key =
        VerifyingKey::from_bytes(intent.node.as_bytes()).map_err(|_| ApplyError::InvalidIntent)?;
    let value = serde_json::to_value(intent).map_err(|_| ApplyError::InvalidIntent)?;
    if !verify(&key, &signed_object_digest(&value), intent.sig.as_bytes()) {
        return Err(ApplyError::InvalidIntent);
    }
    Ok(())
}

fn validate_proof(scene: &Scene, proof: &CommitProof, hash: Hash32) -> Result<(), ApplyError> {
    if proof.rpc_term == 0 || !scene.roster.contains(&proof.leader) {
        return Err(ApplyError::ProofLeaderNotRoster);
    }
    if scene.n == 0 && (proof.rpc_term != 1 || proof.leader != scene.leader) {
        return Err(ApplyError::InvalidGenesis);
    }

    let mut node_signers = BTreeSet::new();
    let mut room_signature = false;
    for cert in &proof.certs {
        match cert.node {
            CertSigner::Room => {
                if room_signature {
                    return Err(ApplyError::DuplicateCert("room".into()));
                }
                room_signature = true;
                if scene.n != 0 {
                    return Err(ApplyError::CertSignerNotRoster);
                }
                let key = VerifyingKey::from_bytes(scene.room.as_bytes())
                    .map_err(|_| ApplyError::InvalidRoomSignature)?;
                if !verify(&key, hash.as_bytes(), cert.sig.as_bytes()) {
                    return Err(ApplyError::InvalidRoomSignature);
                }
            }
            CertSigner::Node(node) => {
                if !scene.roster.contains(&node) {
                    return Err(ApplyError::CertSignerNotRoster);
                }
                if !node_signers.insert(node) {
                    return Err(ApplyError::DuplicateCert(node.to_string()));
                }
                let key = VerifyingKey::from_bytes(node.as_bytes())
                    .map_err(|_| ApplyError::InvalidCertSignature(node))?;
                let digest = cert_digest(
                    &scene.room,
                    scene.n,
                    hash.as_bytes(),
                    proof.rpc_term,
                    &proof.leader,
                    &node,
                );
                if !verify(&key, &digest, cert.sig.as_bytes()) {
                    return Err(ApplyError::InvalidCertSignature(node));
                }
            }
        }
    }

    if scene.n == 0 && !room_signature {
        return Err(ApplyError::MissingRoomSignature);
    }
    let required = majority(scene.roster.len());
    if node_signers.len() < required {
        return Err(ApplyError::InsufficientCerts {
            actual: node_signers.len(),
            required,
        });
    }
    Ok(())
}

fn validate_blob_refs(blobs: &[crate::types::BlobRef]) -> Result<(), ApplyError> {
    if blobs.iter().any(|blob| blob.bytes > MAX_BLOB_BYTES) {
        return Err(ApplyError::BlobTooLarge);
    }
    Ok(())
}

fn validate_blobs(scene: &Scene, resources: &ApplyResources) -> Result<(), ApplyError> {
    let blobs = match &scene.body {
        Body::Speech { blobs, .. } => blobs.as_slice(),
        _ => &[],
    };
    validate_blob_refs(blobs)?;
    for blob in blobs {
        let materialized = resources
            .blobs
            .get(&blob.sha256)
            .ok_or(ApplyError::MissingBlob(blob.sha256))?;
        if materialized.bytes != blob.bytes {
            return Err(ApplyError::BlobLengthMismatch(blob.sha256));
        }
        if materialized.sha256 != blob.sha256 {
            return Err(ApplyError::BlobHashMismatch(blob.sha256));
        }
    }
    Ok(())
}

fn transition(state: &ChainState, scene: &Scene, hash: Hash32) -> ChainState {
    let mut next = state.clone();
    next.room = Some(scene.room);
    next.head_n = Some(scene.n);
    next.head_hash = Some(hash);
    next.head_term = Some(scene.term);

    if closes_grant(&scene.body).is_some() {
        next.live_grant = None;
    }

    match &scene.body {
        Body::Genesis { stake, floor, .. } => {
            next.roster = scene.roster.clone();
            apply_config(&mut next, stake, floor);
        }
        Body::Grant { to, intent_id, .. } => {
            next.live_grant = Some(LiveGrant {
                hash,
                to: to.clone(),
                term: scene.term,
                n: scene.n,
            });
            next.consumed_intents.insert(*intent_id);
        }
        Body::Membership { stake, floor, .. } => apply_config(&mut next, stake, floor),
        Body::ViewChange { next_roster, .. } => next.roster = next_roster.clone(),
        Body::Speech { .. } | Body::Breakout { .. } => {}
    }
    next
}

fn apply_config(state: &mut ChainState, stake: &crate::types::StakePolicy, floor: &FloorConfig) {
    state.stake = Some(stake.clone());
    state.floor_mode = Some(floor.mode);
    state.moderator = floor.moderator.clone();
    state.timeout_secs = Some(floor.timeout_secs);
}

fn closes_grant(body: &Body) -> Option<Hash32> {
    match body {
        Body::Speech { closes_grant, .. } | Body::Breakout { closes_grant, .. } => {
            Some(*closes_grant)
        }
        Body::Membership { closes_grant, .. } | Body::ViewChange { closes_grant, .. } => {
            *closes_grant
        }
        Body::Genesis { .. } | Body::Grant { .. } => None,
    }
}

fn majority(roster_len: usize) -> usize {
    roster_len / 2 + 1
}

fn is_sorted_unique<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn has_duplicates<T: Ord + Clone>(values: &[T]) -> bool {
    let unique: BTreeSet<_> = values.iter().cloned().collect();
    unique.len() != values.len()
}
