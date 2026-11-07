//! What can go wrong translating, and what translation leaves out.

/// Why a request cannot be translated for the route's provider.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TranslateError {
    /// The request holds something the provider's API cannot carry; the
    /// message names it (a PDF toward a Chat Completions provider, an image
    /// by file id, a response kept at another provider).
    #[error("this route's provider cannot take {0}")]
    Unsupported(&'static str),
    /// A tool call's arguments are not a JSON object where the provider
    /// needs one.
    #[error("tool call {id} has arguments that are not a JSON object")]
    BadArguments {
        /// The call.
        id: String,
    },
    /// Encoding JSON failed (it does not, for these types).
    #[error("encoding JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Something a translation dropped because the other API cannot say it,
/// for the node's log (never shown to anyone else, never with content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    /// What kind of thing (`server_tool_use`, `web_search`, ...).
    pub kind: String,
    /// Where it was (`content block`, `tool`, `item`, ...).
    pub place: &'static str,
}

impl Dropped {
    /// A dropped `kind` found in `place`.
    #[must_use]
    pub fn new(kind: impl Into<String>, place: &'static str) -> Self {
        Self {
            kind: kind.into(),
            place,
        }
    }
}
