//! Advertise only capabilities implemented by the read-only node.

use serde::{Deserialize, Serialize};

use super::Empty;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Initialize {
    pub(crate) process_id: u32,
    pub(crate) root_uri: String,
    pub(crate) workspace_folders: Vec<Folder>,
    pub(crate) capabilities: ClientCapabilities,
    pub(crate) initialization_options: Options,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Folder {
    pub(crate) uri: String,
    pub(crate) name: String,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClientCapabilities {
    pub(crate) general: General,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) experimental: Option<Experimental>,
    pub(crate) window: Window,
    pub(crate) workspace: Workspace,
    pub(crate) text_document: TextDocument,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct General {
    pub(crate) position_encodings: [&'static str; 1],
}
impl Default for General {
    fn default() -> Self {
        Self {
            position_encodings: ["utf-16"],
        }
    }
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Window {
    pub(crate) work_done_progress: bool,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Workspace {
    pub(crate) configuration: bool,
    #[serde(rename = "workspaceFolders")]
    pub(crate) folders: bool,
    pub(crate) apply_edit: bool,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TextDocument {
    pub(crate) document_symbol: SymbolCapabilities,
    pub(crate) hover: HoverCapabilities,
    pub(crate) diagnostic: DiagnosticCapabilities,
    pub(crate) call_hierarchy: Empty,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SymbolCapabilities {
    pub(crate) hierarchical_document_symbol_support: bool,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HoverCapabilities {
    pub(crate) content_format: [&'static str; 2],
}
impl Default for HoverCapabilities {
    fn default() -> Self {
        Self {
            content_format: ["plaintext", "markdown"],
        }
    }
}
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiagnosticCapabilities {
    pub(crate) dynamic_registration: bool,
    pub(crate) related_document_support: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Initialized {
    pub(crate) capabilities: Capabilities,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Capabilities {
    pub(crate) position_encoding: Option<String>,
    pub(crate) text_document_sync: Option<Sync>,
    pub(crate) workspace_symbol_provider: Option<Provider>,
    pub(crate) document_symbol_provider: Option<Provider>,
    pub(crate) definition_provider: Option<Provider>,
    pub(crate) references_provider: Option<Provider>,
    pub(crate) hover_provider: Option<Provider>,
    pub(crate) implementation_provider: Option<Provider>,
    pub(crate) type_definition_provider: Option<Provider>,
    pub(crate) call_hierarchy_provider: Option<Provider>,
    pub(crate) diagnostic_provider: Option<DiagnosticProvider>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum Provider {
    Flag(bool),
    Options(Empty),
}
impl Provider {
    pub(crate) const fn enabled(&self) -> bool {
        match self {
            Self::Flag(flag) => *flag,
            Self::Options(_) => true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiagnosticProvider {
    pub(crate) identifier: Option<String>,
    #[serde(default)]
    pub(crate) workspace_diagnostics: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum Sync {
    Kind(u8),
    Options(SyncOptions),
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncOptions {
    #[serde(default)]
    pub(crate) open_close: bool,
    pub(crate) change: Option<u8>,
}
impl Sync {
    pub(crate) const fn kind(&self) -> u8 {
        match self {
            Self::Kind(kind) => *kind,
            Self::Options(options) => match options.change {
                Some(kind) => kind,
                None => 0,
            },
        }
    }
    pub(crate) const fn open_close(&self) -> bool {
        match self {
            Self::Kind(kind) => *kind != 0,
            Self::Options(options) => options.open_close,
        }
    }
}

/// No arbitrary initialization options: only operator-controlled policies.
///
/// rust-analyzer reads these both from initialization options and from
/// `workspace/configuration` answers; both carry this one value.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Settings {
    pub(crate) cargo: Cargo,
    pub(crate) proc_macro: Enabled,
    pub(crate) check_on_save: bool,
    /// Conservative mode: the manifests `policy::check` approved. A
    /// non-empty list turns rust-analyzer's own project discovery off, so it
    /// never loads a project nobody checked. Full analysis leaves it empty
    /// (omitted), letting the server discover projects itself.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) linked_projects: Vec<std::path::PathBuf>,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Cargo {
    pub(crate) build_scripts: Enabled,
    pub(crate) autoreload: bool,
}
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Enabled {
    pub(crate) enable: bool,
}
impl Settings {
    /// `projects` are checked project directories; their manifests are linked.
    pub(crate) fn new(full: bool, projects: &[std::path::PathBuf]) -> Self {
        Self {
            cargo: Cargo {
                build_scripts: Enabled { enable: full },
                autoreload: true,
            },
            proc_macro: Enabled { enable: full },
            check_on_save: full,
            linked_projects: projects
                .iter()
                .map(|project| project.join("Cargo.toml"))
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum Options {
    Rust(Settings),
    TypeScript(TypeScriptOptions),
    Other(Empty),
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TypeScriptOptions {
    pub(crate) disable_automatic_typing_acquisition: bool,
    /// Always empty. typescript-language-server turns this list into
    /// tsserver's `--globalPlugins`/`--pluginProbeLocations`; with none,
    /// and without `--allowLocalPluginLoads` (never passed), a project's
    /// tsconfig plugins resolve only beside the trusted TypeScript engine.
    pub(crate) plugins: [Empty; 0],
    pub(crate) tsserver: TypeScriptServer,
}
#[derive(Debug, Serialize)]
pub(crate) struct TypeScriptServer {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<std::path::PathBuf>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Experimental {
    pub(crate) server_status_notification: bool,
}
#[derive(Debug, Deserialize)]
pub(crate) struct RustStatus {
    pub(crate) health: Health,
    pub(crate) quiescent: bool,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Health {
    Ok,
    Warning,
    Error,
}
#[derive(Serialize)]
pub(crate) struct ConfigurationChanged {
    pub(crate) settings: RustSection,
}
#[derive(Serialize)]
pub(crate) struct RustSection {
    #[serde(rename = "rust-analyzer")]
    pub(crate) rust: Settings,
}

/// pyright's `python` section: the operator-pinned interpreter.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PythonSection {
    pub(crate) python_path: std::path::PathBuf,
}
