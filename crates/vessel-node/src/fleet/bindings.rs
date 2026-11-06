//! Which node each member device speaks for, as the home has seen it:
//! `fleet/bindings.json`, kept for good (the review of Track X, round 2,
//! finding A).
//!
//! [Decision Log]
//! - Problem: the home checked a grant only against the grants of member
//!   devices still live. Once the home's owner revoked a member's device
//!   (to pair it again, say), nothing held that member's id, and any
//!   `sync` device could bind it to an impostor; the real member's next
//!   grant was then the one refused, and the home's calls for its id went
//!   to the impostor. A member that had not granted yet could be squatted
//!   the same way, by anyone who knew its id.
//! - Choice: a record of every `(node id, key)` pair the home has bound,
//!   and of the node every member device ever granted for, revoked and
//!   replaced devices included. A grant must agree with it: an id stays
//!   with its key, a key with its id, and a device with its node, for
//!   good. Only the home's owner undoes a binding, with `vessel fleet
//!   forget <node>` on the home (a reinstalled node has a new key); that is
//!   recorded too, with the time.
//! - Alternatives: deriving the id from the key (proposed in the plan; a
//!   protocol change); a `fleet.forget` call (also a protocol change, and
//!   the owner is at the home's machine to edit its data anyway).
//! - A home from before this file starts it from the grants it keeps,
//!   every device's, revoked ones included. Grants it has already deleted
//!   (replaced ones) are not known; only those bindings start afresh.
//! - What it leaves open: a node's very first grant still claims an id
//!   nobody has bound (trust on first grant), until ids come from keys.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use vessel_proto::{DeviceId, NodeId, NodePublicKey};

use super::files::{self, FileError, FleetFiles, FleetLock};

/// The record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bindings {
    /// Every node a member device granted for, with its key.
    #[serde(default)]
    nodes: Vec<NodeBinding>,
    /// The node every member device that granted speaks for.
    #[serde(default)]
    devices: Vec<DeviceBinding>,
    /// Bindings the owner forgot.
    #[serde(default)]
    forgotten: Vec<Forgotten>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeBinding {
    node: NodeId,
    key: NodePublicKey,
    since: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceBinding {
    device: DeviceId,
    node: NodeId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Forgotten {
    node: NodeId,
    key: NodePublicKey,
    at: DateTime<Utc>,
}

impl Bindings {
    /// The key `node` is bound to.
    pub(crate) fn key_of(&self, node: NodeId) -> Option<&NodePublicKey> {
        self.nodes
            .iter()
            .find(|binding| binding.node == node)
            .map(|binding| &binding.key)
    }

    /// The node `key` is bound to.
    pub(crate) fn node_with(&self, key: &NodePublicKey) -> Option<NodeId> {
        self.nodes
            .iter()
            .find(|binding| binding.key == *key)
            .map(|binding| binding.node)
    }

    /// The node member device `device` granted for.
    pub(crate) fn node_of(&self, device: DeviceId) -> Option<NodeId> {
        self.devices
            .iter()
            .find(|binding| binding.device == device)
            .map(|binding| binding.node)
    }

    /// Record that `device` granted for `node`, whose key is `key` (the
    /// caller checked that this agrees with the record).
    pub(crate) fn bind(&mut self, device: DeviceId, node: NodeId, key: NodePublicKey) {
        if self.key_of(node).is_none() {
            self.nodes.push(NodeBinding {
                node,
                key,
                since: Utc::now(),
            });
        }
        if self.node_of(device).is_none() {
            self.devices.push(DeviceBinding { device, node });
        }
    }
}

/// A grant the home keeps, as far as the record needs it.
#[derive(Debug, Deserialize)]
struct KeptGrant {
    member: DeviceId,
    node: NodeId,
    key: NodePublicKey,
}

fn path(files: &FleetFiles) -> PathBuf {
    files.dir().join("bindings.json")
}

/// The grants directory of `files`.
pub(crate) fn grants_dir(files: &FleetFiles) -> PathBuf {
    files.dir().join("grants")
}

/// Read the record; a home from before it starts it from the grants it
/// keeps. Blocks; the caller holds the fleet lock.
pub(crate) fn read(files: &FleetFiles) -> Result<Bindings, FileError> {
    if let Some(bindings) = files::read_json(&path(files))? {
        return Ok(bindings);
    }
    let mut bindings = Bindings::default();
    let dir = grants_dir(files);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(bindings),
        Err(error) => return Err(FileError::io(&dir, error)),
    };
    let mut kept = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| FileError::io(&dir, error))?;
        let file = entry.path();
        if file
            .extension()
            .is_some_and(|extension| extension == "json")
            && let Some(grant) = files::read_json::<KeptGrant>(&file)?
        {
            kept.push(grant);
        }
    }
    // Oldest first would be better, but a kept grant has one binding per
    // device and its node's key; any order gives the same record.
    for grant in kept {
        if bindings
            .key_of(grant.node)
            .is_none_or(|key| *key == grant.key)
            && bindings
                .node_with(&grant.key)
                .is_none_or(|node| node == grant.node)
        {
            bindings.bind(grant.member, grant.node, grant.key);
        }
    }
    Ok(bindings)
}

/// Write the record. Blocks; the caller holds the fleet lock.
pub(crate) fn write(files: &FleetFiles, bindings: &Bindings) -> Result<(), FileError> {
    files::write_json(&path(files), bindings)
}

/// What `vessel fleet forget` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Forgot {
    /// The key the node was bound to.
    pub key: NodePublicKey,
    /// The member devices that granted for it; their grants are deleted.
    /// A device that should not sync any more is revoked separately.
    pub devices: Vec<DeviceId>,
}

/// `vessel fleet forget <node>` (the home's owner, on its data directory):
/// let `node` be bound to another key, for a node reinstalled with a new
/// one. Its grants are deleted, so the home stops calling it until it
/// grants again. Recorded in the file, with the time. `None` if the home
/// has no binding for it. Blocks (takes the fleet lock).
///
/// # Errors
/// The record or a grant could not be read or written.
pub fn forget(files: &FleetFiles, node: NodeId) -> Result<Option<Forgot>, FileError> {
    let _lock = FleetLock::acquire(files)?;
    let mut bindings = read(files)?;
    let Some(at) = bindings
        .nodes
        .iter()
        .position(|binding| binding.node == node)
    else {
        return Ok(None);
    };
    let forgotten = bindings.nodes.remove(at);
    let devices: Vec<DeviceId> = bindings
        .devices
        .iter()
        .filter(|binding| binding.node == node)
        .map(|binding| binding.device)
        .collect();
    let dir = grants_dir(files);
    for device in &devices {
        files::remove(&dir.join(format!("{device}.json")))?;
        files::remove(&dir.join(format!("{device}.credential")))?;
    }
    bindings.forgotten.push(Forgotten {
        node,
        key: forgotten.key,
        at: Utc::now(),
    });
    write(files, &bindings)?;
    tracing::info!(%node, devices = ?devices, "forgot a fleet node's binding; it may grant again under a new key");
    Ok(Some(Forgot {
        key: forgotten.key,
        devices,
    }))
}

#[cfg(test)]
mod tests {
    use vessel_store::DataDir;

    use super::*;

    #[test]
    fn the_record_starts_from_the_grants_kept_and_forgets_only_on_purpose() {
        let dir = tempfile::tempdir().unwrap();
        let files = FleetFiles::new(&DataDir::new(dir.path()));
        let grants = grants_dir(&files);
        std::fs::create_dir_all(&grants).unwrap();
        let (device, node) = (DeviceId::random(), NodeId::random());
        let key = NodePublicKey::from_bytes([7; 32]);
        std::fs::write(
            grants.join(format!("{device}.json")),
            serde_json::json!({ "member": device, "node": node, "key": key, "name": "desktop" })
                .to_string(),
        )
        .unwrap();
        std::fs::write(grants.join(format!("{device}.credential")), "kept").unwrap();

        // A home from before the record: started from the grants it keeps.
        let seeded = read(&files).unwrap();
        assert_eq!(seeded.key_of(node), Some(&key));
        assert_eq!(seeded.node_with(&key), Some(node));
        assert_eq!(seeded.node_of(device), Some(node));
        write(&files, &seeded).unwrap();

        assert_eq!(forget(&files, NodeId::random()).unwrap(), None);
        let forgot = forget(&files, node).unwrap().unwrap();
        assert_eq!(forgot.key, key);
        assert_eq!(forgot.devices, vec![device]);
        assert!(!grants.join(format!("{device}.json")).exists());
        assert!(!grants.join(format!("{device}.credential")).exists());
        let after = read(&files).unwrap();
        assert_eq!(after.key_of(node), None, "free for a new key");
        assert_eq!(after.node_of(device), Some(node), "a device keeps its node");
        assert_eq!(after.forgotten.len(), 1, "and the forgetting is on record");
    }
}
