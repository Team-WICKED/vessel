//! [`Sha256Digest`]: the checksum of a `vesseld` build or release archive
//! (its `-dev-<hash>` name, and what the remote verifies before installing,
//! ADR 0008).

use std::fmt;
use std::path::Path;

use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;

/// A SHA-256 digest.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// The digest of the file at `path`.
    ///
    /// # Errors
    /// The file cannot be read.
    pub async fn of_file(path: &Path) -> std::io::Result<Self> {
        let mut file = tokio::fs::File::open(path).await?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            if let Some(chunk) = buffer.get(..read) {
                hasher.update(chunk);
            }
        }
        Ok(Self(hasher.finalize().into()))
    }

    /// The digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Lowercase hex, as `sha256sum` prints it.
    #[must_use]
    pub fn hex(&self) -> String {
        use fmt::Write as _;

        self.0
            .iter()
            .fold(String::with_capacity(64), |mut text, byte| {
                let _infallible = write!(text, "{byte:02x}");
                text
            })
    }
}

impl std::str::FromStr for Sha256Digest {
    type Err = InvalidDigest;

    /// 64 hex digits, either case.
    fn from_str(text: &str) -> Result<Self, InvalidDigest> {
        let invalid = || InvalidDigest(text.to_owned());
        if text.len() != 64 || !text.is_ascii() {
            return Err(invalid());
        }
        let mut bytes = [0_u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let start = index.checked_mul(2).ok_or_else(invalid)?;
            let pair = text
                .get(start..start.saturating_add(2))
                .ok_or_else(invalid)?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_digit| invalid())?;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Sha256Digest({})", self.hex())
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.hex())
    }
}

/// Text that is not a SHA-256 digest.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a SHA-256 digest (64 hex digits)")]
pub struct InvalidDigest(String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_print_and_parse_as_sha256sum_does() {
        let digest = Sha256Digest::of(b"abc");
        assert_eq!(
            digest.hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(digest.hex().parse::<Sha256Digest>().unwrap(), digest);
        assert_eq!(
            digest.hex().to_uppercase().parse::<Sha256Digest>().unwrap(),
            digest
        );
        for bad in ["", "abc", &"g".repeat(64), &"é".repeat(32)] {
            assert!(bad.parse::<Sha256Digest>().is_err(), "{bad}");
        }
    }
}
