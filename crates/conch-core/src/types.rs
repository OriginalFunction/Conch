use std::{fmt, str::FromStr};

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdError {
    #[error("id must be exactly 64 lowercase hexadecimal characters")]
    InvalidEncoding,
}

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; 32]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }

            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            pub const fn into_bytes(self) -> [u8; 32] {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&hex::encode(self.0))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, formatter)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(encoded: &str) -> Result<Self, Self::Err> {
                if encoded.len() != 64
                    || !encoded
                        .as_bytes()
                        .iter()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
                {
                    return Err(IdError::InvalidEncoding);
                }

                let mut bytes = [0_u8; 32];
                hex::decode_to_slice(encoded, &mut bytes).map_err(|_| IdError::InvalidEncoding)?;
                Ok(Self(bytes))
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let encoded = String::deserialize(deserializer)?;
                encoded.parse().map_err(de::Error::custom)
            }
        }
    };
}

id_type!(NodeId);
id_type!(RoomId);

macro_rules! fixed_hex_type {
    ($name:ident, $length:expr, $encoded_length:expr) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; $length]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; $length]) -> Self {
                Self(bytes)
            }

            pub const fn as_bytes(&self) -> &[u8; $length] {
                &self.0
            }

            pub const fn into_bytes(self) -> [u8; $length] {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&hex::encode(self.0))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, formatter)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(encoded: &str) -> Result<Self, Self::Err> {
                if encoded.len() != $encoded_length
                    || !encoded
                        .as_bytes()
                        .iter()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
                {
                    return Err(IdError::InvalidEncoding);
                }

                let mut bytes = [0_u8; $length];
                hex::decode_to_slice(encoded, &mut bytes).map_err(|_| IdError::InvalidEncoding)?;
                Ok(Self(bytes))
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let encoded = String::deserialize(deserializer)?;
                encoded.parse().map_err(de::Error::custom)
            }
        }
    };
}

fixed_hex_type!(Hash32, 32, 64);
fixed_hex_type!(SignatureBytes, 64, 128);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentId(String);

impl AgentId {
    pub fn new(value: impl Into<String>) -> Result<Self, AgentIdError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'_' | b'.' | b':' | b'-')
            })
        {
            return Err(AgentIdError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for AgentId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AgentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("agent id must be 1-64 characters matching [a-z0-9_.:-]+")]
pub struct AgentIdError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mouth {
    pub agent: AgentId,
    pub node: NodeId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FloorMode {
    Stick,
    Moderator,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FloorConfig {
    pub mode: FloorMode,
    pub timeout_secs: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub moderator: Option<Mouth>,
}

impl FloorConfig {
    pub fn stick(timeout_secs: u64) -> Self {
        Self {
            mode: FloorMode::Stick,
            timeout_secs,
            moderator: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StakePolicy {
    pub agents: bool,
    pub explicit: bool,
    pub allowlist: Vec<NodeId>,
}

impl Default for StakePolicy {
    fn default() -> Self {
        Self {
            agents: true,
            explicit: true,
            allowlist: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GrantReason {
    Queue,
    Moderator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IntentKind {
    Raise,
    Wait,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobRef {
    pub name: String,
    pub sha256: Hash32,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Body {
    Genesis {
        name: String,
        stake: StakePolicy,
        floor: FloorConfig,
        creator_node: NodeId,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional_non_null"
        )]
        parent_room: Option<RoomId>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional_non_null"
        )]
        token_sha256: Option<Hash32>,
    },
    Grant {
        to: Mouth,
        reason: GrantReason,
        intent_id: Hash32,
    },
    Speech {
        closes_grant: Hash32,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        blobs: Vec<BlobRef>,
    },
    Breakout {
        closes_grant: Hash32,
        ticket: Value,
        auto_join: Vec<NodeId>,
    },
    Membership {
        stake: StakePolicy,
        floor: FloorConfig,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional_non_null"
        )]
        closes_grant: Option<Hash32>,
    },
    ViewChange {
        add: Vec<NodeId>,
        remove: Vec<NodeId>,
        next_roster: Vec<NodeId>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional_non_null"
        )]
        closes_grant: Option<Hash32>,
    },
}

impl Eq for Body {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CertSigner {
    Room,
    Node(NodeId),
}

impl Serialize for CertSigner {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Room => serializer.serialize_str("room"),
            Self::Node(node) => node.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for CertSigner {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value == "room" {
            Ok(Self::Room)
        } else {
            value.parse().map(Self::Node).map_err(de::Error::custom)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cert {
    pub node: CertSigner,
    pub sig: SignatureBytes,
}

impl Cert {
    pub const fn node(node: NodeId, sig: SignatureBytes) -> Self {
        Self {
            node: CertSigner::Node(node),
            sig,
        }
    }

    pub const fn room(sig: SignatureBytes) -> Self {
        Self {
            node: CertSigner::Room,
            sig,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitProof {
    pub rpc_term: u64,
    pub leader: NodeId,
    pub certs: Vec<Cert>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scene {
    pub v: u8,
    pub room: RoomId,
    pub n: u64,
    pub term: u64,
    pub parent: Option<Hash32>,
    pub roster: Vec<NodeId>,
    pub leader: NodeId,
    pub ts: u64,
    pub body: Body,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub certs: Vec<Cert>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub v: u8,
    pub id: Hash32,
    pub room: RoomId,
    pub kind: IntentKind,
    pub agent: AgentId,
    pub node: NodeId,
    pub ts: u64,
    pub exp: u64,
    pub sig: SignatureBytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveGrant {
    pub hash: Hash32,
    pub to: Mouth,
    pub term: u64,
    pub n: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainState {
    pub room: Option<RoomId>,
    pub head_n: Option<u64>,
    pub head_hash: Option<Hash32>,
    pub head_term: Option<u64>,
    pub roster: Vec<NodeId>,
    pub stake: Option<StakePolicy>,
    pub floor_mode: Option<FloorMode>,
    pub moderator: Option<Mouth>,
    pub timeout_secs: Option<u64>,
    pub live_grant: Option<LiveGrant>,
    pub consumed_intents: std::collections::BTreeSet<Hash32>,
}

impl ChainState {
    pub fn empty() -> Self {
        Self {
            room: None,
            head_n: None,
            head_hash: None,
            head_term: None,
            roster: Vec::new(),
            stake: None,
            floor_mode: None,
            moderator: None,
            timeout_secs: None,
            live_grant: None,
            consumed_intents: std::collections::BTreeSet::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.head_n.is_none()
    }
}

impl Default for ChainState {
    fn default() -> Self {
        Self::empty()
    }
}

fn deserialize_optional_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}
