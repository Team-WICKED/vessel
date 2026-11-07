//! Changing the frame limit on an open channel (`set_max_frame`): the node
//! admits a client under 64 KiB and raises the limit after the preamble.

use futures_util::{FutureExt as _, SinkExt as _, StreamExt as _};

use super::support::{Channel, assert_closed, connected, connected_with};
use crate::record::{FINAL, FIRST, FIRST_DATA};
use crate::{Config, DEFAULT_MAX_FRAME, SecureError};

const ADMISSION: u32 = 64 * 1024;

fn admission() -> Config {
    Config {
        max_frame: ADMISSION,
        ..Config::default()
    }
}

/// Seal `chunk` with the client's keys and hand it straight to the node.
async fn send_chunk(client: &mut Channel, chunk: &[u8]) {
    let record = client.seal_raw(chunk).unwrap();
    client.carrier_mut().send(record).await.unwrap();
}

fn first_chunk(total: usize, data: usize) -> Vec<u8> {
    let mut chunk = vec![FIRST];
    chunk.extend_from_slice(&u32::try_from(total).unwrap().to_be_bytes());
    chunk.resize(5 + data, b'x');
    chunk
}

#[tokio::test]
async fn a_small_limit_refuses_the_next_announcement_alone() {
    let (mut client, mut node) = connected_with(&Config::default(), &admission()).await;
    assert_eq!(node.max_frame(), ADMISSION);
    let fits = "p".repeat(usize::try_from(ADMISSION).unwrap());
    let (sent, received) = tokio::join!(client.send(fits.clone()), node.next());
    sent.unwrap();
    assert!(received.unwrap().unwrap() == fits);

    // Only the first record of a frame one byte over: refused from its
    // announced length, nothing buffered.
    send_chunk(&mut client, &first_chunk(65_537, FIRST_DATA)).await;
    match node.next().await {
        Some(Err(SecureError::TooLarge { len, max })) => {
            assert_eq!((len, max), (65_537, ADMISSION));
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
    assert_eq!(node.reassembly_capacity(), 0);
    assert_closed(&mut node).await;
}

#[tokio::test]
async fn a_raised_limit_covers_a_frame_already_pipelined() {
    let (mut client, mut node) = connected_with(&Config::default(), &admission()).await;
    // The client does not wait: its preamble and a 1 MiB frame are both in
    // flight before the node has read either.
    let big = "b".repeat(1024 * 1024);
    client.send("preamble".to_owned()).await.unwrap();
    client.send(big.clone()).await.unwrap();

    assert_eq!(node.next().await.unwrap().unwrap(), "preamble");
    node.set_max_frame(DEFAULT_MAX_FRAME);
    assert_eq!(node.max_frame(), DEFAULT_MAX_FRAME);
    assert!(node.next().await.unwrap().unwrap() == big);
}

#[tokio::test]
async fn a_frame_announced_before_a_lower_limit_finishes() {
    let (mut client, mut node) = connected().await;
    let total = FIRST_DATA + 10;
    send_chunk(&mut client, &first_chunk(total, FIRST_DATA)).await;
    // The node has taken the first chunk and waits for the rest.
    assert!(node.next().now_or_never().is_none());
    assert_eq!(node.reassembly_capacity(), FIRST_DATA);

    node.set_max_frame(1000);
    let mut last = vec![FINAL];
    last.resize(11, b'y');
    send_chunk(&mut client, &last).await;
    let frame = node.next().await.unwrap().unwrap();
    assert_eq!(frame.len(), total);
    assert!(frame.ends_with("yyyyyyyyyy"));

    // The lower limit holds from the next announcement on.
    client.send("z".repeat(1001)).await.unwrap();
    assert!(matches!(
        node.next().await,
        Some(Err(SecureError::TooLarge {
            len: 1001,
            max: 1000
        }))
    ));
}

#[tokio::test]
async fn the_sending_side_follows_the_limit_from_its_next_frame() {
    let (mut client, mut node) = connected().await;

    // A frame taken before the limit drops is sent whole.
    let big = "s".repeat(3 * FIRST_DATA);
    client.feed(big.clone()).await.unwrap();
    client.set_max_frame(10);
    let (flushed, received) = tokio::join!(client.flush(), node.next());
    flushed.unwrap();
    assert!(received.unwrap().unwrap() == big);

    // The next one is held to it, and the channel stays open.
    assert!(matches!(
        client.send("eleven char".to_owned()).await,
        Err(SecureError::TooLarge { len: 11, max: 10 })
    ));
    client.send("ten chars!".to_owned()).await.unwrap();
    assert_eq!(node.next().await.unwrap().unwrap(), "ten chars!");
}
