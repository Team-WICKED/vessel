//! Waiting reads leave room on every connection between nodes (the review
//! of Track X, 2026-11-06, item 3). The home reaches a member over one
//! connection, and a member its home over one (its pull loop's); 64
//! waiting reads through either used to fill it, and every other call on
//! it was refused. Now one origin holds at most 8 waits on a node and a
//! connection at most half its calls; the reads past that answer at once.

use std::time::Duration;

use vessel_proto::{EventPage, GetSession, ListSessions, ReadEvents, Scope, SendInput};
use vessel_rpc::CallError;

use crate::across::{Member, home, member_granting};
use crate::secure::Relay;
use crate::support::{prompt, structured_spec, within};

/// Waiting reads one origin may hold on a node (`waits::PER_ORIGIN`).
const PER_ORIGIN: usize = 8;

/// `caller`'s owner asks, over one connection, for 64 waiting reads of a
/// session on `target` (through the home), one after another: only its
/// share waits, the rest answer at once and empty, and other calls go
/// through meanwhile, on that connection and on another. A prompt then
/// wakes the ones waiting.
async fn waits_leave_room(caller: &Member, target: &Member) {
    let session = target
        .owner
        .create_session(structured_spec(&target.fixture, None))
        .await
        .unwrap()
        .id;
    let from = loop {
        let info = target.owner.call(GetSession { session }).await.unwrap();
        if info.status == vessel_proto::SessionStatus::Running {
            break info.next_seq;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let waiter = caller.fixture.connect(&caller.node).await;
    let node = target.id();
    // One after another, as agents start waiting over time: a burst of
    // 64 calls at the same instant fills a connection whatever they are.
    let mut waits: Vec<tokio::task::JoinHandle<Result<EventPage, CallError>>> = Vec::new();
    for _ in 0..64 {
        let client = waiter.clone();
        waits.push(tokio::spawn(async move {
            client
                .call_on(
                    node,
                    ReadEvents {
                        session,
                        from,
                        limit: 10,
                        wait: Some(60),
                    },
                )
                .await
        }));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for _ in 0..800 {
        let finished = waits.iter().filter(|wait| wait.is_finished()).count();
        if finished >= waits.len().saturating_sub(PER_ORIGIN) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (done, waiting): (Vec<_>, Vec<_>) = waits.into_iter().partition(|wait| wait.is_finished());
    assert!(
        waiting.len() <= PER_ORIGIN,
        "{} still waiting",
        waiting.len()
    );
    for answered in done {
        let page = answered.await.unwrap().unwrap();
        assert!(page.records.is_empty(), "nothing had happened: {page:?}");
    }

    // Room for everything else, on the same connection and another.
    let listed = waiter.call_on(node, ListSessions).await;
    assert!(listed.is_ok(), "{listed:?}");
    let other = caller.fixture.connect(&caller.node).await;
    assert!(other.call_on(node, ListSessions).await.is_ok());

    // The ones waiting wake when records land.
    let _sent = target
        .owner
        .call(SendInput {
            session,
            input: prompt("plain"),
        })
        .await
        .unwrap();
    for wait in waiting {
        let page = tokio::time::timeout(Duration::from_secs(20), wait)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!page.records.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_reads_leave_room_on_the_homes_and_a_members_connection() {
    within(async {
        let relay = Relay::start().await;
        let sources = tempfile::tempdir().unwrap();
        let home = home(&relay, sources.path()).await;
        let grant = vessel_node::fleet::DEFAULT_GRANT.to_vec();
        let relaying = member_granting(
            &relay,
            &home,
            "laptop",
            vec![Scope::Sync, Scope::Relay],
            grant.clone(),
        )
        .await;
        let target = member_granting(&relay, &home, "desktop", vec![Scope::Sync], grant).await;
        // The home's one connection to a member.
        waits_leave_room(&home, &target).await;
        // A member's one connection to its home (relayed on to the other).
        waits_leave_room(&relaying, &target).await;
        relay.stop().await;
    })
    .await;
}
