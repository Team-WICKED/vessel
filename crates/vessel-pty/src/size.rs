//! [`WindowSize`]: the PTY's size in character cells.

/// Size of the terminal window in character cells.
///
/// Its own type rather than `vessel_proto::TerminalSize` because this crate
/// sits below the wire contract (ADR 0001: `vessel-pty` depends only on
/// `vessel-proc`); the session runtime converts between the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowSize {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
}

impl WindowSize {
    pub(crate) const fn to_pty(self) -> portable_pty::PtySize {
        portable_pty::PtySize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}
