//! The carriers this crate provides: a byte stream with `u16` lengths, and
//! memory.

use std::io;

use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

use super::support::carriers;
use crate::record::FIRST_DATA;
use crate::{LengthPrefixed, MAX_RECORD, NodeKeypair, initiate, respond};

type Stream = LengthPrefixed<ReadHalf<DuplexStream>, WriteHalf<DuplexStream>>;

fn byte_stream_pair() -> (Stream, Stream) {
    let (one, other) = tokio::io::duplex(4096);
    let (one_read, one_write) = tokio::io::split(one);
    let (other_read, other_write) = tokio::io::split(other);
    (
        LengthPrefixed::new(one_read, one_write),
        LengthPrefixed::new(other_read, other_write),
    )
}

#[tokio::test]
async fn a_channel_runs_over_a_length_prefixed_byte_stream() {
    let keypair = NodeKeypair::generate().unwrap();
    let (client_end, node_end) = byte_stream_pair();
    let (client, node) = tokio::join!(
        initiate(client_end, keypair.public()),
        respond(node_end, &keypair)
    );
    let (mut client, mut node) = (client.unwrap(), node.unwrap());
    // Three records, through a pipe much smaller than one.
    let long = "z".repeat(2 * FIRST_DATA + 7);
    let (sent, received) = tokio::join!(client.send(long.clone()), node.next());
    sent.unwrap();
    assert!(received.unwrap().unwrap() == long);
}

#[tokio::test]
async fn a_length_prefixed_stream_carries_messages_up_to_one_record() {
    let (mut one, mut other) = byte_stream_pair();
    let largest = Bytes::from(vec![9_u8; MAX_RECORD]);
    let (sent, received) = tokio::join!(one.send(largest.clone()), other.next());
    sent.unwrap();
    assert_eq!(received.unwrap().unwrap(), largest);

    let refused = one.send(Bytes::from(vec![0_u8; MAX_RECORD + 1])).await;
    assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::InvalidInput);
}

#[tokio::test]
async fn a_memory_carrier_reports_the_other_end_gone() {
    let (mut one, other) = carriers();
    drop(other);
    let sent = one.send(Bytes::from_static(b"x")).await;
    assert_eq!(sent.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    assert!(one.next().await.is_none());
}
