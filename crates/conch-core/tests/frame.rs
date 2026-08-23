use conch_core::{
    consensus::{GetScenes, SwarmMsg},
    frame::{decode, encode, FrameError},
    types::RoomId,
};

#[test]
fn frame_is_u32be_length_then_json() {
    let message = SwarmMsg::GetScenes(GetScenes {
        room: RoomId::from_bytes([7; 32]),
        from_n: 2,
        to_n: 9,
    });
    let bytes = encode(&message).unwrap();
    assert_eq!(
        u32::from_be_bytes(bytes[..4].try_into().unwrap()),
        (bytes.len() - 4) as u32
    );
    assert_eq!(decode::<SwarmMsg>(&bytes).unwrap(), message);

    let value: serde_json::Value = serde_json::from_slice(&bytes[4..]).unwrap();
    assert_eq!(value["typ"], "get_scenes");
    assert!(value.get("data").is_none(), "wire fields are not nested");
}

#[test]
fn frame_rejects_length_mismatch() {
    let mut bytes = encode(&serde_json::json!({"typ": "x"})).unwrap();
    bytes[3] += 1;
    assert!(matches!(
        decode::<serde_json::Value>(&bytes),
        Err(FrameError::LengthMismatch)
    ));
}
