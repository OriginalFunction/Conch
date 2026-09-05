use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ticket::{JoinRole, Ticket, TicketError},
    types::{AgentId, FloorConfig, Hash32, Mouth, NodeId, RoomId, StakePolicy},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "typ", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientRequest {
    Attach {
        agent: AgentId,
    },
    Create {
        name: String,
        stake: StakePolicy,
        floor: FloorConfig,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<Hash32>,
    },
    Join {
        ticket: ClientTicket,
        #[serde(default)]
        role: JoinRole,
    },
    WaitForFloor {
        room: RoomId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
    Speak {
        room: RoomId,
        text: String,
        request_id: String,
    },
    Yield {
        room: RoomId,
    },
    RaiseHand {
        room: RoomId,
    },
    Grant {
        room: RoomId,
        to: Mouth,
    },
    Yank {
        room: RoomId,
    },
    Breakout {
        room: RoomId,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        members: Option<Vec<NodeId>>,
    },
    Membership {
        room: RoomId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stake: Option<StakePolicy>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        floor: Option<FloorConfig>,
    },
    PutBlob {
        room: RoomId,
        name: String,
        bytes: u64,
    },
    Leave {
        room: RoomId,
        vacate: bool,
    },
    History {
        room: RoomId,
        from_n: u64,
        #[serde(default)]
        follow: bool,
    },
    WaitForHistory {
        room: RoomId,
        after_n: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
    Status {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        room: Option<RoomId>,
    },
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClientTicket {
    Ticket(Box<Ticket>),
    Magnet(String),
}

impl ClientTicket {
    pub fn resolve(self) -> Result<Ticket, TicketError> {
        match self {
            Self::Ticket(ticket) => Ok(*ticket),
            Self::Magnet(magnet) => Ticket::from_magnet(&magnet),
        }
    }
}

impl From<Ticket> for ClientTicket {
    fn from(ticket: Ticket) -> Self {
        Self::Ticket(Box::new(ticket))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ClientError>,
}

impl ClientReply {
    pub fn success(data: Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn failure(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(ClientError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientError {
    pub code: String,
    pub message: String,
}
