//! Conservative startup refuses repository configuration that can execute code.
//!
//! This is a fail-closed compatibility policy, not an OS sandbox. Native tools
//! and operator-owned homes remain trusted. Recheck before every query because
//! a working tree may acquire executable configuration after startup.

use serde::Deserialize;
use serde::de::IgnoredAny;
use std::path::Path;
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

/// Check only inert supported settings. Unrecognized settings are refused:
/// trying to enumerate just today's dangerous options is not fail-closed.
pub(crate) fn check(
    root: &Path,
    language: Language,
    full: bool,
    home: Option<&Path>,
) -> Result<(), Error> {
    if full {
        return Ok(());
    }
    match language {
        Language::Rust => {
            for directory in root.ancestors() {
                for name in ["rust-analyzer.toml", "rust-project.json"] {
                    if directory.join(name).try_exists()? {
                        return Err(Error::ProjectConfiguration);
                    }
                }
                let cargo = directory.join(".cargo");
                // The explicit operator home is trusted; repository config
                // is not. This exception never applies inside the workspace.
                if home.is_some_and(|home| directory == home) && !directory.starts_with(root) {
                    continue;
                }
                for name in ["config", "config.toml"] {
                    let path = cargo.join(name);
                    if path.try_exists()? {
                        let config: CargoConfig = toml_edit::de::from_str(&bounded_text(&path)?)
                            .map_err(|_executable_or_unknown| Error::ProjectConfiguration)?;
                        consume(config);
                    }
                }
                for name in ["rust-toolchain", "rust-toolchain.toml"] {
                    let path = directory.join(name);
                    if path.try_exists()? {
                        check_toolchain(&bounded_text(&path)?)?;
                    }
                }
            }
        }
        Language::TypeScript => check_typescript(root)?,
        Language::Python => check_python(root)?,
        Language::Go | Language::Cpp => {}
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

pub(crate) fn stamp(root: &Path) -> Result<Stamp, Error> {
    use std::hash::{Hash as _, Hasher as _};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    for directory in root.ancestors() {
        for name in [
            "Cargo.toml",
            "Cargo.lock",
            ".cargo/config",
            ".cargo/config.toml",
            "rust-analyzer.toml",
            "rust-project.json",
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
