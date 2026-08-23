use std::{
    collections::BTreeMap,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    encoding::{signed_object_digest, verify},
    types::{BlobRef, ChainState, Hash32, Intent, Mouth, NodeId, RoomId},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TakePhase {
    Open,
    Closing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeakAck {
    pub ok: bool,
    pub grant_hash: Hash32,
    pub rev: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TakeBuffer {
    pub room: RoomId,
    pub grant_hash: Hash32,
    pub holder: Mouth,
    pub phase: TakePhase,
    pub text: String,
    pub rev: u64,
    pub blobs: Vec<BlobRef>,
    pub requests: BTreeMap<String, SpeakAck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenTake {
    pub room: RoomId,
    pub grant_hash: Hash32,
    pub text: String,
    pub rev: u64,
    pub blobs: Vec<BlobRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenGrant {
    pub room: RoomId,
    pub grant_hash: Hash32,
    pub holder: Mouth,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FloorError {
    #[error("no OPEN grant for this mouth")]
    NoGrant,
    #[error("request_id must be lowercase hex containing at least 16 bytes")]
    InvalidRequestId,
    #[error("intent is invalid")]
    InvalidIntent,
    #[error("observer or removed node cannot queue for the floor")]
    NotStaker,
    #[error("wait-for-floor timed out")]
    Timeout,
}

#[derive(Debug, Clone)]
pub struct FloorEngine {
    local_node: NodeId,
    intents: BTreeMap<Mouth, Intent>,
    take: Option<TakeBuffer>,
}

impl FloorEngine {
    pub fn new(local_node: NodeId) -> Self {
        Self {
            local_node,
            intents: BTreeMap::new(),
            take: None,
        }
    }

    pub fn restore(local_node: NodeId, take: Option<TakeBuffer>) -> Self {
        Self {
            local_node,
            intents: BTreeMap::new(),
            take,
        }
    }

    pub fn take(&self) -> Option<&TakeBuffer> {
        self.take.as_ref()
    }

    pub fn intents(&self) -> impl Iterator<Item = &Intent> {
        self.intents.values()
    }

    pub fn upsert_intent(
        &mut self,
        state: &ChainState,
        intent: Intent,
    ) -> Result<bool, FloorError> {
        if state.room != Some(intent.room)
            || intent.v != 1
            || intent.exp <= intent.ts
            || state.consumed_intents.contains(&intent.id)
        {
            return Err(FloorError::InvalidIntent);
        }
        if !state.roster.contains(&intent.node) {
            return Err(FloorError::NotStaker);
        }
        let key = VerifyingKey::from_bytes(intent.node.as_bytes())
            .map_err(|_| FloorError::InvalidIntent)?;
        let value = serde_json::to_value(&intent).map_err(|_| FloorError::InvalidIntent)?;
        if !verify(&key, &signed_object_digest(&value), intent.sig.as_bytes()) {
            return Err(FloorError::InvalidIntent);
        }
        let mouth = Mouth {
            agent: intent.agent.clone(),
            node: intent.node,
        };
        if let Some(current) = self.intents.get(&mouth) {
            if current.id == intent.id && (current.ts != intent.ts || current.kind != intent.kind) {
                return Err(FloorError::InvalidIntent);
            }
            if !intent_supersedes(&intent, current) {
                return Ok(false);
            }
        }
        self.intents.insert(mouth, intent);
        Ok(true)
    }

    pub fn queue_head(&self, state: &ChainState, scene_ts: u64) -> Option<&Intent> {
        self.intents
            .values()
            .filter(|intent| {
                state.roster.contains(&intent.node)
                    && state.room == Some(intent.room)
                    && !state.consumed_intents.contains(&intent.id)
                    && scene_ts < intent.exp
            })
            .min_by_key(|intent| (intent.ts, intent.id))
    }

    pub fn observe_committed(&mut self, state: &ChainState) {
        if self
            .take
            .as_ref()
            .is_some_and(|take| state.room != Some(take.room))
        {
            return;
        }
        let Some(room) = state.room else {
            self.take = None;
            return;
        };
        let Some(grant) = &state.live_grant else {
            self.take = None;
            return;
        };
        if grant.to.node != self.local_node {
            self.take = None;
            return;
        }
        if self
            .take
            .as_ref()
            .is_some_and(|take| take.grant_hash == grant.hash)
        {
            return;
        }
        self.take = Some(TakeBuffer {
            room,
            grant_hash: grant.hash,
            holder: grant.to.clone(),
            phase: TakePhase::Open,
            text: String::new(),
            rev: 0,
            blobs: Vec::new(),
            requests: BTreeMap::new(),
        });
    }

    pub fn open_grant_for(&self, mouth: &Mouth) -> Option<OpenGrant> {
        self.take.as_ref().and_then(|take| {
            (take.phase == TakePhase::Open && &take.holder == mouth).then(|| OpenGrant {
                room: take.room,
                grant_hash: take.grant_hash,
                holder: take.holder.clone(),
            })
        })
    }

    pub fn speak(
        &mut self,
        mouth: &Mouth,
        text: &str,
        request_id: &str,
    ) -> Result<SpeakAck, FloorError> {
        if !valid_request_id(request_id) {
            return Err(FloorError::InvalidRequestId);
        }
        let take = self.take.as_mut().ok_or(FloorError::NoGrant)?;
        if &take.holder != mouth {
            return Err(FloorError::NoGrant);
        }
        if let Some(response) = take.requests.get(request_id) {
            // §12.1 request-id retention wins over phase rejection: a retry
            // after a lost response returns the original acknowledgement.
            return Ok(response.clone());
        }
        if take.phase != TakePhase::Open {
            return Err(FloorError::NoGrant);
        }
        take.text.push_str(text);
        take.rev = take.rev.checked_add(1).ok_or(FloorError::NoGrant)?;
        let response = SpeakAck {
            ok: true,
            grant_hash: take.grant_hash,
            rev: take.rev,
        };
        take.requests
            .insert(request_id.to_owned(), response.clone());
        Ok(response)
    }

    pub fn attach_blob(&mut self, mouth: &Mouth, blob: BlobRef) -> Result<(), FloorError> {
        let take = self.take.as_mut().ok_or(FloorError::NoGrant)?;
        if take.phase != TakePhase::Open || &take.holder != mouth {
            return Err(FloorError::NoGrant);
        }
        if !take.blobs.iter().any(|known| known.sha256 == blob.sha256) {
            take.blobs.push(blob);
        }
        Ok(())
    }

    pub fn freeze(&mut self, room: RoomId, grant_hash: Hash32) -> Result<FrozenTake, FloorError> {
        let take = self.take.as_mut().ok_or(FloorError::NoGrant)?;
        if take.room != room || take.grant_hash != grant_hash {
            return Err(FloorError::NoGrant);
        }
        take.phase = TakePhase::Closing;
        Ok(FrozenTake {
            room: take.room,
            grant_hash: take.grant_hash,
            text: take.text.clone(),
            rev: take.rev,
            blobs: take.blobs.clone(),
        })
    }
}

/// Returns true when `candidate` is the canonical current intent for the same
/// mouth. Different ids use the greater `(ts,id)`; a same-id refresh keeps its
/// original timestamp and advances only `exp`.
pub fn intent_supersedes(candidate: &Intent, current: &Intent) -> bool {
    if candidate.id == current.id {
        candidate.ts == current.ts && candidate.exp > current.exp
    } else {
        (candidate.ts, candidate.id) > (current.ts, current.id)
    }
}

#[derive(Clone)]
pub struct FloorWatch {
    inner: Arc<(Mutex<FloorEngine>, Condvar)>,
}

impl FloorWatch {
    /// Blocking embedding/test helper. `observe_committed` must be called only
    /// with a `ChainState` produced by successful `apply(commit)`; async daemon
    /// callers use `tokio::sync::Notify` and return the stored grant scene.
    pub fn new(engine: FloorEngine) -> Self {
        Self {
            inner: Arc::new((Mutex::new(engine), Condvar::new())),
        }
    }

    pub fn observe_committed(&self, state: &ChainState) {
        let (engine, changed) = &*self.inner;
        engine
            .lock()
            .expect("floor lock is not poisoned")
            .observe_committed(state);
        changed.notify_all();
    }

    pub fn wait_for_floor(&self, mouth: &Mouth, wait: Duration) -> Result<OpenGrant, FloorError> {
        let (engine, changed) = &*self.inner;
        let deadline = Instant::now() + wait;
        let mut guard = engine.lock().expect("floor lock is not poisoned");
        loop {
            if let Some(grant) = guard.open_grant_for(mouth) {
                return Ok(grant);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(FloorError::Timeout);
            }
            let (next, result) = changed
                .wait_timeout(guard, remaining)
                .expect("floor lock is not poisoned");
            guard = next;
            if result.timed_out() && guard.open_grant_for(mouth).is_none() {
                return Err(FloorError::Timeout);
            }
        }
    }

    pub fn with_engine<T>(&self, operation: impl FnOnce(&mut FloorEngine) -> T) -> T {
        let (engine, _) = &*self.inner;
        operation(&mut engine.lock().expect("floor lock is not poisoned"))
    }
}

fn valid_request_id(request_id: &str) -> bool {
    request_id.len() >= 32
        && request_id.len().is_multiple_of(2)
        && request_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
