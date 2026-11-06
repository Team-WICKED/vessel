//! MCP servers for `session/new` and `session/load` (ADR 0012).
//!
//! Vessel's definitions ([`ResolvedMcpServers`], from `vessel-proto`) become
//! ACP's `McpServer` list here, so ACP types stay in this crate. Values go
//! into the request (the agent's stdin), never onto a command line.
//!
//! ACP requires every agent to accept stdio servers; HTTP ones only when it
//! advertises `mcp_capabilities.http`. An HTTP server the agent cannot take
//! is left out rather than failing the session: the session still works,
//! without that server, and the omission is logged by name.

use agent_client_protocol::v1::schema as acp;
use vessel_proto::{McpServerName, McpTransport, ResolvedMcpServers};

/// The servers as ACP sends them, and those the agent cannot take.
pub(crate) fn to_acp(
    servers: &ResolvedMcpServers,
    capabilities: &acp::McpCapabilities,
) -> (Vec<acp::McpServer>, Vec<McpServerName>) {
    let mut sent = Vec::with_capacity(servers.len());
    let mut left_out = Vec::new();
    for (name, transport) in servers {
        match transport {
            McpTransport::Stdio { command, args, env } => {
                let env = env
                    .iter()
                    .map(|(name, value)| acp::EnvVariable::new(name.as_str(), value.expose()))
                    .collect();
                sent.push(acp::McpServer::Stdio(
                    acp::McpServerStdio::new(name.as_str(), command)
                        .args(args.clone())
                        .env(env),
                ));
            }
            McpTransport::Http { url, headers } if capabilities.http => {
                let headers = headers
                    .iter()
                    .map(|(name, value)| acp::HttpHeader::new(name.as_str(), value.expose()))
                    .collect();
                sent.push(acp::McpServer::Http(
                    acp::McpServerHttp::new(name.as_str(), url.as_str()).headers(headers),
                ));
            }
            McpTransport::Http { .. } => left_out.push(name.clone()),
        }
    }
    (sent, left_out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use vessel_proto::ResolvedValue;

    use super::*;

    fn servers() -> ResolvedMcpServers {
        BTreeMap::from([
            (
                "docs".parse().unwrap(),
                McpTransport::Stdio {
                    command: PathBuf::from("/opt/docs"),
                    args: vec!["--stdio".to_owned()],
                    env: BTreeMap::from([(
                        "TOKEN".parse().unwrap(),
                        ResolvedValue::new("t".to_owned()),
                    )]),
                },
            ),
            (
                "web".parse().unwrap(),
                McpTransport::Http {
                    url: "https://example.invalid/mcp".parse().unwrap(),
                    headers: BTreeMap::from([(
                        "Authorization".parse().unwrap(),
                        ResolvedValue::new("Bearer b".to_owned()),
                    )]),
                },
            ),
        ])
    }

    #[test]
    fn stdio_always_http_when_advertised() {
        let (sent, left_out) = to_acp(&servers(), &acp::McpCapabilities::new().http(true));
        assert!(left_out.is_empty());
        let json = serde_json::to_value(&sent).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {"name": "docs", "command": "/opt/docs", "args": ["--stdio"],
                 "env": [{"name": "TOKEN", "value": "t"}]},
                {"type": "http", "name": "web", "url": "https://example.invalid/mcp",
                 "headers": [{"name": "Authorization", "value": "Bearer b"}]},
            ])
        );

        let (sent, left_out) = to_acp(&servers(), &acp::McpCapabilities::new());
        assert_eq!(sent.len(), 1);
        assert_eq!(left_out, ["web".parse::<McpServerName>().unwrap()]);
    }
}
