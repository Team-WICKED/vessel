//! Fleet bundles and MCP server definitions (ADR 0012, protocol 5).
//!
//! [Decision Log]
//! - Purpose: the fleet's home publishes a bundle (instructions, skills, MCP
//!   server definitions) that members fetch and materialise per profile;
//!   sessions get MCP servers at launch.
//! - Constraint: a secret's value never leaves the node that holds it and
//!   never appears on a command line (`ps` shows arguments to every local
//!   user). Definitions travel between nodes; values are resolved on the
//!   node that launches.
//! - Alternatives: `${secret:NAME}` interpolated into any string (a typo
//!   becomes a literal silently, and a secret could be spliced into an
//!   argument); separate definition and resolved structs (two shapes to keep
//!   in step); a resolved type with serde (one stray `Serialize` call puts
//!   secrets on the wire).
//! - Choice: one transport shape generic over its value type.
//!   `McpTransport<ConfigValue>` is what the manifest carries: a value is
//!   literal text or a secret's name, and only environment variables and
//!   headers can hold one, never `args`. `McpTransport<ResolvedValue>` is
//!   what launch receives; `ResolvedValue` has no serde implementation, so
//!   the resolved form cannot be serialised at all, and its `Debug` is
//!   redacted.
//! - Maps keyed by name (`BTreeMap`) make duplicate files, servers,
//!   variables and headers unrepresentable and give a stable order for
//!   hashing and diffs.
//! - Trade-off: a literal environment value is visible in the manifest and
//!   in logs; anything sensitive must be a secret reference.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::DeviceId;
use crate::key::{KeyTextError, hex_array, serde_via_text, write_hex};
use crate::name::{EnvName, HeaderName, McpServerName, SecretName};

/// A bundle generation. The home increases it by one each time it
/// publishes; members compare it to what they last applied.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Generation(u64);

impl Generation {
    /// Wrap a number.
    #[must_use]
    pub const fn new(number: u64) -> Self {
        Self(number)
    }

    /// The number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The generation after this one, or `None` at the end of the range.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(number) => Some(Self(number)),
            None => None,
        }
    }
}

/// The SHA-256 of a blob, lowercase hex on the wire. Blobs are fetched and
/// stored by it, so equal content is fetched once.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobHash([u8; 32]);

impl BlobHash {
    /// Wrap digest bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}

impl fmt::Debug for BlobHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BlobHash({self})")
    }
}

impl FromStr for BlobHash {
    type Err = KeyTextError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        hex_array(s.trim()).map(Self)
    }
}

/// A file's place in a bundle: relative, `/`-separated, and valid as a file
/// name on every OS Vessel runs on, so joining it under a target directory
/// can never leave that directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BundlePath(String);

/// Why a bundle path was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidBundlePath {
    /// The text given.
    pub value: String,
    /// The rule it broke.
    pub reason: BundlePathRule,
}

/// The rules a bundle path follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundlePathRule {
    /// Empty, or longer than `BundlePath::MAX_LEN` bytes.
    Length,
    /// An empty, `.` or `..` segment (including a leading `/`).
    Segment,
    /// A control character or one of `\ : * ? " < > |`.
    Character,
    /// A segment ending in `.` or a space, which Windows drops.
    TrailingDotOrSpace,
    /// A Windows device name (`CON`, `NUL`, `COM1`, …), with or without an
    /// extension.
    ReservedName,
}

impl fmt::Display for InvalidBundlePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rule = match self.reason {
            BundlePathRule::Length => "must be 1 to 1024 bytes",
            BundlePathRule::Segment => "has an empty, `.` or `..` segment",
            BundlePathRule::Character => "has a control character or one of \\ : * ? \" < > |",
            BundlePathRule::TrailingDotOrSpace => "has a segment ending in `.` or a space",
            BundlePathRule::ReservedName => "has a Windows device name as a segment",
        };
        write!(f, "bundle path `{}` {rule}", self.value)
    }
}

impl std::error::Error for InvalidBundlePath {}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn check_segment(segment: &str) -> Result<(), BundlePathRule> {
    if matches!(segment, "" | "." | "..") {
        return Err(BundlePathRule::Segment);
    }
    if segment
        .chars()
        .any(|c| c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return Err(BundlePathRule::Character);
    }
    if segment.ends_with(['.', ' ']) {
        return Err(BundlePathRule::TrailingDotOrSpace);
    }
    let stem = segment.split('.').next().unwrap_or(segment);
    if RESERVED.iter().any(|name| stem.eq_ignore_ascii_case(name)) {
        return Err(BundlePathRule::ReservedName);
    }
    Ok(())
}

impl BundlePath {
    /// Longest allowed path, in bytes.
    pub const MAX_LEN: usize = 1024;

    /// The path text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Its segments, outermost first.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The path under `root`. Safe by construction: no segment can climb
    /// out or name a device.
    #[must_use]
    pub fn under(&self, root: &Path) -> PathBuf {
        self.segments()
            .fold(root.to_path_buf(), |path, segment| path.join(segment))
    }
}

impl fmt::Display for BundlePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for BundlePath {
    type Err = InvalidBundlePath;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let refuse = |reason| InvalidBundlePath {
            value: s.to_owned(),
            reason,
        };
        if s.is_empty() || s.len() > Self::MAX_LEN {
            return Err(refuse(BundlePathRule::Length));
        }
        s.split('/').try_for_each(check_segment).map_err(refuse)?;
        Ok(Self(s.to_owned()))
    }
}

/// An `http://` or `https://` URL with a host and no credentials in it.
///
/// A URL is public: it travels in manifests and, for some harnesses, on a
/// command line that every local user can read (`ps`). So `user:pass@` is
/// refused; a credential goes in a header, as a secret reference
/// (`ConfigValue::Secret`). A token in the query string cannot be told from
/// an ordinary parameter, so the same rule is stated here rather than
/// enforced: keep secrets out of URLs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HttpUrl(String);

/// A URL that is not `http://` or `https://` with a host, or that carries
/// credentials (`user:pass@`). Holds the URL with its credentials already
/// replaced, so neither `Display` nor `Debug` can repeat them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidHttpUrl(pub String);

impl fmt::Display for InvalidHttpUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "not an http:// or https:// URL with a host and without credentials: `{}`",
            self.0
        )
    }
}

impl std::error::Error for InvalidHttpUrl {}

/// The URL with anything before an `@` in its authority replaced, so an
/// error about a URL never repeats the credentials it was refused for.
fn redact_userinfo(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://<redacted>@{host}{tail}"),
        None => url.to_owned(),
    }
}

impl HttpUrl {
    /// The URL text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HttpUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for HttpUrl {
    type Err = InvalidHttpUrl;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s
            .strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"));
        let authority = rest.map(|rest| rest.split(['/', '?', '#']).next().unwrap_or(rest));
        let has_host = authority.is_some_and(|authority| !authority.is_empty());
        let has_credentials = authority.is_some_and(|authority| authority.contains('@'));
        if has_host && !has_credentials && !s.chars().any(|c| c.is_control() || c.is_whitespace()) {
            Ok(Self(s.to_owned()))
        } else {
            Err(InvalidHttpUrl(redact_userinfo(s)))
        }
    }
}

serde_via_text!(BlobHash, BundlePath, HttpUrl);

/// A file in a bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleFile {
    /// Its content's hash.
    pub hash: BlobHash,
    /// Its size in bytes.
    pub size: u64,
    /// Whether it is executable (scripts in skills). Ignored on Windows.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub executable: bool,
}

/// What a home publishes: every file by its path, and the MCP servers with
/// secret references unresolved.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Which publication this is.
    pub generation: Generation,
    /// The files.
    #[serde(default)]
    pub files: BTreeMap<BundlePath, BundleFile>,
    /// The MCP servers.
    #[serde(default)]
    pub mcp: McpDefinitions,
}

/// A value in an MCP server definition: literal text, or the name of a
/// secret each node resolves from its own store. On the wire, a string or
/// `{"secret": "NAME"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConfigValue {
    /// Literal text, visible to everything that sees the manifest.
    Text(String),
    /// A secret, by name.
    Secret(SecretName),
}

impl Serialize for ConfigValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        match self {
            Self::Text(text) => serializer.serialize_str(text),
            Self::Secret(name) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("secret", name)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ConfigValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ConfigValue;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(r#"a string or {"secret": "NAME"}"#)
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<ConfigValue, E> {
                Ok(ConfigValue::Text(text.to_owned()))
            }

            fn visit_string<E: serde::de::Error>(self, text: String) -> Result<ConfigValue, E> {
                Ok(ConfigValue::Text(text))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<ConfigValue, A::Error> {
                #[derive(Deserialize)]
                #[serde(rename_all = "snake_case")]
                enum Key {
                    Secret,
                }
                let (Key::Secret, name) = map
                    .next_entry::<Key, SecretName>()?
                    .ok_or_else(|| serde::de::Error::missing_field("secret"))?;
                if map.next_key::<Key>()?.is_some() {
                    return Err(serde::de::Error::duplicate_field("secret"));
                }
                Ok(ConfigValue::Secret(name))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// A value with secrets filled in, for launching a session on this node.
/// It has no serde implementation on purpose (the module notes say why),
/// and its `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedValue(String);

impl ResolvedValue {
    /// Wrap a resolved value.
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }

    /// The value, for the process environment or a request header only.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ResolvedValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ResolvedValue(<redacted>)")
    }
}

/// How a harness reaches an MCP server. `V` is `ConfigValue` in definitions
/// and `ResolvedValue` at launch; only environment variables and headers
/// hold one, so a secret can never become an argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    deny_unknown_fields,
    bound(serialize = "V: Serialize", deserialize = "V: Deserialize<'de>")
)]
pub enum McpTransport<V> {
    /// A local process speaking MCP on its stdio.
    Stdio {
        /// The program, found on the launching node's `PATH` if relative.
        command: PathBuf,
        /// Its arguments, literal only.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// Its environment.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<EnvName, V>,
    },
    /// A server over streamable HTTP.
    Http {
        /// Its endpoint.
        url: HttpUrl,
        /// Request headers (credentials usually go here).
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<HeaderName, V>,
    },
}

impl<V> McpTransport<V> {
    /// The same transport with every value converted; stops at the first
    /// error (resolving secrets: the first missing one).
    ///
    /// # Errors
    ///
    /// The first error `convert` returns.
    pub fn try_map_values<W, E>(
        self,
        mut convert: impl FnMut(V) -> Result<W, E>,
    ) -> Result<McpTransport<W>, E> {
        fn map<K: Ord, V, W, E>(
            values: BTreeMap<K, V>,
            convert: &mut impl FnMut(V) -> Result<W, E>,
        ) -> Result<BTreeMap<K, W>, E> {
            values
                .into_iter()
                .map(|(key, value)| convert(value).map(|value| (key, value)))
                .collect()
        }
        Ok(match self {
            Self::Stdio { command, args, env } => McpTransport::Stdio {
                command,
                args,
                env: map(env, &mut convert)?,
            },
            Self::Http { url, headers } => McpTransport::Http {
                url,
                headers: map(headers, &mut convert)?,
            },
        })
    }

    /// Every value it holds, in a stable order.
    pub fn values(&self) -> impl Iterator<Item = &V> {
        let (env, headers) = match self {
            Self::Stdio { env, .. } => (Some(env), None),
            Self::Http { headers, .. } => (None, Some(headers)),
        };
        env.into_iter()
            .flat_map(BTreeMap::values)
            .chain(headers.into_iter().flat_map(BTreeMap::values))
    }
}

/// MCP servers by name, with values of type `V`.
pub type McpServers<V> = BTreeMap<McpServerName, McpTransport<V>>;

/// MCP servers as defined in `fleet.toml` and carried in a manifest.
pub type McpDefinitions = McpServers<ConfigValue>;

/// MCP servers ready to hand to a harness at launch.
pub type ResolvedMcpServers = McpServers<ResolvedValue>;

/// `fleet.manifest` (protocol 5): the home's current bundle. Needs `Sync`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetManifest;

/// `fleet.blob` (protocol 5): part of a blob, from `offset`. A member asks
/// only for blobs it lacks, chunk by chunk. Needs `Sync`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadBlob {
    /// Which blob.
    pub hash: BlobHash,
    /// Where to start, in bytes.
    #[serde(default)]
    pub offset: u64,
}

/// Reply to `fleet.blob`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobChunk {
    /// Bytes from the requested offset: at most `MAX_LEN`, fewer at the end.
    #[serde(with = "crate::base64_bytes")]
    pub data: Bytes,
    /// The blob's whole size, so the member knows when it is done.
    pub total: u64,
}

impl BlobChunk {
    /// The most a chunk carries: 1 MiB, about 1.4 MiB as base64, well under
    /// the 16 MiB frame limit and small enough not to hold up other replies
    /// on the connection.
    pub const MAX_LEN: usize = 1 << 20;
}

/// `fleet.watch` (protocol 5): wait for a generation newer than `after`.
/// A long poll, not a subscription, so it needs no new frame type: the home
/// answers as soon as one is published, or with its current generation
/// (possibly `after` itself) when its wait limit passes, and the member asks
/// again. Needs `Sync`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchFleet {
    /// The generation the member has.
    pub after: Generation,
}

/// `fleet.report` (protocol 5): a member says what it applied and which
/// files it left alone because they were edited locally. Needs `Sync`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportApplied {
    /// The generation applied.
    pub generation: Generation,
    /// Files kept as edited, with the incoming version written beside them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<BundlePath>,
}

/// `fleet.members` (protocol 5): the home's members and what each last
/// reported. Needs `Admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListMembers;

/// A member as its home reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetMember {
    /// The member's device on the home.
    pub device: DeviceId,
    /// Its label.
    pub label: String,
    /// The generation it last applied, if it has reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<Generation>,
    /// Files it kept as edited.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<BundlePath>,
    /// When it last reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use super::*;

    #[test]
    fn bundle_paths_stay_inside_their_root() {
        for good in ["AGENTS.md", "skills/deploy/SKILL.md", "a/b.c/d", "x..y"] {
            let path: BundlePath = good.parse().unwrap();
            assert_eq!(
                path.under(Path::new("/root")),
                Path::new("/root").join(good)
            );
        }
        let rule = |text: &str| text.parse::<BundlePath>().unwrap_err().reason;
        assert_eq!(rule(""), BundlePathRule::Length);
        assert_eq!(rule(&"a".repeat(1025)), BundlePathRule::Length);
        for bad in ["/etc/passwd", "a//b", "a/", "../x", "a/./b", "a/../b"] {
            assert_eq!(rule(bad), BundlePathRule::Segment, "{bad:?}");
        }
        for bad in ["a\\b", "C:x", "a\nb", "a|b", "a*b"] {
            assert_eq!(rule(bad), BundlePathRule::Character, "{bad:?}");
        }
        assert_eq!(rule("dir./x"), BundlePathRule::TrailingDotOrSpace);
        assert_eq!(rule("x/name "), BundlePathRule::TrailingDotOrSpace);
        for bad in ["nul", "skills/CON.md", "com1.txt", "Lpt9"] {
            assert_eq!(rule(bad), BundlePathRule::ReservedName, "{bad:?}");
        }
        assert!("console.md".parse::<BundlePath>().is_ok());
    }

    #[test]
    fn generations_count_up_and_stop_at_the_end() {
        assert_eq!(Generation::new(41).next(), Some(Generation::new(42)));
        assert_eq!(Generation::new(u64::MAX).next(), None);
    }

    #[test]
    fn config_values_are_text_or_a_secret_reference() {
        let text = ConfigValue::Text("plain".to_owned());
        let secret = ConfigValue::Secret("DOCS_TOKEN".parse().unwrap());
        assert_eq!(serde_json::to_string(&text).unwrap(), r#""plain""#);
        assert_eq!(
            serde_json::to_string(&secret).unwrap(),
            r#"{"secret":"DOCS_TOKEN"}"#
        );
        for value in [text, secret] {
            let json = serde_json::to_string(&value).unwrap();
            assert_eq!(serde_json::from_str::<ConfigValue>(&json).unwrap(), value);
        }
        for bad in [
            r#"{"secrets":"X"}"#,
            r#"{"secret":"no way"}"#,
            r#"{"secret":"X","secret":"Y"}"#,
            "{}",
            "3",
        ] {
            assert!(serde_json::from_str::<ConfigValue>(bad).is_err(), "{bad}");
        }
    }

    fn docs_server() -> McpTransport<ConfigValue> {
        McpTransport::Stdio {
            command: PathBuf::from("npx"),
            args: vec!["-y".to_owned(), "@example/docs-mcp".to_owned()],
            env: BTreeMap::from([
                (
                    "DOCS_TOKEN".parse().unwrap(),
                    ConfigValue::Secret("DOCS_TOKEN".parse().unwrap()),
                ),
                ("MODE".parse().unwrap(), ConfigValue::Text("ro".to_owned())),
            ]),
        }
    }

    #[test]
    fn manifests_round_trip_and_refuse_unknown_fields() {
        let manifest = Manifest {
            generation: Generation::new(7),
            files: BTreeMap::from([(
                "skills/deploy/SKILL.md".parse().unwrap(),
                BundleFile {
                    hash: BlobHash::from_bytes([0xab; 32]),
                    size: 120,
                    executable: false,
                },
            )]),
            mcp: BTreeMap::from([
                ("docs".parse().unwrap(), docs_server()),
                (
                    "search".parse().unwrap(),
                    McpTransport::Http {
                        url: "https://mcp.example/v1".parse().unwrap(),
                        headers: BTreeMap::new(),
                    },
                ),
            ]),
        };
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(json.contains(r#""type":"stdio""#), "{json}");
        assert!(json.contains(&"ab".repeat(32)), "{json}");
        assert_eq!(serde_json::from_str::<Manifest>(&json).unwrap(), manifest);

        let typo = r#"{"generation":1,"mcp":{"x":{"type":"stdio","command":"a","evn":{}}}}"#;
        assert!(serde_json::from_str::<Manifest>(typo).is_err());
        let duplicate = r#"{"generation":1,"files":{},"files":{}}"#;
        assert!(serde_json::from_str::<Manifest>(duplicate).is_err());
    }

    #[test]
    fn resolving_maps_values_and_never_prints_them() {
        let resolved = docs_server()
            .try_map_values(|value| {
                Ok::<_, Infallible>(ResolvedValue::new(match value {
                    ConfigValue::Text(text) => text,
                    ConfigValue::Secret(_) => "s3cr3t-value".to_owned(),
                }))
            })
            .unwrap();
        assert_eq!(
            resolved
                .values()
                .map(ResolvedValue::expose)
                .collect::<Vec<_>>(),
            ["s3cr3t-value", "ro"]
        );
        assert!(!format!("{resolved:?}").contains("s3cr3t"));

        let missing = docs_server().try_map_values(|value| match value {
            ConfigValue::Text(text) => Ok(text),
            ConfigValue::Secret(name) => Err(name),
        });
        assert_eq!(missing.unwrap_err().as_str(), "DOCS_TOKEN");
    }

    #[test]
    fn http_urls_need_a_scheme_and_a_host() {
        assert!("http://127.0.0.1:8080/mcp".parse::<HttpUrl>().is_ok());
        for bad in [
            "ftp://x",
            "https://",
            "https:///x",
            "mcp.example",
            "https://a b",
        ] {
            assert!(bad.parse::<HttpUrl>().is_err(), "{bad}");
        }
        assert!("https://mcp.example/a@b?c=@".parse::<HttpUrl>().is_ok());
        let error = "https://me:s3cret@mcp.example/v1"
            .parse::<HttpUrl>()
            .unwrap_err();
        assert!(!error.to_string().contains("s3cret"), "{error}");
        assert!(!format!("{error:?}").contains("s3cret"), "{error:?}");
        assert!(
            error.to_string().contains("<redacted>@mcp.example/v1"),
            "{error}"
        );
    }
}
