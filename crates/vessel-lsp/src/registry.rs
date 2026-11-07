//! Operator-owned executable discovery and safe defaults; never installs tools.
//!
//! PATH is an explicit value supplied by the binary. Project files and wire
//! requests cannot select a command, a shell, environment or analysis trust.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use vessel_proc::CommandSpec;
use vessel_proto::lsp::Language;

use crate::error::LspError as Error;

/// Operator-selected executable and literal arguments, not a shell command.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    /// Trusted absolute executable path.
    pub program: PathBuf,
    /// Literal command arguments; never received from RPC callers.
    #[serde(default)]
    pub args: Vec<String>,
}

/// Explicit trusted search path and overrides. Empty by default for tests.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    /// PATH given by the binary, never read from the process by this library.
    pub search_path: OsString,
    /// Explicit account home; never read implicitly from the node environment.
    pub home: Option<PathBuf>,
    /// Windows runtime directory, supplied by the binary from `SystemRoot`.
    pub system_root: Option<OsString>,
    /// Trusted tsserver.js, required in conservative mode when PATH has none.
    pub typescript_server: Option<PathBuf>,
    /// Trusted installed Rust toolchain directory, for isolated homes or explicit pinning.
    pub rust_toolchain: Option<PathBuf>,
    /// Explicit trusted Cargo cache/config home, useful with an isolated server home.
    pub cargo_home: Option<PathBuf>,
    /// Operator overrides per language family.
    pub overrides: BTreeMap<Language, Override>,
}

impl Registry {
    /// Resolve an inert command specification. Run this on a blocking worker.
    ///
    /// # Errors
    /// Fails for absent executables or non-absolute operator overrides.
    pub fn command(
        &self,
        language: Language,
        root: &Path,
        full_analysis: bool,
    ) -> Result<CommandSpec, Error> {
        crate::policy::check(root, language, full_analysis, self.home.as_deref())?;
        let mut spec = self.executable(language, root)?;
        if language == Language::Cpp && !full_analysis {
            if spec
                .args
                .iter()
                .any(|argument| argument.to_string_lossy().starts_with("--query-driver"))
            {
                return Err(Error::ProjectConfiguration);
            }
            spec = spec.args([
                "--enable-config=false",
                "--background-index=false",
                "--query-driver=",
            ]);
        }
        let mut directories: Vec<_> = std::env::split_paths(&self.search_path)
            .filter(|path| path.is_absolute())
            .filter_map(|path| std::fs::canonicalize(path).ok())
            .filter(|path| !path.starts_with(root))
            .collect();
        if let Some(toolchain) = &self.rust_toolchain {
            let bin = std::fs::canonicalize(toolchain.join("bin"))?;
            if !full_analysis && bin.starts_with(root) {
                return Err(Error::ProjectConfiguration);
            }
            directories.insert(0, bin);
        }
        let program = std::fs::canonicalize(&spec.program)?;
        if !full_analysis && program.starts_with(root) {
            return Err(Error::ProjectConfiguration);
        }
        if directories.is_empty() {
            directories.push(program.parent().ok_or(Error::Path)?.to_owned());
        }
        let search_path = std::env::join_paths(&directories).map_err(|_invalid| Error::Path)?;
        // Nothing from the daemon leaks into native runtimes: notably no
        // NODE_OPTIONS, loader injection, compiler wrappers or agent secrets.
        spec = spec
            .env_clear()
            .env("PATH", search_path)
            .env("LANG", "C.UTF-8")
            .env("CARGO_NET_OFFLINE", "true")
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env("RUSTC_WRAPPER", "")
            .env("RUSTC_WORKSPACE_WRAPPER", "")
            .env("PYTHONSAFEPATH", "1")
            .env("PYTHONNOUSERSITE", "1")
            .env("GOTOOLCHAIN", "local")
            .env("GOFLAGS", "-mod=readonly")
            .env("GOENV", "off")
            .env("GOVCS", "*:off")
            .env("GOPROXY", "off")
            .env("GOSUMDB", "off");
        if let Some(home) = &self.home {
            spec = spec
                .env("HOME", home)
                .env("USERPROFILE", home)
                .env("TMPDIR", home)
                .env("TMP", home)
                .env("TEMP", home);
        }
        if let Some(cargo_home) = &self.cargo_home {
            spec = spec.env("CARGO_HOME", cargo_home);
        }
        if let Some(system_root) = &self.system_root {
            spec = spec.env("SystemRoot", system_root);
        }
        if !full_analysis {
            spec = spec.env("CGO_ENABLED", "0");
        }
        // Cargo's environment overrides project build.rustc; wrappers are
        // disabled explicitly. Project [env]/alias/target settings are refused.
        if language == Language::Rust {
            for (name, variable) in [("rustc", "RUSTC"), ("cargo", "CARGO")] {
                if let Some(program) = directories.iter().find_map(|directory| {
                    which::which_in(name, Some(directory.as_os_str()), root).ok()
                }) {
                    spec = spec.env(variable, program);
                }
            }
        }
        Ok(spec)
    }

    fn executable(&self, language: Language, root: &Path) -> Result<CommandSpec, Error> {
        let spec = if let Some(spec) = self.overrides.get(&language) {
            if !spec.program.is_absolute() {
                return Err(Error::Path);
            }
            CommandSpec::new(&spec.program)
                .args(spec.args.clone())
                .cwd(root)
        } else {
            let (names, args): (&[&str], &[&str]) = match language {
                Language::Rust => (&["rust-analyzer"], &[]),
                Language::TypeScript => (&["typescript-language-server"], &["--stdio"]),
                Language::Python => (
                    &["basedpyright-langserver", "pyright-langserver"],
                    &["--stdio"],
                ),
                Language::Go => (&["gopls"], &[]),
                Language::Cpp => (&["clangd"], &[]),
            };
            // Empty/relative PATH entries would search the requested project
            // and turn a Read query into execution of a repository binary.
            let directories: Vec<_> = std::env::split_paths(&self.search_path)
                .filter(|path| path.is_absolute())
                .collect();
            let program = names
                .iter()
                .find_map(|name| {
                    directories.iter().find_map(|directory| {
                        which::which_in(name, Some(directory.as_os_str()), root).ok()
                    })
                })
                .ok_or(Error::NotInstalled)?;
            CommandSpec::new(program)
                .args(args.iter().copied())
                .cwd(root)
        };
        Ok(spec)
    }

    pub(crate) fn typescript_engine(
        &self,
        root: &Path,
        full: bool,
    ) -> Result<Option<PathBuf>, Error> {
        let candidate = self.typescript_server.clone().or_else(|| {
            std::env::split_paths(&self.search_path)
                .filter(|path| path.is_absolute() && !path.starts_with(root))
                .find_map(|directory| {
                    which::which_in("tsserver", Some(directory.as_os_str()), root).ok()
                })
                .and_then(|shim| std::fs::canonicalize(shim).ok())
                .and_then(|shim| {
                    shim.parent()?
                        .parent()
                        .map(|package| package.join("lib/tsserver.js"))
                })
        });
        match candidate {
            Some(path) => {
                let path = std::fs::canonicalize(path)?;
                if !path.is_file() || !full && path.starts_with(root) {
                    return Err(Error::ProjectConfiguration);
                }
                Ok(Some(path))
            }
            None if full => Ok(None),
            None => Err(Error::NotInstalled),
        }
    }
}
