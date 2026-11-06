//! The published `Noise_NK_25519_ChaChaPoly_SHA256` vectors, run against
//! the channel's own `snow` configuration (`handshake::builder`) with the
//! vectors' ephemeral keys injected.
//!
//! Sources (copied 2026-11-06): noise-c `tests/vector/cacophony.txt` and
//! `tests/vector/noise-c-basic.txt`. Paseo keeps the same two in
//! `packages/relay/src/noise/fixtures/`. Messages alternate: the initiator
//! sends 1, 3 and 5, the responder 2, 4 and 6; 1 and 2 are the handshake.

use snow::HandshakeState;

use super::support::{bytes32, hex};
use crate::NodeKeypair;
use crate::handshake::builder;

const INIT_EPHEMERAL: &str = "893e28b9dc6ca8d611ab664754b8ceb7bac5117349a4439a6b0569da977c464a";
const INIT_REMOTE_STATIC: &str = "31e0303fd6418d2f8c0e78b91f22e8caed0fbe48656dcf4767e4834f701b8f62";
const RESP_STATIC: &str = "4a3acbfdb163dec651dfa3194dece676d437029c62a408b4c5ea9114246e4893";
const RESP_EPHEMERAL: &str = "bbdb4cdbd309f1a1f2e1456967fe288cadd6f712d65dc7b7793d5e63da6b375b";

/// The payloads of all six messages, in both vectors.
const PAYLOADS: [&str; 6] = [
    "4c756477696720766f6e204d69736573",
    "4d757272617920526f746862617264",
    "462e20412e20486179656b",
    "4361726c204d656e676572",
    "4a65616e2d426170746973746520536179",
    "457567656e2042f6686d20766f6e2042617765726b",
];

/// Transport messages 3 to 6: the same in both vectors, since the prologue
/// only reaches the handshake's tags and the keys do not depend on it.
const TRANSPORT: [&str; 4] = [
    "a62de29ce27cb80245d440d986ed816c156e9d757d7008df2198b0",
    "174a35f11c689f4530d7208618e0564ae12f2f50ba8eb4df5382ff",
    "337e475ebb8eae60f91974c4e455a5af38d1d8628d1803b160d60442874b0a1777",
    "047e80e060b7bb08b53c5a23dfe9920cae135b9d1dc6302fc475003062723700366346ac9d",
];

struct Vector {
    prologue: &'static str,
    handshake: [&'static str; 2],
    handshake_hash: Option<&'static str>,
}

const CACOPHONY: Vector = Vector {
    prologue: "4a6f686e2047616c74",
    handshake: [
        "ca35def5ae56cec33dc2036731ab14896bc4c75dbb07a61f879f8e3afa4c79448134d00711fdb390a0d178fa008f6d47d2891e5ea18ae136c3b4c23ac384efb0",
        "95ebc60d2b1fa672c1f46a8aa265ef51bfe38e7ccb39ec5be34069f1448088438ea16e3701bc0d77744f117bee22451c9afa7f4cdbbcff00c04a8ee0913c88",
    ],
    handshake_hash: None,
};

const NOISE_C_BASIC: Vector = Vector {
    prologue: "50726f6c6f677565313233",
    handshake: [
        "ca35def5ae56cec33dc2036731ab14896bc4c75dbb07a61f879f8e3afa4c79448134d00711fdb390a0d178fa008f6d47f49a76e297aa164052a3d842aa8ff7d8",
        "95ebc60d2b1fa672c1f46a8aa265ef51bfe38e7ccb39ec5be34069f1448088438ea16e3701bc0d77744f117bee22451628b075da65d4114bd343e2d93006c4",
    ],
    handshake_hash: Some("d5c4ce9ffe8bcd940aa50f842a5d4d90d3f7163f4b3916deb87a5d747712d718"),
};

/// Write `payload` on one side, check the message, read it on the other.
fn exchange(
    writer: &mut HandshakeState,
    reader: &mut HandshakeState,
    payload: &str,
    expected: &str,
) {
    let payload = hex(payload);
    let mut message = vec![0_u8; 1024];
    let len = writer.write_message(&payload, &mut message).unwrap();
    assert_eq!(&message[..len], hex(expected).as_slice());
    let mut read = vec![0_u8; 1024];
    let len = reader.read_message(&message[..len], &mut read).unwrap();
    assert_eq!(&read[..len], payload.as_slice());
}

fn run(vector: &Vector) {
    // The vector names the responder's public key separately: deriving it
    // from the secret checks `NodeKeypair` too.
    let responder_key = NodeKeypair::from_secret(bytes32(RESP_STATIC));
    assert_eq!(
        responder_key.public().as_bytes(),
        &bytes32(INIT_REMOTE_STATIC)
    );

    let prologue = hex(vector.prologue);
    let init_ephemeral = hex(INIT_EPHEMERAL);
    let resp_ephemeral = hex(RESP_EPHEMERAL);
    let mut initiator = builder(&prologue)
        .unwrap()
        .remote_public_key(responder_key.public().as_bytes())
        .unwrap()
        .fixed_ephemeral_key_for_testing_only(&init_ephemeral)
        .build_initiator()
        .unwrap();
    let mut responder = builder(&prologue)
        .unwrap()
        .local_private_key(responder_key.secret())
        .unwrap()
        .fixed_ephemeral_key_for_testing_only(&resp_ephemeral)
        .build_responder()
        .unwrap();

    exchange(
        &mut initiator,
        &mut responder,
        PAYLOADS[0],
        vector.handshake[0],
    );
    exchange(
        &mut responder,
        &mut initiator,
        PAYLOADS[1],
        vector.handshake[1],
    );
    assert!(initiator.is_handshake_finished() && responder.is_handshake_finished());
    if let Some(hash) = vector.handshake_hash {
        assert_eq!(initiator.get_handshake_hash(), hex(hash).as_slice());
        assert_eq!(responder.get_handshake_hash(), hex(hash).as_slice());
    }

    let mut initiator = initiator.into_transport_mode().unwrap();
    let mut responder = responder.into_transport_mode().unwrap();
    for (index, (payload, expected)) in PAYLOADS[2..].iter().zip(TRANSPORT).enumerate() {
        let payload = hex(payload);
        let mut message = vec![0_u8; 1024];
        let mut read = vec![0_u8; 1024];
        let (writer, reader) = if index % 2 == 0 {
            (&mut initiator, &mut responder)
        } else {
            (&mut responder, &mut initiator)
        };
        let len = writer.write_message(&payload, &mut message).unwrap();
        assert_eq!(&message[..len], hex(expected).as_slice());
        let len = reader.read_message(&message[..len], &mut read).unwrap();
        assert_eq!(&read[..len], payload.as_slice());
    }
}

#[test]
fn the_cacophony_vector_passes() {
    run(&CACOPHONY);
}

#[test]
fn the_noise_c_vector_passes_with_its_handshake_hash() {
    run(&NOISE_C_BASIC);
}
