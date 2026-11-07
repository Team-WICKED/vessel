//! Opaque JSON passthrough.
//!
//! [Decision Log]
//! - Purpose: some payloads are open-ended by nature (a tool's input
//!   arguments, a harness's raw tool output). Vessel forwards them to clients
//!   but never decides anything based on their contents.
//! - Constraint: the project forbids untyped `serde_json::Value` trees in
//!   domain and wire types (AGENTS.md section 2). Every frame is an
//!   internally tagged enum, and serde buffers those before decoding, so
//!   `serde_json::RawValue`'s zero-copy capture fails anywhere inside a frame.
//! - Alternatives: `Value` (allocation-heavy tree that invites ad-hoc field
//!   access); `String` (no validity guarantee, double-escaped on the wire);
//!   adjacently tagged frames so `RawValue` capture works (verbose nesting,
//!   and silently breaks if a client writes `content` before the tag).
//! - Choice: a newtype over `Box<RawValue>` whose decoder streams whatever
//!   value it is given into one compact JSON buffer (no intermediate tree,
//!   one pass, works at any nesting depth). It offers no field access, so
//!   code that needs a field must define a typed struct instead.
//! - Cost: the text is re-emitted compactly, not byte-for-byte (whitespace
//!   and float spelling may change; the value is the same). Equality compares
//!   that compact text, which is only used in tests.

use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

/// A JSON value carried as text and never inspected.
pub struct RawJson(Box<RawValue>);

impl RawJson {
    /// Validate `text` as JSON and wrap it.
    ///
    /// # Errors
    /// Fails if `text` is not a single valid JSON value.
    pub fn from_text(text: impl Into<String>) -> Result<Self, serde_json::Error> {
        RawValue::from_string(text.into()).map(Self)
    }

    /// Serialise any value straight into raw form (used by decoders that
    /// already hold a typed foreign value).
    ///
    /// # Errors
    /// Fails if `value` cannot be serialised as JSON.
    pub fn from_serialize<T: Serialize + ?Sized>(value: &T) -> Result<Self, serde_json::Error> {
        serde_json::value::to_raw_value(value).map(Self)
    }

    /// `{}`, the empty object (a tool call's input before its arguments
    /// arrive).
    #[must_use]
    pub fn empty_object() -> Self {
        // `{}` is JSON, so the `null` fallback is never taken; it keeps
        // this constructor free of a panic path.
        RawValue::from_string(String::from("{}"))
            .map_or_else(|_never| Self(RawValue::NULL.to_owned()), Self)
    }

    /// The JSON text.
    #[must_use]
    pub fn as_text(&self) -> &str {
        self.0.get()
    }
}

impl Clone for RawJson {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl PartialEq for RawJson {
    fn eq(&self, other: &Self) -> bool {
        self.as_text() == other.as_text()
    }
}

impl Eq for RawJson {}

impl fmt::Debug for RawJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RawJson").field(&self.as_text()).finish()
    }
}

impl Serialize for RawJson {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RawJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut buffer = Vec::new();
        Transcode(&mut buffer).deserialize(deserializer)?;
        let text = String::from_utf8(buffer).map_err(de::Error::custom)?;
        RawValue::from_string(text)
            .map(Self)
            .map_err(de::Error::custom)
    }
}

/// Writes the value it is given into the buffer as compact JSON.
struct Transcode<'a>(&'a mut Vec<u8>);

impl Transcode<'_> {
    fn scalar<T: Serialize + ?Sized, E: de::Error>(self, value: &T) -> Result<(), E> {
        serde_json::to_writer(&mut *self.0, value).map_err(E::custom)
    }
}

impl<'de> DeserializeSeed<'de> for Transcode<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Transcode<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<(), E> {
        self.scalar(&value)
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<(), E> {
        self.scalar(&value)
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<(), E> {
        self.scalar(&value)
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<(), E> {
        self.scalar(&value)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<(), E> {
        self.scalar(value)
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        self.0.extend_from_slice(b"null");
        Ok(())
    }

    fn visit_none<E: de::Error>(self) -> Result<(), E> {
        self.visit_unit()
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        self.0.push(b'[');
        let mut first = true;
        loop {
            let separator_at = self.0.len();
            if !first {
                self.0.push(b',');
            }
            if seq.next_element_seed(Transcode(&mut *self.0))?.is_none() {
                // No element followed; drop the separator we optimistically wrote.
                self.0.truncate(separator_at);
                break;
            }
            first = false;
        }
        self.0.push(b']');
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        self.0.push(b'{');
        let mut first = true;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            if !first {
                self.0.push(b',');
            }
            first = false;
            serde_json::to_writer(&mut *self.0, key.as_ref()).map_err(de::Error::custom)?;
            self.0.push(b':');
            map.next_value_seed(Transcode(&mut *self.0))?;
        }
        self.0.push(b'}');
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_empty_object_is_empty() {
        assert_eq!(RawJson::empty_object().as_text(), "{}");
    }

    #[test]
    fn round_trips_compactly() {
        let raw = RawJson::from_text(r#"{"b":1,"a":[true,null,"x"],"c":{}}"#).unwrap();
        let wrapped = serde_json::to_string(&raw).unwrap();
        assert_eq!(wrapped, r#"{"b":1,"a":[true,null,"x"],"c":{}}"#);
        let back: RawJson = serde_json::from_str(&wrapped).unwrap();
        assert_eq!(back, raw);
    }

    #[test]
    fn decodes_inside_internally_tagged_enums() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "type")]
        enum Tagged {
            Carrier { raw: RawJson },
        }
        let json = r#"{"type":"Carrier","raw":{"k":[1,2.5,"é\n"],"e":[]}}"#;
        let Tagged::Carrier { raw } = serde_json::from_str(json).unwrap();
        assert_eq!(raw.as_text(), r#"{"k":[1,2.5,"é\n"],"e":[]}"#);
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(RawJson::from_text("{not json").is_err());
    }
}
