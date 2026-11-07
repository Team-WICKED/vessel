//! Plain-text renderings of node, harness and session descriptions. Pure
//! functions returning text, so they are tested without a terminal.

use std::fmt::Write as _;

use vessel_proto::{
    EnvName, Harness, ModelCall, ModelStop, NodeInfo, NodeState, ProfileInfo, ProfileName,
    SessionInfo, SessionMode, SessionStatus, StructuredSupport,
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

/// One model call: the route and model, the tokens, how it ended and how
/// long it took (protocol 7). Never content: the record holds none.
pub(crate) fn model_call(call: &ModelCall) -> String {
    let ended = call.failure.map_or_else(
        || stop(call.stop).to_owned(),
        |failure| format!("failed: {}", failure.as_str()),
    );
    let usage = &call.usage;
    let mut text = format!(
        "[model {} {}: {} in",
        call.route,
        call.model,
        usage.input.get()
    );
    if usage.cache_read.get() > 0 || usage.cache_write.get() > 0 {
        let _infallible = write!(
            text,
            " (+{} cached, +{} to cache)",
            usage.cache_read.get(),
            usage.cache_write.get()
        );
    }
    let _infallible = write!(
        text,
        ", {} out, {ended}, {:.1?}]",
        usage.output.get(),
        call.duration_ms.as_duration()
    );
    text
}

const fn stop(stop: ModelStop) -> &'static str {
    match stop {
        ModelStop::EndTurn => "end turn",
        ModelStop::ToolUse => "tool use",
        ModelStop::MaxTokens => "max tokens",
        ModelStop::StopSequence => "stop sequence",
        ModelStop::Refusal => "refused",
        ModelStop::Pause => "paused",
        ModelStop::Error => "error",
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

/// One profile per line: name, harness, the variables it sets (names only;
/// the node never sends values), and its label.
pub(crate) fn profiles(profiles: &[ProfileInfo]) -> String {
    if profiles.is_empty() {
        return "no profiles\n".to_owned();
    }
    let mut text = String::new();
    for profile in profiles {
        let sets = if profile.sets.is_empty() {
            "-".to_owned()
        } else {
            profile
                .sets
                .iter()
                .map(EnvName::as_str)
                .collect::<Vec<_>>()
                .join(",")
        };
        let _infallible = writeln!(
            text,
            "{:<20} {:<12} {:<32} {}",
            profile.name,
            profile.harness,
            sets,
            profile.label.as_deref().unwrap_or(""),
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
    fn a_model_call_shows_its_counts_never_content() {
        let call: ModelCall = serde_json::from_str(
            r#"{"route":"qwen-coder","model":"qwen/qwen3-coder","api":"openai_chat_completions",
                "usage":{"input":72,"cache_read":128,"output":50},"stop":"tool_use","duration_ms":1234}"#,
        )
        .unwrap();
        assert_eq!(
            model_call(&call),
            "[model qwen-coder qwen/qwen3-coder: 72 in (+128 cached, +0 to cache), 50 out, tool use, 1.2s]"
        );
        let failed: ModelCall = serde_json::from_str(
            r#"{"route":"r","model":"m","api":"anthropic_messages","stop":"error",
                "failure":"context_too_long","duration_ms":40}"#,
        )
        .unwrap();
        assert_eq!(
            model_call(&failed),
            "[model r m: 0 in, 0 out, failed: context_too_long, 40.0ms]"
        );
    }

    #[test]
    fn profiles_show_variable_names_and_labels() {
        let profiles = [
            ProfileInfo {
                name: "claude".parse().unwrap(),
                label: None,
                harness: HarnessKind::ClaudeCode,
                sets: Vec::new(),
                model: None,
            },
            ProfileInfo {
                name: "claude-alt".parse().unwrap(),
                label: Some("second account".to_owned()),
                harness: HarnessKind::ClaudeCode,
                sets: vec![
                    "CLAUDE_CONFIG_DIR".parse().unwrap(),
                    "ANTHROPIC_BASE_URL".parse().unwrap(),
                ],
                model: None,
            },
        ];
        let text = profiles_text(&profiles);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with("claude ") && lines[0].contains(" - "),
            "{text}"
        );
        assert!(
            lines[1].contains("CLAUDE_CONFIG_DIR,ANTHROPIC_BASE_URL")
                && lines[1].ends_with("second account"),
            "{text}"
        );
        assert_eq!(profiles_text(&[]), "no profiles\n");
    }

    fn profiles_text(profiles: &[ProfileInfo]) -> String {
        super::profiles(profiles)
    }

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
