//! Independent framing so the fake cannot hide a bug in vessel-lsp's codec.

use serde::{Serialize, de::DeserializeOwned};
use std::io::{BufRead, Write};

#[derive(Debug, thiserror::Error)]
pub(super) enum TransportError {
    #[error("pipe failed")]
    Io(#[from] std::io::Error),
    #[error("JSON failed")]
    Json(#[from] serde_json::Error),
    #[error("invalid fixture protocol")]
    Protocol,
    #[error("invalid length")]
    Length(#[from] std::num::ParseIntError),
}

pub(super) fn read<T: DeserializeOwned>(
    input: &mut impl BufRead,
) -> Result<Option<T>, TransportError> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if line == "\r\n" {
            break;
        }
        if let Some(given) = line.strip_prefix("Content-Length:") {
            length = Some(given.trim().parse::<usize>()?);
        }
        if line.len() > 8192 {
            return Err(TransportError::Protocol);
        }
    }
    let length = length.ok_or(TransportError::Protocol)?;
    if length > 4 * 1024 * 1024 {
        return Err(TransportError::Protocol);
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

pub(super) fn write(
    output: &mut impl Write,
    message: &impl Serialize,
) -> Result<(), TransportError> {
    let body = serde_json::to_vec(message)?;
    write!(output, "Content-Length: {}\r\n\r\n", body.len())?;
    output.write_all(&body)?;
    output.flush()?;
    Ok(())
}
