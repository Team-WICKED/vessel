//! TLS for `wss://` relays (plan, decision 1).
//!
//! [Decision Log]
//! - Purpose: a relay usually sits behind an existing HTTPS reverse proxy
//!   on 443, so nodes and clients must speak `wss://` to it. TLS adds no
//!   secrecy to the channel (ADR 0009); it gets the connection through.
//! - Constraint: ADR 0005's amendment: the crypto provider is chosen
//!   explicitly, never left to a process-wide default (milestone 1 panicked
//!   on a provider nobody installed).
//! - Alternatives: `aws-lc-rs` (rustls's default; `CMake` and `NASM` on
//!   Windows), `rustls-rustcrypto` (younger), native TLS (a system library
//!   per platform).
//! - Choice: rustls with `ring`, handed to every config through
//!   `builder_with_provider`, and the operating system's root certificates.
//!   Nothing global is installed, so a library user's own choice is never
//!   disturbed.

use std::fmt;
use std::sync::Arc;

use rustls::{ClientConfig, RootCertStore};
use tokio_tungstenite::Connector;

/// Why TLS could not be set up.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TlsError {
    /// The operating system gave no usable root certificate.
    #[error("no root certificates could be loaded from the operating system")]
    NoRoots,
    /// rustls refused the configuration.
    #[error("TLS could not be configured")]
    Rustls(#[source] rustls::Error),
}

/// How `wss://` relays are verified. Cheap to clone.
#[derive(Clone)]
pub struct TlsConfig {
    config: Arc<ClientConfig>,
}

impl fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsConfig").finish_non_exhaustive()
    }
}

impl TlsConfig {
    /// Trust the operating system's root certificates.
    ///
    /// # Errors
    /// None could be loaded, or rustls refused the configuration.
    pub fn native_roots() -> Result<Self, TlsError> {
        let found = rustls_native_certs::load_native_certs();
        for error in &found.errors {
            tracing::debug!(%error, "a root certificate store could not be read");
        }
        let mut roots = RootCertStore::empty();
        let (added, ignored) = roots.add_parsable_certificates(found.certs);
        tracing::debug!(added, ignored, "loaded root certificates");
        if roots.is_empty() {
            return Err(TlsError::NoRoots);
        }
        Self::with_roots(roots)
    }

    /// Trust exactly `roots` (tests, private certificate authorities).
    ///
    /// # Errors
    /// rustls refused the configuration.
    pub fn with_roots(roots: RootCertStore) -> Result<Self, TlsError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(TlsError::Rustls)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            config: Arc::new(config),
        })
    }

    /// The connector for `tokio-tungstenite` (used for `wss://` only).
    pub(crate) fn connector(&self) -> Connector {
        Connector::Rustls(Arc::clone(&self.config))
    }
}
