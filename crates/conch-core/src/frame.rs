use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;

pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame payload exceeds 64 MiB")]
    TooLarge,
    #[error("frame length does not match its payload")]
    LengthMismatch,
    #[error("frame JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(message)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    let length = u32::try_from(payload.len()).map_err(|_| FrameError::TooLarge)?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<T, FrameError> {
    let prefix: [u8; 4] = frame
        .get(..4)
        .ok_or(FrameError::LengthMismatch)?
        .try_into()
        .expect("slice length checked");
    let length = u32::from_be_bytes(prefix) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    let payload = frame.get(4..).ok_or(FrameError::LengthMismatch)?;
    if payload.len() != length {
        return Err(FrameError::LengthMismatch);
    }
    decode_payload(payload)
}

pub fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    Ok(serde_json::from_slice(payload)?)
}
