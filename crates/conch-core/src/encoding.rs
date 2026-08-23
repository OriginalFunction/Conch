use std::cmp::Ordering;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::types::{NodeId, RoomId};

/// Serialize a JSON value according to RFC 8785 (JCS).
///
/// `serde_json::Value` cannot contain non-finite numbers or unpaired Unicode
/// surrogates, so every value reaching this function is valid JCS input.
pub fn canonical_json(value: &Value) -> Vec<u8> {
    let mut output = Vec::new();
    write_value(value, &mut output);
    output
}

/// Hash an immutable scene. The top-level `certs` sidecar is deliberately
/// excluded; a nested key with that name remains part of the scene.
pub fn scene_hash(scene_json: &Value) -> [u8; 32] {
    let mut immutable = scene_json.clone();
    if let Value::Object(object) = &mut immutable {
        object.remove("certs");
    }
    sha256(&canonical_json(&immutable))
}

/// Build the exact term-bound commit certificate digest from spec §8.1.
pub fn cert_digest(
    room: &RoomId,
    n: u64,
    hash: &[u8; 32],
    rpc_term: u64,
    leader: &NodeId,
    node: &NodeId,
) -> [u8; 32] {
    #[derive(Serialize)]
    struct CertPayload<'a> {
        room: &'a RoomId,
        n: u64,
        hash: String,
        rpc_term: u64,
        leader: &'a NodeId,
        node: &'a NodeId,
    }

    let payload = CertPayload {
        room,
        n,
        hash: hex::encode(hash),
        rpc_term,
        leader,
        node,
    };
    let value = serde_json::to_value(payload).expect("commit cert payload is JSON-serializable");
    sha256(&canonical_json(&value))
}

/// Ed25519-sign a digest directly. Callers hash structured payloads first.
pub fn sign(signing_key: &SigningKey, digest32: &[u8; 32]) -> [u8; 64] {
    signing_key.sign(digest32).to_bytes()
}

/// Verify an Ed25519 signature over the raw 32 digest bytes.
pub fn verify(verifying_key: &VerifyingKey, digest32: &[u8; 32], signature: &[u8; 64]) -> bool {
    verifying_key
        .verify(digest32, &Signature::from_bytes(signature))
        .is_ok()
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn write_value(value: &Value, output: &mut Vec<u8>) {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(true) => output.extend_from_slice(b"true"),
        Value::Bool(false) => output.extend_from_slice(b"false"),
        Value::Number(number) => {
            let value = number
                .as_f64()
                .expect("serde_json numbers are finite IEEE-754 values");
            let mut buffer = ryu_js::Buffer::new();
            output.extend_from_slice(buffer.format(value).as_bytes());
        }
        Value::String(string) => write_string(string, output),
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_value(value, output);
            }
            output.push(b']');
        }
        Value::Object(object) => {
            let mut entries: Vec<_> = object.iter().collect();
            entries.sort_by(|(left, _), (right, _)| utf16_cmp(left, right));

            output.push(b'{');
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_string(key, output);
                output.push(b':');
                write_value(value, output);
            }
            output.push(b'}');
        }
    }
}

fn write_string(value: &str, output: &mut Vec<u8>) {
    output.push(b'"');
    for character in value.chars() {
        match character {
            '\u{0008}' => output.extend_from_slice(br"\b"),
            '\u{0009}' => output.extend_from_slice(br"\t"),
            '\u{000a}' => output.extend_from_slice(br"\n"),
            '\u{000c}' => output.extend_from_slice(br"\f"),
            '\u{000d}' => output.extend_from_slice(br"\r"),
            '"' => output.extend_from_slice(br#"\""#),
            '\\' => output.extend_from_slice(br"\\"),
            control if control <= '\u{001f}' => {
                let escaped = format!("\\u{:04x}", control as u32);
                output.extend_from_slice(escaped.as_bytes());
            }
            other => {
                let mut encoded = [0_u8; 4];
                output.extend_from_slice(other.encode_utf8(&mut encoded).as_bytes());
            }
        }
    }
    output.push(b'"');
}

fn utf16_cmp(left: &str, right: &str) -> Ordering {
    let mut left = left.encode_utf16();
    let mut right = right.encode_utf16();

    loop {
        match (left.next(), right.next()) {
            (Some(left), Some(right)) => match left.cmp(&right) {
                Ordering::Equal => {}
                ordering => return ordering,
            },
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (None, None) => return Ordering::Equal,
        }
    }
}
