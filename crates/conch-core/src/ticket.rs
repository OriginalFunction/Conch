use std::{collections::BTreeMap, path::PathBuf, str::FromStr};

use ed25519_dalek::{SigningKey, VerifyingKey};
use form_urlencoded;
use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    encoding::{sign, signed_object_digest, verify},
    types::{AgentId, FloorConfig, FloorMode, Hash32, NodeId, RoomId, SignatureBytes, StakePolicy},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JoinRole {
    #[default]
    Stake,
    Observe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketSource {
    Inline(Box<Ticket>),
    File(PathBuf),
    Http(String),
}

impl TicketSource {
    pub fn parse(reference: &str) -> Result<Self, TicketError> {
        if reference.starts_with("conch:") {
            Ticket::from_magnet(reference)
                .map(Box::new)
                .map(Self::Inline)
        } else if reference.starts_with("http://") || reference.starts_with("https://") {
            Ok(Self::Http(reference.to_owned()))
        } else {
            Ok(Self::File(PathBuf::from(reference)))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    pub room: RoomId,
    pub role: JoinRole,
    pub agents: Vec<AgentId>,
    pub ts: u64,
    pub sig: SignatureBytes,
}

impl Declaration {
    pub fn signed(
        room: RoomId,
        role: JoinRole,
        agents: Vec<AgentId>,
        ts: u64,
        key: &SigningKey,
    ) -> Self {
        let mut declaration = Self {
            room,
            role,
            agents,
            ts,
            sig: SignatureBytes::from_bytes([0; 64]),
        };
        declaration.sig = SignatureBytes::from_bytes(sign(
            key,
            &signed_object_digest(
                &serde_json::to_value(&declaration).expect("declaration is serializable"),
            ),
        ));
        declaration
    }

    pub fn verify(&self, node: NodeId) -> bool {
        VerifyingKey::from_bytes(node.as_bytes()).is_ok_and(|key| {
            verify(
                &key,
                &signed_object_digest(
                    &serde_json::to_value(self).expect("declaration is serializable"),
                ),
                self.sig.as_bytes(),
            )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    pub v: u8,
    pub id: RoomId,
    pub name: String,
    pub trackers: Vec<String>,
    pub peers: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub token: Option<Hash32>,
    pub stake: StakePolicy,
    pub floor: FloorConfig,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_non_null"
    )]
    pub parent: Option<RoomId>,
    pub genesis: Hash32,
}

#[derive(Debug, Error)]
pub enum TicketError {
    #[error("ticket JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ticket magnet must start with conch:1:")]
    InvalidScheme,
    #[error("ticket room id is invalid")]
    InvalidRoom,
    #[error("ticket field {0} is duplicated")]
    DuplicateField(&'static str),
    #[error("ticket magnet is missing its required g genesis pin")]
    MissingGenesis,
    #[error("ticket genesis pin is invalid")]
    InvalidGenesis,
    #[error("ticket token is invalid")]
    InvalidToken,
    #[error("ticket version must be 1")]
    InvalidVersion,
    #[error("ticket name must contain 1-128 characters")]
    InvalidName,
    #[error("ticket floor configuration is invalid")]
    InvalidFloor,
}

impl Ticket {
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, TicketError> {
        let ticket: Self = serde_json::from_slice(bytes)?;
        ticket.validate()?;
        Ok(ticket)
    }

    pub fn from_magnet(magnet: &str) -> Result<Self, TicketError> {
        let body = magnet
            .strip_prefix("conch:1:")
            .ok_or(TicketError::InvalidScheme)?;
        let (id, query) = body.split_once('?').unwrap_or((body, ""));
        let id = RoomId::from_str(id).map_err(|_| TicketError::InvalidRoom)?;
        let mut scalar = BTreeMap::<String, String>::new();
        let mut trackers = Vec::new();
        let mut peers = Vec::new();
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "tr" => trackers.push(value.into_owned()),
                "x.peer" => peers.push(value.into_owned()),
                "dn" | "g" | "token" => {
                    let key = key.into_owned();
                    let field = match key.as_str() {
                        "dn" => "dn",
                        "g" => "g",
                        _ => "token",
                    };
                    if scalar.insert(key, value.into_owned()).is_some() {
                        return Err(TicketError::DuplicateField(field));
                    }
                }
                _ => {}
            }
        }
        let genesis = scalar
            .remove("g")
            .ok_or(TicketError::MissingGenesis)?
            .parse()
            .map_err(|_| TicketError::InvalidGenesis)?;
        let token = scalar
            .remove("token")
            .map(|value| value.parse().map_err(|_| TicketError::InvalidToken))
            .transpose()?;
        let name = scalar.remove("dn").unwrap_or_else(|| id.to_string());
        let ticket = Self {
            v: 1,
            id,
            name,
            trackers,
            peers,
            token,
            stake: StakePolicy::default(),
            floor: FloorConfig::stick(30),
            parent: None,
            genesis,
        };
        ticket.validate()?;
        Ok(ticket)
    }

    pub fn to_magnet(&self) -> String {
        let mut serializer = form_urlencoded::Serializer::new(String::new());
        serializer.append_pair("dn", &self.name);
        serializer.append_pair("g", &self.genesis.to_string());
        for tracker in &self.trackers {
            serializer.append_pair("tr", tracker);
        }
        for peer in &self.peers {
            serializer.append_pair("x.peer", peer);
        }
        if let Some(token) = self.token {
            serializer.append_pair("token", &token.to_string());
        }
        format!("conch:1:{}?{}", self.id, serializer.finish())
    }

    pub fn validate(&self) -> Result<(), TicketError> {
        if self.v != 1 {
            return Err(TicketError::InvalidVersion);
        }
        let name_len = self.name.chars().count();
        if name_len == 0 || name_len > 128 {
            return Err(TicketError::InvalidName);
        }
        let valid_floor = self.floor.timeout_secs >= 1
            && matches!(
                (self.floor.mode, self.floor.moderator.is_some()),
                (FloorMode::Stick, false) | (FloorMode::Moderator, true)
            );
        if !valid_floor {
            return Err(TicketError::InvalidFloor);
        }
        Ok(())
    }
}

pub fn eligible(policy: &StakePolicy, node: NodeId, role: JoinRole, agents: &[AgentId]) -> bool {
    role != JoinRole::Observe
        && (policy.allowlist.is_empty() || policy.allowlist.contains(&node))
        && ((policy.explicit && role == JoinRole::Stake) || (policy.agents && !agents.is_empty()))
}

fn deserialize_optional_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: for<'value> Deserialize<'value>,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Err(de::Error::custom(
            "optional fields must be omitted, not null",
        ));
    }
    T::deserialize(value).map(Some).map_err(de::Error::custom)
}
