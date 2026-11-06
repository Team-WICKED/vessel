//! `fake-ssh`: a stand-in for the OpenSSH client, so tests of the SSH
//! transport (ADR 0007) run the real remote commands without a network.
//!
//! Behaviour:
//!
//! - Parses the `ssh` command line as OpenSSH does: single-letter flags
//!   (`-T`, `-N`, ...) and options that take a value (`-o`, `-p`, `-l`,
//!   ...), before and after the destination, up to `--` or the first other
//!   word, which starts the remote command. Like `ssh`, it joins the
//!   command's words with spaces into one line for the remote shell.
//! - "Remote" means this machine: the destination is ignored, and the
//!   command runs under `sh -c` in `$FAKE_SSH_HOME`, with `HOME` set to it,
//!   so a test gets a fresh remote home directory. Stdin, stdout and stderr
//!   pass through, and the exit code is the command's. Its own failures,
//!   and a command ended by a signal, exit 255, as `ssh` does.
//! - `FAKE_SSH_DROP_AFTER=N`: the connection "drops" after the command has
//!   sent N bytes of output: the command is killed (as `sshd` ends a session
//!   whose client vanished), nothing more is forwarded, and `fake-ssh` exits
//!   255.
//! - Unix only (it needs `sh`); elsewhere it exits 255 and says so.

use std::ffi::OsString;
use std::process::ExitCode;

/// `ssh`'s exit code for its own errors.
const SSH_ERROR: u8 = 255;

/// `ssh` options that take a value (OpenSSH 9).
const WITH_VALUE: &[u8] = b"BbcDEeFIiJLlmOoPpRSWw";

fn main() -> ExitCode {
    let Some(command) = remote_command(std::env::args_os().skip(1)) else {
        complain("fake-ssh: no destination or no remote command");
        return ExitCode::from(SSH_ERROR);
    };
    let Some(home) = std::env::var_os("FAKE_SSH_HOME") else {
        complain("fake-ssh: FAKE_SSH_HOME is not set");
        return ExitCode::from(SSH_ERROR);
    };
    let drop_after = std::env::var("FAKE_SSH_DROP_AFTER")
        .ok()
        .and_then(|bytes| bytes.parse::<u64>().ok());
    run(&command, &home, drop_after)
}

/// The remote command, joined like `ssh` joins it; `None` without a
/// destination or a command.
///
/// OpenSSH reads options before the destination and again after it, until
/// the first other word or `--`; everything from there is the command.
fn remote_command(mut args: impl Iterator<Item = OsString>) -> Option<OsString> {
    let mut destination = false;
    loop {
        let arg = args.next()?;
        let Some(text) = arg.to_str() else {
            return command_after(std::iter::once(arg).chain(args));
        };
        if text == "--" {
            if !destination {
                let _destination = args.next()?;
            }
            return command_after(args);
        }
        let Some(flags) = text.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            if destination {
                return command_after(std::iter::once(arg).chain(args));
            }
            destination = true;
            continue;
        };
        // `-p22` carries its value; `-p 22` (or `-Tp 22`) takes the next
        // word.
        let takes_next = flags
            .bytes()
            .position(|option| WITH_VALUE.contains(&option))
            .is_some_and(|position| position.saturating_add(1) == flags.len());
        if takes_next {
            let _value = args.next()?;
        }
    }
}

fn command_after(args: impl Iterator<Item = OsString>) -> Option<OsString> {
    let mut command = OsString::new();
    for (index, word) in args.enumerate() {
        if index > 0 {
            command.push(" ");
        }
        command.push(word);
    }
    (!command.is_empty()).then_some(command)
}

#[cfg(unix)]
fn run(command: &OsString, home: &OsString, drop_after: Option<u64>) -> ExitCode {
    use std::process::{Command, Stdio};

    let mut shell = Command::new("sh");
    let _ = shell
        .arg("-c")
        .arg(command)
        .current_dir(home)
        .env("HOME", home)
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit());
    let Some(limit) = drop_after else {
        let _ = shell.stdout(Stdio::inherit());
        return match shell.status() {
            Ok(status) => exit_code(status),
            Err(error) => {
                complain(&format!("fake-ssh: cannot run sh: {error}"));
                ExitCode::from(SSH_ERROR)
            }
        };
    };
    let _ = shell.stdout(Stdio::piped());
    let mut child = match shell.spawn() {
        Ok(child) => child,
        Err(error) => {
            complain(&format!("fake-ssh: cannot run sh: {error}"));
            return ExitCode::from(SSH_ERROR);
        }
    };
    let Some(mut output) = child.stdout.take() else {
        return ExitCode::from(SSH_ERROR);
    };
    let dropped = forward_until(&mut output, limit);
    if dropped {
        drop(output);
        // The connection is gone: the remote session ends abruptly.
        if let Err(error) = child.kill() {
            complain(&format!("fake-ssh: cannot end the command: {error}"));
        }
        drop(child.wait());
        return ExitCode::from(SSH_ERROR);
    }
    match child.wait() {
        Ok(status) => exit_code(status),
        Err(_) => ExitCode::from(SSH_ERROR),
    }
}

/// Copy the command's output to stdout until it ends (`false`) or `limit`
/// bytes went through (`true`: the connection drops).
#[cfg(unix)]
fn forward_until(output: &mut impl std::io::Read, limit: u64) -> bool {
    use std::io::Write as _;

    let mut stdout = std::io::stdout().lock();
    let mut left = limit;
    let mut buffer = [0_u8; 8192];
    loop {
        if left == 0 {
            return true;
        }
        let want = usize::try_from(left)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let Some(window) = buffer.get_mut(..want) else {
            return true;
        };
        let read = match output.read(window) {
            Ok(0) | Err(_) => return false,
            Ok(read) => read,
        };
        let Some(chunk) = buffer.get(..read) else {
            return true;
        };
        if stdout
            .write_all(chunk)
            .and_then(|()| stdout.flush())
            .is_err()
        {
            return false;
        }
        left = left.saturating_sub(u64::try_from(read).unwrap_or(u64::MAX));
    }
}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::from(SSH_ERROR), ExitCode::from)
}

#[cfg(not(unix))]
fn run(_command: &OsString, _home: &OsString, _drop_after: Option<u64>) -> ExitCode {
    complain("fake-ssh: runs only on Unix (it needs sh)");
    ExitCode::from(SSH_ERROR)
}

fn complain(line: &str) {
    use std::io::Write as _;

    let written = writeln!(std::io::stderr().lock(), "{line}");
    drop(written);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Option<String> {
        remote_command(line.split(' ').map(OsString::from))
            .map(|command| command.to_string_lossy().into_owned())
    }

    #[test]
    fn parses_the_command_line_as_openssh_does() {
        let expected = Some("vesseld attach".to_owned());
        assert_eq!(
            parse("-T -o BatchMode=yes -p 22 me@host -- vesseld attach"),
            expected
        );
        assert_eq!(parse("me@host -T -o X=y vesseld attach"), expected);
        assert_eq!(parse("-- me@host vesseld attach"), expected);
        assert_eq!(parse("-Tp22 -lme host vesseld attach"), expected);
        assert_eq!(parse("host -- -v"), Some("-v".to_owned()));
        assert_eq!(parse("-T host"), None);
        assert_eq!(parse("-o"), None);
    }
}
