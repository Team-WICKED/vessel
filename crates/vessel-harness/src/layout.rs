//! [`layout`]: where a harness keeps configuration a fleet shares, inside
//! its home ([`home`](crate::home)).
//!
//! Track F's fleet sync materialises instruction files and skills into each
//! harness's home; which names to use is harness knowledge, so it lives
//! here (`AGENTS.md` section 3).
//!
//! Grounding (2026-11-06; file names in real homes, `--help` with throwaway
//! homes, and the harnesses' own code, never configuration contents):
//!
//! - Claude Code 2.1.318: `CLAUDE.md` (user-level instructions) and
//!   `skills/<name>/SKILL.md` in `CLAUDE_CONFIG_DIR` (default `~/.claude`).
//! - Codex CLI 0.156.1: `AGENTS.md` and `skills/<name>/SKILL.md` in
//!   `CODEX_HOME` (default `~/.codex`); its bundled skill installer writes
//!   to `$CODEX_HOME/skills`. `skills/.system/` holds Codex's own skills:
//!   a sync must leave dot-directories there alone.
//! - `OpenCode` 1.18.23: `AGENTS.md` and `{skill,skills}/**/SKILL.md` in its
//!   configuration directory (`~/.config/opencode`). It also reads
//!   `~/.claude/CLAUDE.md` and `~/.claude/skills`, `~/.agents/skills`
//!   unless told not to, so a fleet that fills Claude Code's home reaches it
//!   twice; deduplicating is the fleet's call.
//! - Gemini CLI (not installed where this was grounded): `GEMINI.md` in
//!   `~/.gemini`, its documented default context file name (configurable
//!   with `context.fileName`). Skills are unconfirmed, so `None`.

use vessel_proto::HarnessKind;

use crate::harnesses::descriptor;

/// Where shared configuration lives inside a harness's home. Every path is
/// relative to the home; `None` means the harness has no such place, or it
/// was not confirmed (the module docs say which).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessLayout {
    /// The user-level instruction file the harness reads in every session.
    pub instructions: Option<&'static str>,
    /// The directory of skills: one directory per skill, holding
    /// `SKILL.md`.
    pub skills: Option<&'static str>,
}

/// The layout of `harness`'s home.
#[must_use]
pub const fn layout(harness: HarnessKind) -> HarnessLayout {
    descriptor(harness).layout
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_are_as_grounded() {
        let named = |instructions, skills| HarnessLayout {
            instructions,
            skills,
        };
        assert_eq!(
            layout(HarnessKind::ClaudeCode),
            named(Some("CLAUDE.md"), Some("skills"))
        );
        assert_eq!(
            layout(HarnessKind::Codex),
            named(Some("AGENTS.md"), Some("skills"))
        );
        assert_eq!(
            layout(HarnessKind::OpenCode),
            named(Some("AGENTS.md"), Some("skills"))
        );
        assert_eq!(layout(HarnessKind::Gemini), named(Some("GEMINI.md"), None));
    }
}
