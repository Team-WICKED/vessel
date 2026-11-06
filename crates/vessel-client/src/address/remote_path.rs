//! A path on the machine behind an SSH node: [`RemotePath`].

use std::fmt;
use std::str::FromStr;

/// A path on a remote Unix machine: absolute, or under the remote user's
/// home (`~` or `~/...`). Held as text, since it names a file on another
/// machine and never touches the local file system. No control
/// characters (they would change the remote shell command).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemotePath(String);

/// Text that is not a [`RemotePath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a remote path (absolute, or starting with `~/`, without control characters)")]
pub struct InvalidRemotePath(String);

impl RemotePath {
    /// The node's default data directory, `~/.vessel`.
    #[must_use]
    pub fn default_data_dir() -> Self {
        Self("~/.vessel".to_owned())
    }

    /// The path as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path below the remote home, without `~/`: `Some("x/y")` for
    /// `~/x/y`, `Some("")` for `~`; `None` for an absolute path.
    #[must_use]
    pub fn home_relative(&self) -> Option<&str> {
        if self.0 == "~" {
            return Some("");
        }
        self.0.strip_prefix("~/")
    }

    /// `segment` below this path. `segment` is a fixed relative path
    /// (`bin/vesseld-0.2.0`), checked like a path.
    ///
    /// # Errors
    /// `segment` is empty, absolute, or has control characters.
    pub fn join(&self, segment: &str) -> Result<Self, InvalidRemotePath> {
        if segment.is_empty() || segment.starts_with('/') || segment.contains(char::is_control) {
            return Err(InvalidRemotePath(segment.to_owned()));
        }
        let mut path = self.0.trim_end_matches('/').to_owned();
        path.push('/');
        path.push_str(segment);
        Ok(Self(path))
    }
}

impl FromStr for RemotePath {
    type Err = InvalidRemotePath;

    fn from_str(text: &str) -> Result<Self, InvalidRemotePath> {
        let rooted = text.starts_with('/') || text == "~" || text.starts_with("~/");
        if !rooted || text.contains(char::is_control) {
            return Err(InvalidRemotePath(text.to_owned()));
        }
        Ok(Self(text.to_owned()))
    }
}

impl fmt::Display for RemotePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths_are_rooted_and_join_fixed_segments() {
        let home: RemotePath = "~/.vessel".parse().unwrap();
        assert_eq!(home, RemotePath::default_data_dir());
        assert_eq!(home.home_relative(), Some(".vessel"));
        assert_eq!(
            home.join("bin/vesseld-0.2.0").unwrap().as_str(),
            "~/.vessel/bin/vesseld-0.2.0"
        );
        let absolute: RemotePath = "/srv/vessel/".parse().unwrap();
        assert_eq!(absolute.home_relative(), None);
        assert_eq!(absolute.join("bin").unwrap().as_str(), "/srv/vessel/bin");
        assert_eq!("~".parse::<RemotePath>().unwrap().home_relative(), Some(""));
        for text in ["relative/dir", "", "~user/dir", "/a\nb", "/tab\there"] {
            assert!(text.parse::<RemotePath>().is_err(), "{text:?}");
        }
        assert!(home.join("/etc").is_err());
        assert!(home.join("").is_err());
        assert!(home.join("a\rb").is_err());
    }
}
