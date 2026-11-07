//! Open families: the shapes every model API uses for "one of these kinds,
//! and more to come".
//!
//! [Decision Log]
//! - Purpose: the APIs add content blocks, items, stream events and enum
//!   values all the time (server tools, compaction items, new stop
//!   reasons). A proxy that refused or misread every new kind would break
//!   with each harness release; one that decoded them into `Value` trees
//!   would break the workspace's typing rule (AGENTS.md section 2).
//! - Constraint: serde's derive has no internally tagged variant that keeps
//!   an unknown tag's name (`#[serde(other)]` takes only a unit variant),
//!   and its untagged fallback also swallows a known kind with a missing
//!   field, turning a malformed `tool_use` into an "unknown" one.
//! - Alternatives: `#[serde(untagged)]` fallbacks (silent on malformed known
//!   kinds); a hand-written `Deserialize` per family (around twenty
//!   families, each one a place for the encoder and the decoder to drift
//!   apart); a crate for it (none in the workspace, and the job is small).
//! - Choice: two macros. [`open_enum!`] is an object tagged by one field
//!   (`type`, or `role` for Chat Completions messages): each known tag
//!   decodes into its payload struct and must decode (a malformed known
//!   kind is an error), any other tag decodes into `Unknown { kind, raw }`.
//!   [`open_str!`] is a string from a known set plus `Unknown(String)`.
//!   Both generate the encoder from the same table as the decoder.
//! - Cost: a tagged value is read twice (once into
//!   [`RawJson`](vessel_proto::RawJson), once into
//!   its payload) and its tag probed in between. Model requests are small
//!   next to a model call, so clarity wins over the extra pass.
//! - `Unknown` keeps the whole object (`raw`), so every type encodes back to
//!   what it decoded; translators log and drop it, never look inside.

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The tag `field` of the JSON object `text`, or `default` when the object
/// has none (the Responses API lets a message item omit `"type"`).
pub(crate) fn tag_of(
    text: &str,
    field: &'static str,
    default: Option<&'static str>,
) -> Result<String, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let tag = TagProbe(field).deserialize(&mut deserializer)?;
    deserializer.end()?;
    match (tag, default) {
        (Some(tag), _) => Ok(tag),
        (None, Some(default)) => Ok(default.to_owned()),
        (None, None) => Err(de::Error::missing_field(field)),
    }
}

/// Reads one string field of an object and skips the rest.
struct TagProbe(&'static str);

impl<'de> DeserializeSeed<'de> for TagProbe {
    type Value = Option<String>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for TagProbe {
    type Value = Option<String>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "an object tagged by `{}`", self.0)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut tag = None;
        while let Some(key) = map.next_key::<Cow<'de, str>>()? {
            if key == self.0 && tag.is_none() {
                tag = Some(map.next_value::<String>()?);
            } else {
                let IgnoredAny = map.next_value()?;
            }
        }
        Ok(tag)
    }
}

/// An object tagged by one field: known tags decode into their payload
/// struct, any other into `Unknown { kind, raw }`. See the module's decision
/// log.
///
/// ```text
/// open_enum! {
///     /// A content block.
///     pub enum Block in "type" {
///         /// Text.
///         "text" => Text(TextBlock),
///     }
/// }
/// ```
///
/// `in "type" or "message"` makes a missing tag mean `message`.
macro_rules! open_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident in $field:literal $(or $default:literal)? {
            $(
                $(#[$vmeta:meta])*
                $tag:literal => $variant:ident($payload:ty),
            )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq)]
        $vis enum $name {
            $(
                $(#[$vmeta])*
                $variant($payload),
            )+
            /// A kind this version does not know, kept whole: dropped (and
            /// logged) when translating, encoded back as it came otherwise.
            Unknown {
                /// Its tag.
                kind: String,
                /// The whole object, its tag included.
                raw: ::vessel_proto::RawJson,
            },
        }

        impl $name {
            /// Its tag on the wire.
            #[must_use]
            $vis fn kind(&self) -> &str {
                match self {
                    $(Self::$variant(_) => $tag,)+
                    Self::Unknown { kind, .. } => kind,
                }
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                /// The payload's fields after the tag.
                #[derive(::serde::Serialize)]
                struct Tagged<'a, T> {
                    #[serde(rename = $field)]
                    tag: &'static str,
                    #[serde(flatten)]
                    value: &'a T,
                }
                match self {
                    $(Self::$variant(value) => Tagged { tag: $tag, value }.serialize(serializer),)+
                    Self::Unknown { raw, .. } => raw.serialize(serializer),
                }
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                use ::serde::de::Error as _;
                let raw = <::vessel_proto::RawJson as ::serde::Deserialize>::deserialize(deserializer)?;
                let default: Option<&'static str> = None $(.or(Some($default)))?;
                let kind = $crate::wire::open::tag_of(raw.as_text(), $field, default)
                    .map_err(D::Error::custom)?;
                match kind.as_str() {
                    $(
                        $tag => ::serde_json::from_str(raw.as_text())
                            .map(Self::$variant)
                            .map_err(|error| D::Error::custom(format_args!("a `{kind}`: {error}"))),
                    )+
                    _ => Ok(Self::Unknown { kind, raw }),
                }
            }
        }
    };
}

/// A string from a known set, or `Unknown` with the string as it came.
macro_rules! open_str {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$vmeta:meta])*
                $text:literal => $variant:ident,
            )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        $vis enum $name {
            $(
                $(#[$vmeta])*
                $variant,
            )+
            /// A value this version does not know (never one of the above
            /// when decoded).
            Unknown(String),
        }

        impl $name {
            /// The value on the wire.
            #[must_use]
            $vis fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $text,)+
                    Self::Unknown(text) => text,
                }
            }

            /// The value `text` names.
            #[must_use]
            $vis fn parse(text: &str) -> Self {
                match text {
                    $($text => Self::$variant,)+
                    _ => Self::Unknown(text.to_owned()),
                }
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = <::std::borrow::Cow<'de, str> as ::serde::Deserialize>::deserialize(deserializer)?;
                Ok(Self::parse(&text))
            }
        }
    };
}

pub(crate) use {open_enum, open_str};

/// A field an API lets be a plain string or a list of parts (Anthropic's
/// `system` and `content`, the Responses `input`, Chat's `content`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextOr<T> {
    /// The string form.
    Text(String),
    /// The list form.
    Parts(Vec<T>),
}

impl<T: Serialize> Serialize for TextOr<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Text(text) => serializer.serialize_str(text),
            Self::Parts(parts) => parts.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for TextOr<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(TextOrVisitor(PhantomData))
    }
}

struct TextOrVisitor<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> Visitor<'de> for TextOrVisitor<T> {
    type Value = TextOr<T>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string or a list")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
        Ok(TextOr::Text(text.to_owned()))
    }

    fn visit_string<E: de::Error>(self, text: String) -> Result<Self::Value, E> {
        Ok(TextOr::Text(text))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        // The hint comes from the input: trust it only so far.
        let mut parts = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(64));
        while let Some(part) = seq.next_element()? {
            parts.push(part);
        }
        Ok(TextOr::Parts(parts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    open_str! {
        /// A test set.
        enum Colour {
            /// Red.
            "red" => Red,
        }
    }

    /// A test payload.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Named {
        name: String,
    }

    /// A test payload with nothing in it.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Empty {}

    open_enum! {
        /// A test family.
        enum Shape in "type" or "named" {
            /// Has a name.
            "named" => Named(Named),
            /// Has nothing.
            "empty" => Empty(Empty),
        }
    }

    #[test]
    fn known_tags_decode_and_encode_back() {
        let named: Shape = serde_json::from_str(r#"{"name":"a","type":"named"}"#).unwrap();
        assert_eq!(named, Shape::Named(Named { name: "a".into() }));
        assert_eq!(
            serde_json::to_string(&named).unwrap(),
            r#"{"type":"named","name":"a"}"#
        );
        let empty: Shape = serde_json::from_str(r#"{"type":"empty"}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&empty).unwrap(),
            r#"{"type":"empty"}"#
        );
        assert_eq!(empty.kind(), "empty");
    }

    #[test]
    fn unknown_tags_keep_their_name_and_their_object() {
        let text = r#"{"type":"hexagon","sides":6,"nested":{"type":"named"}}"#;
        let unknown: Shape = serde_json::from_str(text).unwrap();
        let Shape::Unknown { kind, raw } = &unknown else {
            panic!("decoded as known: {unknown:?}");
        };
        assert_eq!(kind, "hexagon");
        assert_eq!(raw.as_text(), text);
        assert_eq!(serde_json::to_string(&unknown).unwrap(), text);
    }

    #[test]
    fn a_malformed_known_tag_is_an_error_not_unknown() {
        let error = serde_json::from_str::<Shape>(r#"{"type":"named"}"#).unwrap_err();
        assert!(error.to_string().contains("a `named`"), "{error}");
        assert!(
            error.to_string().contains("missing field `name`"),
            "{error}"
        );
        assert!(serde_json::from_str::<Shape>(r#"{"type":7}"#).is_err());
        assert!(serde_json::from_str::<Shape>("[]").is_err());
    }

    #[test]
    fn a_missing_tag_takes_the_default() {
        let named: Shape = serde_json::from_str(r#"{"name":"b"}"#).unwrap();
        assert_eq!(named, Shape::Named(Named { name: "b".into() }));
        assert_eq!(
            tag_of("{}", "type", None).unwrap_err().to_string(),
            "missing field `type`"
        );
    }

    #[test]
    fn open_strings_keep_unknown_values() {
        let red: Colour = serde_json::from_str(r#""red""#).unwrap();
        assert_eq!(red, Colour::Red);
        let blue: Colour = serde_json::from_str(r#""blue""#).unwrap();
        assert_eq!(blue, Colour::Unknown("blue".into()));
        assert_eq!(serde_json::to_string(&blue).unwrap(), r#""blue""#);
    }

    #[test]
    fn text_or_parts_takes_either_form() {
        let text: TextOr<Named> = serde_json::from_str(r#""hi""#).unwrap();
        assert_eq!(text, TextOr::Text("hi".into()));
        let parts: TextOr<Named> = serde_json::from_str(r#"[{"name":"x"}]"#).unwrap();
        assert_eq!(parts, TextOr::Parts(vec![Named { name: "x".into() }]));
        assert_eq!(serde_json::to_string(&parts).unwrap(), r#"[{"name":"x"}]"#);
        assert!(serde_json::from_str::<TextOr<Named>>("7").is_err());
    }
}
