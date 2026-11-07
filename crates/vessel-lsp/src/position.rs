//! Convert saved-text positions without confusing Unicode scalars with UTF-16.
//!
//! Combining marks are separate scalar columns, as in an agent's text input;
//! astral characters occupy one scalar column but two LSP UTF-16 units. A
//! split surrogate or a position beyond the line is an error, never rounded.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

pub use vessel_proto::lsp::Position as ScalarPosition;

/// A zero-based LSP position, measured in UTF-16 code units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct Utf16Position {
    /// Zero-based line.
    pub line: u32,
    /// UTF-16 units from the line's start.
    pub character: u32,
}

/// A position does not describe a scalar boundary in this saved document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PositionError {
    /// The requested line is absent.
    #[error("line is outside the saved document")]
    Line,
    /// The requested column is absent or splits a surrogate pair.
    #[error("column is outside the line or splits a UTF-16 surrogate pair")]
    Column,
    /// The saved document exceeds the protocol's position representation.
    #[error("position exceeds the protocol's numeric range")]
    Overflow,
    /// An index does not fit this platform.
    #[error("position does not fit this platform")]
    Index(#[from] std::num::TryFromIntError),
}

/// Convert a scalar position to the server's UTF-16 position.
///
/// # Errors
/// Rejects a missing line/column and numeric overflow.
pub fn to_utf16(text: &str, position: ScalarPosition) -> Result<Utf16Position, PositionError> {
    let line_index = position.line.get().saturating_sub(1);
    let line = line_at(text, line_index)?;
    let wanted = position.column.get().saturating_sub(1);
    let mut scalars = 0_u32;
    let mut units = 0_u32;
    for ch in line.chars() {
        if scalars == wanted {
            break;
        }
        scalars = scalars.checked_add(1).ok_or(PositionError::Overflow)?;
        let width = if u32::from(ch) >= 0x1_0000 { 2 } else { 1 };
        units = units.checked_add(width).ok_or(PositionError::Overflow)?;
    }
    if scalars != wanted {
        return Err(PositionError::Column);
    }
    Ok(Utf16Position {
        line: line_index,
        character: units,
    })
}

/// Convert a server position to scalar columns against the same snapshot.
///
/// # Errors
/// Rejects nonexistent lines/columns, split surrogates and numeric overflow.
pub fn from_utf16(text: &str, position: Utf16Position) -> Result<ScalarPosition, PositionError> {
    let line = line_at(text, position.line)?;
    let mut units = 0_u32;
    let mut scalars = 0_u32;
    for ch in line.chars() {
        if units >= position.character {
            break;
        }
        let width = if u32::from(ch) >= 0x1_0000 { 2 } else { 1 };
        units = units.checked_add(width).ok_or(PositionError::Overflow)?;
        scalars = scalars.checked_add(1).ok_or(PositionError::Overflow)?;
    }
    if units != position.character {
        return Err(PositionError::Column);
    }
    let line = position
        .line
        .checked_add(1)
        .and_then(NonZeroU32::new)
        .ok_or(PositionError::Overflow)?;
    let column = scalars
        .checked_add(1)
        .and_then(NonZeroU32::new)
        .ok_or(PositionError::Overflow)?;
    Ok(ScalarPosition { line, column })
}

fn line_at(text: &str, index: u32) -> Result<&str, PositionError> {
    let index = usize::try_from(index)?;
    // split, unlike lines(), retains the empty last line after a newline.
    let line = text.split('\n').nth(index).ok_or(PositionError::Line)?;
    Ok(line.strip_suffix('\r').unwrap_or(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar(line: u32, column: u32) -> ScalarPosition {
        ScalarPosition {
            line: NonZeroU32::new(line).unwrap(),
            column: NonZeroU32::new(column).unwrap(),
        }
    }

    #[test]
    fn astral_combining_and_crlf_roundtrip() {
        let text = "a😀e\u{301}\r\n한글\n";
        for (column, units) in [(1, 0), (2, 1), (3, 3), (4, 4), (5, 5)] {
            let expected = Utf16Position {
                line: 0,
                character: units,
            };
            assert_eq!(to_utf16(text, scalar(1, column)).unwrap(), expected);
            assert_eq!(from_utf16(text, expected).unwrap(), scalar(1, column));
        }
        assert_eq!(
            to_utf16(text, scalar(3, 1)).unwrap(),
            Utf16Position {
                line: 2,
                character: 0
            }
        );
        assert!(to_utf16(text, scalar(1, 6)).is_err());
        assert!(to_utf16(text, scalar(4, 1)).is_err());
        assert!(
            from_utf16(
                text,
                Utf16Position {
                    line: 0,
                    character: 2
                }
            )
            .is_err()
        );
    }

    #[test]
    fn empty_document_has_one_empty_line() {
        assert_eq!(
            to_utf16("", scalar(1, 1)).unwrap(),
            Utf16Position {
                line: 0,
                character: 0
            }
        );
        assert!(to_utf16("", scalar(1, 2)).is_err());
        assert!(
            from_utf16(
                "x",
                Utf16Position {
                    line: u32::MAX,
                    character: 0
                }
            )
            .is_err()
        );
    }
}
