use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{AgentId, RoomId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "typ", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientRequest {
    Attach {
        agent: AgentId,
    },
    Create {
        name: String,
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
    History {
        room: RoomId,
        from_n: u64,
    },
    Status {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        room: Option<RoomId>,
    },
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
