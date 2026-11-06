//! A home's sources and a member's harness homes in temporary directories.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use vessel_fleet::{BlobStore, Bundled, Managed, Sources, Target, bundle};
use vessel_proto::{BlobChunk, Manifest};

/// Write `text` at `path`, creating its directories.
pub(crate) fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

pub(crate) fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

pub(crate) fn skill_md(name: &str) -> String {
    format!("---\nname: {name}\ndescription: test skill\n---\n# {name}\n")
}

/// A home: sources (`codex/AGENTS.md`, `codex/skills/`) and its store.
pub(crate) struct Home {
    pub(crate) dir: TempDir,
    pub(crate) blobs: BlobStore,
    pub(crate) sources: Sources,
    pub(crate) published: Option<Manifest>,
}

impl Home {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let codex = dir.path().join("codex");
        write(&codex.join("AGENTS.md"), "instructions v1\n");
        write(&codex.join("skills/review/SKILL.md"), &skill_md("review"));
        write(&codex.join("skills/review/notes/how.md"), "how to review\n");
        let blobs = BlobStore::open(dir.path().join("data/fleet/blobs")).unwrap();
        let sources = Sources {
            instructions: Some(codex.join("AGENTS.md")),
            skills: Some(codex.join("skills")),
            skip: vec![".system".parse().unwrap(), "paseo*".parse().unwrap()],
            mcp: std::collections::BTreeMap::default(),
        };
        Self {
            dir,
            blobs,
            sources,
            published: None,
        }
    }

    pub(crate) fn codex(&self) -> PathBuf {
        self.dir.path().join("codex")
    }

    /// Bundle again, as the home does after a change.
    pub(crate) fn publish(&mut self) -> Bundled {
        let bundled = bundle(&self.sources, &self.blobs, self.published.as_ref()).unwrap();
        self.published = Some(bundled.manifest.clone());
        bundled
    }
}

/// A member: its own store, managed record and one Claude home.
pub(crate) struct Member {
    pub(crate) dir: TempDir,
    pub(crate) blobs: BlobStore,
    pub(crate) managed: Managed,
}

impl Member {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(dir.path().join("data/fleet/blobs")).unwrap();
        Self {
            dir,
            blobs,
            managed: Managed::default(),
        }
    }

    pub(crate) fn claude(&self) -> Target {
        Target {
            root: self.dir.path().join("claude"),
            instructions: PathBuf::from("CLAUDE.md"),
            skills: PathBuf::from("skills"),
        }
    }

    /// Fetch every blob of `manifest` it lacks from `home`, chunk by
    /// chunk, as over `fleet.blob`; how many it fetched.
    pub(crate) fn fetch(&self, manifest: &Manifest, home: &BlobStore) -> usize {
        let mut fetched = 0_usize;
        for file in manifest.files.values() {
            if self.blobs.contains(&file.hash) {
                continue;
            }
            let mut writer = self.blobs.writer(file.hash).unwrap();
            let mut offset = 0_u64;
            loop {
                let (data, total) = home
                    .read_chunk(&file.hash, offset, BlobChunk::MAX_LEN)
                    .unwrap();
                std::io::Write::write_all(&mut writer, &data).unwrap();
                offset = offset
                    .checked_add(u64::try_from(data.len()).unwrap())
                    .unwrap();
                if offset >= total {
                    break;
                }
            }
            let _stored = writer.finish().unwrap();
            fetched = fetched.checked_add(1).unwrap();
        }
        fetched
    }

    /// Plan and apply `manifest` into `targets`.
    pub(crate) fn apply(
        &mut self,
        manifest: &Manifest,
        targets: &[Target],
    ) -> vessel_fleet::Applied {
        let plan = vessel_fleet::plan(manifest, targets, &self.managed, &[]).unwrap();
        vessel_fleet::apply(&plan, &self.blobs, &mut self.managed, &[])
    }
}
