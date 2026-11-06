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
//!   grant for them is taken only by a member device paired after the
//!   forget. A reinstalled node joins again with a new invitation anyway
//!   (its credential went with its data), so this costs the owner nothing,
//!   while every device that existed before cannot take them. (Pinning
//!   the new key instead, `forget --key`, would need the owner to find and
//!   type the reinstalled node's key.)
//! - "After" is an order, not a time (round 4): the forget is the home
//!   node's own call (`fleet.forget`), which withdraws every invitation
//!   outstanding and records the devices that exist; a device in that set
//!   counts as old, and no device stored later comes from an invitation
//!   made before (`Devices::withdraw_all`). Comparing pairing times let an
//!   invitation made before the forget and used after it through, and
//!   trusted the home's clock. A forget recorded before this kept no
//!   devices; for it alone the time still decides.
//! - Names from before names were recorded (round 4): a record written
//!   then has none, so a revoked member's name was not reserved. Reading
//!   fills every node without a name from the grants the home keeps,
//!   revoked devices' included, and the home saves that once at start.
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
    /// The devices that existed then: none of them takes the id or the
    /// name. Absent in a forget recorded before devices were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before: Option<Vec<DeviceId>>,
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

    /// Whether a forget of node `node`, or of a node named `name`, keeps
    /// member device `device` (paired at `paired`) from taking them: it
    /// existed when the node was forgotten (for a forget recorded before
    /// devices were, it was paired at or before it).
    pub(crate) fn forgotten_for(
        &self,
        node: NodeId,
        name: Option<&str>,
        device: DeviceId,
        paired: Option<DateTime<Utc>>,
    ) -> bool {
        self.forgotten
            .iter()
            .filter(|forgotten| {
                forgotten.node == node
                    || name.is_some_and(|name| forgotten.name.as_deref() == Some(name))
            })
            .any(|forgotten| match &forgotten.before {
                Some(before) => before.contains(&device),
                None => paired.is_none_or(|paired| paired <= forgotten.at),
            })
    }

    /// Give every node without a recorded name the name of a kept grant
    /// for it, unless another node holds that name; whether any changed.
    fn name_from(&mut self, kept: &[KeptGrant]) -> bool {
        let mut changed = false;
        for grant in kept {
            let unnamed = self
                .nodes
                .iter()
                .any(|binding| binding.node == grant.node && binding.name.is_none());
            if unnamed && self.node_named(&grant.name).is_none() {
                for binding in &mut self.nodes {
                    if binding.node == grant.node {
                        binding.name = Some(grant.name.clone());
                    }
                }
                changed = true;
            }
        }
        changed
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

/// The grants the home keeps, every device's (revoked ones included).
fn kept_grants(files: &FleetFiles) -> Result<Vec<KeptGrant>, FileError> {
    let dir = grants_dir(files);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
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
    Ok(kept)
}

/// Read the record: a home from before it starts it from the grants it
/// keeps, and a record from before names fills them in from those grants.
/// Blocks.
pub(crate) fn read(files: &FleetFiles) -> Result<Bindings, FileError> {
    let kept = kept_grants(files)?;
    if let Some(mut bindings) = files::read_json::<Bindings>(&path(files))? {
        let _named = bindings.name_from(&kept);
        return Ok(bindings);
    }
    let mut bindings = Bindings::default();
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

/// Save the names a record from before names lacks (`read` fills them in
/// memory); whether it did. Run once by the home at start. Blocks.
///
/// # Errors
/// The record or a grant could not be read or written.
pub(crate) fn upgrade(files: &FleetFiles) -> Result<bool, FileError> {
    let _lock = FleetLock::acquire(files)?;
    let Some(mut bindings) = files::read_json::<Bindings>(&path(files))? else {
        return Ok(false);
    };
    if !bindings.name_from(&kept_grants(files)?) {
        return Ok(false);
    }
    write(files, &bindings)?;
    Ok(true)
}

/// Write the record. Blocks; the caller holds the fleet lock.
pub(crate) fn write(files: &FleetFiles, bindings: &Bindings) -> Result<(), FileError> {
    files::write_json(&path(files), bindings)
}

/// What [`forget`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Forgot {
    /// The key the node was bound to.
    pub(crate) key: NodePublicKey,
    /// The name it was known by, now released.
    pub(crate) name: Option<String>,
    /// The member devices that granted for it; their grants are deleted.
    /// A device that should not sync any more is revoked separately.
    pub(crate) devices: Vec<DeviceId>,
}

/// `fleet.forget` on the home (`vessel fleet forget <node>`): let `node`
/// be bound to another key (a node reinstalled with a new one) or name.
/// Its grants are deleted, so the home stops calling it; its id and name
/// go next only to a member device not in `before`, the devices that
/// exist now (the caller withdrew the invitations outstanding, so every
/// later device comes from a later one). Recorded in the file, with the
/// time. `None` if the home has no binding for it. Blocks (takes the
/// fleet lock).
///
/// # Errors
/// The record or a grant could not be read or written.
pub(crate) fn forget(
    files: &FleetFiles,
    node: NodeId,
    before: Vec<DeviceId>,
) -> Result<Option<Forgot>, FileError> {
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
        before: Some(before),
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

    /// A kept grant of `device` for `node` (key 7), named `name`.
    fn keep(files: &FleetFiles, device: DeviceId, node: NodeId, name: &str) {
        let grants = grants_dir(files);
        std::fs::create_dir_all(&grants).unwrap();
        let key = NodePublicKey::from_bytes([7; 32]);
        std::fs::write(
            grants.join(format!("{device}.json")),
            serde_json::json!({ "member": device, "node": node, "key": key, "name": name })
                .to_string(),
        )
        .unwrap();
        std::fs::write(grants.join(format!("{device}.credential")), "kept").unwrap();
    }

    #[test]
    fn the_record_starts_from_the_grants_kept_and_forgets_only_on_purpose() {
        let dir = tempfile::tempdir().unwrap();
        let files = FleetFiles::new(&DataDir::new(dir.path()));
        let (device, node) = (DeviceId::random(), NodeId::random());
        let key = NodePublicKey::from_bytes([7; 32]);
        keep(&files, device, node, "desktop");

        // A home from before the record: started from the grants it keeps.
        let seeded = read(&files).unwrap();
        assert_eq!(seeded.key_of(node), Some(&key));
        assert_eq!(seeded.node_with(&key), Some(node));
        assert_eq!(seeded.node_of(device), Some(node));
        assert_eq!(seeded.name_of(node), Some("desktop"));
        assert_eq!(seeded.node_named("desktop"), Some(node));
        write(&files, &seeded).unwrap();

        let existing = DeviceId::random();
        assert_eq!(
            forget(&files, NodeId::random(), vec![existing]).unwrap(),
            None
        );
        let forgot = forget(&files, node, vec![device, existing])
            .unwrap()
            .unwrap();
        assert_eq!(forgot.key, key);
        assert_eq!(forgot.name.as_deref(), Some("desktop"));
        assert_eq!(forgot.devices, vec![device]);
        assert!(!grants_dir(&files).join(format!("{device}.json")).exists());
        let after = read(&files).unwrap();
        assert_eq!(after.key_of(node), None, "free for a new key");
        assert_eq!(after.node_named("desktop"), None, "and its name");
        assert_eq!(after.node_of(device), Some(node), "a device keeps its node");
        // ... but only for a device that did not exist at the forget.
        for old in [device, existing] {
            assert!(after.forgotten_for(node, None, old, None));
            assert!(after.forgotten_for(NodeId::random(), Some("desktop"), old, None));
        }
        let new = DeviceId::random();
        assert!(!after.forgotten_for(node, Some("desktop"), new, None));
    }

    /// Round 4: a record 5dd51d3 wrote has no names, so a revoked member's
    /// name was not reserved. Reading fills them from the kept grants
    /// (revoked devices' included), and `upgrade` saves that.
    #[test]
    fn a_record_from_before_names_gets_them_from_the_grants_kept() {
        let dir = tempfile::tempdir().unwrap();
        let files = FleetFiles::new(&DataDir::new(dir.path()));
        let (device, node) = (DeviceId::random(), NodeId::random());
        keep(&files, device, node, "desktop");
        let key = NodePublicKey::from_bytes([7; 32]);
        std::fs::write(
            path(&files),
            serde_json::json!({
                "nodes": [{ "node": node, "key": key, "since": Utc::now() }],
                "devices": [{ "device": device, "node": node }],
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(read(&files).unwrap().node_named("desktop"), Some(node));
        assert!(upgrade(&files).unwrap());
        let saved: Bindings = files::read_json(&path(&files)).unwrap().unwrap();
        assert_eq!(saved.name_of(node), Some("desktop"));
        assert!(!upgrade(&files).unwrap(), "once");
    }

    /// A forget recorded before devices were (f3ca416) is decided by time.
    #[test]
    fn a_forget_from_before_devices_were_recorded_goes_by_time() {
        let (node, at) = (NodeId::random(), Utc::now());
        let bindings: Bindings = serde_json::from_value(serde_json::json!({
            "forgotten": [{ "node": node, "key": NodePublicKey::from_bytes([7; 32]), "at": at }],
        }))
        .unwrap();
        let device = DeviceId::random();
        assert!(bindings.forgotten_for(node, None, device, Some(at)));
        assert!(bindings.forgotten_for(node, None, device, None));
        let later = at.checked_add_signed(chrono::TimeDelta::seconds(1));
        assert!(!bindings.forgotten_for(node, None, device, later));
    }
}
