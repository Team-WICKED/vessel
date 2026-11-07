//! The minimal typed LSP edge. Unknown server fields are ignored, not retained.
//!
//! A `RawValue` exists only inside the transport envelope until correlation
//! selects its concrete decoder. It never leaves this crate. Opaque call
//! hierarchy data uses `RawJson` and is only sent back, never inspected.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

pub(crate) mod data;
pub(crate) mod initialize;

/// JSON-RPC permits integer or string IDs, including server-generated IDs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Id {
    Number(i64),
    Text(String),
}

#[derive(Debug, Deserialize)]
pub(crate) struct Incoming {
    pub(crate) jsonrpc: String,
    pub(crate) id: Option<Id>,
    pub(crate) method: Option<String>,
    pub(crate) params: Option<Box<RawValue>>,
    pub(crate) result: Option<Box<RawValue>>,
    pub(crate) error: Option<RpcError>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct RpcError {
    pub(crate) code: i32,
    pub(crate) message: String,
}

#[derive(Serialize)]
pub(crate) struct Request<'a, P> {
    pub(crate) jsonrpc: &'static str,
    pub(crate) id: Id,
    pub(crate) method: &'a str,
    pub(crate) params: P,
}

#[derive(Serialize)]
pub(crate) struct Notification<'a, P> {
    pub(crate) jsonrpc: &'static str,
    pub(crate) method: &'a str,
    pub(crate) params: P,
}

#[derive(Serialize)]
pub(crate) struct Response<T> {
    pub(crate) jsonrpc: &'static str,
    pub(crate) id: Id,
    pub(crate) result: T,
}

#[derive(Serialize)]
pub(crate) struct Failure {
    pub(crate) jsonrpc: &'static str,
    pub(crate) id: Id,
    pub(crate) error: RpcError,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct Empty {}

#[derive(Debug, Deserialize)]
pub(crate) struct Progress {
    pub(crate) token: Id,
    pub(crate) value: WorkDone,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum WorkDone {
    Begin,
    Report,
    End,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProgressCreate {
    pub(crate) token: Id,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Configuration {
    pub(crate) items: Vec<ConfigurationItem>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ConfigurationItem {
    pub(crate) section: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApplyEditDenied {
    pub(crate) applied: bool,
    pub(crate) failure_reason: &'static str,
}
