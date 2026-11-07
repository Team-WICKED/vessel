//! Conservative startup refuses repository configuration that can execute code.
//!
//! This is a fail-closed compatibility policy, not an OS sandbox. Native tools
//! and operator-owned homes remain trusted. Recheck before every query because
//! a working tree may acquire executable configuration after startup. For
//! Rust it also decides which Cargo projects the server may load at all: the
//! server is never left to discover a project this module did not check.

use serde::Deserialize;
use serde::de::IgnoredAny;
use std::path::{Path, PathBuf};
use vessel_proto::lsp::Language;

use crate::error::LspError as Error;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoConfig {
    #[serde(default)]
    build: Option<SafeBuild>,
    #[serde(default)]
    profile: Option<std::collections::BTreeMap<String, SafeProfile>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct SafeBuild {
    #[serde(default)]
    target_dir: Option<String>,
    #[serde(default)]
    jobs: Option<i64>,
    #[serde(default)]
    incremental: Option<bool>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct SafeProfile {
    #[serde(default)]
    #[serde(rename = "debug")]
    _debug: Option<IgnoredAny>,
}

/// Most Cargo projects one conservative Rust server links.
pub(crate) const MAX_PROJECTS: usize = 32;
/// Directory entries examined when looking for child projects.
const MAX_ENTRIES: usize = 4096;

/// Check only inert supported settings for a server analysing `root`.
/// Unrecognized settings are refused: trying to enumerate just today's
/// dangerous options is not fail-closed.
///
/// Conservative Rust returns the project directories whose `Cargo.toml` the
/// server may load (`linkedProjects`); nothing else is ever discovered.
/// Every other case returns an empty list.
pub(crate) fn check(
    root: &Path,
    language: Language,
    full: bool,
    home: Option<&Path>,
) -> Result<Vec<PathBuf>, Error> {
    if full {
        return Ok(Vec::new());
    }
    match language {
        Language::Rust => return check_rust(root, home),
        Language::TypeScript => check_typescript(root)?,
        Language::Python => check_python(root)?,
        Language::Go | Language::Cpp => {}
    }
    Ok(Vec::new())
}

/// [Decision Log]
/// - Purpose: no Cargo, rustup or rust-analyzer configuration that could run
///   code reaches a conservative server, wherever the project sits.
/// - Before: only the root and its parents were checked, while
///   rust-analyzer itself found `sub/Cargo.toml` one level down and then ran
///   Cargo/rustc with cwd `sub`, reading `sub/`'s files (review finding).
/// - Alternatives: walk the whole tree for configuration (unbounded, and
///   rust-analyzer would still discover projects we never looked at); or
///   refuse roots without a manifest (breaks monorepos with Rust in a
///   subdirectory).
/// - Chosen: find the projects rust-analyzer would find for this root (the
///   nearest manifest at or above it, else its direct children), check every
///   directory from the root and from each project up to the filesystem
///   root, and hand exactly that list to the server with discovery off.
/// - Trade-off: projects deeper than one level below a manifest-less root
///   are not linked; query them with a deeper workspace root.
fn check_rust(root: &Path, home: Option<&Path>) -> Result<Vec<PathBuf>, Error> {
    let mut checked = std::collections::BTreeSet::new();
    for directory in root.ancestors() {
        if checked.insert(directory.to_owned()) {
            check_rust_directory(directory, root, home)?;
        }
    }
    let projects = cargo_projects(root)?;
    if projects.is_empty() {
        return Err(Error::NoProject);
    }
    for project in &projects {
        for directory in project.ancestors() {
            if checked.insert(directory.to_owned()) {
                check_rust_directory(directory, root, home)?;
            }
        }
    }
    Ok(projects)
}

/// The projects rust-analyzer would discover for `root`: the nearest
/// manifest at or above it, otherwise manifests one directory below it.
fn cargo_projects(root: &Path) -> Result<Vec<PathBuf>, Error> {
    for directory in root.ancestors() {
        if manifest(directory)? {
            return Ok(vec![directory.to_owned()]);
        }
    }
    let mut projects = Vec::new();
    for (index, entry) in std::fs::read_dir(root)?.enumerate() {
        if index >= MAX_ENTRIES {
            return Err(Error::Limit);
        }
        let entry = entry?;
        // file_type does not follow symlinks: a linked directory could
        // lead anywhere and is never treated as a project.
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let directory = entry.path();
        if manifest(&directory)? {
            projects.push(directory);
            if projects.len() > MAX_PROJECTS {
                return Err(Error::Limit);
            }
        }
    }
    projects.sort();
    Ok(projects)
}

fn manifest(directory: &Path) -> Result<bool, Error> {
    match std::fs::symlink_metadata(directory.join("Cargo.toml")) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        // A symlinked or special manifest is not followed.
        Ok(_) => Err(Error::ProjectConfiguration),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn check_rust_directory(directory: &Path, root: &Path, home: Option<&Path>) -> Result<(), Error> {
    for name in [
        "rust-analyzer.toml",
        "rust-project.json",
        ".rust-project.json",
    ] {
        if directory.join(name).try_exists()? {
            return Err(Error::ProjectConfiguration);
        }
    }
    // The explicit operator home is trusted; repository config is not.
    // This exception never applies inside the workspace.
    let trusted_home = home.is_some_and(|home| directory == home) && !directory.starts_with(root);
    if !trusted_home {
        for name in ["config", "config.toml"] {
            let path = directory.join(".cargo").join(name);
            if path.try_exists()? {
                let config: CargoConfig = toml_edit::de::from_str(&bounded_text(&path)?)
                    .map_err(|_executable_or_unknown| Error::ProjectConfiguration)?;
                consume(config);
            }
        }
    }
    // Defence in depth: rustup is never reached through a proxy (see
    // `toolchain`), so these files cannot select a compiler; refusing
    // path/custom toolchains still keeps the policy fail-closed.
    for name in ["rust-toolchain", "rust-toolchain.toml"] {
        let path = directory.join(name);
        if path.try_exists()? {
            check_toolchain(&bounded_text(&path)?)?;
        }
    }
    Ok(())
}

fn bounded_text(path: &Path) -> Result<String, Error> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink()
        || path.parent().is_some_and(|parent| {
            std::fs::symlink_metadata(parent)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
        })
    {
        return Err(Error::ProjectConfiguration);
    }
    // Oversized, special (a FIFO) and non-UTF-8 files are not inert
    // configuration this policy can vouch for.
    let bytes = match crate::documents::read_regular(path, 65_536) {
        Ok(bytes) => bytes,
        Err(Error::Limit | Error::Path) => return Err(Error::ProjectConfiguration),
        Err(error) => return Err(error),
    };
    String::from_utf8(bytes).map_err(|_not_utf8| Error::ProjectConfiguration)
}

fn check_toolchain(text: &str) -> Result<(), Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct File {
        toolchain: Toolchain,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Toolchain {
        channel: String,
        components: Option<Vec<String>>,
        targets: Option<Vec<String>>,
        profile: Option<String>,
    }
    let channel = if text.trim_start().starts_with('[') {
        let file: File =
            toml_edit::de::from_str(text).map_err(|_invalid| Error::ProjectConfiguration)?;
        drop((
            file.toolchain.components,
            file.toolchain.targets,
            file.toolchain.profile,
        ));
        file.toolchain.channel
    } else {
        text.trim().to_owned()
    };
    // Installed official toolchains are trusted; named/path toolchains can
    // point at repository binaries, so they require full analysis.
    let official = ["stable", "beta", "nightly"].contains(&channel.as_str())
        || channel.strip_prefix("nightly-").is_some_and(|date| {
            date.bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'-')
        })
        || channel.starts_with(|ch: char| ch.is_ascii_digit())
            && channel
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.');
    if !official {
        return Err(Error::ProjectConfiguration);
    }
    Ok(())
}

fn consume(config: CargoConfig) {
    if let Some(build) = config.build {
        drop((build.target_dir, build.jobs, build.incremental));
    }
    drop(config.profile);
}

fn check_typescript(root: &Path) -> Result<(), Error> {
    #[derive(Deserialize)]
    struct Config {
        extends: Option<IgnoredAny>,
        references: Option<IgnoredAny>,
        #[serde(rename = "compilerOptions")]
        compiler: Option<Compiler>,
    }
    #[derive(Deserialize)]
    struct Compiler {
        plugins: Option<IgnoredAny>,
    }
    for directory in root.ancestors() {
        for name in ["tsconfig.json", "jsconfig.json"] {
            let path = directory.join(name);
            if path.try_exists()? {
                let config: Config = serde_json::from_str(&bounded_text(&path)?)
                    .map_err(|_unsupported| Error::ProjectConfiguration)?;
                if config.extends.is_some()
                    || config.references.is_some()
                    || config
                        .compiler
                        .is_some_and(|compiler| compiler.plugins.is_some())
                {
                    return Err(Error::ProjectConfiguration);
                }
            }
        }
    }
    Ok(())
}

fn check_python(root: &Path) -> Result<(), Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields, rename_all = "camelCase")]
    struct SafeConfig {
        include: Option<Vec<String>>,
        exclude: Option<Vec<String>>,
        ignore: Option<Vec<String>>,
        extra_paths: Option<Vec<String>>,
        python_version: Option<String>,
        python_platform: Option<String>,
        type_checking_mode: Option<String>,
    }
    #[derive(Deserialize)]
    struct Pyproject {
        tool: Option<Tools>,
    }
    #[derive(Deserialize)]
    struct Tools {
        pyright: Option<SafeConfig>,
        basedpyright: Option<SafeConfig>,
    }
    fn consume(config: SafeConfig) {
        drop((
            config.include,
            config.exclude,
            config.ignore,
            config.extra_paths,
            config.python_version,
            config.python_platform,
            config.type_checking_mode,
        ));
    }
    for directory in root.ancestors() {
        for name in ["pyrightconfig.json", "basedpyrightconfig.json"] {
            let path = directory.join(name);
            if path.try_exists()? {
                consume(
                    serde_json::from_str(&bounded_text(&path)?)
                        .map_err(|_unsupported| Error::ProjectConfiguration)?,
                );
            }
        }
        let pyproject = directory.join("pyproject.toml");
        if pyproject.try_exists()? {
            let config: Pyproject = toml_edit::de::from_str(&bounded_text(&pyproject)?)
                .map_err(|_unsupported| Error::ProjectConfiguration)?;
            if let Some(tool) = config.tool {
                for config in [tool.pyright, tool.basedpyright].into_iter().flatten() {
                    consume(config);
                }
            }
        }
    }
    for name in ["sitecustomize.py", "usercustomize.py", "os.py"] {
        if root.join(name).try_exists()? {
            return Err(Error::ProjectConfiguration);
        }
    }
    Ok(())
}

/// Ephemeral metadata identity for project configuration. No contents or paths
/// leave the actor; changes restart the server after a fresh policy check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp(u64);

/// Covers the server root's parents and every linked project's parents, so a
/// changed manifest or configuration restarts the server after a recheck.
pub(crate) fn stamp(root: &Path, projects: &[PathBuf]) -> Result<Stamp, Error> {
    use std::hash::{Hash as _, Hasher as _};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    projects.hash(&mut hash);
    let mut directories = std::collections::BTreeSet::new();
    for start in std::iter::once(root).chain(projects.iter().map(PathBuf::as_path)) {
        directories.extend(start.ancestors());
    }
    for directory in directories {
        directory.hash(&mut hash);
        for name in [
            "Cargo.toml",
            "Cargo.lock",
            ".cargo/config",
            ".cargo/config.toml",
            "rust-analyzer.toml",
            "rust-project.json",
            ".rust-project.json",
            "rust-toolchain",
            "rust-toolchain.toml",
            "go.mod",
            "go.work",
            "tsconfig.json",
            "jsconfig.json",
            "pyrightconfig.json",
            "basedpyrightconfig.json",
            "pyproject.toml",
            "compile_commands.json",
            ".clangd",
        ] {
            match std::fs::symlink_metadata(directory.join(name)) {
                Ok(metadata) => {
                    metadata.len().hash(&mut hash);
                    metadata.modified().ok().hash(&mut hash);
                    metadata.file_type().is_symlink().hash(&mut hash);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0_u8.hash(&mut hash),
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(Stamp(hash.finish()))
}
