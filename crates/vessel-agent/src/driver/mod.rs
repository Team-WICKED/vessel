//! The ACP connection driver: one task per agent session.
//!
//! Owns the ACP connection, the session state (ready or not, turn running
//! or not, pending permissions, announced tool calls) and turns ACP traffic
//! into [`AgentOutput`]s. Must not touch the process (the owner of
//! [`AgentProcess`](crate::AgentProcess) stops it) or read ACP payloads
//! (the `mapper` does).
//!
//! [Decision Log]
//! - Purpose: ACP handlers run inside the SDK's dispatch loop, which is
//!   blocked until they return; a handler that waited for a client's
//!   permission answer, or for later traffic, would stall or deadlock the
//!   connection.
//! - Choice: handlers only forward what arrives into one bounded `inbound`
//!   channel and return. The handshake and each prompt run as spawned
//!   connection tasks that also report into `inbound`. One loop consumes
//!   `inbound` and client commands and owns all state.
//! - Why one channel: the SDK routes a prompt's response only after the
//!   handlers for earlier notifications have returned, so a single FIFO
//!   keeps "last message chunk, then turn ended" in order. Two channels
//!   could deliver the end of a turn before its last chunk. The same holds
//!   for `session/load`, whose replayed history arrives before its
//!   response: the handshake runs in a task, so the loop keeps draining the
//!   replay while the handshake waits.
//! - Backpressure: `inbound` and the outputs are bounded, and the SDK reads
//!   the agent's stdout through `transport`, which holds each update and
//!   permission request back until this loop has taken the previous ones.
//!   When the consumer stops reading, the loop stops, the transport stops
//!   reading, and the agent blocks on its writes. (The SDK's own queues are
//!   unbounded; `transport` is what bounds what reaches them.)
//!
//! Layout: this module sets the connection up and runs the loop, which
//! takes the client's commands (`commands`) and the agent's traffic
//! (`agent`) in order; `close` settles what a closed connection leaves
//! open. All of them share the one [`Driver`] state.

mod agent;
mod close;
mod commands;

use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::v1::schema as acp;
use agent_client_protocol::v1::{ConnectionTo, Responder};
use agent_client_protocol::{Agent, Client, Error};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use vessel_proto::{AgentEvent, HarnessSessionId, ToolCallId};

use crate::error::Disconnect;
use crate::handshake::{self, AgentSetup, HandshakeError};
use crate::input::Command;
use crate::mapper;
use crate::output::AgentOutput;
use crate::permissions::Permissions;
use crate::recent::Recent;
use crate::transport::{self, Gate};

/// Bound of the inbound queue between the SDK's handlers and the loop.
const INBOUND: usize = 64;

/// After the connection closed with a turn open, how long to wait for each
/// further item while the turn's outcome is still on its way.
const AFTER_CLOSE: Duration = Duration::from_secs(2);

/// Something that arrived from the agent, in arrival order.
enum Inbound {
    Started(Result<acp::SessionId, HandshakeError>),
    Update(acp::SessionNotification),
    Permission(
        acp::RequestPermissionRequest,
        Responder<acp::RequestPermissionResponse>,
    ),
    TurnDone(Result<acp::PromptResponse, Error>),
}

/// Drive the ACP connection over the agent's stdio until it ends, then
/// report why.
pub(crate) async fn run(
    stdin: ChildStdin,
    stdout: ChildStdout,
    commands: mpsc::Receiver<Command>,
    outputs: mpsc::Sender<AgentOutput>,
    setup: AgentSetup,
) {
    let (inbound, inbound_receiver) = mpsc::channel(INBOUND);
    let gate = Gate::new();
    let transport = transport::lines(stdin, stdout, gate.clone());
    // Outside the connection's closure, so it can still be settled when the
    // connection fails rather than closes (see the end of this function).
    let mut driver = Driver {
        inbound: inbound.clone(),
        inbound_receiver,
        commands,
        commands_open: true,
        outputs: outputs.clone(),
        session: None,
        turn: false,
        cancelling: false,
        permissions: Permissions::default(),
        tool_calls: Recent::default(),
        gate,
    };
    let ended = Client
        .v1()
        .name("vessel")
        .on_receive_notification(
            {
                let inbound = inbound.clone();
                async move |notification: acp::SessionNotification, _cx| {
                    forward(&inbound, Inbound::Update(notification)).await;
                    Ok::<(), Error>(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: acp::RequestPermissionRequest, responder, _cx| {
                forward(&inbound, Inbound::Permission(request, responder)).await;
                Ok::<(), Error>(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, async |cx| driver.run(cx, setup).await)
        .await;
    let disconnect = match ended {
        Ok(disconnect) => disconnect,
        Err(error) => {
            // The transport failed (for example the agent wrote bytes that
            // are not UTF-8): the loop was dropped without seeing a close,
            // so the turn and the permission requests it left open are
            // settled here, as on a close.
            driver.abandon_turn().await;
            Disconnect::Protocol {
                message: describe(&error),
            }
        }
    };
    drop(driver);
    if outputs
        .send(AgentOutput::Disconnected(disconnect))
        .await
        .is_err()
    {
        tracing::debug!("agent outputs dropped before the disconnect");
    }
}

/// Session state, owned by the driver loop.
struct Driver {
    /// Kept for the tasks the loop spawns (handshake, prompts).
    inbound: mpsc::Sender<Inbound>,
    inbound_receiver: mpsc::Receiver<Inbound>,
    commands: mpsc::Receiver<Command>,
    commands_open: bool,
    outputs: mpsc::Sender<AgentOutput>,
    /// The ACP session, once the handshake succeeded.
    session: Option<acp::SessionId>,
    /// Whether a prompt is in flight.
    turn: bool,
    /// The turn in flight was cancelled: permission requests still on
    /// their way are answered `cancelled` as they arrive.
    cancelling: bool,
    permissions: Permissions,
    /// Tool calls already announced on the timeline.
    /// Recent ones only, so a long session does not keep them all.
    tool_calls: Recent<ToolCallId>,
    /// Holds the agent's lines back while this loop is behind (see
    /// `transport`); each update and permission request taken from
    /// `inbound` returns its permit.
    gate: Gate,
}

impl Driver {
    async fn run(
        &mut self,
        cx: ConnectionTo<Agent>,
        setup: AgentSetup,
    ) -> Result<Disconnect, Error> {
        cx.spawn({
            let cx = cx.clone();
            let inbound = self.inbound.clone();
            async move {
                let started = handshake::run_within(&cx, &setup).await;
                forward(&inbound, Inbound::Started(started)).await;
                Ok(())
            }
        })?;
        loop {
            // Order matters. Commands first: they are few (the queue holds
            // 16 and every caller waits for its reply) and cheap, while the
            // agent can stream updates faster than a slow client reads them,
            // keeping `inbound` ready on every turn of the loop; polled
            // second, a `CancelTurn` or a permission answer would wait for
            // the stream to end. Taking a command before queued updates is
            // safe: a turn's end (`TurnEnded`) comes only from its
            // `TurnDone`, which is handled in order with the updates. The
            // close is last, so everything queued is handled before it.
            tokio::select! {
                biased;
                command = self.commands.recv(), if self.session.is_some() && self.commands_open => {
                    match command {
                        Some(command) => self.command(&cx, command).await,
                        None => self.commands_open = false,
                    }
                }
                item = self.inbound_receiver.recv() => {
                    // The loop holds a sender itself, so this never ends
                    // while it runs.
                    let Some(item) = item else {
                        return Ok(Disconnect::Closed);
                    };
                    if let Some(end) = self.handle(item).await {
                        return Ok(end);
                    }
                }
                () = cx.incoming_closed() => {
                    self.settle_after_close().await;
                    return Ok(Disconnect::Closed);
                }
            }
        }
    }

    /// Apply one inbound item; `Some` ends the session.
    async fn handle(&mut self, item: Inbound) -> Option<Disconnect> {
        match item {
            Inbound::Started(Ok(session)) => {
                let harness_session = HarnessSessionId::new(Arc::clone(&session.0));
                self.session = Some(session);
                self.emit(AgentEvent::Ready { harness_session }).await;
            }
            Inbound::Started(Err(error)) => {
                return Some(Disconnect::Handshake {
                    message: describe(&error),
                });
            }
            Inbound::Update(notification) => {
                self.gate.release();
                self.update(notification).await;
            }
            Inbound::Permission(request, responder) => {
                self.gate.release();
                self.permission(request, responder).await;
            }
            Inbound::TurnDone(result) => {
                self.turn = false;
                self.cancelling = false;
                self.withdraw_permissions().await;
                let event = match result {
                    Ok(response) => AgentEvent::TurnEnded {
                        reason: mapper::stop_reason(response.stop_reason),
                    },
                    Err(error) => AgentEvent::TurnFailed {
                        message: describe(&error),
                    },
                };
                self.emit(event).await;
            }
        }
        None
    }

    /// `&mut self` rather than `&self`: the session holds permission
    /// responders, which are `Send` but not `Sync`, so a shared borrow held
    /// across this `.await` would make the driver future non-`Send`.
    async fn emit(&mut self, event: AgentEvent) {
        if self.outputs.send(AgentOutput::Event(event)).await.is_err() {
            tracing::debug!("agent outputs dropped");
        }
    }
}

async fn forward(inbound: &mpsc::Sender<Inbound>, item: Inbound) {
    // Fails only after the loop ended; the connection is going away and a
    // dropped permission responder answers the agent with an error.
    if inbound.send(item).await.is_err() {
        tracing::debug!("agent traffic after the session loop ended");
    }
}

/// An error and its sources, as one line for people.
pub(crate) fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
