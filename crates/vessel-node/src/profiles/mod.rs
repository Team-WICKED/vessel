//! The node's harness profiles (ADR 0011): `profiles.toml` in the data
//! directory, `profile.list` and `profile.reload`, and what a new session is
//! launched with (`launch`).
//!
//! [Decision Log]
//! - Purpose: a session runs with the account and configuration its
//!   profile names; the node owner defines the names, clients only pick.
//! - Reading: at start a broken file stops the node, so it never runs
//!   sessions under a guess. On `profile.reload` a broken file leaves the
//!   profiles in effect and the error names the profile, key and position
//!   (never a value: `vessel_harness::ProfilesError`). A file other users
//!   can read is used, with a warning, as for `token`: the owner wrote it
//!   and decides.
//! - State: the parsed set behind a `std` `RwLock`, swapped whole on reload
//!   and cloned out under the lock (never held across `.await`), so a
//!   session resolves against one consistent set and a reload never
//!   changes a session already started.
//! - Alternatives: watching the file (a half-saved file would be read);
//!   reading it per session (a broken edit would fail every new session
//!   instead of the one reload call).

pub(crate) mod launch;
pub(crate) mod moving;

use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

use vessel_harness::{Profile, Profiles, ProfilesError, parse_profiles};
use vessel_proto::{HarnessKind, ProfileInfo, ProfileName};
use vessel_store::{DataDir, StoreError};

/// Why `profiles.toml` could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ProfilesFileError {
    /// It could not be read.
    #[error("cannot read profiles.toml")]
    Read(#[source] StoreError),
    /// It does not parse; the message says where.
    #[error(transparent)]
    Parse(#[from] ProfilesError),
}

/// The profiles in effect on this node.
#[derive(Debug)]
pub(crate) struct NodeProfiles {
    dir: DataDir,
    /// What `~/` expands to; `None` if the node user has no home.
    user_home: Option<PathBuf>,
    current: RwLock<Arc<Profiles>>,
}

impl NodeProfiles {
    /// Read `profiles.toml` (none means the default profiles only). Blocks
    /// on the file system.
    ///
    /// # Errors
    /// The file cannot be read or does not parse.
    pub(crate) fn load(
        dir: DataDir,
        user_home: Option<PathBuf>,
    ) -> Result<Self, ProfilesFileError> {
        let profiles = read(&dir, user_home.as_deref())?;
        log_loaded(&profiles);
        Ok(Self {
            dir,
            user_home,
            current: RwLock::new(Arc::new(profiles)),
        })
    }

    /// The profiles in effect: the defaults, then the defined ones, only
    /// `harness`'s if given. Names, labels and variable names only.
    pub(crate) fn list(&self, harness: Option<HarnessKind>) -> Vec<ProfileInfo> {
        self.snapshot()
            .all()
            .filter(|profile| harness.is_none_or(|harness| profile.harness() == harness))
            .map(Profile::info)
            .collect()
    }

    /// Read the file again and use it; a file that cannot be used leaves
    /// the profiles in effect. Blocks on the file system.
    ///
    /// # Errors
    /// The file cannot be read or does not parse.
    pub(crate) fn reload(&self) -> Result<Vec<ProfileInfo>, ProfilesFileError> {
        let profiles = read(&self.dir, self.user_home.as_deref())?;
        log_loaded(&profiles);
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(profiles);
        Ok(self.list(None))
    }

    /// The profile named `name`, a defined one or a harness's default.
    pub(crate) fn get(&self, name: &ProfileName) -> Option<Profile> {
        self.snapshot().get(name).cloned()
    }

    /// What `~/` expands to, and where default homes are.
    pub(crate) fn user_home(&self) -> Option<&std::path::Path> {
        self.user_home.as_deref()
    }

    fn snapshot(&self) -> Arc<Profiles> {
        Arc::clone(&self.current.read().unwrap_or_else(PoisonError::into_inner))
    }
}

fn read(dir: &DataDir, user_home: Option<&std::path::Path>) -> Result<Profiles, ProfilesFileError> {
    match dir.read_profiles().map_err(ProfilesFileError::Read)? {
        Some(file) => Ok(parse_profiles(&file.text, user_home)?),
        None => Ok(Profiles::default()),
    }
}

fn log_loaded(profiles: &Profiles) {
    let names: Vec<&str> = profiles
        .defined()
        .map(|profile| profile.name().as_str())
        .collect();
    tracing::info!(profiles = ?names, "harness profiles loaded");
}
