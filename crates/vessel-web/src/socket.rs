//! The browser's websocket (`/api/ws`): the hello, then the bridge.
//!
//! The hello must come first and within the deadline
//! ([`UiConfig::hello_deadline`](crate::UiConfig)): a socket that says
//! nothing, or the wrong thing, holds nothing for long and learns nothing.

use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;

use crate::bridge;
use crate::server::Ui;
use crate::source::NodeSource;
use crate::wire::{CLOSE_UNAUTHORIZED, ClientFrame, PresentedToken, ServerFrame};

/// The largest frame either way: a pasted block of text or a prompt with
/// its context, with room to spare.
const MAX_FRAME: usize = 4 * 1024 * 1024;

pub(crate) async fn upgrade<S: NodeSource>(
    State(ui): State<Arc<Ui<S>>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade
        .max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |socket| run(ui, socket))
}

async fn run<S: NodeSource>(ui: Arc<Ui<S>>, mut socket: WebSocket) {
    let hello = tokio::time::timeout(ui.hello_deadline, hello(&mut socket)).await;
    let accepted = hello
        .ok()
        .flatten()
        .is_some_and(|token| ui.token.matches(token.as_str()));
    if !accepted {
        tracing::info!("refused a browser: no valid token in time");
        close(socket, CLOSE_UNAUTHORIZED, "a valid token must come first").await;
        return;
    }
    let welcome = ServerFrame::Welcome {
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    if !send(&mut socket, &welcome).await {
        return;
    }
    bridge::run(Arc::clone(&ui.registry), socket).await;
}

/// The token of the first frame, if it is a hello.
async fn hello(socket: &mut WebSocket) -> Option<PresentedToken> {
    loop {
        match socket.recv().await? {
            Ok(Message::Text(text)) => {
                return match serde_json::from_str::<ClientFrame>(&text) {
                    Ok(ClientFrame::Hello { token }) => Some(token),
                    Ok(
                        ClientFrame::Call { .. }
                        | ClientFrame::Follow { .. }
                        | ClientFrame::Unfollow { .. }
                        | ClientFrame::Op { .. },
                    )
                    | Err(_) => None,
                };
            }
            Ok(Message::Ping(_) | Message::Pong(_)) => {}
            Ok(Message::Binary(_) | Message::Close(_)) | Err(_) => return None,
        }
    }
}

/// Send `frame` as JSON; whether it went.
async fn send(socket: &mut WebSocket, frame: &ServerFrame) -> bool {
    match serde_json::to_string(frame) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(error) => {
            tracing::warn!(%error, "could not encode a frame for the browser");
            false
        }
    }
}

async fn close(mut socket: WebSocket, code: u16, reason: &'static str) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    let _sent = socket.send(Message::Close(Some(frame))).await;
}
