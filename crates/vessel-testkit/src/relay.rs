//! A relay that misbehaves on command (feature `relay`): a websocket proxy
//! in front of a real relay that passes everything through, except the
//! binary messages a test tells it to drop, replay, reorder, corrupt or
//! delay.
//!
//! [Decision Log]
//! - Purpose: milestone 3 must show that a relay tampering with records
//!   ends the connection and that nothing from it reaches the RPC layer
//!   (ADR 0009), for the node and client code, not just for the channel.
//! - Alternatives: a second relay implementation with hooks (duplicates
//!   the real one and drifts from it); hooks inside `vessel-relay` (test
//!   code in the shipped relay).
//! - Choice: a proxy that speaks websocket on both sides and forwards every
//!   message as is, so registration, pairing, close codes and pings are the
//!   real relay's. Only client connections (`/v1/connect`) are tampered
//!   with, which covers both directions of every relayed channel; control
//!   and node data connections pass untouched.
//! - Orders apply to the next binary message in their direction, handshake
//!   messages included: queue them once the channel is up to hit records.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

/// Which way a message travels on a relayed connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Toward {
    /// From the client to the node.
    Node,
    /// From the node to the client.
    Client,
}

/// What to do to one binary message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tamper {
    /// Never forward it.
    Drop,
    /// Forward it twice.
    Replay,
    /// Hold it back and forward it after the next one.
    Reorder,
    /// Flip one bit in its middle byte.
    Corrupt,
    /// Forward it after a pause (the channel tolerates this).
    Delay(Duration),
}

#[derive(Debug, Default)]
struct Orders {
    toward_node: VecDeque<Tamper>,
    toward_client: VecDeque<Tamper>,
}

impl Orders {
    fn next(&mut self, toward: Toward) -> Option<Tamper> {
        match toward {
            Toward::Node => self.toward_node.pop_front(),
            Toward::Client => self.toward_client.pop_front(),
        }
    }
}

/// A misbehaving relay: point nodes and clients at [`url`](Self::url)
/// instead of the real relay's.
#[derive(Debug)]
pub struct MisbehavingRelay {
    url: String,
    orders: Arc<Mutex<Orders>>,
    applied: watch::Sender<u64>,
    task: JoinHandle<()>,
}

impl MisbehavingRelay {
    /// A proxy on a loopback port in front of the relay at `upstream`
    /// (`ws://host:port`, with any path prefix).
    ///
    /// # Errors
    /// No loopback port could be bound.
    pub async fn start(upstream: &str) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let orders = Arc::new(Mutex::new(Orders::default()));
        let (applied, _receiver) = watch::channel(0);
        let task = tokio::spawn(accept_loop(
            listener,
            upstream.trim_end_matches('/').to_owned(),
            Arc::clone(&orders),
            applied.clone(),
        ));
        Ok(Self {
            url: format!("ws://{address}"),
            orders,
            applied,
            task,
        })
    }

    /// The relay URL to give nodes and clients.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Do `tamper` to the next binary message travelling `toward`.
    pub fn tamper(&self, toward: Toward, tamper: Tamper) {
        let mut orders = self.orders.lock().unwrap_or_else(PoisonError::into_inner);
        match toward {
            Toward::Node => orders.toward_node.push_back(tamper),
            Toward::Client => orders.toward_client.push_back(tamper),
        }
    }

    /// Wait until `count` orders have been carried out in total.
    pub async fn applied(&self, count: u64) {
        let mut applied = self.applied.subscribe();
        let _reached = applied.wait_for(|done| *done >= count).await;
    }

    /// The local address, for tests that need it.
    #[must_use]
    pub fn address(&self) -> Option<SocketAddr> {
        self.url.strip_prefix("ws://")?.parse().ok()
    }
}

impl Drop for MisbehavingRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn accept_loop(
    listener: TcpListener,
    upstream: String,
    orders: Arc<Mutex<Orders>>,
    applied: watch::Sender<u64>,
) {
    while let Ok((stream, _peer)) = listener.accept().await {
        let upstream = upstream.clone();
        let orders = Arc::clone(&orders);
        let applied = applied.clone();
        drop(tokio::spawn(async move {
            proxy(stream, &upstream, &orders, &applied).await;
        }));
    }
}

async fn proxy(
    stream: TcpStream,
    upstream: &str,
    orders: &Arc<Mutex<Orders>>,
    applied: &watch::Sender<u64>,
) {
    let mut target = String::new();
    let accepted =
        tokio_tungstenite::accept_hdr_async(stream, |request: &Request, response: Response| {
            target = request.uri().to_string();
            Ok(response)
        })
        .await;
    let Ok(downstream) = accepted else {
        return;
    };
    let Ok((upstream, _response)) =
        tokio_tungstenite::connect_async(format!("{upstream}{target}")).await
    else {
        return;
    };
    let tampered = target.starts_with("/v1/connect");
    let (down_tx, down_rx) = downstream.split();
    let (up_tx, up_rx) = upstream.split();
    let toward_node = Direction {
        toward: Toward::Node,
        tampered,
        orders: Arc::clone(orders),
        applied: applied.clone(),
    };
    let toward_client = Direction {
        toward: Toward::Client,
        tampered,
        orders: Arc::clone(orders),
        applied: applied.clone(),
    };
    tokio::select! {
        () = toward_node.forward(down_rx, up_tx) => {}
        () = toward_client.forward(up_rx, down_tx) => {}
    }
}

/// One direction of one proxied connection.
struct Direction {
    toward: Toward,
    tampered: bool,
    orders: Arc<Mutex<Orders>>,
    applied: watch::Sender<u64>,
}

impl Direction {
    fn order(&self) -> Option<Tamper> {
        if !self.tampered {
            return None;
        }
        let order = self
            .orders
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next(self.toward);
        if order.is_some() {
            self.applied
                .send_modify(|count| *count = count.saturating_add(1));
        }
        order
    }

    async fn forward<S, T>(self, mut from: SplitStream<S>, mut to: SplitSink<T, Message>)
    where
        S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>,
        T: futures_util::Sink<Message>,
    {
        let mut held: Option<Message> = None;
        while let Some(Ok(message)) = from.next().await {
            let closing = matches!(message, Message::Close(_));
            let order = if matches!(message, Message::Binary(_)) {
                self.order()
            } else {
                None
            };
            let mut out = Vec::new();
            match (order, message) {
                (Some(Tamper::Drop), _message) => {}
                (Some(Tamper::Replay), message) => {
                    out.push(message.clone());
                    out.push(message);
                }
                (Some(Tamper::Reorder), message) => held = Some(message),
                (Some(Tamper::Corrupt), Message::Binary(record)) => {
                    let mut bytes = record.to_vec();
                    let middle = bytes.len() >> 1;
                    if let Some(byte) = bytes.get_mut(middle) {
                        *byte ^= 1;
                    }
                    out.push(Message::Binary(bytes.into()));
                }
                (Some(Tamper::Delay(pause)), message) => {
                    tokio::time::sleep(pause).await;
                    out.push(message);
                }
                (Some(Tamper::Corrupt) | None, message) => {
                    out.push(message);
                    if let Some(earlier) = held.take() {
                        out.push(earlier);
                    }
                }
            }
            for message in out {
                if to.send(message).await.is_err() {
                    return;
                }
            }
            if closing {
                return;
            }
        }
        let _closed = to.close().await;
    }
}
