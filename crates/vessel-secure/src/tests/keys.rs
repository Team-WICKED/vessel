//! Keys, routing ids and fingerprints.

use super::support::{bytes32, hex};
use crate::{NodeKeypair, fingerprint, routing_id};

/// RFC 7748, section 6.1: Alice's key pair.
const SECRET: &str = "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a";
const PUBLIC: &str = "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a";
/// The first 16 bytes of SHA-256 of `PUBLIC` (computed with Python's
/// `hashlib`, 2026-11-06).
const ROUTING_ID: &str = "300c9c9603b92a4b39ed3958bf924011";

#[test]
fn the_public_key_is_derived_as_x25519_specifies() {
    let keypair = NodeKeypair::from_secret(bytes32(SECRET));
    assert_eq!(keypair.public().as_bytes(), &bytes32(PUBLIC));
    assert_eq!(
        keypair.public().to_string(),
        "x25519:hSDwCYkwp1R0i33ctD73Wg2_Og0mOBr066SpjqqbTmo"
    );
}

#[test]
fn the_routing_id_is_truncated_sha256() {
    let keypair = NodeKeypair::from_secret(bytes32(SECRET));
    let id = routing_id(keypair.public());
    assert_eq!(id.as_bytes().as_slice(), hex(ROUTING_ID).as_slice());
    assert_eq!(id.to_string(), ROUTING_ID);
}

#[test]
fn the_fingerprint_is_the_routing_id_in_groups() {
    let keypair = NodeKeypair::from_secret(bytes32(SECRET));
    assert_eq!(
        fingerprint(keypair.public()),
        "300c 9c96 03b9 2a4b 39ed 3958 bf92 4011"
    );
}

#[test]
fn debug_never_shows_the_secret() {
    let keypair = NodeKeypair::from_secret(bytes32(SECRET));
    let shown = format!("{keypair:?}");
    assert!(!shown.contains(SECRET), "{shown}");
    assert!(!shown.contains("7707"), "{shown}");
    assert!(shown.contains("x25519:"), "{shown}");
}

#[test]
fn generated_keys_differ() {
    let first = NodeKeypair::generate().unwrap();
    let second = NodeKeypair::generate().unwrap();
    assert_ne!(first.public(), second.public());
}
