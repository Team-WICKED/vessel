//! Validated names: profiles, environment variables, MCP servers, secrets
//! and HTTP headers.
//!
//! [Decision Log]
//! - Purpose: these names end up as file names, TOML keys, command-line
//!   overrides (`codex -c mcp_servers.<name>.command=…`), environment
//!   variable names and HTTP header names. A name that is valid in one place
//!   and needs quoting or escaping in another is how injection starts.
//! - Constraint: one error type and one implementation; five newtypes that
//!   differ only in their character rule must not be five copies.
//! - Alternatives: plain `String`s checked where they are used (every user
//!   repeats the check, or forgets it); one `Name` type for all (a profile
//!   name could then be passed where a secret name is wanted).
//! - Choice: a `NameKind` enum owns the rules; a private macro declares each
//!   newtype from its kind. Parsing is the only constructor, so a value of
//!   the type is valid by construction, on the wire as everywhere else.
//! - Trade-off: the rules are deliberately narrower than what each context
//!   allows (environment variables on Windows may contain more, for
//!   example); a wider rule can be added later, a narrower one cannot.

use std::fmt;

/// Which kind of name, and so which rule applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NameKind {
    /// A harness profile: `[A-Za-z0-9._-]`, 1 to 64.
    Profile,
    /// An environment variable: `[A-Za-z_][A-Za-z0-9_]*`, up to 128.
    EnvVar,
    /// An MCP server: `[A-Za-z0-9_-]`, 1 to 64. No `.`, so it stays one key
    /// in dotted TOML paths.
    McpServer,
    /// A secret on a node: the environment variable rule.
    Secret,
    /// An HTTP header field name: an RFC 9110 token, up to 128.
    Header,
    /// A model route in a node's `models.toml` (protocol 7): the profile
    /// rule.
    ModelRoute,
    /// A model provider in a node's `models.toml` (protocol 7): the profile
    /// rule, so no `:` (it is sealed into reasoning envelopes).
    Provider,
}

impl NameKind {
    /// Whether `text` is a valid name of this kind.
    #[must_use]
    pub fn accepts(self, text: &str) -> bool {
        let rule = self.chars();
        let mut chars = text.chars();
        text.len() <= rule.max_len && chars.next().is_some_and(rule.first) && chars.all(rule.rest)
    }

    const fn chars(self) -> CharRule {
        match self {
            Self::Profile | Self::ModelRoute | Self::Provider => CharRule::same(64, profile_char),
            Self::EnvVar | Self::Secret => CharRule {
                max_len: 128,
                first: env_first,
                rest: env_char,
            },
            Self::McpServer => CharRule::same(64, mcp_char),
            Self::Header => CharRule::same(128, token_char),
        }
    }

    /// The rule, for error messages.
    #[must_use]
    pub const fn rule(self) -> &'static str {
        match self {
            Self::Profile | Self::ModelRoute | Self::Provider => {
                "1-64 letters, digits, `.`, `_` or `-`"
            }
            Self::EnvVar | Self::Secret => {
                "a letter or `_`, then letters, digits or `_`; at most 128"
            }
            Self::McpServer => "1-64 letters, digits, `_` or `-`",
            Self::Header => "1-128 HTTP token characters",
        }
    }
}

impl fmt::Display for NameKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Profile => "profile name",
            Self::EnvVar => "environment variable name",
            Self::McpServer => "MCP server name",
            Self::Secret => "secret name",
            Self::Header => "HTTP header name",
            Self::ModelRoute => "model route name",
            Self::Provider => "model provider name",
        })
    }
}

/// A kind's character rule: a length bound in bytes (every allowed
/// character is ASCII, so bytes are characters), the first character, and
/// the rest.
struct CharRule {
    max_len: usize,
    first: fn(char) -> bool,
    rest: fn(char) -> bool,
}

impl CharRule {
    const fn same(max_len: usize, every: fn(char) -> bool) -> Self {
        Self {
            max_len,
            first: every,
            rest: every,
        }
    }
}

const fn profile_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

const fn env_first(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

const fn env_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

const fn mcp_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

/// RFC 9110 section 5.6.2 `tchar`.
const fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '!' | '#'
                | '$'
                | '%'
                | '&'
                | '\''
                | '*'
                | '+'
                | '-'
                | '.'
                | '^'
                | '_'
                | '`'
                | '|'
                | '~'
        )
}

/// A name that broke its kind's rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidName {
    /// What it was meant to name.
    pub kind: NameKind,
    /// The text given.
    pub value: String,
}

impl fmt::Display for InvalidName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is not a valid {} ({})",
            self.value,
            self.kind,
            self.kind.rule()
        )
    }
}

impl std::error::Error for InvalidName {}

/// Declares a name newtype checked by a `NameKind` rule.
macro_rules! name {
    ($(#[$meta:meta])* $name:ident, $kind:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// The rule this name follows.
            pub const KIND: NameKind = NameKind::$kind;

            /// The name.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = InvalidName;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                if Self::KIND.accepts(s) {
                    Ok(Self(s.to_owned()))
                } else {
                    Err(InvalidName { kind: Self::KIND, value: s.to_owned() })
                }
            }
        }

        crate::key::serde_via_text!($name);
    };
}

name! {
    /// A profile's name: what `--profile` and `profiles.toml` use.
    ProfileName, Profile
}

name! {
    /// An environment variable's name.
    EnvName, EnvVar
}

name! {
    /// An MCP server's name, unique within a fleet's definitions.
    McpServerName, McpServer
}

name! {
    /// A secret's name in a node's `secrets.toml`; definitions refer to
    /// secrets by it, so the value never leaves the node it is on.
    SecretName, Secret
}

name! {
    /// An HTTP header field name.
    HeaderName, Header
}

name! {
    /// A model route's name in a node's `models.toml`, which profiles name
    /// with `model = "..."` (protocol 7).
    ModelRouteName, ModelRoute
}

name! {
    /// A model provider's name in a node's `models.toml` (protocol 7).
    ProviderName, Provider
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepts(kind: NameKind) -> impl Fn(&str) -> bool {
        move |text| kind.accepts(text)
    }

    #[test]
    fn profile_names_are_plain_words() {
        let ok = accepts(NameKind::Profile);
        for good in ["work", "claude-alt", "a.b_c", &"x".repeat(64)] {
            assert!(ok(good), "{good:?}");
        }
        for bad in ["", "two words", "a/b", "é", &"x".repeat(65)] {
            assert!(!ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn env_and_secret_names_follow_posix() {
        for kind in [NameKind::EnvVar, NameKind::Secret] {
            let ok = accepts(kind);
            for good in ["PATH", "_x", "CLAUDE_CONFIG_DIR", "a1"] {
                assert!(ok(good), "{good:?}");
            }
            for bad in ["", "1A", "A-B", "A=B", "A B", &"A".repeat(129)] {
                assert!(!ok(bad), "{bad:?}");
            }
        }
    }

    #[test]
    fn mcp_server_names_stay_one_toml_key() {
        let ok = accepts(NameKind::McpServer);
        assert!(ok("docs-mcp_2"));
        for bad in ["", "a.b", "a b", "a\"b"] {
            assert!(!ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn header_names_are_tokens() {
        let ok = accepts(NameKind::Header);
        assert!(ok("X-Api-Key"));
        for bad in ["", "a:b", "a b", "a\nb"] {
            assert!(!ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn names_parse_and_travel_as_text() {
        let name: EnvName = "HOME".parse().unwrap();
        assert_eq!(serde_json::to_string(&name).unwrap(), r#""HOME""#);
        let error = serde_json::from_str::<SecretName>(r#""no way""#).unwrap_err();
        assert!(error.to_string().contains("secret name"), "{error}");
        assert_eq!(
            "a.b".parse::<McpServerName>().unwrap_err(),
            InvalidName {
                kind: NameKind::McpServer,
                value: "a.b".to_owned()
            }
        );
    }
}
