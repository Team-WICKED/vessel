//! Plain-text renderings of node, harness and session descriptions. Pure
//! functions returning text, so they are tested without a terminal.

use std::fmt::Write as _;

use vessel_proto::{
    Harness, NodeInfo, NodeState, ProfileName, SessionInfo, SessionMode, SessionStatus,
    StructuredSupport,
};

/// A session's status in a few words.
pub(crate) fn status(status: &SessionStatus) -> String {
    match status {
        SessionStatus::Starting => "starting".to_owned(),
        SessionStatus::Running => "running".to_owned(),
        SessionStatus::Exited { code: Some(code) } => format!("exited ({code})"),
        SessionStatus::Exited { code: None } => "exited (killed)".to_owned(),
        SessionStatus::Failed { message } => format!("failed: {message}"),
        SessionStatus::Interrupted => "interrupted".to_owned(),
    }
}

/// A profile's name, or `default` for the harness's implicit one.
pub(crate) fn profile(profile: Option<&ProfileName>) -> &str {
    profile.map_or("default", ProfileName::as_str)
}

/// `terminal` or `structured`, plus what matters about the mode.
pub(crate) fn mode(mode: &SessionMode) -> String {
    match mode {
        SessionMode::Terminal { args, .. } if args.is_empty() => "terminal".to_owned(),
        SessionMode::Terminal { args, .. } => format!("terminal {}", args.join(" ")),
        SessionMode::Structured {
            model: Some(model), ..
        } => format!("structured ({model})"),
        SessionMode::Structured { model: None, .. } => "structured".to_owned(),
    }
}

pub(crate) fn node(node: &NodeInfo) -> String {
    format!(
        "node      {} ({})\nhost      {} {}/{}\nversion   {} (protocol {}..={})\nstarted   {}\n",
        node.name,
        node.id,
        node.hostname,
        node.os,
        node.arch,
        node.version,
        node.protocol.min,
        node.protocol.max,
        node.started_at.to_rfc3339(),
    )
}

/// `vessel node status`: the node, whether it takes new sessions, how many
/// are live, and whether it runs this client's version.
pub(crate) fn node_status(node: &NodeInfo, sessions: &[SessionInfo], client: &str) -> String {
    let state = match node.state {
        NodeState::Serving => "serving",
        NodeState::Draining => "draining (refuses new sessions; stops when the last one ends)",
    };
    let live = sessions
        .iter()
        .filter(|session| session.status.is_live())
        .count();
    let mut text = self::node(node);
    let _infallible = writeln!(text, "state     {state}");
    if let Some(key) = &node.public_key {
        // What a person compares with the fingerprint shown when pairing.
        let _infallible = writeln!(text, "key       {}", vessel_client::fingerprint(key));
    }
    let _infallible = writeln!(text, "sessions  {live} live, {} in all", sessions.len());
    if node.version != client {
        let _infallible = writeln!(
            text,
            "update    this client is {client}; `vessel node update` installs it there"
        );
    }
    text
}

pub(crate) fn harnesses(harnesses: &[Harness]) -> String {
    if harnesses.is_empty() {
        return "no harnesses found on the node's PATH\n".to_owned();
    }
    let mut text = String::new();
    for harness in harnesses {
        let structured = match &harness.structured {
            StructuredSupport::Available => "structured: yes".to_owned(),
            StructuredSupport::Unavailable { reason } => format!("structured: no ({reason})"),
        };
        // Writing to a `String` cannot fail.
        let _infallible = writeln!(
            text,
            "{:<12} {:<28} {}  {}",
            harness.kind,
            harness.version.as_deref().unwrap_or("-"),
            harness.path.display(),
            structured,
        );
    }
    text
}

pub(crate) fn sessions(sessions: &[SessionInfo]) -> String {
    if sessions.is_empty() {
        return "no sessions\n".to_owned();
    }
    let mut text = String::new();
    for session in sessions {
        let _infallible = writeln!(
            text,
            "{}  {:<12} {:<18} {:<16} {}",
            session.id,
            session.spec.harness,
            status(&session.status),
            mode(&session.spec.mode),
            session.created_at.format("%Y-%m-%d %H:%M:%S"),
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use vessel_proto::{HarnessKind, SessionId, SessionSpec, TerminalSize};

    use super::*;

    #[test]
    fn sessions_render_one_line_each() {
        let session = {
            let mut info = SessionInfo::new(
                SessionId::random(),
                SessionSpec::new(
                    HarnessKind::Codex,
                    SessionMode::Terminal {
                        args: vec!["--full-auto".to_owned()],
                        size: TerminalSize::default(),
                    },
                ),
                DateTime::UNIX_EPOCH,
            );
            info.status = SessionStatus::Exited { code: None };
            info
        };
        let text = sessions(std::slice::from_ref(&session));
        assert!(text.starts_with(&session.id.to_string()));
        assert!(text.contains("codex"));
        assert!(text.contains("exited (killed)"));
        assert!(text.contains("terminal --full-auto"));
        assert!(text.ends_with("1970-01-01 00:00:00\n"));
        assert_eq!(sessions(&[]), "no sessions\n");
    }
}
