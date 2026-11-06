//! What a fake was started with, written to a file: `report <path>` in
//! `fake-tui` (a line) and `fake-acp` (a prompt).
//!
//! [Decision Log]
//! - Purpose: tests that check where secret values go (the milestone 4
//!   canary test) must see a harness's arguments and whole environment,
//!   and the MCP servers ACP gave it, values included, without those values
//!   passing through the session itself: `env NAME` and `mcp` answer in
//!   the session's output, which the node keeps in its log and store, where
//!   the same test requires that no value appears.
//! - Choice: the fake writes one JSON object to a path the test chose
//!   (outside every directory the test searches) and answers only
//!   `reported`. It goes through a temporary file beside it and a rename,
//!   so a test polling for the file never reads half of it.
//! - The environment is written whole: a test asserting that a value is in
//!   no variable but the expected ones needs every name. The file is the
//!   test's own, in its temporary directory.

use std::io;
use std::path::Path;

use serde_json::{Map, Value};

/// Write `{"args": [...], "env": {...}}` (and `"mcp"`, when given) about
/// this process to `path`.
///
/// # Errors
/// The file could not be written or moved into place.
pub fn write_report(path: &Path, mcp: Option<Value>) -> io::Result<()> {
    let args: Vec<Value> = std::env::args_os()
        .skip(1)
        .map(|arg| Value::String(arg.to_string_lossy().into_owned()))
        .collect();
    let env: Map<String, Value> = std::env::vars_os()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                Value::String(value.to_string_lossy().into_owned()),
            )
        })
        .collect();
    let mut report = Map::new();
    let _none = report.insert("args".to_owned(), Value::Array(args));
    let _none = report.insert("env".to_owned(), Value::Object(env));
    if let Some(mcp) = mcp {
        let _none = report.insert("mcp".to_owned(), mcp);
    }
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    std::fs::write(&partial, Value::Object(report).to_string())?;
    std::fs::rename(&partial, path)
}
