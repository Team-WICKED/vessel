//! The book: checking entries, the layers and their restrictions, edits
//! that keep the user's file intact.

use std::path::{Path, PathBuf};

use vessel_proto::NodeId;

use super::*;
use crate::address::{NodeAddress, RemotePath, SshTarget};

const ID: &str = "6f1c3a52-7e1b-4c55-9a43-2f9a0c5e8d11";

fn local() -> NodeUrl {
    "127.0.0.1:7077".parse().unwrap()
}

/// A temporary directory with `nodes.toml` holding `text`.
fn book_with(text: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nodes.toml");
    std::fs::write(&path, text).unwrap();
    (dir, path)
}

fn open(text: &str) -> Result<BookFile, BookError> {
    let (_dir, path) = book_with(text);
    BookFile::open(&path)
}

fn name(text: &str) -> NodeName {
    text.parse().unwrap()
}

const EXAMPLE: &str = r#"
# My machines.
default_node = "gpu-box"

[nodes.gpu-box]
ssh = "me@gpu-box"   # the big one
data_dir = "~/.vessel-gpu"
id = "6f1c3a52-7e1b-4c55-9a43-2f9a0c5e8d11"

[nodes.lab]
url = "ws://10.0.0.5:7077"
token = { file = "~/.vessel/tokens/lab" }
"#;

#[test]
fn a_book_reads_into_checked_entries() {
    let file = open(EXAMPLE).unwrap();
    let book = file.book();
    assert_eq!(book.default_node, Some(name("gpu-box")));
    let (_, gpu) = book.get("gpu-box").unwrap();
    let NodeAddress::Ssh(target) = &gpu.address else {
        panic!("{gpu:?}");
    };
    assert_eq!(target.user.as_deref(), Some("me"));
    assert_eq!(target.host, "gpu-box");
    assert_eq!(
        target.data_dir,
        Some("~/.vessel-gpu".parse::<RemotePath>().unwrap())
    );
    assert_eq!(gpu.id, Some(ID.parse().unwrap()));
    assert_eq!(gpu.token, None);
    let (_, lab) = book.get("lab").unwrap();
    assert!(matches!(lab.address, NodeAddress::Ws(_)));
    let token = lab.token.as_ref().unwrap();
    assert_eq!(
        token.path(Some(Path::new("/home/me"))),
        PathBuf::from("/home/me/.vessel/tokens/lab")
    );
    assert_eq!(token.path(None), PathBuf::from("~/.vessel/tokens/lab"));
    assert!(book.get("nope").is_none());
    assert!(book.get("not a name").is_none());
}

#[test]
fn a_missing_book_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let file = BookFile::open(&dir.path().join("absent.toml")).unwrap();
    assert_eq!(file.book(), &NodeBook::default());
    assert_eq!(
        NodeBook::path_in(Path::new("/cfg")),
        PathBuf::from("/cfg/vessel/nodes.toml")
    );
}

#[test]
fn typos_and_bad_entries_are_errors_that_say_where() {
    let typo = open("[nodes.box]\nsssh = \"box\"\n").unwrap_err();
    let BookError::Syntax { message, .. } = &typo else {
        panic!("{typo:?}");
    };
    assert!(message.contains("sssh"), "{message}");
    assert!(message.contains("line 2"), "{message}");

    let cases = [
        ("[nodes.box]\n", EntryProblem::NoTransport),
        (
            "[nodes.box]\nssh = \"box\"\nurl = \"ws://box\"\n",
            EntryProblem::BothTransports,
        ),
        (
            "[nodes.box]\nssh = \"box\"\ntoken = { file = \"t\" }\n",
            EntryProblem::TokenOverSsh,
        ),
        (
            "[nodes.box]\nurl = \"ws://box\"\ndata_dir = \"~/v\"\n",
            EntryProblem::DataDirOverWs,
        ),
        (
            "[nodes.box]\nssh = \"box\"\nid = \"12\"\n",
            EntryProblem::Id("12".to_owned()),
        ),
    ];
    for (text, expected) in cases {
        let error = open(text).unwrap_err();
        let BookError::Entry { name, problem, .. } = &error else {
            panic!("{text}: {error:?}");
        };
        assert_eq!(name, "box");
        assert_eq!(problem, &expected, "{text}");
    }
    for text in [
        "[nodes.box]\nssh = \"-oProxyCommand=touch /tmp/x\"\n",
        "[nodes.box]\nurl = \"ftp://box\"\n",
        "[nodes.box]\nssh = \"box\"\ndata_dir = \"relative\"\n",
        "[nodes.\"two words\"]\nssh = \"box\"\n",
    ] {
        assert!(matches!(open(text), Err(BookError::Entry { .. })), "{text}");
    }
    assert!(matches!(
        open("default_node = \"ghost\"\n"),
        Err(BookError::UnknownDefault { .. })
    ));
}

#[test]
fn node_names_are_plain_words() {
    for good in ["gpu-box", "lab.2", "a_b", "9"] {
        assert!(good.parse::<NodeName>().is_ok(), "{good}");
    }
    for bad in ["", "-x", ".x", "a b", "me@box", "ws://x", "a:1", "ü"] {
        assert!(bad.parse::<NodeName>().is_err(), "{bad}");
    }
}

/// A project file with `text`, in a project directory below `root`.
fn project_in(root: &Path, text: &str) -> PathBuf {
    let project = root.join("work").join("repo");
    std::fs::create_dir_all(project.join(".vessel")).unwrap();
    std::fs::write(project.join(".vessel").join("config.toml"), text).unwrap();
    project
}

#[test]
fn layers_pick_flag_then_project_then_user_then_local() {
    let (dir, path) = book_with(EXAMPLE);
    let book = BookFile::open(&path).unwrap().book().clone();
    let project_dir = project_in(dir.path(), "default_node = \"lab\"\n");
    let nested = project_dir.join("src").join("deep");
    std::fs::create_dir_all(&nested).unwrap();
    let project = ProjectConfig::find(&nested, None).unwrap().unwrap();
    assert_eq!(project.default_node, Some(name("lab")));

    // --node / VESSEL_NODE: a book name first.
    let named = resolve(&book, &path, Some(&project), Some("gpu-box"), &local()).unwrap();
    assert_eq!(named.name, Some(name("gpu-box")));
    assert_eq!(named.origin, Origin::Named);
    assert_eq!(named.pinned, Some(ID.parse().unwrap()));
    // Then an address.
    let address = resolve(
        &book,
        &path,
        Some(&project),
        Some("ssh://box:2222"),
        &local(),
    )
    .unwrap();
    assert_eq!(address.name, None);
    let NodeAddress::Ssh(target) = &address.address else {
        panic!("{address:?}");
    };
    assert_eq!((target.host.as_str(), target.port), ("box", Some(2222)));
    let ws = resolve(&book, &path, None, Some("other-host"), &local()).unwrap();
    assert_eq!(ws.address.to_string(), "ws://other-host:7077/rpc");
    assert!(matches!(
        resolve(&book, &path, None, Some("ftp://x"), &local()),
        Err(BookError::Unknown { .. })
    ));

    // The project's default beats the user's.
    let from_project = resolve(&book, &path, Some(&project), None, &local()).unwrap();
    assert_eq!(from_project.name, Some(name("lab")));
    assert_eq!(from_project.origin, Origin::Project(project.path.clone()));
    assert!(from_project.token.is_some());

    let from_user = resolve(&book, &path, None, None, &local()).unwrap();
    assert_eq!(from_user.name, Some(name("gpu-box")));
    assert_eq!(from_user.origin, Origin::User);

    let nothing = resolve(&NodeBook::default(), &path, None, None, &local()).unwrap();
    assert_eq!(nothing.origin, Origin::Default);
    assert_eq!(nothing.address, NodeAddress::Ws(local()));
}

#[test]
fn a_book_name_shadows_the_same_text_as_a_host() {
    let (_dir, path) = book_with("[nodes.gpu-box]\nssh = \"me@10.0.0.9\"\n");
    let book = BookFile::open(&path).unwrap().book().clone();
    let resolved = resolve(&book, &path, None, Some("gpu-box"), &local()).unwrap();
    assert!(matches!(resolved.address, NodeAddress::Ssh(_)));
}

#[test]
fn a_project_may_only_choose_among_the_users_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nodes.toml");
    std::fs::write(&path, EXAMPLE).unwrap();
    let book = BookFile::open(&path).unwrap().book().clone();

    // Adding a node, a transport or a token source: refused outright.
    for text in [
        "[nodes.evil]\nssh = \"attacker.example\"\n",
        "[nodes.gpu-box]\nssh = \"attacker.example\"\n",
        "default_node = \"lab\"\ntoken = { file = \"/etc/shadow\" }\n",
        "url = \"ws://attacker.example\"\n",
    ] {
        let root = tempfile::tempdir().unwrap();
        let project = project_in(root.path(), text);
        let error = ProjectConfig::find(&project, None).unwrap_err();
        let BookError::Syntax { message, .. } = &error else {
            panic!("{text}: {error:?}");
        };
        assert!(message.contains("may only set `default_node`"), "{message}");
    }

    // Naming a node the user does not have.
    let root = tempfile::tempdir().unwrap();
    let project_dir = project_in(root.path(), "default_node = \"attacker\"\n");
    let project = ProjectConfig::find(&project_dir, None).unwrap().unwrap();
    let error = resolve(&book, &path, Some(&project), None, &local()).unwrap_err();
    let BookError::UnknownDefault { path: from, .. } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(from, &project.path);
    // An explicit --node still works in such a project.
    assert!(resolve(&book, &path, Some(&project), Some("lab"), &local()).is_ok());
}

#[test]
fn the_project_walk_stops_below_home() {
    let home = tempfile::tempdir().unwrap();
    // The node's data directory, `~/.vessel`, is not a project.
    std::fs::create_dir_all(home.path().join(".vessel")).unwrap();
    std::fs::write(
        home.path().join(".vessel").join("config.toml"),
        "default_node = \"x\"\n",
    )
    .unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    assert_eq!(ProjectConfig::find(&work, Some(home.path())).unwrap(), None);
    // Without that boundary it would have been found.
    assert!(ProjectConfig::find(&work, None).unwrap().is_some());
}

#[test]
fn edits_keep_comments_and_layout() {
    let (_dir, path) = book_with(EXAMPLE);
    let mut file = BookFile::open(&path).unwrap();
    let mut target: SshTarget = "ssh://ci@build:2222".parse().unwrap();
    target.data_dir = Some("/srv/vessel".parse().unwrap());
    let entry = BookEntry {
        address: NodeAddress::Ssh(target),
        token: None,
        id: None,
    };
    file.add(&name("build"), &entry).unwrap();
    assert_eq!(file.book().get("build").unwrap().1, &entry);

    let id: NodeId = "0b5e4f3a-1111-4222-8333-944455556666".parse().unwrap();
    file.pin("build", id).unwrap();
    file.pin("build", id).unwrap();
    let other = NodeId::random();
    assert!(matches!(
        file.pin("build", other),
        Err(BookError::PinnedElsewhere { .. })
    ));
    file.set_default(Some("build")).unwrap();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# My machines."), "{text}");
    assert!(text.contains("# the big one"), "{text}");
    assert!(text.contains("[nodes.build]"), "{text}");
    assert!(text.contains("default_node = \"build\""), "{text}");
    assert!(!text.contains("[nodes]\n"), "{text}");
    // Re-read from disk: the same book.
    let reread = BookFile::open(&path).unwrap();
    assert_eq!(reread.book(), file.book());
    assert_eq!(reread.book().get("build").unwrap().1.id, Some(id));

    // Removing the default node clears the default.
    file.remove("build").unwrap();
    assert_eq!(file.book().default_node, None);
    assert!(file.book().get("build").is_none());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# the big one"), "{text}");
    assert!(!text.contains("build"), "{text}");
}

#[test]
fn edits_refuse_what_does_not_fit() {
    let dir = tempfile::tempdir().unwrap();
    // The directory does not exist yet: the first edit creates it.
    let path = dir.path().join("vessel").join("nodes.toml");
    let mut file = BookFile::open(&path).unwrap();
    let ws = BookEntry {
        address: NodeAddress::Ws("ws://10.0.0.5:7077".parse().unwrap()),
        token: Some(TokenSource::File(PathBuf::from("~/.vessel/tokens/lab"))),
        id: None,
    };
    file.add(&name("lab"), &ws).unwrap();
    assert!(path.exists());
    assert!(matches!(
        file.add(&name("lab"), &ws),
        Err(BookError::Exists { .. })
    ));
    assert!(matches!(
        file.remove("ghost"),
        Err(BookError::Missing { .. })
    ));
    assert!(matches!(
        file.set_default(Some("ghost")),
        Err(BookError::Missing { .. })
    ));
    let ssh_with_token = BookEntry {
        address: NodeAddress::Ssh("box".parse().unwrap()),
        token: Some(TokenSource::File(PathBuf::from("t"))),
        id: None,
    };
    assert!(matches!(
        file.add(&name("box"), &ssh_with_token),
        Err(BookError::Entry {
            problem: EntryProblem::TokenOverSsh,
            ..
        })
    ));
    // Nothing half-written was left behind.
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, vec![std::ffi::OsString::from("nodes.toml")]);
    let reread = BookFile::open(&path).unwrap();
    assert_eq!(reread.book().get("lab").unwrap().1, &ws);
}

#[test]
fn an_inline_nodes_table_is_edited_in_place() {
    let (_dir, path) = book_with("nodes = { a = { ssh = \"a\" } }\n");
    let mut file = BookFile::open(&path).unwrap();
    file.add(
        &name("b"),
        &BookEntry {
            address: NodeAddress::Ssh("b".parse().unwrap()),
            token: None,
            id: None,
        },
    )
    .unwrap();
    let reread = BookFile::open(&path).unwrap();
    assert!(reread.book().get("a").is_some());
    assert!(reread.book().get("b").is_some());
}
