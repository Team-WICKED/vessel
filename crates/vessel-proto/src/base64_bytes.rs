//! Opaque bytes on the wire, as standard base64 text: terminal I/O and
//! fleet blobs share this one encoding (use with `#[serde(with = ...)]`).

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use serde::de::{self, Visitor};
use serde::{Deserializer, Serializer};

pub(crate) fn serialize<S: Serializer>(bytes: &Bytes, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&STANDARD.encode(bytes))
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Bytes, D::Error> {
    deserializer.deserialize_str(Base64Visitor)
}

struct Base64Visitor;

impl Visitor<'_> for Base64Visitor {
    type Value = Bytes;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("base64-encoded bytes")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
        STANDARD.decode(text).map(Bytes::from).map_err(E::custom)
    }
}
