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
//! - Names too (round 3, finding 1): the name a node granted under is
//!   reserved for it, also while its device is revoked; before, a `sync`
//!   device's own node took a revoked member's name, the member was then
//!   refused for the clash, and calls by that name (`vessel mcp`, `--to`)
//!   went to the other node. A node keeps its name; renaming it is the
//!   owner's `forget` too.
//! - After a forget (round 3, finding 2), the id and the name are known
//!   and free, and the node is seen leaving the fleet's list: the next
//!   grant for them is taken only from a member device paired after the
//!   forget. A reinstalled node joins again with a new invitation anyway
//!   (its credential went with its data), so this costs the owner nothing,
//!   while every device that existed before cannot take them. (Pinning
//!   the new key instead, `forget --key`, would need the owner to find and
//!   type the reinstalled node's key.) An invitation made before the
//!   forget and used after it counts as after: invite after forgetting.
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
    /// The name it granted under, reserved for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
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
    /// The name it was released from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
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

    /// The name `node` is known by on this home.
    pub(crate) fn name_of(&self, node: NodeId) -> Option<&str> {
        self.nodes
            .iter()
            .find(|binding| binding.node == node)
            .and_then(|binding| binding.name.as_deref())
    }

    /// The node the name `name` is reserved for.
    pub(crate) fn node_named(&self, name: &str) -> Option<NodeId> {
        self.nodes
            .iter()
            .find(|binding| binding.name.as_deref() == Some(name))
            .map(|binding| binding.node)
    }

    /// When the owner last forgot node `node`, or a node named `name`: a
    /// grant for that id or name is then taken only from a member device
    /// paired after it.
    pub(crate) fn forgotten_at(&self, node: NodeId, name: Option<&str>) -> Option<DateTime<Utc>> {
        self.forgotten
            .iter()
            .filter(|forgotten| {
                forgotten.node == node
                    || name.is_some_and(|name| forgotten.name.as_deref() == Some(name))
            })
            .map(|forgotten| forgotten.at)
            .max()
    }

    /// The node member device `device` granted for.
    pub(crate) fn node_of(&self, device: DeviceId) -> Option<NodeId> {
        self.devices
            .iter()
            .find(|binding| binding.device == device)
            .map(|binding| binding.node)
    }

    /// Record that `device` granted for `node`, whose key is `key`, under
    /// `name` (the caller checked that this agrees with the record).
    pub(crate) fn bind(
        &mut self,
        device: DeviceId,
        node: NodeId,
        key: NodePublicKey,
        name: Option<&str>,
    ) {
        match self.nodes.iter_mut().find(|binding| binding.node == node) {
            Some(binding) => {
                if binding.name.is_none() {
                    binding.name = name.map(str::to_owned);
                }
            }
            None => self.nodes.push(NodeBinding {
                node,
                key,
                name: name.map(str::to_owned),
                since: Utc::now(),
            }),
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
    name: String,
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
            // A name two kept grants share stays with the first.
            let taken = bindings
                .node_named(&grant.name)
                .is_some_and(|node| node != grant.node);
            let name = (!taken).then_some(grant.name.as_str());
            bindings.bind(grant.member, grant.node, grant.key, name);
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
    /// The name it was known by, now released.
    pub name: Option<String>,
    /// The member devices that granted for it; their grants are deleted.
    /// A device that should not sync any more is revoked separately.
    pub devices: Vec<DeviceId>,
}

/// `vessel fleet forget <node>` (the home's owner, on its data directory):
/// let `node` be bound to another key (a node reinstalled with a new one)
/// or name. Its grants are deleted, so the home stops calling it; its id
/// and name go next to a member device paired after this, which the node
/// gets by being invited and joining again. Recorded in the file, with the
/// time. `None` if the home has no binding for it. Blocks (takes the fleet
/// lock).
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
        name: forgotten.name.clone(),
        at: Utc::now(),
    });
    write(files, &bindings)?;
    tracing::info!(%node, devices = ?devices, "forgot a fleet node's binding; it may grant again under a new key");
    Ok(Some(Forgot {
        key: forgotten.key,
        name: forgotten.name,
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
        assert_eq!(seeded.name_of(node), Some("desktop"));
        assert_eq!(seeded.node_named("desktop"), Some(node));
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
        assert_eq!(after.node_named("desktop"), None, "and its name");
        assert_eq!(forgot.name.as_deref(), Some("desktop"));
        assert!(after.forgotten_at(node, None).is_some());
        assert!(
            after
                .forgotten_at(NodeId::random(), Some("desktop"))
                .is_some()
        );
        assert_eq!(after.node_of(device), Some(node), "a device keeps its node");
        assert_eq!(after.forgotten.len(), 1, "and the forgetting is on record");
    }
}
