//! `vessel init` (plan F5) against a temporary home with made-up accounts:
//! `fake-tui` as `claude` and `codex` (answering their login checks),
//! `fake-acp` as Claude's adapter, and `fake-tool` as `npm`, `systemctl` and
//! `launchctl`, logging every call. `PATH` holds only those, so no real
//! installer or service manager can be reached.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tempfile::TempDir;
use tokio::io::AsyncWriteExt as _;
use vessel_harness::fakes::{Role, install};
use vessel_proto::HarnessKind;
use vessel_testkit::Fake;

use crate::fleet::{RelayedHome, init_home};
use crate::support::within;

const VESSEL: &str = env!("CARGO_BIN_EXE_vessel");

/// A home, a data directory and a search path for one test.
struct Machine {
    home: TempDir,
    data: TempDir,
    bin: TempDir,
    log: PathBuf,
}

impl Machine {
    /// A home with the default Claude and Codex homes and two other
    /// accounts' homes (made-up names), one more Claude directory with no
    /// login, and the fakes on the search path.
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let account = |name: &str, file: &str| {
            let dir = home.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(file), "").unwrap();
        };
        account(".claude", ".credentials.json");
        account(".claude-work", ".credentials.json");
        account(".codex", "auth.json");
        account(".codex-review", "auth.json");
        account(".claude-scratch", "settings.json");

        let bin = tempfile::tempdir().unwrap();
        let tui = Fake::Tui.path().unwrap();
        let acp = Fake::Acp.path().unwrap();
        drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Terminal, &tui).unwrap());
        drop(install(bin.path(), HarnessKind::ClaudeCode, Role::Structured, &acp).unwrap());
        drop(install(bin.path(), HarnessKind::Codex, Role::Terminal, &tui).unwrap());
        drop(install(bin.path(), HarnessKind::Codex, Role::Structured, &acp).unwrap());
        for tool in ["npm", "systemctl", "launchctl"] {
            tool_as(bin.path(), tool);
        }
        let data = tempfile::tempdir().unwrap();
        let log = data.path().join("tools.log");
        Self {
            home,
            data,
            bin,
            log,
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.data.path().join("vessel")
    }

    /// `vessel init <args>` with `stdin`; (success, stdout, stderr).
    async fn init(&self, args: &[&str], stdin: &[u8]) -> (bool, String, String) {
        self.init_with(args, stdin, &[]).await
    }

    /// [`init`](Self::init) with more environment.
    async fn init_with(
        &self,
        args: &[&str],
        stdin: &[u8],
        env: &[(&str, &str)],
    ) -> (bool, String, String) {
        let mut command = tokio::process::Command::new(VESSEL);
        let _configured = command.envs(env.iter().copied());
        let _configured = command
            .arg("--data-dir")
            .arg(self.data_dir())
            .arg("init")
            .args(args)
            .env("PATH", self.search_path())
            .env("VESSEL_HOME", self.home.path())
            .env("HOME", self.home.path())
            .env("FAKE_TOOL_LOG", &self.log)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("RUST_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(stdin).await.unwrap();
        drop(input);
        let output = child.wait_with_output().await.unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// Only the fakes; on Windows also the system directory, where
    /// `cmd.exe` (which runs the `.cmd` shims) is.
    fn search_path(&self) -> std::ffi::OsString {
        let mut dirs = vec![self.bin.path().to_path_buf()];
        if cfg!(windows)
            && let Some(root) = std::env::var_os("SystemRoot")
        {
            dirs.push(PathBuf::from(root).join("System32"));
        }
        std::env::join_paths(dirs).unwrap()
    }

    /// The fake tools' calls so far.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Every file under the home and the data directory, with its
    /// contents (the tools' log left out).
    fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::new();
        for root in [self.home.path(), self.data.path()] {
            collect(root, &mut files);
        }
        let _log = files.remove(&self.log);
        files
    }
}

fn collect(dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, files);
        } else {
            let _old = files.insert(path.clone(), std::fs::read(&path).unwrap_or_default());
        }
    }
}

/// `fake-tool` reachable as `name` in `bin`.
fn tool_as(bin: &Path, name: &str) {
    let tool = Fake::Tool.path().unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&tool, bin.join(name)).unwrap();
    #[cfg(windows)]
    std::fs::write(
        bin.join(format!("{name}.cmd")),
        format!(
            "@echo off\r\nset FAKE_TOOL_NAME={name}\r\n\"{}\" %*\r\nexit /b %ERRORLEVEL%\r\n",
            tool.display()
        ),
    )
    .unwrap();
}

/// The report line for `step`, if any, without its leading spaces.
fn line<'a>(stdout: &'a str, step: &str) -> Option<&'a str> {
    stdout.lines().map(str::trim_start).find(|line| {
        line.split_once("  ")
            .is_some_and(|(_, rest)| rest.trim_start().starts_with(step))
    })
}

fn outcome<'a>(stdout: &'a str, step: &str) -> &'a str {
    let found = line(stdout, step).unwrap_or_else(|| panic!("no `{step}` in:\n{stdout}"));
    found.split("  ").next().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_sets_a_machine_up_and_a_second_run_changes_nothing() {
    within(async {
        let machine = Machine::new();
        let (ok, first, stderr) = machine.init(&["--yes"], b"").await;
        assert!(ok, "{first}\n{stderr}");
        assert_eq!(outcome(&first, "node identity"), "done");
        assert_eq!(outcome(&first, "harness claude-code"), "already done");
        assert_eq!(outcome(&first, "harness gemini"), "skipped");
        // --yes alone installs nothing.
        assert_eq!(outcome(&first, "install gemini"), "skipped");
        assert!(machine.calls().is_empty(), "{:?}", machine.calls());
        assert_eq!(outcome(&first, "profile claude-work"), "done");
        assert_eq!(outcome(&first, "profile codex-review"), "done");
        assert!(line(&first, "profile claude-scratch").is_none(), "{first}");
        for login in ["claude", "claude-work", "codex", "codex-review"] {
            assert_eq!(
                outcome(&first, &format!("login {login}:")),
                "already done",
                "{first}"
            );
        }
        assert!(first.contains("Left to do:"), "{first}");

        let profiles = std::fs::read_to_string(machine.data_dir().join("profiles.toml")).unwrap();
        assert!(profiles.contains("[profiles.claude-work]"), "{profiles}");
        assert!(
            profiles.contains("CLAUDE_CONFIG_DIR = \"~/.claude-work\""),
            "{profiles}"
        );
        assert!(
            profiles.contains("CODEX_HOME = \"~/.codex-review\""),
            "{profiles}"
        );

        let before = machine.files();
        let (ok, second, stderr) = machine.init(&["--yes"], b"").await;
        assert!(ok, "{second}\n{stderr}");
        assert_eq!(outcome(&second, "node identity"), "already done");
        assert_eq!(outcome(&second, "profile claude-work"), "already done");
        assert!(
            !second
                .lines()
                .any(|line| line.trim_start().starts_with("done ")),
            "{second}"
        );
        assert_eq!(machine.files(), before, "the second run changed files");
        assert!(machine.calls().is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_is_installed_or_written_without_consent() {
    within(async {
        let machine = Machine::new();
        // Every question answered no: gemini, opencode, then the profiles.
        let (ok, stdout, stderr) = machine.init(&[], b"n\nn\nno\n").await;
        assert!(ok, "{stdout}\n{stderr}");
        assert!(
            stderr.contains("Install gemini with `npm install -g"),
            "{stderr}"
        );
        assert_eq!(outcome(&stdout, "install gemini"), "skipped");
        assert_eq!(outcome(&stdout, "profile claude-work"), "skipped");
        assert!(machine.calls().is_empty(), "{:?}", machine.calls());
        assert!(!machine.data_dir().join("profiles.toml").exists());

        // No answers at all (end of input) is no as well.
        let (ok, stdout, _) = machine.init(&[], b"").await;
        assert!(ok);
        assert_eq!(outcome(&stdout, "install opencode"), "skipped");
        assert!(machine.calls().is_empty());

        // A yes to one install runs that one only.
        let (ok, stdout, stderr) = machine.init(&[], b"y\nn\nn\n").await;
        assert!(ok, "{stdout}\n{stderr}");
        assert_eq!(outcome(&stdout, "install gemini"), "done");
        assert_eq!(machine.calls(), ["npm install -g @google/gemini-cli"]);

        // --install needs --yes; with it, everything missing is installed.
        let (ok, _, stderr) = machine.init(&["--install"], b"").await;
        assert!(!ok, "{stderr}");
        let (ok, stdout, stderr) = machine.init(&["--yes", "--install"], b"").await;
        assert!(ok, "{stdout}\n{stderr}");
        assert_eq!(outcome(&stdout, "install opencode"), "done");
        assert!(
            machine
                .calls()
                .contains(&"npm install -g opencode-ai".to_owned()),
            "{:?}",
            machine.calls()
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_pairs_with_the_home_once_and_then_is_already_done() {
    within(async {
        let homes = tempfile::tempdir().unwrap();
        let home_claude = homes.path().join("home-claude");
        let home = RelayedHome::start(&home_claude).await;
        init_home(&home.fixture, &homes.path().join("sources"), &home_claude).await;
        let invited = home
            .fixture
            .vessel(&["fleet", "invite", "--label", "laptop"], &[])
            .await;
        assert!(invited.ok, "{}", invited.stderr);
        let link = format!("{}\n", invited.stdout.trim());

        let machine = Machine::new();
        // Piped, the home cannot be confirmed at a terminal: --yes is needed.
        let (ok, _, stderr) = machine.init(&["--join"], link.as_bytes()).await;
        assert!(!ok);
        assert!(stderr.contains("--yes"), "{stderr}");

        let (ok, stdout, stderr) = machine.init(&["--join", "--yes"], link.as_bytes()).await;
        assert!(ok, "{stdout}\n{stderr}");
        assert_eq!(outcome(&stdout, "fleet"), "done");
        let home_id = home.fixture.node().info().id.to_string();
        assert!(stdout.contains(&home_id), "{stdout}");

        // A member has nothing to join: no link is read.
        let (ok, again, stderr) = machine.init(&["--join", "--yes"], b"").await;
        assert!(ok, "{again}\n{stderr}");
        assert_eq!(outcome(&again, "fleet"), "already done");
        assert!(again.contains(&format!("a member of {home_id}")), "{again}");
        home.stop().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_service_is_installed_once_and_a_changed_file_is_left_alone() {
    within(async {
        let machine = Machine::new();
        let (ok, stdout, stderr) = machine.init(&["--yes", "--service"], b"").await;
        assert!(ok, "{stdout}\n{stderr}");
        if cfg!(windows) {
            assert_eq!(outcome(&stdout, "service"), "skipped");
            assert!(
                stdout.contains("not available on this system yet"),
                "{stdout}"
            );
            return;
        }
        assert_eq!(outcome(&stdout, "service"), "done");
        let (file, enabled) = if cfg!(target_os = "macos") {
            (
                machine
                    .home
                    .path()
                    .join("Library/LaunchAgents/vessel.node.plist"),
                "launchctl load -w",
            )
        } else {
            (
                machine
                    .home
                    .path()
                    .join(".config/systemd/user/vessel.service"),
                "systemctl --user enable --now vessel.service",
            )
        };
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains(VESSEL), "{content}");
        assert!(content.contains("node"), "{content}");
        assert!(stdout.contains("to undo:"), "{stdout}");
        let calls = machine.calls();
        assert!(
            calls.iter().any(|call| call.starts_with(enabled)),
            "{calls:?}"
        );

        let (ok, again, _) = machine.init(&["--yes", "--service"], b"").await;
        assert!(ok);
        assert_eq!(outcome(&again, "service"), "already done");
        let installs = |calls: &[String]| {
            calls
                .iter()
                .filter(|call| call.starts_with(enabled))
                .count()
        };
        assert_eq!(installs(&machine.calls()), 1, "{:?}", machine.calls());

        std::fs::write(&file, "# mine\n").unwrap();
        let (ok, changed, _) = machine.init(&["--yes", "--service"], b"").await;
        assert!(ok);
        assert_eq!(outcome(&changed, "service"), "skipped");
        assert!(changed.contains("differs"), "{changed}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "# mine\n");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_logged_out_profile_gets_its_login_command_and_no_value_is_shown() {
    within(async {
        let machine = Machine::new();
        std::fs::create_dir_all(machine.data_dir()).unwrap();
        let secret = "made-up-token-7f3e";
        let profiles = format!(
            "[profiles.claude-api]\nharness = \"claude\"\n\
             env = {{ CLAUDE_CONFIG_DIR = \"~/.claude-api\", ANTHROPIC_AUTH_TOKEN = \"{secret}\" }}\n"
        );
        std::fs::write(machine.data_dir().join("profiles.toml"), profiles).unwrap();

        let (ok, stdout, stderr) = machine
            .init_with(&["--yes"], b"", &[("FAKE_TUI_LOGGED_OUT", "1")])
            .await;
        assert!(ok, "{stdout}\n{stderr}");
        assert!(!stdout.contains(secret) && !stderr.contains(secret), "{stdout}{stderr}");
        assert_eq!(outcome(&stdout, "login claude-work:"), "skipped");
        let home = machine.home.path();
        let quoted = |dir: &str| format!("'{}'", home.join(dir).display());
        let prefix = |variable: &str, dir: &str| {
            if cfg!(windows) {
                format!("$env:{variable}={}; ", quoted(dir))
            } else {
                format!("{variable}={} ", quoted(dir))
            }
        };
        for expected in [
            format!(
                "log in to claude-work: `{}claude auth login`",
                prefix("CLAUDE_CONFIG_DIR", ".claude-work")
            ),
            format!(
                "log in to codex-review: `{}codex login`",
                prefix("CODEX_HOME", ".codex-review")
            ),
            format!(
                "log in to claude-api: `{}claude auth login`",
                prefix("CLAUDE_CONFIG_DIR", ".claude-api")
            ),
            "log in to claude: `claude auth login`".to_owned(),
        ] {
            assert!(stdout.contains(&expected), "{expected} not in:\n{stdout}");
        }
    })
    .await;
}
