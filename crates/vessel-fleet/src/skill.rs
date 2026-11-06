//! A skill's name: the `name` field in its `SKILL.md` frontmatter.
//!
//! [Decision Log]
//! - Purpose: a skill is installed under its name, not its directory's
//!   (the operator's skills include a directory named in another script
//!   whose `SKILL.md` says `name: node-add-uploader`), as the operator's
//!   sync script and the harnesses do.
//! - Choice: read that one field and nothing else, strictly, without a YAML
//!   library: the frontmatter is the block between a first line `---` and
//!   the next `---`; the field is a top-level `name:` line (no
//!   indentation, so a `name:` inside an indented block value is not it);
//!   its value is plain or quoted with nothing to unescape. Anything a YAML
//!   reader could read two ways (a comment, an escape, a flow or block
//!   value, a second `name:`) is refused, so the name Vessel picks is the
//!   name every harness picks.
//! - The name must work as one directory name on every OS (it becomes
//!   `skills/<name>` in the bundle, under the bundle path rules), and may
//!   not start with `.`: dot directories under a harness's skills are its
//!   own (Codex's `skills/.system`), and no bundle writes there.

use std::str::FromStr as _;

use vessel_proto::BundlePath;

/// Why a `SKILL.md` names no usable skill.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkillNameError {
    /// The file does not start with a `---` line.
    #[error("SKILL.md does not start with `---` frontmatter")]
    NoFrontmatter,
    /// No closing `---`.
    #[error("SKILL.md's frontmatter has no closing `---`")]
    Unterminated,
    /// No top-level `name:`.
    #[error("SKILL.md's frontmatter has no `name:`")]
    NoName,
    /// More than one.
    #[error("SKILL.md's frontmatter has more than one `name:`")]
    ManyNames,
    /// A value a YAML reader could take another way.
    #[error("SKILL.md's `name:` value `{0}` is not a plain or simply quoted name")]
    Unclear(String),
    /// A value that is not usable as a directory name.
    #[error("SKILL.md's name `{0}` cannot be a directory name on every system")]
    NotAName(String),
}

/// The skill name `SKILL.md`'s text declares.
///
/// # Errors
/// See [`SkillNameError`].
pub fn skill_name(text: &str) -> Result<String, SkillNameError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return Err(SkillNameError::NoFrontmatter);
    }
    let mut name = None;
    let mut closed = false;
    for line in lines {
        let line = line.trim_end();
        if line == "---" {
            closed = true;
            break;
        }
        let Some(value) = line.strip_prefix("name:") else {
            continue;
        };
        if name.is_some() {
            return Err(SkillNameError::ManyNames);
        }
        name = Some(value.trim());
    }
    if !closed {
        return Err(SkillNameError::Unterminated);
    }
    let value = name.ok_or(SkillNameError::NoName)?;
    let name = unquote(value).ok_or_else(|| SkillNameError::Unclear(value.to_owned()))?;
    // A dot name is a harness's own (Codex keeps `skills/.system`).
    let usable = !name.is_empty()
        && !name.starts_with('.')
        && !name.contains(['/', '\\'])
        && BundlePath::from_str(&format!("{}/{name}", crate::SKILLS))
            .is_ok_and(|path| path.segments().count() == 2);
    if !usable {
        return Err(SkillNameError::NotAName(name.to_owned()));
    }
    Ok(name.to_owned())
}

/// A plain value, or one in single or double quotes with nothing inside
/// that would need unescaping; `None` for anything else.
fn unquote(value: &str) -> Option<&str> {
    for quote in ['"', '\''] {
        if let Some(rest) = value.strip_prefix(quote) {
            let inner = rest.strip_suffix(quote)?;
            let escaping = inner.contains(quote) || (quote == '"' && inner.contains('\\'));
            return (!escaping).then_some(inner);
        }
    }
    // Plain: no comment, no mapping, no indicator a YAML reader treats
    // specially at the start.
    let special_start = value.starts_with([
        '[', ']', '{', '}', '&', '*', '!', '|', '>', '%', '@', '`', '#', ',', '?', '-', ':',
    ]);
    let ambiguous = value.contains(" #") || value.contains(": ") || value.ends_with(':');
    (!special_start && !ambiguous).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name_line: &str) -> Result<String, SkillNameError> {
        skill_name(&format!(
            "---\n{name_line}\ndescription: >\n  text\n  name: not-this\n---\n# Body\nname: nor-this\n"
        ))
    }

    #[test]
    fn the_top_level_name_is_read_plain_or_quoted() {
        assert_eq!(
            named("name: node-add-uploader").unwrap(),
            "node-add-uploader"
        );
        assert_eq!(named("name: \"quoted name\"").unwrap(), "quoted name");
        assert_eq!(named("name: 'single'").unwrap(), "single");
        assert_eq!(named("name:   spaced   ").unwrap(), "spaced");
        let windows = "\u{feff}---\r\nname: crlf\r\n---\r\n";
        assert_eq!(skill_name(windows).unwrap(), "crlf");
    }

    #[test]
    fn anything_a_yaml_reader_could_read_otherwise_is_refused() {
        for line in [
            "name: with # comment",
            "name: \"esc\\\"aped\"",
            "name: 'it''s'",
            "name: [a, b]",
            "name: |",
            "name: key: value",
            "name: \"unclosed",
        ] {
            assert!(
                matches!(named(line), Err(SkillNameError::Unclear(_))),
                "{line}"
            );
        }
        assert_eq!(named("name: a\nname: b"), Err(SkillNameError::ManyNames));
        assert_eq!(named("title: x"), Err(SkillNameError::NoName));
        assert_eq!(skill_name("name: x\n"), Err(SkillNameError::NoFrontmatter));
        assert_eq!(
            skill_name("---\nname: x\n"),
            Err(SkillNameError::Unterminated)
        );
    }

    #[test]
    fn a_name_must_be_one_directory_name_everywhere() {
        for line in [
            "name: ..",
            "name: a/b",
            "name: \"a\\\\b\"",
            "name: CON",
            "name: \"\"",
            "name: trailing.",
        ] {
            assert!(named(line).is_err(), "{line}");
        }
        assert_eq!(named("name: 노드-추가").unwrap(), "노드-추가");
    }
}
