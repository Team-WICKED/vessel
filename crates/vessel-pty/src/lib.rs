//! Terminal sessions: a command running in a pseudo-terminal.
//!
//! Owns the PTY and the threads that move bytes in and out of it, and stops
//! the command's whole process group. Must not know about sessions, events
//! or harnesses: it takes a [`CommandSpec`] and produces [`Bytes`].
//!
//! [Decision Log]
//! - Purpose: terminal mode runs a harness's real TUI, so clients see
//!   exactly what a local terminal would (ADR 0003).
//! - Alternatives: piped stdio (TUIs refuse to start or lose colour and
//!   cursor control); writing PTY code per OS by hand (Unix `openpty` plus
//!   Windows `ConPTY` FFI, which needs `unsafe`, forbidden in this workspace).
//! - Choice: `portable-pty` (from wezterm; Unix PTYs and Windows `ConPTY`
//!   behind one API). Its reader, writer and `wait` are blocking, so each
//!   runs on a dedicated thread and talks to async code over bounded
//!   channels: one reader thread (output), one control thread (input bytes
//!   and resizes, in order) and one waiter thread (exit status).
//! - Backpressure: the reader blocks when [`PtyOutput`] is full, the kernel
//!   PTY buffer then fills and the program blocks on write. Nothing is
//!   dropped; a slow consumer slows the program down.
//! - Ending: on Unix the program is a session leader (`portable-pty` calls
//!   `setsid`), so its pid names its process group, which is signalled to
//!   stop and swept with `SIGKILL` once the leader exits; output then ends
//!   because no process holds the PTY any more. On Windows the pseudo
//!   console is closed when the program exits, which ends the output.
//! - Windows gap: stopping terminates the direct child only (for an npm
//!   `.cmd` shim that is `cmd.exe`). The harness under it is attached to
//!   the same pseudo console and is told the console closed
//!   (`CTRL_CLOSE_EVENT`), which ends programs that keep the default
//!   handler. `vessel-proc` puts its children in job objects instead, but
//!   `portable-pty` starts the program running, and joining a job
//!   afterwards can miss children it starts in its first milliseconds.
//!
//! [`CommandSpec`]: vessel_proc::CommandSpec
//! [`Bytes`]: bytes::Bytes

mod command;
mod error;
mod kill;
mod process;
mod size;
mod streams;
mod threads;

pub use error::{PtyClosed, PtyError};
pub use process::PtyProcess;
pub use size::WindowSize;
pub use streams::{PtyInput, PtyOutput};
pub use vessel_proc::Signal;
