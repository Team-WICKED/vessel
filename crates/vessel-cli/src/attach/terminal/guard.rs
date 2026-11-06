//! [`RawMode`]: the local terminal in raw mode, restored on every way out.

use std::io::{IsTerminal as _, Write};

/// Turns off what a harness may have turned on in the local terminal, so
/// leaving an attach never strands the user in its modes: pops the kitty
/// keyboard flags, leaves the alternate screen, shows the cursor, and turns
/// off bracketed paste, focus reports, mouse reports (all three tracking
/// modes and SGR encoding) and `modifyOtherKeys`. Each is a no-op when it
/// is not on.
const RESET: &[u8] = b"\x1b[<u\x1b[?1049l\x1b[?25h\x1b[?2004l\x1b[?1004l\
\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[>4;0m";

/// Raw mode while alive. Dropping it (detach, session end, error, signal:
/// every path out of the attach) writes [`RESET`] and leaves raw mode.
pub(super) struct RawMode;

impl RawMode {
    /// `None` when stdin is not a terminal (piped input stays as it is).
    pub(super) fn enable() -> std::io::Result<Option<Self>> {
        if !std::io::stdin().is_terminal() {
            return Ok(None);
        }
        crossterm::terminal::enable_raw_mode()?;
        Ok(Some(Self))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // Only to a terminal: redirected output (`> file`) must not get
        // control sequences appended.
        if std::io::stdout().is_terminal()
            && let Err(error) = reset(&mut std::io::stdout().lock())
        {
            tracing::warn!(%error, "could not reset the terminal; run `reset`");
        }
        if let Err(error) = crossterm::terminal::disable_raw_mode() {
            tracing::warn!(%error, "could not restore the terminal; run `reset`");
        }
    }
}

/// Write [`RESET`] and flush.
fn reset(out: &mut impl Write) -> std::io::Result<()> {
    out.write_all(RESET)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reset_turns_off_every_mode_a_harness_may_have_turned_on() {
        let mut written = Vec::new();
        reset(&mut written).unwrap();
        let text = String::from_utf8(written).unwrap();
        for sequence in [
            "\x1b[<u",
            "\x1b[?1049l",
            "\x1b[?25h",
            "\x1b[?2004l",
            "\x1b[?1004l",
            "\x1b[?1000l",
            "\x1b[?1002l",
            "\x1b[?1003l",
            "\x1b[?1006l",
            "\x1b[>4;0m",
        ] {
            assert!(text.contains(sequence), "{sequence:?} missing");
        }
        assert!(text.starts_with("\x1b[<u"), "the keyboard flags go first");
    }
}
