//! Typed fixture messages for the deliberately small fake server.

use super::types::{
    Capabilities, Changed, ConfigurationRequest, Diagnostic, DiagnosticProvider, Failure,
    FailureDetail, HierarchyItem, HierarchyRequest, Hover, Id, Incoming, Initialize,
    InitializeResult, Location, Markup, Message, Mode, Notification, Open, Outgoing, Position,
    Progress, Published, Range, RawOptions, Report, Response, RustStatus, Section, ServerRequest,
    Symbol, Ticket, Token, WorkDone, WorkspaceItem, WorkspaceReport,
};
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;

use super::transport::{self, TransportError as Error};

pub(super) struct Peer {
    mode: Mode,
    root: String,
    uri: String,
    text: String,
    version: i32,
    launch: Option<PathBuf>,
}
impl Peer {
    pub(super) fn new(mode: Mode, launch: Option<PathBuf>) -> Self {
        Self {
            mode,
            root: String::new(),
            uri: String::new(),
            text: String::new(),
            version: 0,
            launch,
        }
    }

    /// Append one `key=value` line to the launch record, if one was asked for.
    fn record(&self, key: &str, value: &str) -> Result<(), Error> {
        if let Some(path) = &self.launch {
            let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
            writeln!(file, "{key}={value}")?;
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the deterministic fixture dispatch keeps each supported method next to its response"
    )]
    pub(super) fn handle(
        &mut self,
        message: Message,
        output: &mut impl Write,
    ) -> Result<bool, Error> {
        let params = message.params.as_ref().map_or("null", |raw| raw.get());
        let Some(method) = message.method.as_deref() else {
            if matches!(&message.id, Some(Id::Text(id)) if id == "python-config") {
                let result = message.result.as_ref().map_or("null", |raw| raw.get());
                self.record("python", result)?;
            }
            return Ok(false);
        };
        let Some(id) = message.id else {
            return self.notification(method, params, output);
        };
        match method {
            "initialize" => self.initialize(id, params, output)?,
            "shutdown" => respond(output, id, ())?,
            "workspace/symbol" | "textDocument/documentSymbol" => {
                let name = if self.text.contains("changed") {
                    "changed"
                } else {
                    "target"
                };
                let uri = if self.mode == Mode::Outside {
                    "file:///outside/secret.rs".into()
                } else {
                    self.uri.clone()
                };
                respond(
                    output,
                    id,
                    vec![Symbol {
                        name: name.into(),
                        kind: 12,
                        location: Location {
                            uri,
                            range: range(),
                        },
                    }],
                )?;
            }
            "textDocument/definition"
            | "textDocument/references"
            | "textDocument/implementation"
            | "textDocument/typeDefinition" => {
                respond(
                    output,
                    id,
                    vec![Location {
                        uri: self.uri.clone(),
                        range: range(),
                    }],
                )?;
            }
            "textDocument/hover" => match self.mode {
                Mode::ContentModified => {
                    self.mode = Mode::Normal;
                    transport::write(
                        output,
                        &Failure {
                            jsonrpc: "2.0",
                            id,
                            error: FailureDetail {
                                code: -32801,
                                message: "index changed",
                            },
                        },
                    )?;
                }
                Mode::Hang => {}
                Mode::Crash => return Ok(true),
                Mode::Oversized => {
                    output.write_all(b"Content-Length: 999999999\r\n\r\n")?;
                    output.flush()?;
                }
                Mode::GuardGo
                | Mode::GuardCpp
                | Mode::Normal
                | Mode::Incremental
                | Mode::Outside
                | Mode::Silent
                | Mode::Push
                | Mode::PythonConfig => {
                    respond(
                        output,
                        id,
                        Hover {
                            contents: Markup {
                                kind: "plaintext",
                                value: "fn target()",
                            },
                            range: range(),
                        },
                    )?;
                }
            },
            "textDocument/prepareCallHierarchy" => respond(output, id, vec![self.item()])?,
            "callHierarchy/incomingCalls" | "callHierarchy/outgoingCalls" => {
                let request: HierarchyRequest = serde_json::from_str(params)?;
                if request.item.data.ticket != "keep-opaque" {
                    return Err(Error::Protocol);
                }
                if method == "callHierarchy/incomingCalls" {
                    respond(
                        output,
                        id,
                        vec![Incoming {
                            from: self.item(),
                            from_ranges: vec![range()],
                        }],
                    )?;
                } else {
                    respond(
                        output,
                        id,
                        vec![Outgoing {
                            to: self.item(),
                            from_ranges: vec![range()],
                        }],
                    )?;
                }
            }
            "textDocument/diagnostic" => respond(
                output,
                id,
                Report {
                    kind: "full",
                    items: self.diagnostics(),
                },
            )?,
            "workspace/diagnostic" => respond(
                output,
                id,
                WorkspaceReport {
                    items: vec![WorkspaceItem {
                        uri: self.uri.clone(),
                        version: self.version,
                        report: Report {
                            kind: "full",
                            items: self.diagnostics(),
                        },
                    }],
                },
            )?,
            _ => return Err(Error::Protocol),
        }
        Ok(false)
    }

    fn initialize(&mut self, id: Id, params: &str, output: &mut impl Write) -> Result<(), Error> {
        let request: Initialize = serde_json::from_str(params)?;
        let raw: RawOptions = serde_json::from_str(params)?;
        self.record("init", raw.initialization_options.get())?;
        // Every normal test proves the conservative settings reach the peer.
        if request
            .initialization_options
            .cargo
            .is_some_and(|cargo| cargo.build_scripts.enable)
            || request
                .initialization_options
                .proc_macro
                .is_some_and(|macros| macros.enable)
            || request.initialization_options.check_on_save
        {
            return Err(Error::Protocol);
        }
        refuse_injected_environment()?;
        if self.mode == Mode::GuardGo {
            for (name, expected) in [
                ("GOTOOLCHAIN", "local"),
                ("GOFLAGS", "-mod=readonly"),
                ("GOPROXY", "off"),
                ("GOENV", "off"),
            ] {
                if std::env::var(name).ok().as_deref() != Some(expected) {
                    return Err(Error::Protocol);
                }
            }
            // Only a repository trap is on the supplied PATH in this test;
            // the launcher's filtered PATH must make it unreachable.
            if std::process::Command::new("go").arg("env").status().is_ok() {
                return Err(Error::Protocol);
            }
        }
        if self.mode == Mode::GuardCpp
            && !std::env::args().any(|argument| argument == "--query-driver=")
        {
            return Err(Error::Protocol);
        }
        self.root = request.root_uri;
        self.uri = format!("{}/fixture.rs", self.root);
        transport::write(
            output,
            &ServerRequest {
                jsonrpc: "2.0",
                id: Id::Text("progress".into()),
                method: "window/workDoneProgress/create",
                params: Token { token: "index" },
            },
        )?;
        respond(
            output,
            id,
            InitializeResult {
                capabilities: Capabilities {
                    position_encoding: "utf-16",
                    text_document_sync: if self.mode == Mode::Incremental { 2 } else { 1 },
                    workspace_symbol_provider: true,
                    document_symbol_provider: true,
                    definition_provider: true,
                    references_provider: true,
                    hover_provider: true,
                    implementation_provider: true,
                    type_definition_provider: true,
                    call_hierarchy_provider: true,
                    diagnostic_provider: if matches!(self.mode, Mode::Silent | Mode::Push) {
                        None
                    } else {
                        Some(DiagnosticProvider {
                            inter_file_dependencies: false,
                            workspace_diagnostics: true,
                        })
                    },
                },
            },
        )?;
        for kind in ["begin", "end"] {
            transport::write(
                output,
                &Notification {
                    jsonrpc: "2.0",
                    method: "$/progress",
                    params: Progress {
                        token: "index",
                        value: WorkDone {
                            kind,
                            title: "fixture indexing",
                        },
                    },
                },
            )?;
        }
        if self.mode == Mode::PythonConfig {
            ask_python(output)?;
        }
        Ok(())
    }

    fn notification(
        &mut self,
        method: &str,
        params: &str,
        output: &mut impl Write,
    ) -> Result<bool, Error> {
        match method {
            "exit" => return Ok(true),
            "initialized" => {
                transport::write(
                    output,
                    &Notification {
                        jsonrpc: "2.0",
                        method: "experimental/serverStatus",
                        params: RustStatus {
                            health: "ok",
                            quiescent: true,
                        },
                    },
                )?;
            }
            "workspace/didChangeConfiguration" | "textDocument/didClose" => {}
            "textDocument/didOpen" => {
                let request: Open = serde_json::from_str(params)?;
                self.uri = request.text_document.uri;
                self.version = request.text_document.version;
                self.text = request.text_document.text;
                self.publish(output)?;
            }
            "textDocument/didChange" => {
                let request: Changed = serde_json::from_str(params)?;
                self.uri = request.text_document.uri;
                self.version = request.text_document.version;
                let change = request
                    .content_changes
                    .into_iter()
                    .next()
                    .ok_or(Error::Protocol)?;
                if self.mode == Mode::Incremental && change.range.is_none() {
                    return Err(Error::Protocol);
                }
                self.text = change.text;
                self.publish(output)?;
            }
            _ => return Err(Error::Protocol),
        }
        Ok(false)
    }

    fn publish(&self, output: &mut impl Write) -> Result<(), Error> {
        if self.mode != Mode::Silent {
            transport::write(
                output,
                &Notification {
                    jsonrpc: "2.0",
                    method: "textDocument/publishDiagnostics",
                    params: Published {
                        uri: self.uri.clone(),
                        version: self.version,
                        diagnostics: self.diagnostics(),
                    },
                },
            )?;
        }
        Ok(())
    }
    fn diagnostics(&self) -> Vec<Diagnostic> {
        if self.text.contains("ERROR") {
            vec![Diagnostic {
                range: range(),
                severity: 1,
                message: "fixture diagnostic",
            }]
        } else {
            Vec::new()
        }
    }
    fn item(&self) -> HierarchyItem {
        HierarchyItem {
            name: "target",
            kind: 12,
            uri: self.uri.clone(),
            range: range(),
            selection_range: range(),
            data: Ticket {
                ticket: "keep-opaque".into(),
            },
        }
    }
}
fn respond<T: Serialize>(output: &mut impl Write, id: Id, result: T) -> Result<(), Error> {
    transport::write(
        output,
        &Response {
            jsonrpc: "2.0",
            id,
            result,
        },
    )
}
fn range() -> Range {
    Range {
        start: Position {
            line: 1,
            character: 7,
        },
        end: Position {
            line: 1,
            character: 13,
        },
    }
}

/// pyright asks for the `python` section to find its interpreter.
fn ask_python(output: &mut impl Write) -> Result<(), Error> {
    transport::write(
        output,
        &ServerRequest {
            jsonrpc: "2.0",
            id: Id::Text("python-config".into()),
            method: "workspace/configuration",
            params: ConfigurationRequest {
                items: [Section { section: "python" }],
            },
        },
    )
}

/// No loader or interpreter injection may reach a language server.
fn refuse_injected_environment() -> Result<(), Error> {
    for name in [
        "NODE_OPTIONS",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "PYTHONPATH",
    ] {
        if std::env::var_os(name).is_some() {
            return Err(Error::Protocol);
        }
    }
    Ok(())
}
