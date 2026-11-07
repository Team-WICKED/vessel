//! Independent typed messages used by the fake language server.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    ContentModified,
    GuardGo,
    GuardCpp,
    Normal,
    Hang,
    Crash,
    Incremental,
    Outside,
    Silent,
    Oversized,
    Push,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub(super) enum Id {
    Number(i64),
    Text(String),
}
#[derive(Deserialize)]
pub(super) struct Message {
    pub(super) id: Option<Id>,
    pub(super) method: Option<String>,
    pub(super) params: Option<Box<RawValue>>,
}
#[derive(Serialize)]
pub(super) struct Response<T> {
    pub(super) jsonrpc: &'static str,
    pub(super) id: Id,
    pub(super) result: T,
}
#[derive(Serialize)]
pub(super) struct Failure {
    pub(super) jsonrpc: &'static str,
    pub(super) id: Id,
    pub(super) error: FailureDetail,
}
#[derive(Serialize)]
pub(super) struct FailureDetail {
    pub(super) code: i32,
    pub(super) message: &'static str,
}
#[derive(Serialize)]
pub(super) struct Notification<T> {
    pub(super) jsonrpc: &'static str,
    pub(super) method: &'static str,
    pub(super) params: T,
}
#[derive(Serialize)]
pub(super) struct ServerRequest<T> {
    pub(super) jsonrpc: &'static str,
    pub(super) id: Id,
    pub(super) method: &'static str,
    pub(super) params: T,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) struct Position {
    pub(super) line: u32,
    pub(super) character: u32,
}
#[derive(Clone, Copy, Serialize)]
pub(super) struct Range {
    pub(super) start: Position,
    pub(super) end: Position,
}
#[derive(Serialize)]
pub(super) struct Location {
    pub(super) uri: String,
    pub(super) range: Range,
}
#[derive(Serialize)]
pub(super) struct Symbol {
    pub(super) name: String,
    pub(super) kind: u32,
    pub(super) location: Location,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HierarchyItem {
    pub(super) name: &'static str,
    pub(super) kind: u32,
    pub(super) uri: String,
    pub(super) range: Range,
    pub(super) selection_range: Range,
    pub(super) data: Ticket,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Ticket {
    pub(super) ticket: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Incoming {
    pub(super) from: HierarchyItem,
    pub(super) from_ranges: Vec<Range>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Outgoing {
    pub(super) to: HierarchyItem,
    pub(super) from_ranges: Vec<Range>,
}
#[derive(Serialize)]
pub(super) struct Hover {
    pub(super) contents: Markup,
    pub(super) range: Range,
}
#[derive(Serialize)]
pub(super) struct Markup {
    pub(super) kind: &'static str,
    pub(super) value: &'static str,
}
#[derive(Serialize)]
pub(super) struct Diagnostic {
    pub(super) range: Range,
    pub(super) severity: u8,
    pub(super) message: &'static str,
}
#[derive(Serialize)]
pub(super) struct Published {
    pub(super) uri: String,
    pub(super) version: i32,
    pub(super) diagnostics: Vec<Diagnostic>,
}
#[derive(Serialize)]
pub(super) struct Report {
    pub(super) kind: &'static str,
    pub(super) items: Vec<Diagnostic>,
}
#[derive(Serialize)]
pub(super) struct WorkspaceReport {
    pub(super) items: Vec<WorkspaceItem>,
}
#[derive(Serialize)]
pub(super) struct WorkspaceItem {
    pub(super) uri: String,
    pub(super) version: i32,
    #[serde(flatten)]
    pub(super) report: Report,
}
#[derive(Serialize)]
pub(super) struct InitializeResult {
    pub(super) capabilities: Capabilities,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the LSP fixture advertises independent protocol capability flags"
)]
pub(super) struct Capabilities {
    pub(super) position_encoding: &'static str,
    pub(super) text_document_sync: u8,
    pub(super) workspace_symbol_provider: bool,
    pub(super) document_symbol_provider: bool,
    pub(super) definition_provider: bool,
    pub(super) references_provider: bool,
    pub(super) hover_provider: bool,
    pub(super) implementation_provider: bool,
    pub(super) type_definition_provider: bool,
    pub(super) call_hierarchy_provider: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) diagnostic_provider: Option<DiagnosticProvider>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DiagnosticProvider {
    pub(super) inter_file_dependencies: bool,
    pub(super) workspace_diagnostics: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Initialize {
    pub(super) root_uri: String,
    pub(super) initialization_options: Options,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Options {
    pub(super) cargo: Option<Cargo>,
    pub(super) proc_macro: Option<Enabled>,
    #[serde(default)]
    pub(super) check_on_save: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Cargo {
    pub(super) build_scripts: Enabled,
}
#[derive(Deserialize)]
pub(super) struct Enabled {
    pub(super) enable: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Open {
    pub(super) text_document: OpenDocument,
}
#[derive(Deserialize)]
pub(super) struct OpenDocument {
    pub(super) uri: String,
    pub(super) version: i32,
    pub(super) text: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Changed {
    pub(super) text_document: Versioned,
    pub(super) content_changes: Vec<ContentChange>,
}
#[derive(Deserialize)]
pub(super) struct Versioned {
    pub(super) uri: String,
    pub(super) version: i32,
}
#[derive(Deserialize)]
pub(super) struct ContentChange {
    pub(super) text: String,
    pub(super) range: Option<IgnoredRange>,
}
#[derive(Deserialize)]
pub(super) struct IgnoredRange {}
#[derive(Deserialize)]
pub(super) struct HierarchyRequest {
    pub(super) item: ItemData,
}
#[derive(Deserialize)]
pub(super) struct ItemData {
    pub(super) data: Ticket,
}
#[derive(Serialize)]
pub(super) struct Progress {
    pub(super) token: &'static str,
    pub(super) value: WorkDone,
}
#[derive(Serialize)]
pub(super) struct WorkDone {
    pub(super) kind: &'static str,
    pub(super) title: &'static str,
}
#[derive(Serialize)]
pub(super) struct Token {
    pub(super) token: &'static str,
}

#[derive(Serialize)]
pub(super) struct RustStatus {
    pub(super) health: &'static str,
    pub(super) quiescent: bool,
}
