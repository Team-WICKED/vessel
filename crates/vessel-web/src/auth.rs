//! Mutual challenge/response and single-use bootstrap redemption.
//!
//! [Decision Log]
//! - A reconnect must not give a port squatter a reusable secret. Each
//!   side proves its key with HMAC-SHA256 over fresh nonces and role tags.
//! - The URL code is consumed atomically once; the welcome gives the tab
//!   an independent random session key. History cannot recover that key.
//! - One launch has one session key; tabs may intentionally copy their
//!   sessionStorage, but a new browser context needs a new UI launch.
//! - Reuse the workspace's `RustCrypto` HMAC, not custom cryptography.
use std::sync::atomic::{AtomicBool, Ordering};

use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;
use subtle::ConstantTimeEq as _;

use crate::token::{LaunchToken, MintError};
use crate::wire::{AuthMode, LaunchId, Nonce, PresentedToken, ServerFrame};

#[derive(Debug)]
pub(crate) struct Auth {
    bootstrap: LaunchToken,
    session: LaunchToken,
    pub(crate) launch: LaunchId,
    consumed: AtomicBool,
}

impl Auth {
    pub(crate) fn new(bootstrap: LaunchToken) -> Result<Self, MintError> {
        Ok(Self {
            bootstrap,
            session: LaunchToken::mint()?,
            launch: LaunchId::from_random(LaunchToken::mint()?),
            consumed: AtomicBool::new(false),
        })
    }

    pub(crate) fn welcome(
        &self,
        mode: AuthMode,
        server: &Nonce,
        client: &Nonce,
        proof: &PresentedToken,
    ) -> Option<ServerFrame> {
        let key = match mode {
            AuthMode::Bootstrap => &self.bootstrap,
            AuthMode::Resume => &self.session,
        };
        let transcript = format!(
            "vessel-ui-v1\nclient\n{}\n{}\n{}\n",
            self.launch.as_str(),
            server.as_str(),
            client.as_str()
        );
        let expected = sign(key.expose(), &transcript)?;
        if !bool::from(expected.as_bytes().ct_eq(proof.as_str().as_bytes())) {
            return None;
        }
        if mode == AuthMode::Bootstrap && self.consumed.swap(true, Ordering::AcqRel) {
            return None;
        }
        let session = (mode == AuthMode::Bootstrap)
            .then(|| PresentedToken::new(self.session.expose().to_owned()));
        let version = env!("CARGO_PKG_VERSION").to_owned();
        let transcript = format!(
            "vessel-ui-v1\nserver\n{}\n{}\n{}\n{}\n{}",
            self.launch.as_str(),
            server.as_str(),
            client.as_str(),
            version,
            session.as_ref().map_or("", PresentedToken::as_str)
        );
        Some(ServerFrame::Welcome {
            version,
            launch: self.launch.clone(),
            proof: PresentedToken::new(sign(key.expose(), &transcript)?),
            session,
        })
    }

    pub(crate) fn authorized(&self, key: &str) -> bool {
        self.consumed.load(Ordering::Acquire) && self.session.matches(key)
    }
}

/// ASCII hex keys are imported as UTF-8 on both sides, not decoded bytes.
fn sign(key: &str, transcript: &str) -> Option<String> {
    use std::fmt::Write as _;
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).ok()?;
    mac.update(transcript.as_bytes());
    Some(
        mac.finalize()
            .into_bytes()
            .iter()
            .fold(String::with_capacity(64), |mut out, byte| {
                let _written = write!(out, "{byte:02x}");
                out
            }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(auth: &Auth, mode: AuthMode, server: &Nonce, client: &Nonce) -> PresentedToken {
        let key = match mode {
            AuthMode::Bootstrap => &auth.bootstrap,
            AuthMode::Resume => &auth.session,
        };
        PresentedToken::new(
            sign(
                key.expose(),
                &format!(
                    "vessel-ui-v1\nclient\n{}\n{}\n{}\n",
                    auth.launch.as_str(),
                    server.as_str(),
                    client.as_str()
                ),
            )
            .unwrap(),
        )
    }
    #[test]
    fn bootstrap_is_consumed_once_and_history_cannot_authenticate_again() {
        let auth = Auth::new(LaunchToken::mint().unwrap()).unwrap();
        let server = Nonce::from_random(LaunchToken::mint().unwrap());
        let client = Nonce::from_random(LaunchToken::mint().unwrap());
        let proof = hello(&auth, AuthMode::Bootstrap, &server, &client);
        assert!(!auth.authorized(auth.bootstrap.expose()));
        let welcome = auth
            .welcome(AuthMode::Bootstrap, &server, &client, &proof)
            .unwrap();
        let ServerFrame::Welcome {
            session: Some(session),
            ..
        } = welcome
        else {
            panic!("no session")
        };
        assert!(!auth.bootstrap.matches(session.as_str()));
        assert!(auth.authorized(session.as_str()));
        assert!(
            auth.welcome(AuthMode::Bootstrap, &server, &client, &proof)
                .is_none()
        );
        let server2 = Nonce::from_random(LaunchToken::mint().unwrap());
        let bootstrap_again = hello(&auth, AuthMode::Bootstrap, &server2, &client);
        assert!(
            auth.welcome(AuthMode::Bootstrap, &server2, &client, &bootstrap_again)
                .is_none()
        );
        // A captured proof cannot authenticate on a fresh connection.
        let resume = hello(&auth, AuthMode::Resume, &server, &client);
        assert!(
            auth.welcome(AuthMode::Resume, &server2, &client, &resume)
                .is_none()
        );
        let resume = hello(&auth, AuthMode::Resume, &server2, &client);
        assert!(matches!(
            auth.welcome(AuthMode::Resume, &server2, &client, &resume),
            Some(ServerFrame::Welcome { session: None, .. })
        ));
    }

    #[test]
    fn simultaneous_redemption_has_one_winner() {
        let auth = std::sync::Arc::new(Auth::new(LaunchToken::mint().unwrap()).unwrap());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let auth = std::sync::Arc::clone(&auth);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let server = Nonce::from_random(LaunchToken::mint().unwrap());
                    let client = Nonce::from_random(LaunchToken::mint().unwrap());
                    let proof = hello(&auth, AuthMode::Bootstrap, &server, &client);
                    let _leader = barrier.wait();
                    auth.welcome(AuthMode::Bootstrap, &server, &client, &proof)
                        .is_some()
                })
            })
            .collect();
        assert_eq!(
            workers
                .into_iter()
                .map(|worker| usize::from(worker.join().unwrap()))
                .sum::<usize>(),
            1
        );
    }
}
