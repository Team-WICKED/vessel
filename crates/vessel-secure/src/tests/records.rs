//! Frames of every size round-trip, the limit holds on both sides, and the
//! channel ends cleanly only between frames.

use futures_util::{FutureExt as _, SinkExt as _, StreamExt as _};

use super::support::{connected, connected_with, through_middle};
use crate::record::{FINAL, FIRST, FIRST_DATA, NEXT_DATA};
use crate::{Config, DEFAULT_MAX_FRAME, MAX_RECORD, SecureError};

/// A frame of `len` bytes whose content shows where each byte belongs.
fn frame(len: usize) -> String {
    (0..len)
        .map(|at| char::from(b'a' + u8::try_from(at % 26).unwrap()))
        .collect()
}

/// Send `len` bytes through a tap and count the records it took.
async fn records_for(len: usize) -> usize {
    let (mut client, mut node, mut middle) = through_middle().await;
    let sent = frame(len);
    client.send(sent.clone()).await.unwrap();
    let mut records = 0;
    loop {
        let record = middle.client_record().await;
        assert!(record.len() <= MAX_RECORD);
        middle.pass_to_node(record).await;
        records += 1;
        if let Some(received) = node.next().now_or_never() {
            assert_eq!(received.unwrap().unwrap(), sent);
            return records;
        }
    }
}

#[tokio::test]
async fn frames_take_as_few_records_as_fit() {
    assert_eq!(records_for(0).await, 1);
    assert_eq!(records_for(1).await, 1);
    assert_eq!(records_for(FIRST_DATA).await, 1);
    assert_eq!(records_for(FIRST_DATA + 1).await, 2);
    assert_eq!(records_for(FIRST_DATA + NEXT_DATA).await, 2);
    assert_eq!(records_for(FIRST_DATA + NEXT_DATA + 1).await, 3);
}

#[tokio::test]
async fn a_frame_of_exactly_one_record_fills_it() {
    let (mut client, _node, mut middle) = through_middle().await;
    client.send(frame(FIRST_DATA)).await.unwrap();
    assert_eq!(middle.client_record().await.len(), MAX_RECORD);
}

#[tokio::test]
async fn a_frame_of_the_full_limit_round_trips() {
    let (mut client, mut node) = connected().await;
    let sent = frame(usize::try_from(DEFAULT_MAX_FRAME).unwrap());
    let (sending, received) = tokio::join!(client.send(sent.clone()), node.next());
    sending.unwrap();
    assert!(received.unwrap().unwrap() == sent);
}

#[tokio::test]
async fn a_frame_over_the_limit_is_refused_before_sending() {
    let (mut client, mut node) = connected().await;
    let limit = usize::try_from(DEFAULT_MAX_FRAME).unwrap();
    let refused = client.send(frame(limit + 1)).await;
    assert!(
        matches!(
            refused,
            Err(SecureError::TooLarge { len, max: DEFAULT_MAX_FRAME }) if len == u64::try_from(limit + 1).unwrap()
        ),
        "{refused:?}"
    );
    // Nothing was sealed, so the channel goes on.
    client.send("next".to_owned()).await.unwrap();
    assert_eq!(node.next().await.unwrap().unwrap(), "next");
}

#[tokio::test]
async fn the_receiver_refuses_an_announced_length_over_its_limit() {
    // The sender allows more than the receiver does.
    let sender = Config {
        max_frame: DEFAULT_MAX_FRAME + 1,
        ..Config::default()
    };
    let (mut client, mut node) = connected_with(&sender, &Config::default()).await;

    // Only the first record is ever sent: the length alone is refused,
    // before anything is buffered.
    let mut chunk = vec![FIRST];
    chunk.extend_from_slice(&(DEFAULT_MAX_FRAME + 1).to_be_bytes());
    chunk.resize(5 + FIRST_DATA, b'x');
    let record = client.seal_raw(&chunk).unwrap();
    client.carrier_mut().send(record).await.unwrap();
    match node.next().await {
        Some(Err(SecureError::TooLarge { len, max })) => {
            assert_eq!(len, u64::from(DEFAULT_MAX_FRAME) + 1);
            assert_eq!(max, DEFAULT_MAX_FRAME);
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
    assert_eq!(node.reassembly_capacity(), 0);
}

#[tokio::test]
async fn memory_grows_with_the_data_not_the_announced_length() {
    let (mut client, mut node) = connected().await;
    let mut chunk = vec![FIRST];
    chunk.extend_from_slice(&DEFAULT_MAX_FRAME.to_be_bytes());
    chunk.resize(5 + FIRST_DATA, b'x');
    let record = client.seal_raw(&chunk).unwrap();
    client.carrier_mut().send(record).await.unwrap();
    // The record is waiting in memory, so one poll takes it.
    assert!(node.next().now_or_never().is_none());
    assert_eq!(node.reassembly_capacity(), FIRST_DATA);
}

#[tokio::test]
async fn frames_flow_both_ways_at_once() {
    let (mut client, mut node) = connected().await;
    let (mut client_tx, mut client_rx) = (&mut client).split();
    let (mut node_tx, mut node_rx) = (&mut node).split();
    let count = 200;
    let client_sends = async {
        for index in 0..count {
            client_tx.send(format!("c{index}")).await.unwrap();
        }
    };
    let node_sends = async {
        for index in 0..count {
            node_tx
                .send(frame(index * 997 % (2 * FIRST_DATA)))
                .await
                .unwrap();
        }
    };
    let client_reads = async {
        for index in 0..count {
            let received = client_rx.next().await.unwrap().unwrap();
            assert_eq!(received, frame(index * 997 % (2 * FIRST_DATA)));
        }
    };
    let node_reads = async {
        for index in 0..count {
            assert_eq!(node_rx.next().await.unwrap().unwrap(), format!("c{index}"));
        }
    };
    tokio::join!(client_sends, node_sends, client_reads, node_reads);
}

#[tokio::test]
async fn a_carrier_ending_between_frames_ends_the_stream() {
    let (mut client, mut node) = connected().await;
    client.send("last".to_owned()).await.unwrap();
    drop(client);
    assert_eq!(node.next().await.unwrap().unwrap(), "last");
    assert!(node.next().await.is_none());
}

#[tokio::test]
async fn a_carrier_ending_inside_a_frame_is_an_error() {
    let (mut client, mut node) = connected().await;
    let mut chunk = vec![FIRST];
    chunk.extend_from_slice(&u32::try_from(FIRST_DATA + 10).unwrap().to_be_bytes());
    chunk.resize(5 + FIRST_DATA, b'x');
    let record = client.seal_raw(&chunk).unwrap();
    client.carrier_mut().send(record).await.unwrap();
    drop(client);
    match node.next().await {
        Some(Err(SecureError::Carrier(error))) => {
            assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        }
        other => panic!("expected the carrier to end mid-frame, got {other:?}"),
    }
    assert!(node.next().await.is_none());
}

#[tokio::test]
async fn the_channel_closes_when_the_nonce_counter_is_exhausted() {
    let (mut client, mut node) = connected().await;
    node.set_receiving_nonce(u64::MAX);
    client.send("one more".to_owned()).await.unwrap();
    assert!(matches!(
        node.next().await,
        Some(Err(SecureError::Exhausted))
    ));
    super::support::assert_closed(&mut node).await;
}

#[tokio::test]
async fn a_frame_that_is_not_utf8_closes_the_channel() {
    let (mut client, mut node) = connected().await;
    let mut chunk = vec![FIRST | FINAL];
    chunk.extend_from_slice(&2_u32.to_be_bytes());
    chunk.extend_from_slice(&[0xff, 0xfe]);
    let record = client.seal_raw(&chunk).unwrap();
    client.carrier_mut().send(record).await.unwrap();
    assert!(matches!(node.next().await, Some(Err(SecureError::NotUtf8))));
    super::support::assert_closed(&mut node).await;
}
