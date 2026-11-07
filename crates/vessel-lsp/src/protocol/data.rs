//! Typed source messages. Opaque hierarchy data is only round-tripped.

use crate::position::Utf16Position;
use serde::{Deserialize, Serialize};
use vessel_proto::RawJson;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub(crate) struct Range {
    pub(crate) start: Utf16Position,
    pub(crate) end: Utf16Position,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Location {
    pub(crate) uri: String,
    pub(crate) range: Range,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocationLink {
    pub(crate) target_uri: String,
    pub(crate) target_selection_range: Range,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum Locations {
    One(Location),
    Many(Vec<Location>),
    Links(Vec<LocationLink>),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Symbol {
    pub(crate) name: String,
    pub(crate) kind: u32,
    pub(crate) location: Location,
    pub(crate) container_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DocumentSymbol {
    pub(crate) name: String,
    pub(crate) kind: u32,
    pub(crate) selection_range: Range,
    #[serde(default)]
    pub(crate) children: Vec<DocumentSymbol>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum DocumentSymbols {
    Nested(Vec<DocumentSymbol>),
    Flat(Vec<Symbol>),
}

#[derive(Debug, Deserialize)]
pub(crate) struct Hover {
    pub(crate) contents: HoverContents,
    pub(crate) range: Option<Range>,
}
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum HoverContents {
    Markup(MarkupContent),
    Many(Vec<MarkedString>),
    One(MarkedString),
}
#[derive(Debug, Deserialize)]
pub(crate) struct MarkupContent {
    pub(crate) kind: MarkupKind,
    pub(crate) value: String,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MarkupKind {
    Plaintext,
    Markdown,
}
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum MarkedString {
    Text(String),
    Code { language: String, value: String },
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Diagnostic {
    pub(crate) range: Range,
    pub(crate) severity: Option<u8>,
    pub(crate) message: String,
    pub(crate) source: Option<String>,
}
#[derive(Debug, Deserialize)]
pub(crate) struct Published {
    pub(crate) uri: String,
    pub(crate) version: Option<i32>,
    pub(crate) diagnostics: Vec<Diagnostic>,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum DiagnosticReport {
    Full { items: Vec<Diagnostic> },
    Unchanged,
}
#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceDiagnostics {
    pub(crate) items: Vec<WorkspaceDiagnostic>,
}
#[derive(Debug, Deserialize)]
pub(crate) struct WorkspaceDiagnostic {
    pub(crate) uri: String,
    pub(crate) version: Option<i32>,
    #[serde(flatten)]
    pub(crate) report: DiagnosticReport,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HierarchyItem {
    pub(crate) name: String,
    pub(crate) kind: u32,
    pub(crate) uri: String,
    pub(crate) range: Range,
    pub(crate) selection_range: Range,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<RawJson>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IncomingCall {
    pub(crate) from: HierarchyItem,
    pub(crate) from_ranges: Vec<Range>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OutgoingCall {
    pub(crate) to: HierarchyItem,
    pub(crate) from_ranges: Vec<Range>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Document {
    pub(crate) uri: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DocumentParams {
    pub(crate) text_document: Document,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct At {
    pub(crate) text_document: Document,
    pub(crate) position: Utf16Position,
}
#[derive(Serialize)]
pub(crate) struct ReferenceParams {
    #[serde(flatten)]
    pub(crate) at: At,
    pub(crate) context: ReferenceContext,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReferenceContext {
    pub(crate) include_declaration: bool,
}
#[derive(Serialize)]
pub(crate) struct Search<'a> {
    pub(crate) query: &'a str,
}
#[derive(Serialize)]
pub(crate) struct Hierarchy {
    pub(crate) item: HierarchyItem,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Pull {
    pub(crate) text_document: Document,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) identifier: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkspacePull {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) identifier: Option<String>,
    pub(crate) previous_result_ids: Vec<PreviousResult>,
}
#[derive(Serialize)]
pub(crate) struct PreviousResult {
    uri: String,
    value: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Open<'a> {
    pub(crate) text_document: OpenDocument<'a>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpenDocument<'a> {
    pub(crate) uri: &'a str,
    pub(crate) language_id: &'a str,
    pub(crate) version: i32,
    pub(crate) text: &'a str,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Change<'a> {
    pub(crate) text_document: Versioned<'a>,
    pub(crate) content_changes: Vec<ContentChange<'a>>,
}
#[derive(Serialize)]
pub(crate) struct Versioned<'a> {
    pub(crate) uri: &'a str,
    pub(crate) version: i32,
}
#[derive(Serialize)]
pub(crate) struct ContentChange<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) range: Option<Range>,
    pub(crate) text: &'a str,
}
