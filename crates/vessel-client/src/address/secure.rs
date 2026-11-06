//! A paired node's address: [`SecureTarget`] (ADR 0009, ADR 0010).

use std::fmt;

use vessel_proto::{NodePublicKey, RouteUrl};

/// A node reached over a secure channel: the key pinned at pairing, and the
/// routes to try (direct addresses first, then relays, plan decision 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecureTarget {
    /// The node's static key; every handshake checks it.
    pub key: NodePublicKey,
    /// Relays, in the order to try them.
    pub relays: Vec<RouteUrl>,
    /// Addresses to dial directly, tried before the relays.
    pub direct: Vec<RouteUrl>,
}

impl SecureTarget {
    /// Every route, in the order to try them.
    pub fn routes(&self) -> impl Iterator<Item = &RouteUrl> {
        self.direct.iter().chain(&self.relays)
    }
}

impl fmt::Display for SecureTarget {
    /// The key, shortened, and the first route: enough to tell nodes apart
    /// in a list.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key = self.key.to_string();
        let short = key.get(..15).unwrap_or(&key);
        write!(formatter, "{short}… ")?;
        let mut routes = self.routes();
        match routes.next() {
            Some(first) => {
                write!(formatter, "via {first}")?;
                let more = routes.count();
                if more > 0 {
                    write!(formatter, " (+{more})")?;
                }
                Ok(())
            }
            None => formatter.write_str("(no route)"),
        }
    }
}
