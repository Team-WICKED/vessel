//! PTY sessions against the `fake-tui` testkit binary.
//!
//! The output stream ends only when no process holds the PTY any more, and
//! `fake-tui` keeps a grandchild that holds it; so "output reached EOF"
//! proves the whole group is gone. Tests wait on output, never on time; a
//! generous timeout only turns a hang into a failure.

use bytes::Bytes;
use vessel_proc::CommandSpec;
use vessel_pty::{PtyProcess, WindowSize};

use support::{Screen, fake_tui, within};

#[cfg(test)]
mod support {
    use std::future::Future;
    use std::time::Duration;

    use vessel_proc::CommandSpec;
    use vessel_pty::PtyOutput;
    use vessel_testkit::Fake;

    const SAFETY_NET: Duration = Duration::from_secs(30);

    /// The safety net starts after the fakes are built: the first build of
    /// a run can take a minute (see `vessel_testkit::within`).
    pub(super) async fn within<F: Future>(test: F) -> F::Output {
        vessel_testkit::within(SAFETY_NET, test)
            .await
            .expect("test timed out")
    }

    pub(super) fn fake_tui() -> CommandSpec {
        CommandSpec::new(Fake::Tui.path().unwrap())
    }

    /// Everything the PTY has printed so far.
    pub(super) struct Screen {
        output: PtyOutput,
        text: String,
    }

    impl Screen {
        pub(super) const fn new(output: PtyOutput) -> Self {
            Self {
                output,
                text: String::new(),
            }
        }

        /// Read until the output contains `needle` after `from`; returns the
        /// position just past it.
        pub(super) async fn expect(&mut self, from: usize, needle: &str) -> usize {
            loop {
                if let Some(found) = self.text.get(from..).and_then(|rest| rest.find(needle)) {
                    return from.saturating_add(found).saturating_add(needle.len());
                }
                let Some(chunk) = self.output.next().await else {
                    panic!("output ended before {needle:?}; screen: {:?}", self.text);
                };
                self.text.push_str(&String::from_utf8_lossy(&chunk));
            }
        }

        /// Read until the PTY is closed and drained.
        pub(super) async fn drain(&mut self) {
            while let Some(chunk) = self.output.next().await {
                self.text.push_str(&String::from_utf8_lossy(&chunk));
            }
        }
    }
}

const SIZE: WindowSize = WindowSize { cols: 80, rows: 24 };

#[tokio::test]
async fn echoes_input_and_reports_the_exit_code() {
    within(async {
        let (mut pty, output) = PtyProcess::spawn(&fake_tui(), SIZE).unwrap();
        let mut screen = Screen::new(output);
        let at = screen.expect(0, "ready").await;
        // A carriage return is what the Enter key sends.
        pty.write(Bytes::from_static(b"hello\r")).await.unwrap();
        let _ = screen.expect(at, "echo: hello").await;
        pty.write(Bytes::from_static(b"exit 5\r")).await.unwrap();
        assert_eq!(pty.wait().await, Some(5));
        // The leader exited on its own; the group sweep removed the
        // grandchild, so the output ends.
        screen.drain().await;
        assert!(pty.write(Bytes::from_static(b"late\r")).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn sets_term_unless_the_spec_mentions_it() {
    within(async {
        let (mut pty, output) = PtyProcess::spawn(&fake_tui(), SIZE).unwrap();
        let mut screen = Screen::new(output);
        let at = screen.expect(0, "ready").await;
        pty.write(Bytes::from_static(b"env TERM\r")).await.unwrap();
        let _ = screen.expect(at, "env TERM=xterm-256color").await;
        pty.kill().unwrap();
        let _ = pty.wait().await;

        let spec = fake_tui().env("TERM", "dumb");
        let (mut pty, output) = PtyProcess::spawn(&spec, SIZE).unwrap();
        let mut screen = Screen::new(output);
        let at = screen.expect(0, "ready").await;
        pty.write(Bytes::from_static(b"env TERM\r")).await.unwrap();
        let _ = screen.expect(at, "env TERM=dumb").await;
        pty.kill().unwrap();
        let _ = pty.wait().await;
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn resize_reaches_the_program() {
    within(async {
        let (mut pty, output) = PtyProcess::spawn(&fake_tui(), SIZE).unwrap();
        let mut screen = Screen::new(output);
        let at = screen.expect(0, "size 80x24").await;
        let at = screen.expect(at, "ready").await;
        pty.input()
            .resize(WindowSize {
                cols: 100,
                rows: 30,
            })
            .await
            .unwrap();
        let _ = screen.expect(at, "size 100x30").await;
        pty.signal(vessel_pty::Signal::Hangup).unwrap();
        assert_eq!(pty.wait().await, None, "ended by SIGHUP");
        screen.drain().await;
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn kill_stops_the_whole_group() {
    within(async {
        let (mut pty, output) = PtyProcess::spawn(&fake_tui(), SIZE).unwrap();
        let mut screen = Screen::new(output);
        let _ = screen.expect(0, "ready").await;
        pty.kill().unwrap();
        assert_eq!(pty.wait().await, None);
        screen.drain().await;
        // Stopping an exited program is a no-op.
        pty.kill().unwrap();
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_a_running_pty_kills_its_group() {
    within(async {
        let (pty, output) = PtyProcess::spawn(&fake_tui(), SIZE).unwrap();
        let mut screen = Screen::new(output);
        let _ = screen.expect(0, "ready").await;
        drop(pty);
        screen.drain().await;
    })
    .await;
}

#[test]
fn missing_program_fails_to_spawn() {
    let spec = CommandSpec::new("/definitely/not/a/program");
    assert!(PtyProcess::spawn(&spec, SIZE).is_err());
}
