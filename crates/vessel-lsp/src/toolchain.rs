//! The operator's Rust toolchain, resolved once to absolute binaries.
//!
//! [Decision Log]
//! - Purpose: a repository never chooses the compiler or Cargo that a
//!   conservative rust-analyzer runs.
//! - Before: `RUSTC`/`CARGO` named the first `rustc`/`cargo` on PATH, usually
//!   rustup *proxies*. A proxy picks its toolchain from the working directory
//!   of the process that runs it (`rust-toolchain(.toml)`, directory
//!   overrides), and rust-analyzer runs rustc and Cargo from inside the
//!   project: a `path` toolchain in `sub/` made the proxy run the
//!   repository's own `bin/rustc` (review finding, reproduced with
//!   rust-analyzer 1.98.1).
//! - Alternatives: keep proxies and only refuse toolchain files (a race or a
//!   missed directory reopens it); ask rustup per launch in the project
//!   directory (the very lookup the repository controls).
//! - Chosen: resolve once, from the operator pin or from rustup's *default*
//!   toolchain (`rustup default`, then `rustup which --toolchain`), run in
//!   the node-owned empty directory. Servers get absolute `RUSTC`/`CARGO`,
//!   the toolchain's own `bin` first on PATH, and `RUSTUP_TOOLCHAIN` set to
//!   the toolchain directory so any proxy still reached resolves the same
//!   toolchain and never reads a toolchain file. A directory (not a name) is
//!   used because rustup accepts it for installed, linked and standalone
//!   toolchains alike, and a name could later be re-linked elsewhere.
//! - Trade-off: a toolchain installed or re-defaulted after the first Rust
//!   launch is seen only after a node restart. Resolution failures are not
//!   cached, so a missing rustup is retried on the next launch.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use vessel_proc::CommandSpec;

use crate::error::LspError as Error;
use crate::registry::Registry;

/// Absolute, canonical Rust tools that never consult a working directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RustTools {
    pub(crate) rustc: PathBuf,
    pub(crate) cargo: PathBuf,
    /// Value for `RUSTUP_TOOLCHAIN` when the toolchain is rustup's or pinned.
    pub(crate) toolchain: Option<PathBuf>,
    /// Not a toolchain rustup installed itself: an explicit operator pin,
    /// or a linked or path default. Such a directory can sit in (or point
    /// into) a repository, so it must lie outside every configured root.
    pub(crate) unverified: bool,
}

impl RustTools {
    /// Every path a conservative launch must find outside repository roots.
    pub(crate) fn paths(&self) -> impl Iterator<Item = &Path> {
        [
            Some(self.rustc.as_path()),
            Some(self.cargo.as_path()),
            self.toolchain.as_deref(),
        ]
        .into_iter()
        .flatten()
    }
}

/// Resolve the operator's toolchain. `cwd` is a node-owned empty directory.
pub(crate) async fn resolve(
    registry: &Registry,
    cwd: &Path,
    deadline: Duration,
) -> Result<RustTools, Error> {
    if let Some(toolchain) = &registry.rust_toolchain {
        let toolchain = std::fs::canonicalize(toolchain)?;
        // A pin is a path the operator typed, not one rustup vouches for:
        // it gets the same every-configured-root rule as a linked default.
        return Ok(RustTools {
            rustc: binary(&toolchain, "rustc")?,
            cargo: binary(&toolchain, "cargo")?,
            toolchain: Some(toolchain),
            unverified: true,
        });
    }
    let directories = registry.trusted_directories();
    let first = |name: &str| {
        directories
            .iter()
            .find_map(|directory| which::which_in(name, Some(directory.as_os_str()), cwd).ok())
    };
    let rustc = first("rustc").ok_or(Error::NotInstalled)?;
    let Some(rustup) = proxy(&rustc)? else {
        let cargo = first("cargo").ok_or(Error::NotInstalled)?;
        return Ok(RustTools {
            rustc: std::fs::canonicalize(rustc)?,
            cargo: std::fs::canonicalize(cargo)?,
            toolchain: None,
            unverified: false,
        });
    };
    let default = run_rustup(registry, &rustup, cwd, &["default"], deadline).await?;
    // `rustup default` prints "<name>" or "<name> (default)".
    let name = default
        .split_whitespace()
        .next()
        .filter(|name| !name.starts_with('-'))
        .ok_or(Error::NotInstalled)?
        .to_owned();
    let which = |tool| ["which", "--toolchain", name.as_str(), tool];
    let rustc = run_rustup(registry, &rustup, cwd, &which("rustc"), deadline).await?;
    let cargo = run_rustup(registry, &rustup, cwd, &which("cargo"), deadline).await?;
    let (rustc, cargo) = (PathBuf::from(rustc.trim()), PathBuf::from(cargo.trim()));
    if !rustc.is_absolute() || !cargo.is_absolute() {
        return Err(Error::NotInstalled);
    }
    // rustup reports `<rustup home>/toolchains/<name>/bin/<tool>`. An
    // installed toolchain is a real directory there; a linked one is a
    // symlink, and anything else is not rustup's own installation.
    let reported = rustc.parent().and_then(Path::parent);
    let installed = reported.is_some_and(|directory| {
        std::fs::symlink_metadata(directory).is_ok_and(|metadata| metadata.is_dir())
            && directory
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "toolchains")
    });
    let rustc = std::fs::canonicalize(rustc)?;
    let toolchain = rustc
        .parent()
        .and_then(Path::parent)
        .ok_or(Error::NotInstalled)?
        .to_owned();
    Ok(RustTools {
        cargo: std::fs::canonicalize(cargo)?,
        rustc,
        toolchain: Some(toolchain),
        unverified: !installed,
    })
}

fn binary(toolchain: &Path, name: &str) -> Result<PathBuf, Error> {
    let path = toolchain
        .join("bin")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let path = std::fs::canonicalize(path)?;
    if !path.is_file() {
        return Err(Error::NotInstalled);
    }
    Ok(path)
}

/// The rustup binary behind `rustc`, if `rustc` is one of its proxies:
/// proxies are links to rustup (Unix) or copies next to it (Windows).
fn proxy(rustc: &Path) -> Result<Option<PathBuf>, Error> {
    let canonical = std::fs::canonicalize(rustc)?;
    if canonical
        .file_stem()
        .is_some_and(|stem| stem == "rustup" || stem == "rustup-init")
    {
        return Ok(Some(canonical));
    }
    let sibling = rustc
        .parent()
        .map(|directory| directory.join(format!("rustup{}", std::env::consts::EXE_SUFFIX)));
    Ok(sibling.filter(|path| path.is_file()))
}

/// Run rustup with the same cleared environment servers get, in `cwd`.
async fn run_rustup(
    registry: &Registry,
    rustup: &Path,
    cwd: &Path,
    args: &[&str],
    deadline: Duration,
) -> Result<String, Error> {
    let search_path =
        std::env::join_paths(registry.trusted_directories()).map_err(|_invalid| Error::Path)?;
    let mut spec = CommandSpec::new(rustup)
        .args(args.iter().copied())
        .cwd(cwd)
        .env_clear()
        .env("PATH", search_path)
        .env("RUSTUP_AUTO_INSTALL", "0")
        .env("LANG", "C.UTF-8");
    if let Some(home) = &registry.home {
        spec = spec.env("HOME", home).env("USERPROFILE", home);
    }
    if let Some(system_root) = &registry.system_root {
        spec = spec.env("SystemRoot", OsString::from(system_root));
    }
    let vessel_proc::Spawned {
        mut child,
        stdin,
        stdout,
        mut stderr,
    } = vessel_proc::spawn(&spec)?;
    drop(stdin);
    let run = async {
        let mut output = Vec::new();
        // Answers are one short line; anything larger is not rustup's.
        let mut stdout = stdout.take(4097);
        let mut stderr = (&mut stderr).take(65_536);
        let mut sink = Vec::new();
        let (read, _drained) = tokio::join!(
            stdout.read_to_end(&mut output),
            stderr.read_to_end(&mut sink)
        );
        let _count = read?;
        let status = child.wait().await?;
        Ok::<_, Error>((status, output))
    };
    let (status, output) = tokio::time::timeout(deadline, run)
        .await
        .map_err(|_elapsed| Error::Timeout)??;
    if status != Some(0) || output.len() > 4096 {
        return Err(Error::NotInstalled);
    }
    Ok(String::from_utf8(output)?)
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn executable(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A rustup stand-in: `default` names `name`, `which` answers from
    /// `toolchains`, and `rustc`/`cargo` beside it are proxies (links).
    fn fake_rustup(bin: &Path, name: &str, toolchains: &Path) {
        executable(
            &bin.join("rustup"),
            &format!(
                "#!/bin/sh\ncase \"$1 $2\" in\n'default ') echo '{name} (default)';;\n'which --toolchain') echo \"{}/$3/bin/$4\";;\n*) exit 1;;\nesac\n",
                toolchains.display()
            ),
        );
        std::os::unix::fs::symlink("rustup", bin.join("rustc")).unwrap();
        std::os::unix::fs::symlink("rustup", bin.join("cargo")).unwrap();
    }

    fn toolchain(directory: &Path) {
        for tool in ["rustc", "cargo"] {
            executable(&directory.join("bin").join(tool), "#!/bin/sh\nexit 0\n");
        }
    }

    async fn resolve_with(base: &Path, bin: &Path) -> RustTools {
        let cwd = base.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let registry = Registry {
            search_path: std::env::join_paths([bin, Path::new("/usr/bin"), Path::new("/bin")])
                .unwrap(),
            ..Registry::default()
        };
        resolve(&registry, &cwd, Duration::from_secs(10))
            .await
            .unwrap()
    }

    /// A proxy would obey a toolchain file in its working directory, so
    /// resolution asks rustup for its default toolchain's binaries instead.
    #[tokio::test]
    async fn a_proxy_resolves_to_the_default_toolchain_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let toolchains = base.join("rustup/toolchains");
        let installed = toolchains.join("operator");
        toolchain(&installed);
        let bin = base.join("cargo/bin");
        fake_rustup(&bin, "operator", &toolchains);
        assert_eq!(
            resolve_with(&base, &bin).await,
            RustTools {
                rustc: installed.join("bin/rustc"),
                cargo: installed.join("bin/cargo"),
                toolchain: Some(installed),
                unverified: false,
            }
        );
    }

    /// `rustup toolchain link` makes the default a symlink that may point
    /// into a repository: it is unverified, so it must live outside every
    /// configured root.
    #[tokio::test]
    async fn a_linked_default_toolchain_is_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let toolchains = base.join("rustup/toolchains");
        let target = base.join("checkout/toolchain");
        toolchain(&target);
        std::fs::create_dir_all(&toolchains).unwrap();
        std::os::unix::fs::symlink(&target, toolchains.join("linked")).unwrap();
        let bin = base.join("cargo/bin");
        fake_rustup(&bin, "linked", &toolchains);
        let tools = resolve_with(&base, &bin).await;
        assert!(tools.unverified);
        assert_eq!(tools.toolchain, Some(target));
    }
}

#[cfg(test)]
mod pinned {
    use super::*;

    /// Verification finding: an explicit pin was trusted like a toolchain
    /// rustup installed, so a pin inside another configured root ran that
    /// repository's tools. A pin is unverified.
    #[tokio::test]
    async fn an_explicit_pin_is_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let toolchain = dir.path().join("toolchain");
        std::fs::create_dir_all(toolchain.join("bin")).unwrap();
        for tool in ["rustc", "cargo"] {
            let name = format!("{tool}{}", std::env::consts::EXE_SUFFIX);
            std::fs::write(toolchain.join("bin").join(name), "stand-in").unwrap();
        }
        let registry = Registry {
            rust_toolchain: Some(toolchain),
            ..Registry::default()
        };
        let tools = resolve(&registry, dir.path(), Duration::from_secs(10))
            .await
            .unwrap();
        assert!(tools.unverified);
    }
}
