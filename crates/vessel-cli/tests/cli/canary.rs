//! Milestone 4's done criterion, "no secret value appears in a bundle, a
//! log, `profile.list` or a command line", checked across the whole flow
//! with canary values (distinctive, random per run).
//!
//! - Planted: a `profiles.toml` environment value; two `secrets.toml`
//!   values behind the fleet's MCP servers (a stdio server's environment
//!   and an HTTP server's header, by secret reference); and credentials
//!   Vessel makes itself, read from where they belong: a member's device
//!   credential on its home, the home's on the member, and a session
//!   agent's credential.
//! - Driven: `profile.list` and `profile.reload` (also on a file that does
//!   not parse); terminal and structured sessions of Claude Code and Codex
//!   profiles; a fleet bundle applied by a member; `secret.copy` to it and
//!   its refusals; a session transfer from it and one between profiles;
//!   `vessel mcp` with an agent's credential, and refused without the right
//!   session.
//! - Captured: the nodes' tracing at debug level; every CLI command's
//!   output, with debug logging on; the fakes' arguments; the sessions'
//!   logs; and every file of both data directories (the event store, the
//!   bundle's manifest and blobs, the fleet's state, the harness homes),
//!   except the few files a value belongs in. Every session's agent
//!   credential is a canary too.
//! - Non-vacuous: each canary is shown in its intended place, and only
//!   there: the harness's environment (a profile's value in every one; the
//!   servers' values and the agent's credential in Codex's terminal, which
//!   reads them from there), a structured agent's MCP servers over ACP,
//!   the owner-only MCP file of Claude Code's terminal, the member's
//!   `secrets.toml` and the credential files. The fakes report their
//!   arguments, whole environment and ACP servers to a file outside every
//!   searched directory (`report <path>`, `vessel_testkit::write_report`),
//!   so no value passes through a session's log and the scan can come
//!   last, over everything.
//!
//! Only named profiles with temporary homes are used; the nodes' default
//! profiles live under the fixtures' temporary user homes.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use vessel_client::NodeClient;
use vessel_harness::Lookup;
use vessel_harness::fakes::{Role, claude_project, install};
use vessel_node::{DataDir, FleetConfig, FleetTargets, VesselMcp};
use vessel_proto::{
    AgentEvent, ClientInfo, ContentBlock, EventSeq, HarnessKind, ImportSession, ListPeers, NodeId,
    RelayToken, RouteUrl, SessionEvent, SessionId, SessionInput, SessionRef, TerminalBytes,
    TerminalInput,
};
use vessel_relay::{LinkState, RelayConfig};
use vessel_testkit::Fake;

use crate::support::{Fixture, Ran, within};

const VESSEL: &str = env!("CARGO_BIN_EXE_vessel");

// ---------------------------------------------------------------------
// What is captured, and the canaries.

/// Everything the in-process nodes trace, at debug level. Set once for
/// this test process: other tests' records land here too, harmlessly,
/// since the canaries are this test's own.
fn tracing_log() -> &'static Arc<Mutex<Vec<u8>>> {
    static LOG: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        tracing_subscriber::fmt()
            .with_max_level(tracing_subscriber::filter::LevelFilter::DEBUG)
            .with_ansi(false)
            .with_writer(move || Capture(Arc::clone(&sink)))
            .try_init()
            .expect("no other test in this binary sets a global subscriber");
        log
    })
}

fn traced() -> Vec<u8> {
    tracing_log()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A canary: `CANARY<kind><32 random hex digits>`.
fn canary(kind: &str) -> String {
    format!(
        "CANARY{kind}{}",
        NodeId::random().to_string().replace('-', "").to_uppercase()
    )
}

/// The values planted before the run.
struct Planted {
    /// A profile's environment value (`CANARY_PROFILE`).
    profile: String,
    /// `secrets.toml`'s `DOCS_TOKEN`, a stdio MCP server's environment.
    docs: String,
    /// `secrets.toml`'s `WEB_TOKEN`, an HTTP MCP server's header.
    web: String,
}

/// What the run printed or wrote where no secret value may be, by where.
#[derive(Default)]
struct Captured(Vec<(String, Vec<u8>)>);

impl Captured {
    fn text(&mut self, what: impl Into<String>, text: &str) {
        self.0.push((what.into(), text.as_bytes().to_vec()));
    }

    fn ran(&mut self, what: &str, ran: &Ran) {
        self.text(format!("`vessel {what}` stdout"), &ran.stdout);
        self.text(format!("`vessel {what}` stderr"), &ran.stderr);
    }

    /// Every regular file below `root`, except those in `intended` (or
    /// below one), where a value belongs.
    fn tree(&mut self, root: &Path, intended: &[PathBuf]) {
        let mut pending = vec![root.to_owned()];
        while let Some(path) = pending.pop() {
            if intended.iter().any(|place| path.starts_with(place)) {
                continue;
            }
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                for entry in std::fs::read_dir(&path).unwrap() {
                    pending.push(entry.unwrap().path());
                }
            } else if metadata.is_file()
                && let Ok(bytes) = std::fs::read(&path)
            {
                self.0.push((path.display().to_string(), bytes));
            }
        }
    }

    /// No canary in anything captured.
    fn assert_clean(&self, canaries: &[(&str, &str)]) {
        assert!(
            self.0.len() > 50,
            "too little was captured to mean anything"
        );
        for (what, bytes) in &self.0 {
            for (name, value) in canaries {
                assert!(
                    !contains(bytes, value.as_bytes()),
                    "the {name} canary appears in {what}"
                );
            }
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// The secret in a credential file Vessel wrote (`secret = "..."`).
fn credential_secret(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap();
    text.lines()
        .find_map(|line| line.strip_prefix("secret = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_else(|| panic!("no secret in {}", path.display()))
        .to_owned()
}

#[cfg(unix)]
fn assert_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{}", path.display());
}

#[cfg(not(unix))]
fn assert_owner_only(_path: &Path) {}

// ---------------------------------------------------------------------
// Two nodes on a relay.

struct Relay {
    url: RouteUrl,
    shutdown: CancellationToken,
    serving: tokio::task::JoinHandle<io::Result<()>>,
}

async fn relay() -> Relay {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let shutdown = CancellationToken::new();
    let serving = tokio::spawn(vessel_relay::serve(
        listener,
        RelayConfig::new(RelayToken::new("canary-relay").unwrap()),
        shutdown.clone(),
    ));
    Relay {
        url,
        shutdown,
        serving,
    }
}

/// Claude Code and Codex fakes, both modes, in `bin`.
fn fakes(bin: &Path) {
    let (tui, acp) = (Fake::Tui.path().unwrap(), Fake::Acp.path().unwrap());
    for harness in [HarnessKind::ClaudeCode, HarnessKind::Codex] {
        drop(install(bin, harness, Role::Terminal, &tui).unwrap());
        drop(install(bin, harness, Role::Structured, &acp).unwrap());
    }
}

/// A node linked to `relay` with the fakes in `bin`, fleet files written
/// into `<data>/fleet-claude`, `vessel mcp` as its agents' tools, and
/// `prepare` run on its data directory first.
async fn node(relay: &RouteUrl, bin: &Path, prepare: impl FnOnce(&Path)) -> Fixture {
    let fixture = Fixture::start_with(|config, data| {
        prepare(data);
        let token = data.join("relay-token");
        std::fs::write(&token, "canary-relay\n").unwrap();
        config.relays = vec![relay.clone()];
        config.relay_token_file = Some(token);
        let applied = data.join("fleet-claude");
        std::fs::create_dir_all(&applied).unwrap();
        config.fleet = FleetConfig {
            targets: FleetTargets::Only(vec![(HarnessKind::ClaudeCode, applied)]),
            poll: Duration::from_millis(50),
            ..FleetConfig::default()
        };
        config.harness_lookup = Lookup::new([bin.to_owned()]);
        config.vessel_mcp = Some(VesselMcp::vessel(PathBuf::from(VESSEL)));
    })
    .await;
    for (_relay, mut state) in fixture.node().relay_links() {
        let _registered = state
            .wait_for(|state| *state == LinkState::Registered)
            .await
            .unwrap();
    }
    fixture
}

/// Two Claude Code profiles and a Codex one, homes under `<data>/homes`,
/// the profile canary in two of them.
fn profiles_toml(data: &Path, value: &str) -> String {
    let home = |name: &str| {
        let home = data.join("homes").join(name);
        std::fs::create_dir_all(&home).unwrap();
        home.display().to_string()
    };
    format!(
        "[profiles.claude-one]\nharness = \"claude\"\n\
         env = {{ CLAUDE_CONFIG_DIR = '{}', CANARY_PROFILE = '{value}' }}\n\n\
         [profiles.claude-two]\nharness = \"claude\"\nenv = {{ CLAUDE_CONFIG_DIR = '{}' }}\n\n\
         [profiles.codex-one]\nharness = \"codex\"\n\
         env = {{ CODEX_HOME = '{}', CANARY_PROFILE = '{value}' }}\n",
        home("claude-one"),
        home("claude-two"),
        home("codex-one"),
    )
}

async fn owner(fixture: &Fixture) -> NodeClient {
    let token = DataDir::new(fixture.data())
        .read_token()
        .unwrap()
        .expose()
        .parse()
        .unwrap();
    let url = fixture.node_address().parse().unwrap();
    let client = ClientInfo {
        name: "canary".to_owned(),
        version: "0".to_owned(),
    };
    NodeClient::connect(&url, &token, client).await.unwrap()
}

async fn until(what: &str, mut ready: impl AsyncFnMut() -> bool) {
    for _ in 0..800 {
        if ready().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never: {what}");
}

/// `vessel <args>` against `fixture`, debug logging on, captured.
async fn vessel(fixture: &Fixture, captured: &mut Captured, args: &[&str], stdin: &[u8]) -> Ran {
    let ran = fixture.vessel_debug(args, stdin).await;
    captured.ran(&args.join(" "), &ran);
    ran
}

// ---------------------------------------------------------------------
// Sessions and what the fakes say.

/// The terminal output and agent messages of session `id` so far.
async fn session_text(client: &NodeClient, id: SessionId) -> String {
    let mut text = String::new();
    let mut from = EventSeq::ZERO;
    loop {
        let page = client.read(id, from, 1000).await.unwrap();
        for record in &page.records {
            if let SessionEvent::Terminal(output) = &record.event {
                text.push_str(&String::from_utf8_lossy(output.data.bytes()));
            }
            if let SessionEvent::Agent {
                event:
                    AgentEvent::MessageChunk {
                        content: ContentBlock::Text { text: chunk },
                    },
            } = &record.event
            {
                text.push_str(chunk);
            }
        }
        if page.at_end || page.records.is_empty() {
            return text;
        }
        from = page.next;
    }
}

/// A session and its mode.
#[derive(Debug, Clone, Copy)]
struct Session {
    id: SessionId,
    terminal: bool,
}

impl Session {
    fn input(self, text: &str) -> SessionInput {
        if self.terminal {
            SessionInput::Terminal {
                input: TerminalInput::Data {
                    data: TerminalBytes::new(Bytes::from(format!("{text}\r"))),
                },
            }
        } else {
            SessionInput::Prompt {
                content: vec![ContentBlock::Text {
                    text: text.to_owned(),
                }],
            }
        }
    }
}

/// What a fake was started with (`report <path>`): its arguments, its
/// whole environment and, for an ACP fake, the MCP servers it was given,
/// values included. The fake writes it to a file outside every directory
/// this test searches, so no value passes through the session's log.
struct Report {
    args: Vec<String>,
    env: BTreeMap<String, String>,
    mcp: Option<Value>,
}

async fn report(client: &NodeClient, session: Session, dir: &Path) -> Report {
    let path = dir.join(format!("{}.json", session.id));
    let question = format!("report {}", path.display());
    client
        .send(session.id, session.input(&question))
        .await
        .unwrap();
    until(&format!("session {}'s report", session.id), async || {
        path.is_file()
    })
    .await;
    let report: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let strings = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    Report {
        args: report["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(strings)
            .collect(),
        env: report["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| (name.clone(), strings(value)))
            .collect(),
        mcp: report.get("mcp").cloned(),
    }
}

/// The value of the variable `name` given to an MCP server in an ACP
/// server list (`env: [{name, value}]`), wherever it is.
fn mcp_variable(servers: &Value, name: &str) -> Option<String> {
    match servers {
        Value::Object(fields) => {
            if fields.get("name").and_then(Value::as_str) == Some(name)
                && let Some(value) = fields.get("value").and_then(Value::as_str)
            {
                return Some(value.to_owned());
            }
            fields.values().find_map(|field| mcp_variable(field, name))
        }
        Value::Array(items) => items.iter().find_map(|item| mcp_variable(item, name)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

/// `vessel run <harness> --profile <profile> [--structured] --detach`.
async fn run(
    fixture: &Fixture,
    captured: &mut Captured,
    harness: &str,
    profile: &str,
    terminal: bool,
) -> Session {
    let cwd = fixture.cwd();
    let mut args = vec![
        "run",
        harness,
        "--profile",
        profile,
        "--detach",
        "--cwd",
        &cwd,
    ];
    if !terminal {
        args.push("--structured");
    }
    let ran = vessel(fixture, captured, &args, b"").await;
    assert!(ran.ok, "{}", ran.stderr);
    Session {
        id: ran.stdout.trim().parse().unwrap(),
        terminal,
    }
}

// ---------------------------------------------------------------------
// The flows.

/// The home's sources, its MCP servers with secret references, and its
/// secrets, set through the CLI from standard input.
async fn make_home(home: &Fixture, planted: &Planted, captured: &mut Captured) {
    let sources = home.data().join("sources");
    std::fs::create_dir_all(sources.join("skills/review")).unwrap();
    std::fs::write(sources.join("AGENTS.md"), "fleet instructions\n").unwrap();
    std::fs::write(
        sources.join("skills/review/SKILL.md"),
        "---\nname: review\ndescription: review code\n---\n# review\n",
    )
    .unwrap();
    let (instructions, skills) = (
        sources.join("AGENTS.md").display().to_string(),
        sources.join("skills").display().to_string(),
    );
    let args = [
        "fleet",
        "init",
        "--instructions",
        &instructions,
        "--skills",
        &skills,
    ];
    let made = vessel(home, captured, &args, b"").await;
    assert!(made.ok, "{}", made.stderr);
    let toml = home.data().join("fleet.toml");
    let mut text = std::fs::read_to_string(&toml).unwrap();
    text.push_str(
        "\n[mcp.servers.docs]\ntype = \"stdio\"\ncommand = \"docs-mcp\"\n\
         env = { DOCS_TOKEN = { secret = \"DOCS_TOKEN\" } }\n\n\
         [mcp.servers.web]\ntype = \"http\"\nurl = \"https://docs.example/mcp\"\n\
         headers = { Authorization = { secret = \"WEB_TOKEN\" } }\n",
    );
    std::fs::write(&toml, text).unwrap();
    for (name, value) in [("DOCS_TOKEN", &planted.docs), ("WEB_TOKEN", &planted.web)] {
        let set = vessel(
            home,
            captured,
            &["fleet", "secret", "set", name],
            value.as_bytes(),
        )
        .await;
        assert!(set.ok, "{}", set.stderr);
    }
    let listed = vessel(home, captured, &["fleet", "secret", "list"], b"").await;
    assert!(listed.stdout.contains("WEB_TOKEN"), "{}", listed.stdout);
}

/// The member joins, granting the home `admin`, and applies the bundle
/// with its MCP servers.
async fn join(home: &Fixture, member: &Fixture, client: &NodeClient, captured: &mut Captured) {
    let invited = vessel(
        home,
        captured,
        &["fleet", "invite", "--label", "member"],
        b"",
    )
    .await;
    assert!(invited.ok, "{}", invited.stderr);
    let link = format!("{}\n", invited.stdout.trim());
    let args = ["fleet", "join", "--yes", "--grant", "read,operate,admin"];
    let joined = vessel(member, captured, &args, link.as_bytes()).await;
    assert!(joined.ok, "{}", joined.stderr);
    let id = member.node().info().id;
    until("the home reaches the member", async || {
        let peers = client.call(ListPeers).await.unwrap();
        peers.iter().any(|peer| peer.node == id && peer.reachable)
    })
    .await;
    let manifest = member.data().join("fleet/manifest.json");
    until("the member has the MCP servers", async || {
        std::fs::read_to_string(&manifest).is_ok_and(|text| text.contains("WEB_TOKEN"))
    })
    .await;
    until("the member applied the bundle", async || {
        member.data().join("fleet-claude/CLAUDE.md").is_file()
    })
    .await;
    for args in [["fleet", "status"], ["fleet", "members"]] {
        let _shown = vessel(home, captured, &args, b"").await;
        let _member = vessel(member, captured, &args, b"").await;
    }
}

/// `secret.copy` to the member, and its refusals.
async fn copy_secrets(
    home: &Fixture,
    member: &Fixture,
    planted: &Planted,
    captured: &mut Captured,
) {
    let (to, own) = (
        member.node().info().id.to_string(),
        home.node().info().id.to_string(),
    );
    let copied = vessel(
        home,
        captured,
        &["fleet", "secret", "copy", "DOCS_TOKEN", "--to", &to],
        b"",
    )
    .await;
    assert!(copied.ok, "{}", copied.stderr);
    let random = NodeId::random().to_string();
    for (name, target) in [
        ("WEB_TOKEN", own.as_str()),
        ("WEB_TOKEN", &random),
        ("NOPE", &to),
    ] {
        let args = ["fleet", "secret", "copy", name, "--to", target];
        let refused = vessel(home, captured, &args, b"").await;
        assert!(!refused.ok, "{args:?}");
    }
    let stored = std::fs::read_to_string(member.data().join("secrets.toml")).unwrap();
    assert!(
        stored.contains(&planted.docs),
        "the copy reached the member"
    );
    assert_owner_only(&member.data().join("secrets.toml"));
    let _names = vessel(member, captured, &["fleet", "secret", "list"], b"").await;

    // A `secrets.toml` that does not parse on the line holding a value: the
    // error says where, never what.
    let path = member.data().join("secrets.toml");
    let good = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("WEB_TOKEN = \"{}\nOTHER = 1\n", planted.web)).unwrap();
    let broken = vessel(member, captured, &["fleet", "secret", "list"], b"").await;
    assert!(!broken.ok);
    assert!(broken.stderr.contains("line"), "{}", broken.stderr);
    std::fs::write(&path, good).unwrap();
}

/// `profile.list`, `profile.reload`, and a reload of a file that does not
/// parse on the line holding a value.
async fn profiles(home: &Fixture, planted: &Planted, captured: &mut Captured) {
    for args in [vec!["profiles"], vec!["profiles", "--reload"]] {
        let listed = vessel(home, captured, &args, b"").await;
        assert!(listed.ok, "{}", listed.stderr);
        assert!(
            listed.stdout.contains("CANARY_PROFILE"),
            "names: {}",
            listed.stdout
        );
    }
    let path = home.data().join("profiles.toml");
    let good = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        format!(
            "{good}\n[profiles.broken]\nharness = \"claude\"\nenv = {{ CANARY_PROFILE = \"{}\n",
            planted.profile
        ),
    )
    .unwrap();
    let broken = vessel(home, captured, &["profiles", "--reload"], b"").await;
    assert!(!broken.ok);
    assert!(broken.stderr.contains("line"), "{}", broken.stderr);
    std::fs::write(&path, good).unwrap();
    let again = vessel(home, captured, &["profiles", "--reload"], b"").await;
    assert!(again.ok, "{}", again.stderr);
    let unknown = vessel(
        home,
        captured,
        &["run", "claude", "--profile", "nope", "--detach"],
        b"",
    )
    .await;
    assert!(!unknown.ok);
}

/// The home's sessions, Claude Code and Codex, terminal and structured,
/// and what each fake reports: its arguments hold no value; a value is in
/// its environment only where it belongs (a profile's in every one; the
/// servers' and the agent's credential in Codex's terminal, which reads
/// them from there); a structured agent gets the servers' values over
/// ACP; the Claude Code terminal's are in its owner-only MCP file. Returns
/// the sessions, that MCP file, and every session's agent credential.
async fn sessions(
    home: &Fixture,
    client: &NodeClient,
    planted: &Planted,
    reports: &Path,
    captured: &mut Captured,
) -> (Vec<Session>, PathBuf, Vec<String>) {
    let mut started = Vec::new();
    let mut agents = Vec::new();
    for (harness, profile) in [("claude", "claude-one"), ("codex", "codex-one")] {
        for terminal in [true, false] {
            let session = run(home, captured, harness, profile, terminal).await;
            let report = report(client, session, reports).await;
            let what = format!("{harness} {profile} (terminal: {terminal})");
            captured.text(format!("{what} arguments"), &report.args.join(" "));
            assert_eq!(
                report.env.get("CANARY_PROFILE"),
                Some(&planted.profile),
                "{what}: the profile's value"
            );
            let codex_terminal = terminal && harness == "codex";
            for (name, value) in &report.env {
                let allowed = name == "CANARY_PROFILE"
                    || (codex_terminal
                        && (name == "DOCS_TOKEN"
                            || name == "VESSEL_AGENT_SECRET"
                            || name.starts_with("VESSEL_MCP_")));
                if !allowed {
                    for secret in [&planted.profile, &planted.docs, &planted.web] {
                        assert!(!value.contains(secret.as_str()), "{what}: {name}");
                    }
                    assert_ne!(name, "VESSEL_AGENT_SECRET", "{what}");
                }
            }
            if codex_terminal {
                assert_eq!(report.env.get("DOCS_TOKEN"), Some(&planted.docs));
                assert!(
                    report.env.iter().any(|(name, value)| {
                        name.starts_with("VESSEL_MCP_") && *value == planted.web
                    }),
                    "Codex reads the header's value from its environment"
                );
                agents.push(report.env["VESSEL_AGENT_SECRET"].clone());
            }
            if let Some(servers) = &report.mcp {
                let text = servers.to_string();
                for value in [&planted.docs, &planted.web] {
                    assert!(text.contains(value.as_str()), "{what}: over ACP");
                }
                agents.push(
                    mcp_variable(servers, "VESSEL_AGENT_SECRET").expect("the agent's credential"),
                );
            }
            started.push(session);
        }
    }
    let mcp = mcp_file(home);
    let text = std::fs::read_to_string(&mcp).unwrap();
    for value in [&planted.docs, &planted.web] {
        assert!(
            text.contains(value.as_str()),
            "the MCP file holds the servers' values"
        );
    }
    assert_owner_only(&mcp);
    (started, mcp, agents)
}

/// The owner-only MCP file of the Claude Code terminal session.
fn mcp_file(home: &Fixture) -> PathBuf {
    let dir = home.data().join("mcp");
    std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| std::fs::read_to_string(path).is_ok_and(|text| text.contains("mcpServers")))
        .expect("a Claude Code terminal session's MCP file")
}

/// The `vessel` server's environment in an MCP file.
fn agent_env(mcp: &Path) -> Vec<(String, String)> {
    let file: Value = serde_json::from_str(&std::fs::read_to_string(mcp).unwrap()).unwrap();
    file["mcpServers"]["vessel"]["env"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, value)| (name.clone(), value.as_str().unwrap().to_owned()))
        .collect()
}

/// `vessel mcp` run as the agent: initialised, its tools listed and one
/// called; then refused with the right secret for another session.
async fn agent_tools(env: &[(String, String)], captured: &mut Captured) {
    let mut command = Command::new(VESSEL);
    let _command = command
        .arg("mcp")
        .envs(
            env.iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
        .env("RUST_LOG", "debug")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    let messages = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": { "name": "canary", "version": "0" } } }),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "list_sessions", "arguments": {} } }),
    ];
    for message in messages {
        stdin
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
    }
    let mut replies = String::new();
    while let Some(line) = stdout.next_line().await.unwrap() {
        replies.push_str(&line);
        replies.push('\n');
        if serde_json::from_str::<Value>(&line).is_ok_and(|reply| reply["id"] == json!(3)) {
            break;
        }
    }
    assert!(replies.contains("\"id\":3"), "{replies}");
    drop(stdin);
    let output = child.wait_with_output().await.unwrap();
    captured.text("`vessel mcp` replies", &replies);
    captured.text(
        "`vessel mcp` stderr",
        &String::from_utf8_lossy(&output.stderr),
    );

    // The right secret for another session: refused, and not repeated.
    let mut wrong: Vec<(String, String)> = env.to_vec();
    for (name, value) in &mut wrong {
        if name == "VESSEL_AGENT_SESSION" {
            *value = SessionId::random().to_string();
        }
    }
    let refused = Command::new(VESSEL)
        .arg("mcp")
        .envs(wrong)
        .env("RUST_LOG", "debug")
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    captured.text(
        "refused `vessel mcp` stdout",
        &String::from_utf8_lossy(&refused.stdout),
    );
    captured.text(
        "refused `vessel mcp` stderr",
        &String::from_utf8_lossy(&refused.stderr),
    );
}

/// A session copied between the home's profiles (the CLI), and one pulled
/// from the member through the envelope.
async fn transfers(
    home: &Fixture,
    member: &Fixture,
    client: &NodeClient,
    structured: Session,
    captured: &mut Captured,
) {
    let info = client.session(structured.id).await.unwrap();
    let conversation = info.harness_session.unwrap();
    let project = claude_project(
        &home.data().join("homes/claude-one"),
        Path::new(&home.cwd()),
    )
    .unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join(format!("{conversation}.jsonl")),
        "{\"turn\":1}\n",
    )
    .unwrap();
    let elsewhere = home.data().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let (id, to) = (structured.id.to_string(), elsewhere.display().to_string());
    let args = [
        "session",
        "copy",
        &id,
        "--profile",
        "claude-two",
        "--cwd",
        &to,
    ];
    let copied = vessel(home, captured, &args, b"").await;
    assert!(copied.ok, "{}", copied.stderr);

    // From the member: a structured session of its default profile.
    let remote = owner(member).await;
    let spec = vessel_proto::SessionSpec::new(
        HarnessKind::ClaudeCode,
        vessel_proto::SessionMode::Structured {
            model: None,
            resume: None,
        },
    )
    .with_cwd(PathBuf::from(member.cwd()));
    let theirs = remote.create_session(spec).await.unwrap().id;
    let mut conversation = None;
    until("the member's session starts", async || {
        conversation = remote.session(theirs).await.unwrap().harness_session;
        conversation.is_some()
    })
    .await;
    let conversation = conversation.unwrap();
    let user_claude = member.data().join("user-home/.claude");
    let project = claude_project(&user_claude, Path::new(&member.cwd())).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join(format!("{conversation}.jsonl")),
        "{\"turn\":2}\n",
    )
    .unwrap();
    let pulled = client
        .import_session(ImportSession {
            from: SessionRef {
                node: member.node().info().id,
                session: theirs,
            },
            profile: Some("claude-two".parse().unwrap()),
            cwd: Some(PathBuf::from(home.cwd())),
            start: false,
        })
        .await
        .unwrap();
    assert_eq!(pulled.written, 1);
}

// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_secret_value_appears_where_it_does_not_belong() {
    let _log = tracing_log();
    within(async {
        let planted = Planted {
            profile: canary("PROFILE"),
            docs: canary("DOCS"),
            web: canary("WEB"),
        };
        let bin = tempfile::tempdir().unwrap();
        fakes(bin.path());
        let relay = relay().await;
        let home = node(&relay.url, bin.path(), |data| {
            std::fs::write(
                DataDir::new(data).profiles_path(),
                profiles_toml(data, &planted.profile),
            )
            .unwrap();
        })
        .await;
        let member = node(&relay.url, bin.path(), |_data| {}).await;
        let client = owner(&home).await;
        let mut captured = Captured::default();

        make_home(&home, &planted, &mut captured).await;
        join(&home, &member, &client, &mut captured).await;
        copy_secrets(&home, &member, &planted, &mut captured).await;
        profiles(&home, &planted, &mut captured).await;
        let reports = tempfile::tempdir().unwrap();
        let (started, mcp, mut agents) =
            sessions(&home, &client, &planted, reports.path(), &mut captured).await;
        let agent = agent_env(&mcp);
        agent_tools(&agent, &mut captured).await;
        transfers(&home, &member, &client, started[1], &mut captured).await;
        // The sessions' logs, as the node serves them and as `vessel log`
        // prints them.
        for session in &started {
            let text = session_text(&client, session.id).await;
            captured.text(format!("session {}", session.id), &text);
            let _log = vessel(&home, &mut captured, &["log", &session.id.to_string()], b"").await;
        }

        // Credentials Vessel made: the member's on the home, the home's on
        // the member, and the Claude Code terminal session's agent.
        let device = credential_secret(&member.data().join("fleet/member.credential"));
        assert_owner_only(&member.data().join("fleet/member.credential"));
        let grants = home.data().join("fleet/grants");
        let grant_file = std::fs::read_dir(&grants)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "credential")
            })
            .expect("the home's credential on the member");
        let grant = credential_secret(&grant_file);
        agents.push(
            agent
                .iter()
                .find(|(name, _)| name == "VESSEL_AGENT_SECRET")
                .map(|(_, value)| value.clone())
                .unwrap(),
        );
        assert_eq!(agents.len(), 4, "every session's agent credential");

        // Everything captured so far, and every file but the few a value
        // belongs in.
        // (The home's `secrets.toml`, `profiles.toml`, the MCP files of
        // its terminal sessions and its credential on the member; the
        // member's copied secret and its credential on the home.)
        let intended = |data: &Path, credential: PathBuf| {
            vec![
                data.join("secrets.toml"),
                data.join("profiles.toml"),
                data.join("mcp"),
                credential,
            ]
        };
        captured.tree(home.data(), &intended(home.data(), grant_file.clone()));
        let member_credential = member.data().join("fleet/member.credential");
        captured.tree(member.data(), &intended(member.data(), member_credential));
        captured.text("the nodes' tracing", &String::from_utf8_lossy(&traced()));
        let mut canaries = vec![
            ("profile", planted.profile.as_str()),
            ("DOCS_TOKEN", planted.docs.as_str()),
            ("WEB_TOKEN", planted.web.as_str()),
            ("member credential", device.as_str()),
            ("home credential", grant.as_str()),
        ];
        canaries.extend(
            agents
                .iter()
                .map(|agent| ("agent credential", agent.as_str())),
        );
        captured.assert_clean(&canaries);
        assert!(
            contains(&traced(), b"client connected"),
            "the nodes' tracing was captured"
        );

        home.stop().await;
        member.stop().await;
        relay.shutdown.cancel();
        relay.serving.await.unwrap().unwrap();
    })
    .await;
}
