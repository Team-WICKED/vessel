//! The open connections of each device, so revoking a device closes them
//! (ADR 0010, `device.revoke`).
//!
//! [Decision Log]
//! - Purpose: a revoked device must lose its open connections at once, not
//!   only be refused next time.
//! - Choice: every device connection runs its RPC server with its own
//!   shutdown token, a child of the node's, registered here under the
//!   device id. Revoking cancels them; the RPC layer then closes each
//!   connection the usual way (answering calls already running, so a
//!   device that revokes itself still gets its `ack`).
//! - Ordering: a connection is registered before its credential is checked
//!   against the store, and revocation commits to the store before it
//!   cancels. So a revocation either is seen by the check, or finds the
//!   registration and closes it; no connection slips between the two.
//! - A registration removes itself when dropped, so the map holds exactly
//!   the open connections.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio_util::sync::CancellationToken;
use vessel_proto::DeviceId;

/// Every open device connection, by device.
#[derive(Debug, Default)]
pub(crate) struct Connected {
    next: AtomicU64,
    open: Mutex<HashMap<DeviceId, HashMap<u64, CancellationToken>>>,
}

impl Connected {
    /// Register a connection of `device`; its token is a child of `parent`
    /// (the node's shutdown), and is cancelled when the device is revoked.
    pub(crate) fn register(
        self: &Arc<Self>,
        device: DeviceId,
        parent: &CancellationToken,
    ) -> Registration {
        let key = self.next.fetch_add(1, Ordering::Relaxed);
        let closing = parent.child_token();
        let _previous = self
            .lock()
            .entry(device)
            .or_default()
            .insert(key, closing.clone());
        Registration {
            connected: Arc::clone(self),
            device,
            key,
            closing,
        }
    }

    /// Whether `device` has a connection open now.
    pub(crate) fn is_open(&self, device: DeviceId) -> bool {
        self.lock()
            .get(&device)
            .is_some_and(|open| !open.is_empty())
    }

    /// Close every open connection of `device`; returns how many there were.
    pub(crate) fn close(&self, device: DeviceId) -> usize {
        let open = self.lock().get(&device).cloned().unwrap_or_default();
        for closing in open.values() {
            closing.cancel();
        }
        open.len()
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<DeviceId, HashMap<u64, CancellationToken>>> {
        // A poisoned map is still a valid map: nothing is half-written in it.
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One open device connection; unregisters itself when dropped.
#[derive(Debug)]
pub(crate) struct Registration {
    connected: Arc<Connected>,
    device: DeviceId,
    key: u64,
    closing: CancellationToken,
}

impl Registration {
    /// The device this connection is registered under.
    pub(crate) const fn device(&self) -> DeviceId {
        self.device
    }

    /// Cancelled when the device is revoked or the node stops.
    pub(crate) const fn closing(&self) -> &CancellationToken {
        &self.closing
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut open = self.connected.lock();
        if let Some(connections) = open.get_mut(&self.device) {
            let _removed = connections.remove(&self.key);
            if connections.is_empty() {
                let _gone = open.remove(&self.device);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_a_device_cancels_its_connections_only() {
        let connected = Arc::new(Connected::default());
        let node = CancellationToken::new();
        let (laptop, phone) = (DeviceId::random(), DeviceId::random());
        let first = connected.register(laptop, &node);
        let second = connected.register(laptop, &node);
        let other = connected.register(phone, &node);

        assert_eq!(connected.close(laptop), 2);
        assert!(first.closing().is_cancelled());
        assert!(second.closing().is_cancelled());
        assert!(!other.closing().is_cancelled());

        drop((first, second));
        assert_eq!(connected.close(laptop), 0, "dropped registrations leave");
        node.cancel();
        assert!(other.closing().is_cancelled(), "the node's stop reaches it");
    }
}
