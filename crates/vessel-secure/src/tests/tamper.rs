//! Every kind of tampering closes the channel, and nothing from a tampered
//! record reaches the reader.
//!
//! Records changed on the way (by a relay, say) fail authentication. Records
//! that authenticate but break the chunk rules (a broken or hostile peer)
//! are sealed here with the client's own keys.

use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt as _, StreamExt as _};

use super::support::{Channel, assert_closed, connected, tamper, through_middle};
use crate::Tamper;
use crate::record::{FINAL, FIRST, FIRST_DATA, NEXT_DATA};

#[tokio::test]
async fn a_corrupted_record_fails_authentication() {
    // One bit flipped in the chunk header, the data, or the tag.
    for position in [0, 1, 2] {
        let (mut client, mut node, mut middle) = through_middle().await;
        client.send("secret".to_owned()).await.unwrap();
        let record = middle.client_record().await;
        let at = [0, record.len().div_ceil(2), record.len() - 1][position];
        let mut changed = BytesMut::from(&record[..]);
        changed[at] ^= 1;
        middle.pass_to_node(changed.freeze()).await;
        assert_eq!(tamper(node.next().await), Tamper::Authentication);
        assert_closed(&mut node).await;
    }
}

#[tokio::test]
async fn a_replayed_record_fails_authentication() {
    let (mut client, mut node, mut middle) = through_middle().await;
    client.send("once".to_owned()).await.unwrap();
    let record = middle.client_record().await;
    middle.pass_to_node(record.clone()).await;
    middle.pass_to_node(record).await;
    assert_eq!(node.next().await.unwrap().unwrap(), "once");
    assert_eq!(tamper(node.next().await), Tamper::Authentication);
    assert_closed(&mut node).await;
}

#[tokio::test]
async fn reordered_records_fail_authentication() {
    let (mut client, mut node, mut middle) = through_middle().await;
    client.send("first".to_owned()).await.unwrap();
    client.send("second".to_owned()).await.unwrap();
    let first = middle.client_record().await;
    let second = middle.client_record().await;
    middle.pass_to_node(second).await;
    middle.pass_to_node(first).await;
    assert_eq!(tamper(node.next().await), Tamper::Authentication);
    assert_closed(&mut node).await;
}

#[tokio::test]
async fn a_dropped_record_fails_the_next_one() {
    let (mut client, mut node, mut middle) = through_middle().await;
    client.send("lost".to_owned()).await.unwrap();
    client.send("after".to_owned()).await.unwrap();
    let _lost = middle.client_record().await;
    let after = middle.client_record().await;
    middle.pass_to_node(after).await;
    assert_eq!(tamper(node.next().await), Tamper::Authentication);
    assert_closed(&mut node).await;
}

#[tokio::test]
async fn a_dropped_chunk_inside_a_frame_delivers_nothing() {
    let (mut client, mut node, mut middle) = through_middle().await;
    let long = "x".repeat(FIRST_DATA + NEXT_DATA + 10);
    client.send(long).await.unwrap();
    let first = middle.client_record().await;
    let _second = middle.client_record().await;
    let third = middle.client_record().await;
    middle.pass_to_node(first).await;
    middle.pass_to_node(third).await;
    assert_eq!(tamper(node.next().await), Tamper::Authentication);
    assert_closed(&mut node).await;
}

#[tokio::test]
async fn truncated_and_oversized_records_are_refused() {
    let (mut client, mut node, mut middle) = through_middle().await;
    client.send("whole".to_owned()).await.unwrap();
    let record = middle.client_record().await;
    middle.pass_to_node(record.slice(..record.len() - 1)).await;
    assert_eq!(tamper(node.next().await), Tamper::Authentication);

    for record in [Bytes::new(), Bytes::from_static(&[0; 15])] {
        let (_client, mut node, mut middle) = through_middle().await;
        middle.pass_to_node(record).await;
        assert_eq!(tamper(node.next().await), Tamper::Authentication);
    }

    let (_client, mut node, mut middle) = through_middle().await;
    middle.pass_to_node(Bytes::from(vec![0; 65_536])).await;
    assert_eq!(tamper(node.next().await), Tamper::RecordTooLong);
    assert_closed(&mut node).await;
}

/// Seal each chunk with the client's keys and send it straight to the node,
/// then read the node's verdict.
async fn sealed(chunks: &[Vec<u8>]) -> (Tamper, Channel) {
    let (mut client, mut node) = connected().await;
    for chunk in chunks {
        let record = client.seal_raw(chunk).unwrap();
        client.carrier_mut().send(record).await.unwrap();
    }
    (tamper(node.next().await), node)
}

fn first(flags: u8, total: usize, data: usize) -> Vec<u8> {
    let mut chunk = vec![flags];
    chunk.extend_from_slice(&u32::try_from(total).unwrap().to_be_bytes());
    chunk.resize(5 + data, b'x');
    chunk
}

fn next(flags: u8, data: usize) -> Vec<u8> {
    let mut chunk = vec![flags];
    chunk.resize(1 + data, b'y');
    chunk
}

#[tokio::test]
async fn chunks_that_break_the_rules_close_the_channel() {
    let long = FIRST_DATA + NEXT_DATA + 5;
    let cases: Vec<(&str, Vec<Vec<u8>>, Tamper)> = vec![
        ("empty chunk", vec![vec![]], Tamper::Truncated),
        ("no length", vec![vec![FIRST, 0, 0]], Tamper::Truncated),
        (
            "reserved flag",
            vec![first(FIRST | FINAL | 4, 0, 0)],
            Tamper::ReservedFlags,
        ),
        (
            "high flag",
            vec![first(FIRST | 0x80, 0, 0)],
            Tamper::ReservedFlags,
        ),
        ("no FIRST", vec![next(FINAL, 3)], Tamper::MissingFirst),
        (
            "FIRST mid-frame",
            vec![first(FIRST, long, FIRST_DATA), first(FIRST | FINAL, 0, 0)],
            Tamper::UnexpectedFirst,
        ),
        ("overrun", vec![first(FIRST | FINAL, 3, 4)], Tamper::Overrun),
        (
            "overrun later",
            vec![first(FIRST, FIRST_DATA + 3, FIRST_DATA), next(FINAL, 4)],
            Tamper::Overrun,
        ),
        (
            "early FINAL",
            vec![first(FIRST | FINAL, 10, 5)],
            Tamper::EarlyFinal,
        ),
        (
            "missing FINAL",
            vec![first(FIRST, 5, 5)],
            Tamper::MissingFinal,
        ),
        (
            "missing FINAL later",
            vec![first(FIRST, FIRST_DATA + 3, FIRST_DATA), next(0, 3)],
            Tamper::MissingFinal,
        ),
        (
            "short first chunk",
            vec![first(FIRST, long, 10)],
            Tamper::ShortChunk,
        ),
        (
            "short later chunk",
            vec![first(FIRST, long, FIRST_DATA), next(0, 10)],
            Tamper::ShortChunk,
        ),
    ];
    for (name, chunks, expected) in cases {
        let (rule, mut node) = sealed(&chunks).await;
        assert_eq!(rule, expected, "{name}");
        assert_closed(&mut node).await;
    }
}

#[tokio::test]
async fn sealed_chunks_that_follow_the_rules_make_a_frame() {
    // The control for the cases above: the same helpers, used correctly.
    let (mut client, mut node) = connected().await;
    let total = FIRST_DATA + 3;
    for chunk in [first(FIRST, total, FIRST_DATA), next(FINAL, 3)] {
        let record = client.seal_raw(&chunk).unwrap();
        client.carrier_mut().send(record).await.unwrap();
    }
    let frame = node.next().await.unwrap().unwrap();
    assert_eq!(frame.len(), total);
    assert!(frame.ends_with("yyy"));
}
