//! `vessel init` (plan F5, ADR 0012 "Joining from scratch"): first-run
//! setup that is safe to repeat. Each step reports `done`, `already done`
//! or `skipped` with why, and what is left for the person is listed at the
//! end.
//!
//! Steps, in order: the data directory and node identity; the harnesses
//! found; install offers for what is missing; profiles for other accounts'
//! homes found beside the default ones; a login check per profile; with
//! `--service`, the node as a per-user service. `--join` (pairing with a
//! fleet's home) needs fleet sync (F2) and refuses until then.
//!
//! [Decision Log]
//! - Purpose: a new machine becomes a working node in minutes, and running
//!   `vessel init` again fills gaps and changes nothing else.
//! - Consent: nothing is installed, written into `profiles.toml` or turned
//!   into a service unless asked. Installs run only after a yes to that
//!   install (a prompt, or `--install` with `--yes`; `--yes` alone does not
//!   install, plan decision 9). Profiles are written after a yes (a prompt,
//!   or `--yes`). `--service` is itself the request. Logins are only
//!   checked; the command to log in is printed for the person to run.
//! - Answers are read from standard input, one line each; the end of input
//!   is no. So a script can pipe answers, and nothing waits on a missing
//!   terminal.
//! - Idempotence comes from checking before acting: an identity, a profile
//!   for the same home, a service file with the same content and an
//!   enabled unit are reported as already done; a service file that
//!   differs is left alone (it may be the person's).
//! - Nothing here knows a harness: install commands, login commands and
//!   where other accounts' homes are come from `vessel-harness`.

mod harnesses;
mod identity;
mod profiles;
mod service;

use std::fmt::Write as _;
use std::path::PathBuf;

use tokio::sync::mpsc;
use vessel_harness::Lookup;
use vessel_node::DataDir;

use crate::error::CliError;
use crate::io;

/// What `vessel init` was asked.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    /// Install the node as a per-user service.
    pub(crate) service: bool,
    /// Join a fleet (F2).
    pub(crate) join: bool,
    /// Who answers the questions.
    pub(crate) consent: Consent,
    /// The user's home, where other accounts' homes are looked for.
    pub(crate) home: PathBuf,
}

/// Who answers `vessel init`'s questions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Consent {
    /// The person, on standard input.
    Ask,
    /// `--yes`: yes to everything but installs, which are not run.
    Yes,
    /// `--yes --install`: yes to everything.
    YesAndInstall,
}

impl Consent {
    /// From the flags.
    pub(crate) const fn new(yes: bool, install: bool) -> Self {
        match (yes, install) {
            (true, true) => Self::YesAndInstall,
            (true, false) => Self::Yes,
            (false, _) => Self::Ask,
        }
    }
}

/// Run every step.
///
/// # Errors
/// `--join` (not available yet), or a step that could not even look (the
/// data directory cannot be prepared, a file cannot be written).
pub(crate) async fn run(data_dir: DataDir, options: Options) -> Result<(), CliError> {
    if options.join {
        // F2: replace this refusal with pairing to the fleet's home (the
        // link read from stdin, never argv), pulling and applying the
        // bundle, and taking the fleet's relays.
        return Err(CliError::JoinNotYet);
    }
    let mut init = Init {
        data_dir,
        lookup: Lookup::from_env(),
        answers: Answers::default(),
        todo: Vec::new(),
        options,
    };
    io::out(b"vessel init\n").await?;
    identity::run(&mut init).await?;
    harnesses::run(&mut init).await?;
    profiles::run(&mut init).await?;
    if init.options.service {
        service::run(&mut init).await?;
    }
    let mut left = String::new();
    if init.todo.is_empty() {
        left.push_str("Nothing left to do.\n");
    } else {
        left.push_str("Left to do:\n");
        for item in &init.todo {
            let _infallible = writeln!(left, "  - {item}");
        }
    }
    io::out(left.as_bytes()).await?;
    Ok(())
}

/// The state the steps share.
struct Init {
    data_dir: DataDir,
    lookup: Lookup,
    answers: Answers,
    /// What the person still has to do, listed at the end.
    todo: Vec<String>,
    options: Options,
}

/// How a step went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    Already,
    Skipped,
}

impl Init {
    /// Report one step's outcome.
    async fn report(&self, outcome: Outcome, step: &str, detail: &str) -> std::io::Result<()> {
        let label = match outcome {
            Outcome::Done => "done",
            Outcome::Already => "already done",
            Outcome::Skipped => "skipped",
        };
        io::out(format!("  {label:<12}  {step}: {detail}\n").as_bytes()).await
    }

    /// Ask a yes-or-no question; `--yes` answers yes.
    async fn confirm(&mut self, question: &str) -> std::io::Result<bool> {
        match self.options.consent {
            Consent::Yes | Consent::YesAndInstall => Ok(true),
            Consent::Ask => self.ask(question).await,
        }
    }

    /// Ask whether to run an install: only `--install` with `--yes`
    /// answers for the person (plan decision 9). `None`: not asked, since
    /// `--yes` alone never installs.
    async fn confirm_install(&mut self, question: &str) -> std::io::Result<Option<bool>> {
        match self.options.consent {
            Consent::YesAndInstall => Ok(Some(true)),
            Consent::Yes => Ok(None),
            Consent::Ask => self.ask(question).await.map(Some),
        }
    }

    async fn ask(&mut self, question: &str) -> std::io::Result<bool> {
        io::note(&format!("{question} [y/N] ")).await?;
        let answer = self.answers.next().await?;
        Ok(answer.is_some_and(|answer| {
            matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        }))
    }
}

/// Answers from standard input, read only once a question is asked.
#[derive(Default)]
struct Answers(Option<mpsc::Receiver<String>>);

impl Answers {
    async fn next(&mut self) -> std::io::Result<Option<String>> {
        if self.0.is_none() {
            self.0 = Some(io::input_lines()?);
        }
        Ok(match &mut self.0 {
            Some(lines) => lines.recv().await,
            None => None,
        })
    }
}

/// `path` with the home directory shown as `~`, for reports.
fn shown(init: &Init, path: &std::path::Path) -> String {
    match path.strip_prefix(&init.options.home) {
        Ok(rest) if !rest.as_os_str().is_empty() => {
            format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display())
        }
        Ok(_) => "~".to_owned(),
        Err(_) => path.display().to_string(),
    }
}
