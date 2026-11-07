//! A relay that tampers with records ends the channel, and nothing from a
//! tampered record reaches the reader: drop, replay, reorder and corrupt,
//! both ways, through `vessel-testkit`'s misbehaving relay in front of a
//! real one. A delay is not tampering and changes nothing.

use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use vessel_relay::{LinkState, WsCarrier};
use vessel_secure::{SecureChannel, SecureError};
use vessel_testkit::relay::{MisbehavingRelay, Tamper, Toward};

use crate::support::{EchoNode, Relay, channel, within};

const TAMPERING: [Tamper; 4] = [
    Tamper::Drop,
    Tamper::Replay,
    Tamper::Reorder,
    Tamper::Corrupt,
];

/// A real relay, the misbehaving one in front of it, an echoing node, and
/// a client's channel to it, all through the misbehaving relay.
struct Setup {
    _relay: Relay,
    proxy: MisbehavingRelay,
    _node: EchoNode,
    client: SecureChannel<WsCarrier>,
}

async fn setup() -> Setup {
    let relay = Relay::start().await;
    let proxy = MisbehavingRelay::start(relay.url.as_str()).await.unwrap();
    let url = proxy.url().parse().unwrap();
    let mut node = EchoNode::start(&url);
    node.wait_for(LinkState::Registered).await;
    let client = channel(&url, &node).await;
    Setup {
        _relay: relay,
        proxy,
        _node: node,
        client,
    }
}

/// Everything the client reads until its channel ends.
async fn read_to_end(client: &mut SecureChannel<WsCarrier>) -> (Vec<String>, Option<SecureError>) {
    let mut frames = Vec::new();
    loop {
        match within(client.next()).await {
            Some(Ok(frame)) => frames.push(frame),
            Some(Err(error)) => {
                assert!(
                    within(client.next()).await.is_none(),
                    "the channel goes on after {error}"
                );
                return (frames, Some(error));
            }
            None => return (frames, None),
        }
    }
}

#[tokio::test]
async fn tampering_toward_the_client_closes_the_clients_channel() {
    for tamper in TAMPERING {
        let mut setup = setup().await;
        setup.proxy.tamper(Toward::Client, tamper);
        within(setup.client.send("first".to_owned())).await.unwrap();
        within(setup.client.send("second".to_owned()))
            .await
            .unwrap();
        let (frames, error) = read_to_end(&mut setup.client).await;
        // Only an untouched prefix may arrive (the replay's original).
        assert!(
            ["first", "second"]
                .starts_with(&frames.iter().map(String::as_str).collect::<Vec<_>>()[..]),
            "{tamper:?}: {frames:?}"
        );
        assert!(frames.len() < 2, "{tamper:?}: everything arrived");
        assert!(
            matches!(error, Some(SecureError::Tampered(_))),
            "{tamper:?}: {error:?}"
        );
    }
}

#[tokio::test]
async fn tampering_toward_the_node_ends_the_connection() {
    for tamper in TAMPERING {
        let mut setup = setup().await;
        setup.proxy.tamper(Toward::Node, tamper);
        within(setup.client.send("first".to_owned())).await.unwrap();
        within(setup.client.send("second".to_owned()))
            .await
            .unwrap();
        // The node's channel fails at the tampered record and it hangs up:
        // the client sees the end, and never an echo of "second".
        let (frames, _end) = read_to_end(&mut setup.client).await;
        assert!(
            !frames.iter().any(|frame| frame == "second"),
            "{tamper:?}: {frames:?}"
        );
        assert!(frames.len() <= 1, "{tamper:?}: {frames:?}");
    }
}

#[tokio::test]
async fn a_delay_is_not_tampering() {
    let mut setup = setup().await;
    setup
        .proxy
        .tamper(Toward::Node, Tamper::Delay(Duration::from_millis(100)));
    setup
        .proxy
        .tamper(Toward::Client, Tamper::Delay(Duration::from_millis(100)));
    for frame in ["late", "on time"] {
        within(setup.client.send(frame.to_owned())).await.unwrap();
        assert_eq!(within(setup.client.next()).await.unwrap().unwrap(), frame);
    }
    within(setup.proxy.applied(2)).await;
}
