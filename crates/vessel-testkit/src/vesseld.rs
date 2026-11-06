//! [`vesseld()`]: the real node binary, for tests outside `vessel-node`.
//!
//! [Decision Log]
//! - Purpose: milestone 2's client-side tests install `vesseld` on a fake
//!   remote (`fake-ssh` and a temporary home) and attach to it, so they
//!   need the node binary itself, not a fake. Cargo gives
//!   `CARGO_BIN_EXE_vesseld` only to `vessel-node`'s own integration tests.
//! - Choice: the same pattern as [`Fake::path`](crate::Fake::path): one
//!   nested `cargo build -p vessel-node --bin vesseld` per test process,
//!   into the test executable's own target directory and profile, cached.
//!   Separate from the fakes' build, so tests that need only fakes never
//!   pay for the node's.
//! - Cost: the first call compiles `vessel-node` under its own feature set
//!   (as the fakes' build does for this crate), which can take a minute on
//!   a cold runner and blocks the calling thread. Call it before
//!   [`within`](crate::within) starts a test's deadline: the path is needed
//!   to set the test up anyway.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::{LocateError, build, profile_dir};

/// Path of a freshly built `vesseld`.
///
/// The first call in a process runs the nested build (see the module
/// documentation); call it before a test's deadline starts.
///
/// # Errors
/// The test executable's location cannot be determined, the nested build
/// fails, or the binary is missing afterwards.
pub fn vesseld() -> Result<PathBuf, LocateError> {
    static BUILT: OnceLock<Result<PathBuf, LocateError>> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let dir = profile_dir()?;
            build(&dir, &["-p", "vessel-node", "--bin", "vesseld"])?;
            let path = dir.join(format!("vesseld{}", std::env::consts::EXE_SUFFIX));
            if path.is_file() {
                Ok(path)
            } else {
                Err(LocateError::Missing { path })
            }
        })
        .clone()
}
