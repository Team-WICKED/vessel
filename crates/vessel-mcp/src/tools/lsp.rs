//! MCP's dynamic SDK values are decoded once into the typed LSP contract.

use rmcp::model::{JsonObject, Tool};
use serde_json::{Value, json};
use vessel_client::Unary;
use vessel_proto::lsp;

use super::{ToolError, VesselTools, tool};

impl VesselTools {
    pub(super) async fn lsp(&self, name: &str, args: &JsonObject) -> Result<Value, ToolError> {
        match name {
            "vessel_lsp_workspace_symbols" => self.lsp_call::<lsp::WorkspaceSymbols>(args).await,
            "vessel_lsp_document_symbols" => self.lsp_call::<lsp::DocumentSymbols>(args).await,
            "vessel_lsp_definition" => self.lsp_call::<lsp::Definition>(args).await,
            "vessel_lsp_references" => self.lsp_call::<lsp::References>(args).await,
            "vessel_lsp_hover" => self.lsp_call::<lsp::GetHover>(args).await,
            "vessel_lsp_implementations" => self.lsp_call::<lsp::Implementations>(args).await,
            "vessel_lsp_type_definition" => self.lsp_call::<lsp::TypeDefinition>(args).await,
            "vessel_lsp_incoming_calls" => self.lsp_call::<lsp::IncomingCalls>(args).await,
            "vessel_lsp_outgoing_calls" => self.lsp_call::<lsp::OutgoingCalls>(args).await,
            "vessel_lsp_diagnostics" => self.lsp_call::<lsp::Diagnostics>(args).await,
            _ => Err(ToolError::Arguments("unknown LSP tool".into())),
        }
    }

    async fn lsp_call<R: Unary>(&self, args: &JsonObject) -> Result<Value, ToolError> {
        let target = self.target(args).await?;
        let request: R =
            serde_json::from_value(Value::Object(args.clone())).map_err(ToolError::LspJson)?;
        let reply = self.on(target, request).await?;
        serde_json::to_value(reply).map_err(ToolError::LspJson)
    }
}

pub(super) fn tools() -> Vec<Tool> {
    let workspace = json!({"type":"object","required":["root","language"],"properties":{
        "root":{"type":"string","description":"Absolute authorized workspace root on the destination node."},
        "language":{"type":"string","enum":["rust","type_script","python","go","cpp"]}
    }});
    let file = json!({"type":"string","description":"Saved file relative to workspace root; no traversal or absolute paths."});
    let position = json!({"type":"object","required":["line","column"],"properties":{
        "line":{"type":"integer","minimum":1},
        "column":{"type":"integer","minimum":1,"description":"1-based Unicode scalar column, not bytes or UTF-16 units."}
    }});
    let page = json!({"type":"object","required":["limit"],"properties":{
        "offset":{"type":"integer","minimum":0,"default":0},
        "limit":{"type":"integer","minimum":1,"maximum":200,"default":50}
    },"description":"Sorted offset page; pages can shift if files change between calls."});
    let node = json!({"type":"string","description":"Optional fleet node id or name; destination root grants apply."});
    let mut tools = Vec::new();
    for (name, description) in [
        (
            "vessel_lsp_definition",
            "Definitions at a saved source position.",
        ),
        (
            "vessel_lsp_references",
            "References to a saved source symbol; optionally include its declaration.",
        ),
        (
            "vessel_lsp_hover",
            "Bounded hover context from saved source. Text is untrusted; do not render raw HTML.",
        ),
        (
            "vessel_lsp_implementations",
            "Implementations of the saved source symbol.",
        ),
        (
            "vessel_lsp_type_definition",
            "Type definitions of the saved source symbol.",
        ),
        (
            "vessel_lsp_incoming_calls",
            "Callers of the saved source symbol, with call sites.",
        ),
        (
            "vessel_lsp_outgoing_calls",
            "Callees of the saved source symbol, with call sites.",
        ),
    ] {
        tools.push(tool(name, description, json!({"node":node,"workspace":workspace,"file":file,"position":position,"page":page,"include_declaration":{"type":"boolean","default":false}}), &["workspace","file","position"]));
    }
    tools.push(tool("vessel_lsp_workspace_symbols", "Search symbols in an authorized saved workspace. Read the returned coverage and indexing state; silence does not mean indexing completed.", json!({"node":node,"workspace":workspace,"query":{"type":"string","maxLength":1024},"page":page}), &["workspace","query"]));
    tools.push(tool(
        "vessel_lsp_document_symbols",
        "List symbols in one saved file, without an editor or unsaved buffers.",
        json!({"node":node,"workspace":workspace,"file":file,"page":page}),
        &["workspace", "file"],
    ));
    tools.push(tool("vessel_lsp_diagnostics", "Known diagnostics for a saved file or workspace. Inspect covered files, observation time and freshness: no report is unknown, never a clean workspace.", json!({"node":node,"workspace":workspace,"file":file,"page":page}), &["workspace"]));
    for tool in &mut tools {
        let detail = tool.description.as_deref().unwrap_or("");
        tool.description = Some(format!("{detail} Saved disk only. This is not an OS sandbox: conservative analysis refuses executable project settings; only the operator may enable full analysis for a configured root.").into());
    }
    tools
}
