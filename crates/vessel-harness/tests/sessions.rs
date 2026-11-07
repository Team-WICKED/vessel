//! Finding a session's files and hard-linking them into another home, on
//! fake harness layouts in temporary directories.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use vessel_harness::{LinkError, SessionFilesError, link_session, session_files};
use vessel_proto::HarnessKind;

use support::{ClaudeHome, ID, TempDir, id, write};

#[cfg(test)]
mod support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh directory under the system temp dir, removed on drop.
    pub(super) struct TempDir(PathBuf);

    impl TempDir {
        pub(super) fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("vessel-sessions-test-{}-{n}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }

        pub(super) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(fs::remove_dir_all(&self.0));
        }
    }

    use std::fs;

    use vessel_proto::HarnessSessionId;

    pub(super) const ID: &str = "5f0c1d2e-3a4b-4c5d-8e6f-7a8b9c0d1e2f";

    pub(super) fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A Claude Code home with one session started in `cwd`: transcript, its
    /// directory, file history, the project's memory, and a session-env
    /// directory that is not part of the conversation.
    pub(super) struct ClaudeHome {
        dir: TempDir,
        pub(super) project: PathBuf,
    }

    impl ClaudeHome {
        pub(super) fn new(cwd: &Path) -> Self {
            let dir = TempDir::new();
            let slug: String = cwd
                .to_str()
                .unwrap()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let project = Path::new("projects").join(slug);
            let home = dir.path();
            write(&home.join(&project).join(format!("{ID}.jsonl")), "line 1\n");
            write(
                &home
                    .join(&project)
                    .join(ID)
                    .join("subagents")
                    .join("a.jsonl"),
                "sub\n",
            );
            write(
                &home
                    .join(&project)
                    .join(ID)
                    .join("tool-results")
                    .join("t.txt"),
                "out\n",
            );
            write(
                &home.join(&project).join("memory").join("MEMORY.md"),
                "per profile\n",
            );
            write(&home.join("file-history").join(ID).join("v1"), "before\n");
            write(&home.join("session-env").join(ID).join("env"), "x\n");
            write(
                &home.join(&project).join("other.jsonl"),
                "another session\n",
            );
            Self { dir, project }
        }

        pub(super) fn path(&self) -> &Path {
            self.dir.path()
        }
    }

    pub(super) fn id(text: &str) -> HarnessSessionId {
        HarnessSessionId::new(text)
    }
}

#[test]
fn claude_sessions_are_found_by_working_directory() {
    let cwd = TempDir::new();
    let home = ClaudeHome::new(cwd.path());
    let files = session_files(HarnessKind::ClaudeCode, home.path(), cwd.path(), &id(ID)).unwrap();
    assert_eq!(files.transcript(), home.project.join(format!("{ID}.jsonl")));
    assert_eq!(
        files.entries(),
        [
            home.project.join(format!("{ID}.jsonl")),
            home.project.join(ID),
            Path::new("file-history").join(ID),
        ]
    );

    let missing = session_files(
        HarnessKind::ClaudeCode,
        home.path(),
        cwd.path(),
        &id("nope"),
    );
    assert!(
        matches!(missing, Err(SessionFilesError::NotFound { .. })),
        "{missing:?}"
    );
    for bad in ["../x", "a/b", ".hidden", ""] {
        let error = session_files(HarnessKind::ClaudeCode, home.path(), cwd.path(), &id(bad));
        assert!(
            matches!(error, Err(SessionFilesError::InvalidId { .. })),
            "{bad}: {error:?}"
        );
    }
    let error = session_files(HarnessKind::Gemini, home.path(), cwd.path(), &id(ID));
    assert!(
        matches!(error, Err(SessionFilesError::Unsupported { .. })),
        "{error:?}"
    );
}

#[test]
fn linking_shares_one_file_and_leaves_memory_alone() {
    let cwd = TempDir::new();
    let from = ClaudeHome::new(cwd.path());
    let to = TempDir::new();
    let files = session_files(HarnessKind::ClaudeCode, from.path(), cwd.path(), &id(ID)).unwrap();

    let linked = link_session(&files, from.path(), to.path()).unwrap();
    assert_eq!(linked.linked, 4, "{linked:?}");
    assert_eq!(linked.already, 0);
    assert!(linked.skipped.is_empty(), "{linked:?}");

    let transcript = files.transcript();
    let (a, b) = (from.path().join(transcript), to.path().join(transcript));
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let (a, b) = (fs::metadata(&a).unwrap(), fs::metadata(&b).unwrap());
        assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()));
        assert_eq!(a.nlink(), 2);
    }
    // One file: what the new home's harness appends, the old home sees.
    fs::OpenOptions::new()
        .append(true)
        .open(&b)
        .unwrap()
        .write_all(b"line 2\n")
        .unwrap();
    assert_eq!(fs::read_to_string(&a).unwrap(), "line 1\nline 2\n");
    for linked_file in [
        from.project.join(ID).join("subagents").join("a.jsonl"),
        from.project.join(ID).join("tool-results").join("t.txt"),
        Path::new("file-history").join(ID).join("v1"),
    ] {
        assert!(
            to.path().join(&linked_file).is_file(),
            "{}",
            linked_file.display()
        );
    }

    // Memory, other sessions and session-env stay where they are.
    assert!(!to.path().join(&from.project).join("memory").exists());
    assert!(!to.path().join(&from.project).join("other.jsonl").exists());
    assert!(!to.path().join("session-env").exists());
    assert_eq!(
        fs::read_to_string(
            from.path()
                .join(&from.project)
                .join("memory")
                .join("MEMORY.md")
        )
        .unwrap(),
        "per profile\n"
    );

    // Linking again is a no-op, on every system (the files' identity
    // tells): moving back, or retrying, works.
    let again = link_session(&files, from.path(), to.path()).unwrap();
    assert_eq!((again.linked, again.already), (0, 4), "{again:?}");
    // The other way round too: the old home has them all already.
    let back = link_session(&files, to.path(), from.path()).unwrap();
    assert_eq!((back.linked, back.already), (0, 4), "{back:?}");
}

#[cfg(unix)]
#[test]
fn created_directories_are_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let cwd = TempDir::new();
    let from = ClaudeHome::new(cwd.path());
    let to = TempDir::new();
    let files = session_files(HarnessKind::ClaudeCode, from.path(), cwd.path(), &id(ID)).unwrap();
    let _linked = link_session(&files, from.path(), to.path()).unwrap();
    for dir in ["projects", "file-history"] {
        let mode = fs::metadata(to.path().join(dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{dir}");
    }
}

#[test]
fn a_conflict_links_nothing() {
    let cwd = TempDir::new();
    let from = ClaudeHome::new(cwd.path());
    let to = TempDir::new();
    let files = session_files(HarnessKind::ClaudeCode, from.path(), cwd.path(), &id(ID)).unwrap();
    let theirs = to.path().join("file-history").join(ID).join("v1");
    write(&theirs, "a different file\n");

    let error = link_session(&files, from.path(), to.path()).unwrap_err();
    assert!(
        matches!(&error, LinkError::Conflict { path } if *path == theirs),
        "{error:?}"
    );
    assert!(!to.path().join(files.transcript()).exists());
    assert_eq!(fs::read_to_string(&theirs).unwrap(), "a different file\n");
}

#[test]
fn a_session_named_memory_is_refused() {
    let cwd = TempDir::new();
    let home = ClaudeHome::new(cwd.path());
    write(&home.path().join(&home.project).join("memory.jsonl"), "x\n");
    let to = TempDir::new();
    let files = session_files(
        HarnessKind::ClaudeCode,
        home.path(),
        cwd.path(),
        &id("memory"),
    )
    .unwrap();
    let error = link_session(&files, home.path(), to.path()).unwrap_err();
    assert!(matches!(error, LinkError::Refused { .. }), "{error:?}");
    assert!(!to.path().join("projects").exists());
    // Nor is it copied to another node.
    let error = files.regular_files(home.path()).unwrap_err();
    assert!(matches!(error, LinkError::Refused { .. }), "{error:?}");
}

#[cfg(unix)]
#[test]
fn symbolic_links_are_never_followed() {
    use std::os::unix::fs::symlink;

    let cwd = TempDir::new();
    let outside = TempDir::new();
    write(&outside.path().join("secret"), "outside the home\n");

    // Inside the session's directory: skipped, not linked, not read.
    let from = ClaudeHome::new(cwd.path());
    let inside = from.path().join(&from.project).join(ID).join("escape");
    symlink(outside.path(), &inside).unwrap();
    let nested_memory = from.path().join(&from.project).join(ID).join("memory");
    fs::create_dir_all(&nested_memory).unwrap();
    let to = TempDir::new();
    let files = session_files(HarnessKind::ClaudeCode, from.path(), cwd.path(), &id(ID)).unwrap();
    let linked = link_session(&files, from.path(), to.path()).unwrap();
    let mut skipped = linked.skipped.clone();
    skipped.sort();
    assert_eq!(
        skipped,
        [
            from.project.join(ID).join("escape"),
            from.project.join(ID).join("memory")
        ]
    );
    // A copy to another node takes exactly the files a move links.
    let regular = files.regular_files(from.path()).unwrap();
    assert_eq!(regular.len(), linked.linked, "{regular:?}");
    assert!(
        regular
            .iter()
            .all(|path| !skipped.iter().any(|left| path.starts_with(left))),
        "{regular:?}"
    );
    assert!(
        !to.path()
            .join(&from.project)
            .join(ID)
            .join("escape")
            .exists()
    );

    // A link in the target home's path: refused before anything is linked.
    let to = TempDir::new();
    symlink(outside.path(), to.path().join("projects")).unwrap();
    let error = link_session(&files, from.path(), to.path()).unwrap_err();
    assert!(matches!(error, LinkError::SymbolicLink { .. }), "{error:?}");
    assert!(!to.path().join("file-history").exists());
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);

    // A target that is a link to the very file: refused, not "already
    // linked" (telling the two apart follows links).
    let to = TempDir::new();
    let transcript = files.transcript().to_owned();
    fs::create_dir_all(to.path().join(transcript.parent().unwrap())).unwrap();
    symlink(from.path().join(&transcript), to.path().join(&transcript)).unwrap();
    let error = link_session(&files, from.path(), to.path()).unwrap_err();
    assert!(matches!(error, LinkError::SymbolicLink { .. }), "{error:?}");

    // A link in the source home's path: refused too.
    let from = ClaudeHome::new(cwd.path());
    let history = from.path().join("file-history");
    fs::rename(&history, outside.path().join("moved")).unwrap();
    symlink(outside.path().join("moved"), &history).unwrap();
    let files = session_files(HarnessKind::ClaudeCode, from.path(), cwd.path(), &id(ID)).unwrap();
    let error = link_session(&files, from.path(), TempDir::new().path()).unwrap_err();
    assert!(matches!(error, LinkError::SymbolicLink { .. }), "{error:?}");
}

#[test]
fn codex_rollouts_are_found_by_id() {
    let home = TempDir::new();
    let day = home
        .path()
        .join("sessions")
        .join("2026")
        .join("11")
        .join("06");
    write(
        &day.join(format!("rollout-2026-11-06T10-00-00-{ID}.jsonl")),
        "{}\n",
    );
    write(&day.join("rollout-2026-11-06T09-00-00-other.jsonl"), "{}\n");
    let files = session_files(HarnessKind::Codex, home.path(), home.path(), &id(ID)).unwrap();
    assert_eq!(
        files.entries(),
        [Path::new("sessions")
            .join("2026")
            .join("11")
            .join("06")
            .join(format!("rollout-2026-11-06T10-00-00-{ID}.jsonl"))]
    );
    let to = TempDir::new();
    assert_eq!(
        link_session(&files, home.path(), to.path()).unwrap().linked,
        1
    );
    let missing = session_files(HarnessKind::Codex, home.path(), home.path(), &id("absent"));
    assert!(
        matches!(missing, Err(SessionFilesError::NotFound { .. })),
        "{missing:?}"
    );

    // Review finding 4: a suffix of the id is another session, not this
    // one.
    let tail = ID.rsplit('-').next().unwrap();
    for partial in [tail, ID.get(9..).unwrap()] {
        let found = session_files(HarnessKind::Codex, home.path(), home.path(), &id(partial));
        assert!(
            matches!(found, Err(SessionFilesError::NotFound { .. })),
            "{partial}: {found:?}"
        );
    }
    // A name not in a rollout's shape is not a match either.
    write(&day.join(format!("rollout-today-{ID}.jsonl")), "{}\n");
    let files = session_files(HarnessKind::Codex, home.path(), home.path(), &id(ID)).unwrap();
    assert_eq!(files.entries().len(), 1);
    // Two rollouts for one id: an error, not the first one found.
    let other_day = home
        .path()
        .join("sessions")
        .join("2026")
        .join("11")
        .join("07");
    write(
        &other_day.join(format!("rollout-2026-11-07T08-00-00-{ID}.jsonl")),
        "{}\n",
    );
    let twice = session_files(HarnessKind::Codex, home.path(), home.path(), &id(ID));
    assert!(
        matches!(twice, Err(SessionFilesError::Ambiguous { count: 2, .. })),
        "{twice:?}"
    );
}

/// Review finding 8: a failure partway through linking takes back what was
/// already linked. The target's history directory refuses new entries
/// (read-only), so the transcript and the session's directory link first
/// and the history file fails last.
#[cfg(unix)]
#[test]
fn a_failure_partway_takes_back_the_links_already_made() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let cwd = TempDir::new();
    let from = ClaudeHome::new(cwd.path());
    let to = TempDir::new();
    let files = session_files(HarnessKind::ClaudeCode, from.path(), cwd.path(), &id(ID)).unwrap();
    let history = to.path().join("file-history").join(ID);
    fs::create_dir_all(&history).unwrap();
    fs::set_permissions(&history, fs::Permissions::from_mode(0o500)).unwrap();
    // A user who may write anyway (root) cannot see the failure.
    if fs::write(history.join("probe"), "x").is_ok() {
        fs::set_permissions(&history, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }

    let error = link_session(&files, from.path(), to.path()).unwrap_err();
    assert!(matches!(error, LinkError::Io { .. }), "{error:?}");
    assert!(
        !to.path().join("projects").exists(),
        "links were left behind"
    );
    assert!(history.is_dir(), "a directory that was there stays");
    // The source is untouched: one link each, as before.
    let transcript = from.path().join(files.transcript());
    assert_eq!(fs::metadata(transcript).unwrap().nlink(), 1);
    fs::set_permissions(&history, fs::Permissions::from_mode(0o700)).unwrap();
}

/// Review finding 8: `Memory` is `memory` on a case-insensitive file
/// system, so it is refused and skipped in any case.
#[test]
fn memory_is_recognised_in_any_case() {
    let cwd = TempDir::new();
    let home = ClaudeHome::new(cwd.path());
    write(&home.path().join(&home.project).join("MEMORY.jsonl"), "x\n");
    let files = session_files(
        HarnessKind::ClaudeCode,
        home.path(),
        cwd.path(),
        &id("MEMORY"),
    )
    .unwrap();
    // Its directory is named like the project's memory: refused.
    fs::create_dir_all(home.path().join(&home.project).join("MEMORY")).unwrap();
    let files_with_dir = session_files(
        HarnessKind::ClaudeCode,
        home.path(),
        cwd.path(),
        &id("MEMORY"),
    )
    .unwrap();
    let to = TempDir::new();
    let error = link_session(&files_with_dir, home.path(), to.path()).unwrap_err();
    assert!(matches!(error, LinkError::Refused { .. }), "{error:?}");
    drop(files);

    // Inside a session's directory, a `Memory` directory is skipped.
    let from = ClaudeHome::new(cwd.path());
    let nested = from.path().join(&from.project).join(ID).join("Memory");
    write(&nested.join("note.md"), "per profile\n");
    let files = session_files(HarnessKind::ClaudeCode, from.path(), cwd.path(), &id(ID)).unwrap();
    let to = TempDir::new();
    let linked = link_session(&files, from.path(), to.path()).unwrap();
    assert_eq!(linked.skipped, [from.project.join(ID).join("Memory")]);
    assert!(
        !to.path()
            .join(&from.project)
            .join(ID)
            .join("Memory")
            .exists()
    );
}
