//! Answer the server's small allowed request set; edits are always refused.

use serde::Serialize;

use crate::error::LspError as Error;
use crate::process::Process;
use crate::protocol::{self as rpc, initialize as init};

#[derive(Serialize)]
#[serde(untagged)]
enum Setting {
    All(init::Settings),
    Cargo(init::Cargo),
    Enabled(init::Enabled),
    Flag(bool),
    Absent,
}

impl Process {
    pub(crate) async fn server_request(
        &mut self,
        id: rpc::Id,
        method: &str,
        params: &str,
    ) -> Result<(), Error> {
        match method {
            "window/workDoneProgress/create" => {
                let request: rpc::ProgressCreate = serde_json::from_str(params)?;
                if let rpc::Id::Text(token) = request.token
                    && token.len() > 1024
                {
                    return Err(Error::Limit);
                }
                self.respond(id, ()).await
            }
            "workspace/configuration" => {
                let request: rpc::Configuration = serde_json::from_str(params)?;
                if request.items.len() > 64 {
                    return Err(Error::Limit);
                }
                let settings = self.settings.clone();
                let values: Vec<_> = request
                    .items
                    .into_iter()
                    .map(|item| match item.section.as_deref() {
                        None | Some("rust-analyzer") => Setting::All(settings.clone()),
                        Some("rust-analyzer.cargo") => Setting::Cargo(settings.cargo.clone()),
                        Some("rust-analyzer.procMacro") => {
                            Setting::Enabled(settings.proc_macro.clone())
                        }
                        Some("rust-analyzer.checkOnSave") => Setting::Flag(settings.check_on_save),
                        Some(_) => Setting::Absent,
                    })
                    .collect();
                self.respond(id, values).await
            }
            "workspace/workspaceFolders" => {
                let folder = init::Folder {
                    uri: crate::documents::uri(&self.root)?,
                    name: "workspace".into(),
                };
                self.respond(id, [folder]).await
            }
            "workspace/applyEdit" => {
                self.respond(
                    id,
                    rpc::ApplyEditDenied {
                        applied: false,
                        failure_reason: "Vessel context is read-only",
                    },
                )
                .await
            }
            "workspace/diagnostic/refresh" => self.respond(id, ()).await,
            _ => {
                let message = rpc::Failure {
                    jsonrpc: "2.0",
                    id,
                    error: rpc::RpcError {
                        code: -32601,
                        message: "unsupported read-only client request".into(),
                    },
                };
                crate::framing::write(&mut self.input, &message).await?;
                Ok(())
            }
        }
    }

    async fn respond<T: Serialize>(&mut self, id: rpc::Id, result: T) -> Result<(), Error> {
        crate::framing::write(
            &mut self.input,
            &rpc::Response {
                jsonrpc: "2.0",
                id,
                result,
            },
        )
        .await?;
        Ok(())
    }
}
