//! The event log on a temporary data directory.

use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;

use fixtures::{at, data_dir, new_session, open, output, plain, record, status};
use vessel_proto::{
    AgentEvent, Creator, DeviceId, EventRecord, EventSeq, HarnessSessionId, NodeId, ProfileName,
    RemoteCreator, SessionEvent, SessionId, SessionInfo, SessionOrigin, SessionStatus,
};
use vessel_store::{ByteCount, Retention, Store, StoreConfig, StoreError};

/// Builders for sessions and records. A `cfg(test)` module so the lints'
/// test allowances (`unwrap` and friends) apply to it.
#[cfg(test)]
mod fixtures {
    use std::path::PathBuf;
    use std::sync::Arc;

    use bytes::Bytes;
    use chrono::{DateTime, Utc};
    use tempfile::TempDir;
    use vessel_proto::{
        EventRecord, EventSeq, HarnessKind, SessionEvent, SessionId, SessionInfo, SessionMode,
        SessionSpec, SessionStatus, TerminalBytes, TerminalOutput, TerminalSize,
    };
    use vessel_store::{DataDir, Store, StoreConfig};

    pub(crate) fn at(second: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000_i64.saturating_add(second), 123_456_789).unwrap()
    }

    pub(crate) fn data_dir() -> (TempDir, DataDir) {
        let temp = tempfile::tempdir().unwrap();
        let dir = DataDir::new(temp.path());
        dir.prepare().unwrap();
        (temp, dir)
    }

    pub(crate) fn new_session(created: i64) -> SessionInfo {
        SessionInfo::new(
            SessionId::random(),
            SessionSpec::new(
                HarnessKind::Codex,
                SessionMode::Terminal {
                    args: Vec::new(),
                    size: TerminalSize::default(),
                },
            )
            .with_cwd(PathBuf::from("/work")),
            at(created),
        )
    }

    pub(crate) fn record(seq: u64, event: SessionEvent) -> Arc<EventRecord> {
        Arc::new(EventRecord {
            seq: EventSeq::new(seq),
            at: at(i64::try_from(seq).unwrap()),
            event,
        })
    }

    pub(crate) fn status(seq: u64, status: SessionStatus) -> Arc<EventRecord> {
        record(seq, SessionEvent::Status { status })
    }

    /// A terminal chunk of `len` bytes (stored as about `4/3 * len` + 40).
    pub(crate) fn output(seq: u64, len: usize) -> Arc<EventRecord> {
        let data = TerminalBytes::new(Bytes::from(vec![b'x'; len]));
        record(seq, SessionEvent::Terminal(TerminalOutput { data }))
    }

    pub(crate) fn plain(records: &[Arc<EventRecord>]) -> Vec<EventRecord> {
        records.iter().map(|record| (**record).clone()).collect()
    }

    pub(crate) async fn open(dir: &DataDir, config: StoreConfig) -> Store {
        Store::open(dir, config).await.unwrap().store
    }
}

#[tokio::test]
async fn appended_records_read_back_in_pages() {
    let (_temp, dir) = data_dir();
    let config = StoreConfig {
        max_page_records: NonZeroU32::new(4).unwrap(),
        ..StoreConfig::default()
    };
    let store = open(&dir, config).await;
    let session = new_session(0);
    store.create_session(session.clone()).await.unwrap();

    let records: Vec<_> = (0..10).map(|seq| output(seq, 10)).collect();
    let (first, rest) = records.split_at(3);
    store.append(session.id, first.to_vec()).await.unwrap();
    store.append(session.id, rest.to_vec()).await.unwrap();

    let page = store
        .read_events(session.id, EventSeq::ZERO, 3)
        .await
        .unwrap();
    assert_eq!(page.records, plain(&records[0..3]));
    assert_eq!((page.next, page.at_end), (EventSeq::new(3), false));

    // The configured cap wins over the caller's limit.
    let page = store
        .read_events(session.id, EventSeq::new(3), 100)
        .await
        .unwrap();
    assert_eq!(page.records, plain(&records[3..7]));

    let page = store
        .read_events(session.id, EventSeq::new(9), 100)
        .await
        .unwrap();
    assert_eq!(page.records, plain(&records[9..]));
    assert_eq!((page.next, page.at_end), (EventSeq::new(10), true));

    let page = store
        .read_events(session.id, EventSeq::new(10), 100)
        .await
        .unwrap();
    assert!(page.records.is_empty());
    assert_eq!((page.next, page.at_end), (EventSeq::new(10), true));

    let stored = store.session(session.id).await.unwrap().unwrap();
    assert_eq!(stored.next_seq, EventSeq::new(10));
}

#[tokio::test]
async fn pages_stop_at_the_byte_cap_but_never_come_back_empty() {
    let (_temp, dir) = data_dir();
    let config = StoreConfig {
        max_page_bytes: ByteCount::new(1),
        ..StoreConfig::default()
    };
    let store = open(&dir, config).await;
    let session = new_session(0);
    store.create_session(session.clone()).await.unwrap();
    store
        .append(session.id, vec![output(0, 100), output(1, 100)])
        .await
        .unwrap();
    let page = store
        .read_events(session.id, EventSeq::ZERO, 10)
        .await
        .unwrap();
    assert_eq!(page.records.len(), 1);
    assert_eq!((page.next, page.at_end), (EventSeq::new(1), false));
}

#[tokio::test]
async fn the_log_only_accepts_its_exact_continuation() {
    let (_temp, dir) = data_dir();
    let store = open(&dir, StoreConfig::default()).await;
    let session = new_session(0);
    store.create_session(session.clone()).await.unwrap();

    assert!(matches!(
        store.create_session(session.clone()).await,
        Err(StoreError::SessionExists(_))
    ));
    let gap = store.append(session.id, vec![output(1, 1)]).await;
    assert!(matches!(
        gap,
        Err(StoreError::OutOfOrder { expected, got, .. })
            if expected == EventSeq::ZERO && got == EventSeq::new(1)
    ));
    // A failing append leaves nothing behind, even its valid prefix.
    let partial = store
        .append(session.id, vec![output(0, 1), output(2, 1)])
        .await;
    assert!(matches!(partial, Err(StoreError::OutOfOrder { .. })));
    let unknown = store.append(SessionId::random(), vec![output(0, 1)]).await;
    assert!(matches!(unknown, Err(StoreError::UnknownSession(_))));
    assert_eq!(
        unknown.unwrap_err().code(),
        vessel_proto::ErrorCode::NotFound
    );

    store.append(session.id, vec![output(0, 1)]).await.unwrap();
    let page = store
        .read_events(session.id, EventSeq::ZERO, 10)
        .await
        .unwrap();
    assert_eq!(page.records, plain(&[output(0, 1)]));
}

#[tokio::test]
async fn concurrent_appends_to_many_sessions_all_commit() {
    let (_temp, dir) = data_dir();
    let store = open(&dir, StoreConfig::default()).await;
    let mut sessions = Vec::new();
    for n in 0..8 {
        let session = new_session(n);
        store.create_session(session.clone()).await.unwrap();
        sessions.push(session.id);
    }
    let writers = sessions.iter().map(|&id| {
        let store = store.clone();
        tokio::spawn(async move {
            for seq in 0..50 {
                store.append(id, vec![output(seq, 8)]).await.unwrap();
            }
        })
    });
    for writer in futures_util::future::join_all(writers).await {
        writer.unwrap();
    }
    for id in sessions {
        let stored = store.session(id).await.unwrap().unwrap();
        assert_eq!(stored.next_seq, EventSeq::new(50));
    }
}

#[tokio::test]
async fn session_rows_follow_the_records() {
    let (_temp, dir) = data_dir();
    let store = open(&dir, StoreConfig::default()).await;
    let session = new_session(0);
    store.create_session(session.clone()).await.unwrap();
    let harness_session = HarnessSessionId::new("acp-session-1");
    let ready = record(
        2,
        SessionEvent::Agent {
            event: AgentEvent::Ready {
                harness_session: harness_session.clone(),
            },
        },
    );
    let exited = status(3, SessionStatus::Exited { code: Some(0) });
    store
        .append(
            session.id,
            vec![
                status(0, SessionStatus::Starting),
                status(1, SessionStatus::Running),
                ready,
                Arc::clone(&exited),
            ],
        )
        .await
        .unwrap();

    let stored = store.session(session.id).await.unwrap().unwrap();
    assert_eq!(stored.status, SessionStatus::Exited { code: Some(0) });
    assert_eq!(stored.ended_at, Some(exited.at));
    assert_eq!(stored.harness_session, Some(harness_session));
    assert_eq!(stored.next_seq, EventSeq::new(4));
    assert_eq!(stored.spec, session.spec);
    assert_eq!(stored.created_at, session.created_at);
}

#[tokio::test]
async fn restart_interrupts_sessions_that_were_live() {
    let (_temp, dir) = data_dir();
    let store = open(&dir, StoreConfig::default()).await;
    let live = new_session(0);
    let ended = new_session(1);
    for session in [&live, &ended] {
        store.create_session(session.clone()).await.unwrap();
    }
    store
        .append(
            live.id,
            vec![
                status(0, SessionStatus::Starting),
                status(1, SessionStatus::Running),
            ],
        )
        .await
        .unwrap();
    store
        .append(
            ended.id,
            vec![
                status(0, SessionStatus::Starting),
                status(1, SessionStatus::Exited { code: Some(3) }),
            ],
        )
        .await
        .unwrap();
    store.close().await;
    assert!(matches!(
        store.append(live.id, vec![output(2, 1)]).await,
        Err(StoreError::Closed)
    ));
    drop(store);

    let reopened = Store::open(&dir, StoreConfig::default()).await.unwrap();
    assert_eq!(reopened.interrupted.len(), 1);
    let interrupted = reopened.interrupted.first().unwrap();
    assert_eq!(interrupted.id, live.id);
    assert_eq!(interrupted.status, SessionStatus::Interrupted);
    assert_eq!(interrupted.next_seq, EventSeq::new(3));
    assert!(interrupted.ended_at.is_some());

    let store = reopened.store;
    let log = store
        .read_events(live.id, EventSeq::ZERO, 10)
        .await
        .unwrap();
    let statuses: Vec<_> = log
        .records
        .iter()
        .map(|record| (record.seq.get(), record.event.clone()))
        .collect();
    assert_eq!(
        statuses,
        vec![
            (
                0,
                SessionEvent::Status {
                    status: SessionStatus::Starting
                }
            ),
            (
                1,
                SessionEvent::Status {
                    status: SessionStatus::Running
                }
            ),
            (
                2,
                SessionEvent::Status {
                    status: SessionStatus::Interrupted
                }
            ),
        ]
    );
    let untouched = store.session(ended.id).await.unwrap().unwrap();
    assert_eq!(untouched.status, SessionStatus::Exited { code: Some(3) });
    assert_eq!(store.sessions().await.unwrap().len(), 2);

    // A second restart finds nothing left to interrupt.
    store.close().await;
    drop(store);
    let again = Store::open(&dir, StoreConfig::default()).await.unwrap();
    assert!(again.interrupted.is_empty());
}

#[tokio::test]
async fn retention_drops_the_oldest_records_but_keeps_the_newest() {
    let (_temp, dir) = data_dir();
    let config = StoreConfig {
        retention: Retention {
            max_session_bytes: ByteCount::new(600),
            ..Retention::default()
        },
        ..StoreConfig::default()
    };
    let store = open(&dir, config).await;
    let session = new_session(0);
    store.create_session(session.clone()).await.unwrap();
    for seq in 0..10 {
        // Each record stores about 180 bytes.
        store
            .append(session.id, vec![output(seq, 100)])
            .await
            .unwrap();
    }
    let page = store
        .read_events(session.id, EventSeq::ZERO, 100)
        .await
        .unwrap();
    let first = page.records.first().unwrap().seq;
    assert!(first > EventSeq::new(5), "kept from {first}");
    assert_eq!(page.records.last().unwrap().seq, EventSeq::new(9));
    assert_eq!((page.next, page.at_end), (EventSeq::new(10), true));
    // Kept records are still a gap-free run.
    for (record, seq) in page.records.iter().zip(first.get()..) {
        assert_eq!(record.seq, EventSeq::new(seq));
    }

    // Even a single oversized record is kept.
    store
        .append(session.id, vec![output(10, 5000)])
        .await
        .unwrap();
    let page = store
        .read_events(session.id, EventSeq::ZERO, 100)
        .await
        .unwrap();
    assert_eq!(page.records, plain(&[output(10, 5000)]));
}

#[tokio::test]
async fn retention_keeps_the_most_recently_ended_sessions() {
    let (_temp, dir) = data_dir();
    let config = StoreConfig {
        retention: Retention {
            max_ended_sessions: 2,
            ..Retention::default()
        },
        ..StoreConfig::default()
    };
    let store = open(&dir, config).await;
    let live = new_session(0);
    store.create_session(live.clone()).await.unwrap();
    let mut ended = Vec::new();
    for n in 1..=3 {
        let session = new_session(n);
        store.create_session(session.clone()).await.unwrap();
        let end = Arc::new(EventRecord {
            seq: EventSeq::ZERO,
            at: at(100 + n),
            event: SessionEvent::Status {
                status: SessionStatus::Exited { code: None },
            },
        });
        store.append(session.id, vec![end]).await.unwrap();
        ended.push(session.id);
    }
    let kept: Vec<SessionId> = store
        .sessions()
        .await
        .unwrap()
        .into_iter()
        .map(|session| session.id)
        .collect();
    let [oldest, second, newest] = ended.as_slice() else {
        panic!("three sessions");
    };
    assert_eq!(kept, vec![*newest, *second, live.id]);
    assert!(matches!(
        store.read_events(*oldest, EventSeq::ZERO, 10).await,
        Err(StoreError::UnknownSession(_))
    ));
}

#[tokio::test]
async fn a_newer_schema_is_refused() {
    let (_temp, dir) = data_dir();
    let store = open(&dir, StoreConfig::default()).await;
    store.close().await;
    drop(store);
    let connection = rusqlite::Connection::open(dir.database_path()).unwrap();
    connection.pragma_update(None, "user_version", 99).unwrap();
    drop(connection);
    assert!(matches!(
        Store::open(&dir, StoreConfig::default()).await,
        Err(StoreError::NewerSchema { found: 99, .. })
    ));
}

#[tokio::test]
async fn reads_scale_past_the_idle_pool() {
    let (_temp, dir) = data_dir();
    let config = StoreConfig {
        idle_readers: NonZeroUsize::new(1).unwrap(),
        ..StoreConfig::default()
    };
    let store = open(&dir, config).await;
    let session = new_session(0);
    store.create_session(session.clone()).await.unwrap();
    let reads = (0..16).map(|_| store.session(session.id));
    for read in futures_util::future::join_all(reads).await {
        assert_eq!(read.unwrap().unwrap().id, session.id);
    }
}

/// The runtime's first snapshot can already show the end of a session that
/// exits within milliseconds. The store must not believe it: the session is
/// stored as starting, survives retention, and takes its records.
#[tokio::test]
async fn a_session_that_ended_before_it_was_stored_is_kept_until_its_records_arrive() {
    let (_temp, dir) = data_dir();
    let config = StoreConfig {
        retention: Retention {
            max_ended_sessions: 1,
            ..Retention::default()
        },
        ..StoreConfig::default()
    };
    let store = open(&dir, config).await;
    let mut quick = new_session(0);
    quick.status = SessionStatus::Exited { code: Some(0) };
    store.create_session(quick.clone()).await.unwrap();
    let stored = store.session(quick.id).await.unwrap().unwrap();
    assert_eq!(stored.status, SessionStatus::Starting);
    assert_eq!(stored.ended_at, None);

    // Two other sessions end, so retention runs with room for one.
    for n in 1..=2 {
        let other = new_session(n);
        store.create_session(other.clone()).await.unwrap();
        let end = status(0, SessionStatus::Exited { code: None });
        store.append(other.id, vec![end]).await.unwrap();
    }
    store
        .append(
            quick.id,
            vec![
                status(0, SessionStatus::Starting),
                status(1, SessionStatus::Exited { code: Some(0) }),
            ],
        )
        .await
        .unwrap();
    let stored = store.session(quick.id).await.unwrap().unwrap();
    assert_eq!(stored.status, SessionStatus::Exited { code: Some(0) });
    assert!(stored.ended_at.is_some());
}

#[tokio::test]
async fn a_zero_limit_still_makes_progress() {
    let (_temp, dir) = data_dir();
    let store = open(&dir, StoreConfig::default()).await;
    let session = new_session(0);
    store.create_session(session.clone()).await.unwrap();
    store
        .append(session.id, vec![output(0, 1), output(1, 1)])
        .await
        .unwrap();
    let page = store
        .read_events(session.id, EventSeq::ZERO, 0)
        .await
        .unwrap();
    assert_eq!(page.records.len(), 1);
    assert_eq!((page.next, page.at_end), (EventSeq::new(1), false));
}

/// Schema 3: a session's profile and creator are stored, and the profile
/// follows `ProfileChanged` records (back to the default included).
#[tokio::test]
async fn profile_and_creator_are_stored_and_moves_projected() {
    let (_temp, dir) = data_dir();
    let store = open(&dir, StoreConfig::default()).await;
    let alt: ProfileName = "claude-alt".parse().unwrap();
    let work: ProfileName = "claude-work".parse().unwrap();
    let device = Creator::Device {
        id: DeviceId::random(),
    };
    let base = new_session(0);
    let session = SessionInfo::new(
        base.id,
        base.spec.with_profile(alt.clone()),
        base.created_at,
    )
    .with_creator(device.clone());
    store.create_session(session.clone()).await.unwrap();
    let stored = store.session(session.id).await.unwrap().unwrap();
    assert_eq!(stored.profile, Some(alt.clone()));
    assert_eq!(stored.created_by, device);

    let moved = |seq, from: Option<&ProfileName>, to: Option<&ProfileName>| {
        record(
            seq,
            SessionEvent::ProfileChanged {
                from: from.cloned(),
                to: to.cloned(),
            },
        )
    };
    store
        .append(
            session.id,
            vec![
                status(0, SessionStatus::Starting),
                moved(1, Some(&alt), Some(&work)),
            ],
        )
        .await
        .unwrap();
    let stored = store.session(session.id).await.unwrap().unwrap();
    assert_eq!(stored.profile, Some(work.clone()));
    // The spec keeps what was asked for.
    assert_eq!(stored.spec.profile, Some(alt));

    // Records without a move leave it; a move to the default clears it.
    store
        .append(session.id, vec![status(2, SessionStatus::Running)])
        .await
        .unwrap();
    assert_eq!(
        store.session(session.id).await.unwrap().unwrap().profile,
        Some(work.clone())
    );
    store
        .append(session.id, vec![moved(3, Some(&work), None)])
        .await
        .unwrap();
    let stored = store.session(session.id).await.unwrap().unwrap();
    assert_eq!(stored.profile, None);
    assert_eq!(stored.created_by, device);
}

/// Schema 4: where a session came from (protocol 6) is stored and read
/// back after the store is opened again, as after a restart; a session of
/// this node's own has neither.
#[tokio::test]
async fn origin_and_via_survive_a_restart() {
    let (_temp, dir) = data_dir();
    let store = open(&dir, StoreConfig::default()).await;
    let home = NodeId::random();
    let imported = SessionOrigin::Imported {
        node: NodeId::random(),
        session: SessionId::random(),
    };
    let remote = SessionOrigin::Remote {
        node: NodeId::random(),
        by: RemoteCreator::Device {
            id: DeviceId::random(),
        },
    };
    let mut sessions = Vec::new();
    for (n, (origin, via)) in [
        (Some(imported), None),
        (Some(remote), Some(home)),
        (None, None),
    ]
    .into_iter()
    .enumerate()
    {
        let mut session = new_session(i64::try_from(n).unwrap());
        session.origin = origin;
        session.via = via;
        store.create_session(session.clone()).await.unwrap();
        sessions.push(session);
    }
    store.close().await;
    drop(store);

    let store = open(&dir, StoreConfig::default()).await;
    for session in &sessions {
        let stored = store.session(session.id).await.unwrap().unwrap();
        assert_eq!(stored.origin, session.origin);
        assert_eq!(stored.via, session.via);
    }
}

/// Sessions stored before schema 4 came from this node: neither column.
#[tokio::test]
async fn sessions_from_schema_3_have_no_origin() {
    let (_temp, dir) = data_dir();
    let store = open(&dir, StoreConfig::default()).await;
    let session = new_session(0);
    store.create_session(session.clone()).await.unwrap();
    store.close().await;
    drop(store);
    let connection = rusqlite::Connection::open(dir.database_path()).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE sessions DROP COLUMN via;
             ALTER TABLE sessions DROP COLUMN origin;",
        )
        .unwrap();
    connection.pragma_update(None, "user_version", 3).unwrap();
    drop(connection);

    let store = open(&dir, StoreConfig::default()).await;
    let stored = store.session(session.id).await.unwrap().unwrap();
    assert_eq!((stored.origin, stored.via), (None, None));
    assert_eq!(stored.created_by, session.created_by);
}

/// Sessions stored before schema 3 read as the owner's, on the default
/// profile: all a node could do then.
#[tokio::test]
async fn sessions_from_schema_2_read_as_the_owners_on_the_default() {
    let (_temp, dir) = data_dir();
    let store = open(&dir, StoreConfig::default()).await;
    let session = new_session(0);
    store.create_session(session.clone()).await.unwrap();
    store.close().await;
    drop(store);
    let connection = rusqlite::Connection::open(dir.database_path()).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE sessions DROP COLUMN profile;
             ALTER TABLE sessions DROP COLUMN created_by;
             ALTER TABLE sessions DROP COLUMN via;
             ALTER TABLE sessions DROP COLUMN origin;",
        )
        .unwrap();
    connection.pragma_update(None, "user_version", 2).unwrap();
    drop(connection);

    let store = open(&dir, StoreConfig::default()).await;
    let stored = store.session(session.id).await.unwrap().unwrap();
    assert_eq!(stored.profile, None);
    assert_eq!(stored.created_by, Creator::Owner);
    assert_eq!(stored.spec, session.spec);
}
