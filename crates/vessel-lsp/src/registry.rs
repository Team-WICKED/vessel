//! Operator-owned executable discovery and safe defaults; never installs tools.
//!
//! PATH is an explicit value supplied by the binary. Project files and wire
//! requests cannot select a command, a shell, environment or analysis trust.
//! Servers run in a node-owned empty directory with a node-owned temporary
//! directory: a repository is never a server's cwd, where name lookups
//! (Windows searches the cwd first) could pick up a repository binary.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use vessel_proc::CommandSpec;
use vessel_proto::lsp::Language;

use crate::error::LspError as Error;
use crate::toolchain::RustTools;

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
    /// Node-owned directory (under the data directory) holding the servers'
    /// empty working directory and their temporary directory. Servers never
    /// run with a repository as cwd; without this no server starts.
    pub state_dir: Option<PathBuf>,
    /// The operator's configured roots. In conservative mode a Rust
    /// toolchain rustup did not install itself (an explicit pin, or a linked
    /// or path default) may live under none of them; every executable must
    /// also be outside the served root.
    pub configured_roots: Vec<PathBuf>,
    /// Trusted tsserver.js, required in conservative mode when PATH has none.
    pub typescript_server: Option<PathBuf>,
    /// Trusted installed Rust toolchain directory. Without it, rustup's
    /// default toolchain (or a plain `rustc` on PATH) is resolved once.
    pub rust_toolchain: Option<PathBuf>,
    /// Explicit trusted Cargo cache/config home, useful with an isolated server home.
    pub cargo_home: Option<PathBuf>,
    /// Trusted Python interpreter handed to pyright as `python.pythonPath`.
    /// Required for conservative Python on Windows.
    pub python_interpreter: Option<PathBuf>,
    /// Operator overrides per language family.
    pub overrides: BTreeMap<Language, Override>,
}

/// Everything one server launch needs, resolved on a blocking worker.
#[derive(Debug)]
pub(crate) struct Prepared {
    pub(crate) command: CommandSpec,
    pub(crate) typescript_engine: Option<PathBuf>,
    pub(crate) python_interpreter: Option<PathBuf>,
}

impl Registry {
    /// Absolute PATH entries in order. Relative or empty entries would
    /// search a requested project and are dropped.
    pub(crate) fn trusted_directories(&self) -> Vec<PathBuf> {
        std::env::split_paths(&self.search_path)
            .filter(|path| path.is_absolute())
            .collect()
    }

    /// The node-owned server cwd and temporary directory, created on demand.
    pub(crate) fn state(&self) -> Result<(PathBuf, PathBuf), Error> {
        let state = self.state_dir.as_ref().ok_or(Error::NotInstalled)?;
        let (cwd, tmp) = (state.join("cwd"), state.join("tmp"));
        std::fs::create_dir_all(&cwd)?;
        std::fs::create_dir_all(&tmp)?;
        Ok((std::fs::canonicalize(cwd)?, std::fs::canonicalize(tmp)?))
    }

    /// True when `path` is outside `root` and every configured root.
    fn outside_roots(&self, path: &Path, root: &Path) -> bool {
        !path.starts_with(root)
            && !self
                .configured_roots
                .iter()
                .filter_map(|configured| std::fs::canonicalize(configured).ok())
                .any(|configured| path.starts_with(configured))
    }

    /// Resolve an inert launch for `root`. Run this on a blocking worker,
    /// after `policy::check` passed for the query that needs it.
    pub(crate) fn prepare(
        &self,
        language: Language,
        root: &Path,
        full_analysis: bool,
        rust: Option<&RustTools>,
    ) -> Result<Prepared, Error> {
        let (cwd, tmp) = self.state()?;
        let mut spec = self.executable(language, &cwd)?;
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
        let mut directories: Vec<_> = self
            .trusted_directories()
            .into_iter()
            .filter_map(|path| std::fs::canonicalize(path).ok())
            .filter(|path| !path.starts_with(root))
            .collect();
        let program = std::fs::canonicalize(&spec.program)?;
        if !full_analysis && program.starts_with(root) {
            return Err(Error::ProjectConfiguration);
        }
        let rust_variables = if language == Language::Rust {
            self.rust_launch(root, full_analysis, rust, &cwd, &mut directories)?
        } else {
            Vec::new()
        };
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
            .env("GOSUMDB", "off")
            // A node-owned temporary directory: never the account home,
            // which other tools and agents share.
            .env("TMPDIR", &tmp)
            .env("TMP", &tmp)
            .env("TEMP", &tmp);
        if let Some(home) = &self.home {
            spec = spec.env("HOME", home).env("USERPROFILE", home);
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
        for (variable, value) in rust_variables {
            spec = spec.env(variable, value);
        }
        let typescript_engine = if language == Language::TypeScript {
            self.typescript_engine(root, full_analysis)?
        } else {
            None
        };
        let python_interpreter = if language == Language::Python {
            self.python(root, full_analysis)?
        } else {
            None
        };
        Ok(Prepared {
            command: spec,
            typescript_engine,
            python_interpreter,
        })
    }

    /// Rust's PATH order and toolchain variables (see `toolchain`).
    fn rust_launch(
        &self,
        root: &Path,
        full_analysis: bool,
        rust: Option<&RustTools>,
        cwd: &Path,
        directories: &mut Vec<PathBuf>,
    ) -> Result<Vec<(&'static str, PathBuf)>, Error> {
        let mut variables = Vec::new();
        if full_analysis {
            // Full analysis permits project configuration, including a
            // toolchain file that the rustup proxies on PATH honour.
            if let Some(toolchain) = &self.rust_toolchain {
                directories.insert(0, std::fs::canonicalize(toolchain.join("bin"))?);
            }
            for (name, variable) in [("rustc", "RUSTC"), ("cargo", "CARGO")] {
                if let Some(program) = directories.iter().find_map(|directory| {
                    which::which_in(name, Some(directory.as_os_str()), cwd).ok()
                }) {
                    variables.push((variable, program));
                }
            }
            return Ok(variables);
        }
        let tools = rust.ok_or(Error::NotInstalled)?;
        // Any toolchain inside the served root is the repository's. One
        // that rustup did not install itself (a pin, a link or a path) is
        // trusted only outside every configured root as well.
        let trusted = |path: &Path| {
            !path.starts_with(root) && (!tools.unverified || self.outside_roots(path, root))
        };
        if !tools.paths().all(trusted) {
            return Err(Error::ProjectConfiguration);
        }
        // The toolchain's own binaries come first, so a PATH lookup of
        // cargo/rustc/rustfmt finds them rather than a rustup proxy.
        for tool in [&tools.cargo, &tools.rustc] {
            if let Some(bin) = tool.parent() {
                directories.retain(|directory| directory != bin);
                directories.insert(0, bin.to_owned());
            }
        }
        variables.push(("RUSTC", tools.rustc.clone()));
        variables.push(("CARGO", tools.cargo.clone()));
        if let Some(toolchain) = &tools.toolchain {
            variables.push(("RUSTUP_TOOLCHAIN", toolchain.clone()));
        }
        Ok(variables)
    }

    /// [Decision Log]
    /// - Purpose: pyright runs a Python interpreter to find search paths;
    ///   the repository must not choose it.
    /// - Constraint: pyright finds `python` by name through Node's spawn,
    ///   and on Windows that search can look in the process cwd first.
    /// - Chosen: servers run in a node-owned empty cwd everywhere; an
    ///   operator-pinned interpreter is passed as `python.pythonPath`; on
    ///   Windows conservative mode refuses Python without one, rather than
    ///   trusting the name lookup there.
    fn python(&self, root: &Path, full: bool) -> Result<Option<PathBuf>, Error> {
        let Some(interpreter) = &self.python_interpreter else {
            if cfg!(windows) && !full {
                return Err(Error::PythonInterpreter);
            }
            return Ok(None);
        };
        if !interpreter.is_absolute() {
            return Err(Error::Path);
        }
        let interpreter = std::fs::canonicalize(interpreter)?;
        if !full && interpreter.starts_with(root) {
            return Err(Error::ProjectConfiguration);
        }
        Ok(Some(interpreter))
    }

    fn executable(&self, language: Language, cwd: &Path) -> Result<CommandSpec, Error> {
        let spec = if let Some(spec) = self.overrides.get(&language) {
            if !spec.program.is_absolute() {
                return Err(Error::Path);
            }
            CommandSpec::new(&spec.program)
                .args(spec.args.clone())
                .cwd(cwd)
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
            let directories = self.trusted_directories();
            let program = names
                .iter()
                .find_map(|name| {
                    directories.iter().find_map(|directory| {
                        which::which_in(name, Some(directory.as_os_str()), cwd).ok()
                    })
                })
                .ok_or(Error::NotInstalled)?;
            CommandSpec::new(program)
                .args(args.iter().copied())
                .cwd(cwd)
        };
        Ok(spec)
    }

    fn typescript_engine(&self, root: &Path, full: bool) -> Result<Option<PathBuf>, Error> {
        let candidate = self.typescript_server.clone().or_else(|| {
            self.trusted_directories()
                .into_iter()
                .filter(|path| !path.starts_with(root))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(toolchain: &Path, unverified: bool) -> RustTools {
        let bin = toolchain.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for tool in ["rustc", "cargo"] {
            std::fs::write(bin.join(tool), "stand-in").unwrap();
        }
        let toolchain = std::fs::canonicalize(toolchain).unwrap();
        RustTools {
            rustc: toolchain.join("bin/rustc"),
            cargo: toolchain.join("bin/cargo"),
            toolchain: Some(toolchain),
            unverified,
        }
    }

    /// The review's rule: a pinned, linked or path toolchain is trusted only
    /// outside every configured root; one rustup installed only needs to be
    /// outside the served root.
    #[test]
    fn an_unverified_toolchain_inside_a_configured_root_is_refused() {
        let state = tempfile::tempdir().unwrap();
        let configured = tempfile::tempdir().unwrap();
        let served = tempfile::tempdir().unwrap();
        let served = std::fs::canonicalize(served.path()).unwrap();
        let mut registry = Registry {
            state_dir: Some(state.path().to_owned()),
            configured_roots: vec![configured.path().to_owned()],
            ..Registry::default()
        };
        let _previous = registry.overrides.insert(
            Language::Rust,
            Override {
                program: std::env::current_exe().unwrap(),
                args: Vec::new(),
            },
        );
        let inside = configured.path().join("toolchain");
        let refused = registry.prepare(Language::Rust, &served, false, Some(&tools(&inside, true)));
        assert!(
            matches!(refused, Err(Error::ProjectConfiguration)),
            "{refused:?}"
        );
        assert!(
            registry
                .prepare(Language::Rust, &served, false, Some(&tools(&inside, false)))
                .is_ok()
        );
        let own = served.join("toolchain");
        let refused = registry.prepare(Language::Rust, &served, false, Some(&tools(&own, false)));
        assert!(
            matches!(refused, Err(Error::ProjectConfiguration)),
            "{refused:?}"
        );
    }
}
