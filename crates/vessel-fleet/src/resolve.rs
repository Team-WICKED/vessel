//! Filling in secret references at launch: [`resolve`].
//!
//! [Decision Log]
//! - Purpose: the manifest names secrets; each node fills them from its
//!   own store when it launches a harness (ADR 0012). A server whose
//!   secrets are not all here is left out of the session, which says so.
//! - Choice: `McpTransport::try_map_values`, run twice: once to see every
//!   value (a lookup that never fails, collecting what is missing), then,
//!   for servers with nothing missing, to take the values. A single pass
//!   that fails would name only the first missing secret, and the person
//!   setting secrets up would learn them one launch at a time.

use std::collections::BTreeSet;
use std::convert::Infallible;

use vessel_proto::{ConfigValue, McpDefinitions, ResolvedMcpServers, ResolvedValue, SecretName};

/// The servers of `definitions` with every secret filled in by `secret`,
/// and every secret that was missing (each once, in order). A server with
/// any missing secret is left out.
#[must_use]
pub fn resolve(
    definitions: &McpDefinitions,
    secret: impl Fn(&SecretName) -> Option<ResolvedValue>,
) -> (ResolvedMcpServers, Vec<SecretName>) {
    let mut resolved = ResolvedMcpServers::new();
    let mut missing = BTreeSet::new();
    for (name, transport) in definitions {
        let mut absent = Vec::new();
        let looked_up = transport.clone().try_map_values(
            |value| -> Result<Option<ResolvedValue>, Infallible> {
                Ok(match value {
                    ConfigValue::Text(text) => Some(ResolvedValue::new(text)),
                    ConfigValue::Secret(name) => {
                        let found = secret(&name);
                        if found.is_none() {
                            absent.push(name);
                        }
                        found
                    }
                })
            },
        );
        let Ok(looked_up) = looked_up;
        if !absent.is_empty() {
            tracing::info!(server = %name, missing = absent.len(), "an MCP server is left out: secrets are missing");
            missing.extend(absent);
            continue;
        }
        if let Ok(server) = looked_up.try_map_values(|value| value.ok_or(())) {
            let _previous = resolved.insert(name.clone(), server);
        }
    }
    (resolved, missing.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use vessel_proto::McpTransport;

    use super::*;

    fn secret_name(text: &str) -> SecretName {
        text.parse().unwrap()
    }

    #[test]
    fn every_missing_secret_is_named_and_its_server_left_out() {
        let definitions: McpDefinitions = serde_json::from_str(
            r#"{
                "docs": {"type": "stdio", "command": "npx", "args": ["-y", "docs"],
                         "env": {"DOCS_TOKEN": {"secret": "DOCS_TOKEN"}, "MODE": "fast"}},
                "tracker": {"type": "http", "url": "https://tracker.example/mcp",
                            "headers": {"Authorization": {"secret": "TRACKER"},
                                        "X-Team": {"secret": "TEAM"}}},
                "plain": {"type": "stdio", "command": "plain-mcp"}
            }"#,
        )
        .unwrap();
        let store: BTreeMap<SecretName, String> =
            [(secret_name("DOCS_TOKEN"), "s3cret".to_owned())].into();
        let (resolved, missing) = resolve(&definitions, |name| {
            store.get(name).cloned().map(ResolvedValue::new)
        });
        assert_eq!(missing, [secret_name("TEAM"), secret_name("TRACKER")]);
        let names: Vec<String> = resolved.keys().map(ToString::to_string).collect();
        assert_eq!(names, ["docs", "plain"]);
        let Some(McpTransport::Stdio { env, args, .. }) = resolved.get(&"docs".parse().unwrap())
        else {
            panic!("docs is a stdio server");
        };
        assert_eq!(args, &["-y", "docs"]);
        let values: Vec<&str> = env.values().map(ResolvedValue::expose).collect();
        assert_eq!(values, ["s3cret", "fast"]);
        assert!(
            !format!("{resolved:?}").contains("s3cret"),
            "redacted in Debug"
        );
    }
}
