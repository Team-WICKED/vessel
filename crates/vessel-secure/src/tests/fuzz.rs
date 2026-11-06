//! Random input for the record parser and the channel: no panic, no frame
//! from anything malformed, and never more memory than the announced
//! frame.
//!
//! A small deterministic generator (xorshift) instead of a fuzzing crate:
//! the run is the same on every machine and in CI, and a failure names its
//! seed and round.

use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};

use super::support::connected;
use crate::record::{FINAL, FIRST, FIRST_DATA, MAX_CHUNK, NEXT_DATA, Reassembly};
use crate::{SecureError, Tamper};

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
    }

    fn chance(&mut self, in_hundred: usize) -> bool {
        self.below(100) < in_hundred
    }
}

/// The canonical chunks of a frame of `total` bytes.
fn canonical(total: usize) -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    let mut sent = 0;
    loop {
        let room = if chunks.is_empty() {
            FIRST_DATA
        } else {
            NEXT_DATA
        };
        let take = (total - sent).min(room);
        let mut flags = if chunks.is_empty() { FIRST } else { 0 };
        if sent + take == total {
            flags |= FINAL;
        }
        let mut chunk = vec![flags];
        if chunks.is_empty() {
            chunk.extend_from_slice(&u32::try_from(total).unwrap().to_be_bytes());
        }
        chunk.resize(chunk.len() + take, 0x5a);
        chunks.push(chunk);
        sent += take;
        if flags & FINAL != 0 {
            return chunks;
        }
    }
}

/// One random change to a chunk sequence.
fn mutate(random: &mut Random, chunks: &mut Vec<Vec<u8>>) {
    let index = random.below(chunks.len());
    match random.below(6) {
        0 => {
            let chunk = &mut chunks[index];
            if !chunk.is_empty() {
                let at = random.below(chunk.len());
                chunk[at] ^= 1 << random.below(8);
            }
        }
        1 => {
            let chunk = &mut chunks[index];
            let keep = random.below(chunk.len() + 1);
            chunk.truncate(keep);
        }
        2 => {
            let extra = random.below(40);
            let chunk = &mut chunks[index];
            chunk.resize(chunk.len() + extra, 0x33);
        }
        3 => {
            let _removed = chunks.remove(index);
        }
        4 => {
            let copy = chunks[index].clone();
            chunks.insert(random.below(chunks.len() + 1), copy);
        }
        _ => {
            if let Some(flags) = chunks[index].first_mut() {
                *flags = u8::try_from(random.below(256)).unwrap();
            }
        }
    }
}

#[test]
fn the_parser_survives_random_chunks_and_never_overallocates() {
    let max_frame: u32 = 300_000;
    let mut random = Random(0x9e37_79b9_7f4a_7c15);
    for round in 0..3_000 {
        let total = match random.below(4) {
            0 => random.below(64),
            1 => random.below(FIRST_DATA + 2),
            _ => random.below(usize::try_from(max_frame).unwrap() + 10),
        };
        let mut chunks = canonical(total);
        let mutated = random.chance(70);
        if mutated {
            for _ in 0..=random.below(3) {
                if chunks.is_empty() {
                    break;
                }
                mutate(&mut random, &mut chunks);
            }
        }
        let mut parser = Reassembly::new(max_frame);
        let mut frames = Vec::new();
        let mut failed = false;
        for chunk in &chunks {
            match parser.push(chunk) {
                Ok(Some(frame)) => frames.push(frame),
                Ok(None) => {}
                Err(SecureError::Tampered(_) | SecureError::TooLarge { .. }) => {
                    failed = true;
                    break;
                }
                Err(other) => panic!("round {round}: unexpected {other:?}"),
            }
            // Never more than the announced frame, never more than the
            // limit, whatever the chunks said.
            assert!(
                parser.capacity() <= usize::try_from(max_frame).unwrap(),
                "round {round}"
            );
        }
        let max = usize::try_from(max_frame).unwrap();
        if !mutated {
            if total > max {
                assert!(failed, "round {round}");
            } else {
                assert!(!failed && !parser.in_frame(), "round {round}");
                assert_eq!(frames.len(), 1, "round {round}");
                assert_eq!(frames[0].len(), total, "round {round}");
                assert!(frames[0].iter().all(|&byte| byte == 0x5a), "round {round}");
            }
        }
        for frame in &frames {
            assert!(frame.len() <= max, "round {round}");
        }
    }
}

#[test]
fn announced_lengths_bound_memory_before_any_data() {
    let mut random = Random(42);
    for _ in 0..1_000 {
        let announced = if random.chance(50) {
            u32::try_from(random.below(usize::try_from(crate::DEFAULT_MAX_FRAME).unwrap() + 1))
                .unwrap()
        } else {
            u32::try_from(random.next() >> 32).unwrap()
        };
        let mut parser = Reassembly::new(crate::DEFAULT_MAX_FRAME);
        let mut chunk = vec![FIRST];
        chunk.extend_from_slice(&announced.to_be_bytes());
        chunk.resize(5 + FIRST_DATA, 0);
        match parser.push(&chunk) {
            Ok(_) => {
                assert!(announced <= crate::DEFAULT_MAX_FRAME);
                assert!(parser.capacity() <= usize::try_from(announced).unwrap());
                assert!(parser.capacity() <= MAX_CHUNK);
            }
            Err(SecureError::TooLarge { len, .. }) => {
                assert_eq!(len, u64::from(announced));
                assert!(announced > crate::DEFAULT_MAX_FRAME);
                assert_eq!(parser.capacity(), 0);
            }
            Err(SecureError::Tampered(_)) => {
                // Shorter than one full chunk but not final, or the like.
                assert!(usize::try_from(announced).unwrap() <= FIRST_DATA);
            }
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn random_records_never_authenticate() {
    let mut random = Random(7);
    for _ in 0..200 {
        let (mut client, mut node) = connected().await;
        let len = match random.below(3) {
            0 => random.below(64),
            1 => random.below(70_000),
            _ => 48,
        };
        let record: Vec<u8> = (0..len)
            .map(|_| u8::try_from(random.below(256)).unwrap())
            .collect();
        client
            .carrier_mut()
            .send(Bytes::from(record))
            .await
            .unwrap();
        let rule = match node.next().await {
            Some(Err(SecureError::Tampered(rule))) => rule,
            other => panic!("{len} random bytes: {other:?}"),
        };
        if len > crate::MAX_RECORD {
            assert_eq!(rule, Tamper::RecordTooLong);
        } else {
            assert_eq!(rule, Tamper::Authentication);
        }
    }
}
