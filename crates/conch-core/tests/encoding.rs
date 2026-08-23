use std::str::FromStr;

use conch_core::{
    encoding::{canonical_json, cert_digest, scene_hash, sign, verify},
    types::{NodeId, RoomId},
};
use ed25519_dalek::SigningKey;
use serde_json::json;
use sha2::Digest;

fn id(byte: u8) -> NodeId {
    NodeId::from_bytes([byte; 32])
}

#[test]
fn scene_hash_deletes_certs_key_not_empty_array() {
    let with_key = json!({"v": 1, "n": 0, "certs": []});
    let without = json!({"v": 1, "n": 0});

    assert_eq!(scene_hash(&with_key), scene_hash(&without));
    assert!(
        with_key.get("certs").is_some(),
        "hashing must not mutate input"
    );
}

#[test]
fn scene_hash_only_deletes_the_top_level_certs_key() {
    let nested = json!({"v": 1, "n": 0, "body": {"certs": []}});
    let nested_without = json!({"v": 1, "n": 0, "body": {}});

    assert_ne!(scene_hash(&nested), scene_hash(&nested_without));
}

#[test]
fn canonical_json_matches_rfc_8785_number_vector() {
    let value: serde_json::Value = serde_json::from_str(
        r#"[333333333.33333329,1E30,4.50,2e-3,0.000000000000000000000000001]"#,
    )
    .unwrap();

    assert_eq!(
        canonical_json(&value),
        br#"[333333333.3333333,1e+30,4.5,0.002,1e-27]"#
    );
}

#[test]
fn canonical_json_uses_ecmascript_number_boundaries() {
    let value: serde_json::Value = serde_json::from_str(r#"[-0,1e-7,1e-6,1e20,1e21]"#).unwrap();

    assert_eq!(
        canonical_json(&value),
        br#"[0,1e-7,0.000001,100000000000000000000,1e+21]"#
    );
}

#[test]
fn canonical_json_sorts_object_keys_by_utf16_code_units() {
    let value = json!({
        "\u{1f600}": "non-bmp sorts before bmp private-use",
        "\u{e000}": "bmp private-use"
    });

    assert_eq!(
        String::from_utf8(canonical_json(&value)).unwrap(),
        "{\"😀\":\"non-bmp sorts before bmp private-use\",\"\":\"bmp private-use\"}"
    );
}

#[test]
fn canonical_json_uses_required_string_escapes() {
    let value = json!({"s": "\u{0000}\u{0008}\t\n\u{000c}\r\"\\/"});

    assert_eq!(
        String::from_utf8(canonical_json(&value)).unwrap(),
        r#"{"s":"\u0000\b\t\n\f\r\"\\/"}"#
    );
}

#[test]
fn cert_preimage_is_payload_not_scene_hash_and_binds_term_and_leader() {
    let room = RoomId::from_bytes([0xaa; 32]);
    let leader = id(0xbb);
    let other_leader = id(0xcc);
    let node = id(0xdd);
    let scene = json!({"v": 1, "n": 0, "room": room.to_string()});
    let hash = scene_hash(&scene);

    let digest = cert_digest(&room, 0, &hash, 1, &leader, &node);

    assert_ne!(hash, digest);
    assert_ne!(digest, cert_digest(&room, 0, &hash, 2, &leader, &node));
    assert_ne!(
        digest,
        cert_digest(&room, 0, &hash, 1, &other_leader, &node)
    );
}

#[test]
fn cert_digest_matches_the_exact_jcs_payload_vector() {
    let room = RoomId::from_bytes([0xaa; 32]);
    let digest = cert_digest(&room, 7, &[0x11; 32], 4, &id(0xbb), &id(0xdd));

    assert_eq!(
        hex::encode(digest),
        "45d103f3d0b370b79a5baacae2dd2692af72cf9a930520d2ed8fc44c5d87ac0a"
    );
}

#[test]
fn sign_and_verify_use_the_raw_32_byte_digest() {
    let signing_key = SigningKey::from_bytes(&[7; 32]);
    let verifying_key = signing_key.verifying_key();
    let digest = [0x42; 32];
    let signature = sign(&signing_key, &digest);

    assert!(verify(&verifying_key, &digest, &signature));

    let hex_ascii = hex::encode(digest);
    let wrong_digest: [u8; 32] = sha2::Sha256::digest(hex_ascii.as_bytes()).into();
    assert!(!verify(&verifying_key, &wrong_digest, &signature));
}

#[test]
fn node_id_is_lowercase_32_byte_public_key_hex() {
    let node = id(0xab);
    let encoded = node.to_string();

    assert_eq!(encoded.len(), 64);
    assert_eq!(encoded, "ab".repeat(32));
    assert_eq!(NodeId::from_str(&encoded).unwrap(), node);
    assert!(NodeId::from_str(&encoded.to_uppercase()).is_err());
    assert!(NodeId::from_str("ab").is_err());
}
