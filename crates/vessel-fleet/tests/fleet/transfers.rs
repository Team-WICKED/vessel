//! Transfers (protocol 6): snapshot a set of files into a staging store,
//! place a received set into another home.

use std::fs;
use std::path::{Path, PathBuf};

use vessel_fleet::{
    BlobStore, Incoming, Limits, OnConflict, PlaceError, Placed, SnapshotError, place, snapshot,
};
use vessel_proto::{BundleFile, BundlePath};

use crate::support::{read, write};

fn paths(list: &[&str]) -> Vec<PathBuf> {
    list.iter().map(PathBuf::from).collect()
}

fn bundle(text: &str) -> BundlePath {
    text.parse().unwrap()
}

/// `files` put in `store` and aimed at their own paths.
fn incoming(store: &BlobStore, files: &[(&str, &str)]) -> Vec<Incoming> {
    files
        .iter()
        .map(|(path, text)| Incoming {
            bundle: bundle(path),
            place: PathBuf::from(path),
            file: BundleFile {
                hash: store.insert(text.as_bytes()).unwrap(),
                size: u64::try_from(text.len()).unwrap(),
                executable: false,
            },
        })
        .collect()
}

#[test]
fn a_snapshot_is_fixed_when_taken_and_follows_no_link() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    write(&home.join("projects/-a/s.jsonl"), "line 1\n");
    write(&home.join("projects/-a/s/sub/x.json"), "{}\n");
    write(&home.join("file-history/s/1"), "v1\n");
    let store = BlobStore::open(temp.path().join("staging")).unwrap();
    let mut listed = paths(&[
        "projects/-a/s.jsonl",
        "projects/-a/s/sub/x.json",
        "file-history/s/1",
        "projects/-a/gone.jsonl",
    ]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let outside = temp.path().join("outside");
        write(&outside.join("secret"), "not for export\n");
        symlink(outside.join("secret"), home.join("projects/-a/link.jsonl")).unwrap();
        symlink(&outside, home.join("projects/-b")).unwrap();
        let fifo = std::process::Command::new("mkfifo")
            .arg(home.join("projects/-a/fifo"))
            .status()
            .unwrap();
        assert!(fifo.success());
        fs::set_permissions(
            home.join("file-history/s/1"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        listed.extend(paths(&[
            "projects/-a/link.jsonl",
            "projects/-b/secret",
            "projects/-a/fifo",
        ]));
    }

    let taken = snapshot(&home, &listed, &store, Limits::PROTOCOL).unwrap();
    assert_eq!(
        taken
            .files
            .keys()
            .map(BundlePath::as_str)
            .collect::<Vec<_>>(),
        [
            "file-history/s/1",
            "projects/-a/s.jsonl",
            "projects/-a/s/sub/x.json"
        ]
    );
    let mut skipped = taken.skipped.clone();
    skipped.sort();
    #[cfg(unix)]
    assert_eq!(
        skipped,
        paths(&[
            "projects/-a/fifo",
            "projects/-a/gone.jsonl",
            "projects/-a/link.jsonl",
            "projects/-b/secret",
        ])
    );
    #[cfg(unix)]
    assert!(taken.files[&bundle("file-history/s/1")].executable);

    // The harness goes on appending; the snapshot keeps what it copied.
    write(&home.join("projects/-a/s.jsonl"), "line 1\nline 2\n");
    let transcript = &taken.files[&bundle("projects/-a/s.jsonl")];
    let (data, total) = store.read_chunk(&transcript.hash, 0, 1024).unwrap();
    assert_eq!((data.as_slice(), total), (&b"line 1\n"[..], 7));
}

#[test]
fn paths_that_cannot_travel_are_refused_by_name() {
    let temp = tempfile::tempdir().unwrap();
    let store = BlobStore::open(temp.path().join("staging")).unwrap();
    for (path, has_rule) in [("projects/CON/x", true), ("../x", false), ("/abs", false)] {
        let error = snapshot(temp.path(), &paths(&[path]), &store, Limits::PROTOCOL).unwrap_err();
        assert!(
            matches!(&error, SnapshotError::Path { reason, .. } if reason.is_some() == has_rule),
            "{path}: {error:?}"
        );
        assert!(!error.is_limit());
    }
}

#[test]
fn limits_name_the_limit_and_the_size() {
    let temp = tempfile::tempdir().unwrap();
    write(&temp.path().join("big"), "0123456789");
    write(&temp.path().join("a"), "123456");
    write(&temp.path().join("b"), "654321");
    let store = BlobStore::open(temp.path().join("staging")).unwrap();

    let one = Limits {
        file: 9,
        total: 100,
    };
    let error = snapshot(temp.path(), &paths(&["big"]), &store, one).unwrap_err();
    assert!(
        matches!(
            &error,
            SnapshotError::FileTooLarge {
                size: 10,
                limit: 9,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(error.is_limit());
    let message = error.to_string();
    assert!(
        message.contains("10 bytes") && message.contains("9 bytes"),
        "{message}"
    );

    let total = Limits {
        file: 10,
        total: 11,
    };
    let error = snapshot(temp.path(), &paths(&["a", "b"]), &store, total).unwrap_err();
    assert!(
        matches!(
            &error,
            SnapshotError::TotalTooLarge {
                size: 12,
                limit: 11
            }
        ),
        "{error:?}"
    );
    assert!(error.is_limit());
}

#[test]
fn a_session_is_placed_whole_or_not_at_all() {
    let temp = tempfile::tempdir().unwrap();
    let store = BlobStore::open(temp.path().join("staging")).unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let set = incoming(
        &store,
        &[
            ("projects/-b/s.jsonl", "conversation\n"),
            ("file-history/s/1", "v1\n"),
        ],
    );

    let placed = place(&home, &set, &store, OnConflict::Refuse).unwrap();
    assert_eq!(
        placed,
        Placed {
            written: 2,
            same: 0,
            beside: Vec::new()
        }
    );
    assert_eq!(read(&home.join("projects/-b/s.jsonl")), "conversation\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&home.join("projects/-b/s.jsonl")), 0o600);
        assert_eq!(mode(&home.join("projects/-b")), 0o700);
    }
    // The same set again: nothing to write.
    let again = place(&home, &set, &store, OnConflict::Refuse).unwrap();
    assert_eq!((again.written, again.same), (0, 2));

    // A different file in the way: nothing at all is written.
    write(&home.join("file-history/s/1"), "edited here\n");
    let mut more = incoming(&store, &[("projects/-b/new.jsonl", "new\n")]);
    more.extend(set);
    let error = place(&home, &more, &store, OnConflict::Refuse).unwrap_err();
    assert!(
        matches!(&error, PlaceError::Conflict { path } if path.ends_with("1")),
        "{error:?}"
    );
    assert!(!home.join("projects/-b/new.jsonl").exists());
    assert_eq!(read(&home.join("file-history/s/1")), "edited here\n");

    // A home that is not there is not made.
    let error = place(&temp.path().join("none"), &more, &store, OnConflict::Refuse).unwrap_err();
    assert!(matches!(error, PlaceError::Io { .. }), "{error:?}");
    assert!(!temp.path().join("none").exists());
}

#[test]
fn memory_is_written_beside_and_never_through_a_link() {
    let temp = tempfile::tempdir().unwrap();
    let store = BlobStore::open(temp.path().join("staging")).unwrap();
    let home = temp.path().join("home");
    write(&home.join("projects/-b/memory/MEMORY.md"), "mine\n");
    let set = incoming(
        &store,
        &[
            ("projects/-b/memory/MEMORY.md", "theirs\n"),
            ("projects/-b/memory/new.md", "new\n"),
        ],
    );

    let placed = place(&home, &set, &store, OnConflict::Beside).unwrap();
    assert_eq!(
        placed,
        Placed {
            written: 1,
            same: 0,
            beside: vec![bundle("projects/-b/memory/MEMORY.md")]
        }
    );
    assert_eq!(read(&home.join("projects/-b/memory/MEMORY.md")), "mine\n");
    assert_eq!(
        read(&home.join("projects/-b/memory/MEMORY.md.vessel-incoming")),
        "theirs\n"
    );
    // Again: the incoming version is there already; reported, not rewritten.
    let again = place(&home, &set, &store, OnConflict::Beside).unwrap();
    assert_eq!((again.written, again.same, again.beside.len()), (0, 1, 1));

    #[cfg(unix)]
    {
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, home.join("projects/-c")).unwrap();
        let linked = incoming(&store, &[("projects/-c/memory/a.md", "a\n")]);
        for on_conflict in [OnConflict::Beside, OnConflict::Refuse] {
            let error = place(&home, &linked, &store, on_conflict).unwrap_err();
            assert!(matches!(error, PlaceError::Blocked { .. }), "{error:?}");
        }
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

    // A damaged blob is never written, and leaves the store.
    let damaged = incoming(&store, &[("projects/-b/memory/d.md", "right\n")]);
    fs::write(store.path(&damaged[0].file.hash), "wrong\n").unwrap();
    let error = place(&home, &damaged, &store, OnConflict::Beside).unwrap_err();
    assert!(matches!(error, PlaceError::DamagedBlob { .. }), "{error:?}");
    assert!(!home.join("projects/-b/memory/d.md").exists());
    assert!(!store.contains(&damaged[0].file.hash));
}
