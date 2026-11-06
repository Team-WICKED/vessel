//! A node's name in the book: [`NodeName`].

use std::fmt;
use std::str::FromStr;

/// A node's name in the book.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeName(String);

/// Text that is not a node name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "`{0}` is not a node name (letters, digits, `.`, `_` and `-`, starting with a letter or digit)"
)]
pub struct InvalidNodeName(String);

impl NodeName {
    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for NodeName {
    type Err = InvalidNodeName;

    fn from_str(text: &str) -> Result<Self, InvalidNodeName> {
        let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
        let starts_plain = text.starts_with(|c: char| c.is_ascii_alphanumeric());
        if !starts_plain || !text.chars().all(plain) || text.len() > 64 {
            return Err(InvalidNodeName(text.to_owned()));
        }
        Ok(Self(text.to_owned()))
    }
}

impl fmt::Display for NodeName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
