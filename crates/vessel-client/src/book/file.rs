//! Reading and editing the book's files.
//!
//! The user's book is read into a [`NodeBook`] and kept as a `toml_edit`
//! document, so edits change only the lines they must: comments, order
//! and layout stay as the user wrote them (a comment attached to a line
//! that is removed goes with it). Every edit is checked by
//! reading the edited text back as a book before it is written, and the
//! write is a rename of a fully written temporary file, so a crash leaves
//! the old file or the new one, never half of one.
//!
//! Two `vessel` processes editing at once: each reads, edits and renames;
//! the later rename wins and the other edit is lost. Edits are rare and by
//! hand (`vessel nodes add`) or once per node (the pinned id), so no lock.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table, TableLike, value};
use vessel_proto::NodeId;

use super::content::{BookContent, ProjectContent, check, entry_table};
use super::{BookEntry, BookError, EntryProblem, NodeBook, NodeName};

/// The user's book file: its checked content, and the document for edits.
#[derive(Debug)]
pub struct BookFile {
    path: PathBuf,
    document: DocumentMut,
    book: NodeBook,
}

impl BookFile {
    /// Read the book at `path`; a missing file is an empty book.
    ///
    /// # Errors
    /// The file cannot be read, is not TOML, or does not describe nodes.
    pub fn open(path: &Path) -> Result<Self, BookError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(BookError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        Self::parse(path, &text)
    }

    fn parse(path: &Path, text: &str) -> Result<Self, BookError> {
        let syntax = |error: toml_edit::TomlError| BookError::Syntax {
            path: path.to_owned(),
            message: error.to_string(),
        };
        let document: DocumentMut = text.parse().map_err(syntax)?;
        let content: BookContent =
            toml_edit::de::from_str(text).map_err(|error| BookError::Syntax {
                path: path.to_owned(),
                message: error.to_string(),
            })?;
        let book = check(path, content)?;
        Ok(Self {
            path: path.to_owned(),
            document,
            book,
        })
    }

    /// The book.
    #[must_use]
    pub const fn book(&self) -> &NodeBook {
        &self.book
    }

    /// Where it is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Add `entry` as `name` and write the file.
    ///
    /// # Errors
    /// The name is taken, or the file cannot be written.
    pub fn add(&mut self, name: &NodeName, entry: &BookEntry) -> Result<(), BookError> {
        if self.book.nodes.contains_key(name) {
            return Err(BookError::Exists {
                path: self.path.clone(),
                name: name.clone(),
            });
        }
        let table = entry_table(entry).map_err(|problem| BookError::Entry {
            path: self.path.clone(),
            name: name.to_string(),
            problem,
        })?;
        let _absent = self
            .nodes_table()?
            .insert(name.as_str(), Item::Table(table));
        self.save()
    }

    /// Remove `name` (and `default_node` if it named it) and write the
    /// file.
    ///
    /// # Errors
    /// No such node, or the file cannot be written.
    pub fn remove(&mut self, name: &str) -> Result<(), BookError> {
        let Some((name, _entry)) = self.book.get(name) else {
            return Err(BookError::Missing {
                path: self.path.clone(),
                name: name.to_owned(),
            });
        };
        let name = name.clone();
        let _removed = self.nodes_table()?.remove(name.as_str());
        if self.book.default_node.as_ref() == Some(&name) {
            let _default = self.document.remove("default_node");
        }
        self.save()
    }

    /// Make `name` the default node (`None`: no default) and write the
    /// file.
    ///
    /// # Errors
    /// No such node, or the file cannot be written.
    pub fn set_default(&mut self, name: Option<&str>) -> Result<(), BookError> {
        match name {
            Some(name) => {
                let Some((name, _entry)) = self.book.get(name) else {
                    return Err(BookError::Missing {
                        path: self.path.clone(),
                        name: name.to_owned(),
                    });
                };
                let name = name.as_str();
                match self.document.get_mut("default_node") {
                    // Replace only the value, so the comments around the key
                    // (often the file's opening comment) stay.
                    Some(item) => {
                        let decor = item.as_value().map(|old| old.decor().clone());
                        *item = value(name);
                        if let (Some(decor), Some(new)) = (decor, item.as_value_mut()) {
                            *new.decor_mut() = decor;
                        }
                    }
                    None => {
                        let _absent = self.document.insert("default_node", value(name));
                    }
                }
            }
            None => {
                let _default = self.document.remove("default_node");
            }
        }
        self.save()
    }

    /// Record `id` as the id of `name`, seen on first contact, and write
    /// the file. Nothing to do if it is already recorded.
    ///
    /// # Errors
    /// No such node, a different id is pinned, or the file cannot be
    /// written.
    pub fn pin(&mut self, name: &str, id: NodeId) -> Result<(), BookError> {
        let Some((name, entry)) = self.book.get(name) else {
            return Err(BookError::Missing {
                path: self.path.clone(),
                name: name.to_owned(),
            });
        };
        match entry.id {
            Some(pinned) if pinned == id => return Ok(()),
            Some(pinned) => {
                return Err(BookError::PinnedElsewhere {
                    path: self.path.clone(),
                    name: name.clone(),
                    pinned,
                    found: id,
                });
            }
            None => {}
        }
        let name = name.clone();
        if let Some(table) = self
            .nodes_table()?
            .get_mut(name.as_str())
            .and_then(Item::as_table_like_mut)
        {
            let _absent = table.insert("id", value(id.to_string()));
        }
        self.save()
    }

    /// The `nodes` table (a `[nodes.*]` section or an inline table),
    /// created if missing; an implicit table, so no bare `[nodes]` header
    /// is written.
    fn nodes_table(&mut self) -> Result<&mut dyn TableLike, BookError> {
        let path = self.path.clone();
        self.document
            .entry("nodes")
            .or_insert_with(|| {
                let mut nodes = Table::new();
                nodes.set_implicit(true);
                Item::Table(nodes)
            })
            .as_table_like_mut()
            // A `nodes` that is not a table never passed `check`.
            .ok_or_else(|| BookError::Syntax {
                path,
                message: "`nodes` is not a table".to_owned(),
            })
    }

    /// Check the edited document as a book, then replace the file with it.
    fn save(&mut self) -> Result<(), BookError> {
        let text = self.document.to_string();
        let checked = Self::parse(&self.path, &text)?;
        write_atomically(&self.path, &text).map_err(|source| BookError::Io {
            path: self.path.clone(),
            source,
        })?;
        *self = checked;
        Ok(())
    }
}

/// A project's `.vessel/config.toml`, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    /// Where it is.
    pub path: PathBuf,
    /// The node this project runs on, by name in the user's book.
    pub default_node: Option<NodeName>,
}

impl ProjectConfig {
    /// The nearest `.vessel/config.toml` in `start` or its parents. The
    /// walk stops below `home`: `~/.vessel` is the data directory of the
    /// node on this machine, not a project.
    ///
    /// # Errors
    /// A project file that cannot be read, is not TOML, or sets anything
    /// but a valid `default_node`.
    pub fn find(start: &Path, home: Option<&Path>) -> Result<Option<Self>, BookError> {
        for directory in start.ancestors() {
            if Some(directory) == home {
                break;
            }
            let path = directory.join(".vessel").join("config.toml");
            match std::fs::read_to_string(&path) {
                Ok(text) => return Self::parse(&path, &text).map(Some),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                // A `.vessel` that is not a directory, or one we may not read:
                // not this project's file.
                Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {}
                Err(source) => return Err(BookError::Io { path, source }),
            }
        }
        Ok(None)
    }

    fn parse(path: &Path, text: &str) -> Result<Self, BookError> {
        let content: ProjectContent =
            toml_edit::de::from_str(text).map_err(|error| BookError::Syntax {
                path: path.to_owned(),
                message: format!(
                    "{error}(a project file may only set `default_node`, to a node in your book)"
                ),
            })?;
        let default_node = content
            .default_node
            .map(|name| {
                name.parse().map_err(|problem| BookError::Entry {
                    path: path.to_owned(),
                    name,
                    problem: EntryProblem::Name(problem),
                })
            })
            .transpose()?;
        Ok(Self {
            path: path.to_owned(),
            default_node,
        })
    }
}

/// Replace `path` with `text`: write a temporary file next to it, flush it
/// to disk, rename it over the old one. Creates the directory if needed.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(directory)?;
    let mut temporary = tempfile_in(directory)?;
    temporary.1.write_all(text.as_bytes())?;
    temporary.1.sync_all()?;
    drop(temporary.1);
    std::fs::rename(&temporary.0, path).inspect_err(|_failed| {
        let _gone = std::fs::remove_file(&temporary.0);
    })
}

/// A new file in `directory` with a name nothing else uses.
fn tempfile_in(directory: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    let mut attempt = 0_u32;
    loop {
        let name = format!(".nodes.toml.{}.{attempt}.tmp", std::process::id());
        let path = directory.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt < 16 => {
                attempt = attempt.saturating_add(1);
            }
            Err(error) => return Err(error),
        }
    }
}
