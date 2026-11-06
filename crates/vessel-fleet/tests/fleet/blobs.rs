//! The blob store: content addressed, chunked reads, checked writes.

use std::collections::BTreeSet;
use std::io::{ErrorKind, Write as _};

use vessel_fleet::{BlobStore, InvalidBlob};

#[test]
fn blobs_are_stored_once_read_in_chunks_and_checked_on_arrival() {
    let temp = tempfile::tempdir().unwrap();
    let store = BlobStore::open(temp.path().join("blobs")).unwrap();
    let content: Vec<u8> = (0..3000_u32).flat_map(u32::to_le_bytes).collect();
    let hash = store.insert(&content).unwrap();
    assert_eq!(
        store.insert(&content).unwrap(),
        hash,
        "same content, same blob"
    );
    assert!(store.contains(&hash));

    let (first, total) = store.read_chunk(&hash, 0, 5000).unwrap();
    let (second, _) = store.read_chunk(&hash, 5000, 5000).unwrap();
    let (last, _) = store.read_chunk(&hash, 10_000, 5000).unwrap();
    assert_eq!(total, 12_000);
    assert_eq!(last.len(), 2000, "the last chunk is what is left");
    assert_eq!([first, second, last].concat(), content);
    let (empty, _) = store.read_chunk(&hash, total, 10).unwrap();
    assert!(empty.is_empty());
    assert_eq!(
        store.read_chunk(&hash, total + 1, 10).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );

    // A member receiving it: right content passes, wrong content never
    // appears in the store.
    let other = BlobStore::open(temp.path().join("member")).unwrap();
    let mut writer = other.writer(hash).unwrap();
    writer.write_all(&content).unwrap();
    assert_eq!(writer.finish().unwrap().0, hash);
    let wrong = BlobStore::open(temp.path().join("wrong")).unwrap();
    let mut writer = wrong.writer(hash).unwrap();
    writer.write_all(b"not it").unwrap();
    let error = writer.finish().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert!(
        error
            .get_ref()
            .unwrap()
            .downcast_ref::<InvalidBlob>()
            .is_some()
    );
    assert!(!wrong.contains(&hash));

    // Clean-up keeps what is still published.
    let kept = store.insert(b"kept").unwrap();
    assert_eq!(store.retain(&BTreeSet::from([kept])).unwrap(), 1);
    assert!(store.contains(&kept) && !store.contains(&hash));
}

#[test]
fn clean_up_leaves_a_blob_being_written_alone() {
    let temp = tempfile::tempdir().unwrap();
    let store = BlobStore::open(temp.path().join("blobs")).unwrap();
    let kept = store.insert(b"kept").unwrap();
    let content = b"arriving in chunks";
    let other = BlobStore::open(temp.path().join("other")).unwrap();
    let hash = other.insert(content).unwrap();

    let mut writer = store.writer(hash).unwrap();
    writer.write_all(&content[..8]).unwrap();
    assert_eq!(store.retain(&BTreeSet::from([kept])).unwrap(), 0);
    writer.write_all(&content[8..]).unwrap();
    assert_eq!(writer.finish().unwrap().0, hash, "its file is still there");
    assert!(store.contains(&hash) && store.contains(&kept));
}

#[test]
fn a_read_past_the_end_says_so() {
    let temp = tempfile::tempdir().unwrap();
    let store = BlobStore::open(temp.path().join("blobs")).unwrap();
    let hash = store.insert(b"four").unwrap();
    let error = store.read_chunk(&hash, 5, 10).unwrap_err();
    assert_eq!(
        error
            .get_ref()
            .unwrap()
            .downcast_ref::<vessel_fleet::PastEnd>(),
        Some(&vessel_fleet::PastEnd {
            offset: 5,
            total: 4
        })
    );
}
