//! A skill's name: the `name` field in its `SKILL.md` frontmatter.
//!
//! [Decision Log]
//! - Purpose: a skill is installed under its name, not its directory's
//!   (the operator's skills include a directory named in another script
//!   whose `SKILL.md` says `name: node-add-uploader`), as the operator's
//!   sync script and the harnesses do.
//! - Choice: read that one field and nothing else, strictly, without a YAML
//!   library: the frontmatter is the block between a first line `---` and
//!   the next `---`; the field is a top-level key `name` (plain or quoted,
//!   `name:`, `"name":`, `name :`), its value plain or quoted with nothing
//!   to unescape. Anything a YAML reader could read another way is
//!   refused, so the name Vessel picks is the name every harness picks:
//!   a comment, an escape, a flow or block value, a second `name` key in
//!   any spelling, a value continued on a more indented line (`name: foo`
//!   then `  bar` is `foo bar`), a complex key (`? name`), and a plain
//!   value YAML reads as something other than text (`~`, `null`, `true`,
//!   `yes`, `12`, `1e3`, `0x1f`, `1:30`, a date). The independent review
//!   checked these against `PyYAML`.
//! - The name must work as one directory name on every OS (it becomes
//!   `skills/<name>` in the bundle, under the bundle path rules), and may
//!   not start with `.`: dot directories under a harness's skills are its
//!   own (Codex's `skills/.system`), and no bundle writes there. A
//!   [`SkillName`] can only be made by these checks.

use std::fmt;
use std::str::FromStr as _;

use vessel_proto::BundlePath;

/// A skill's name, checked: usable as one directory name everywhere, not
/// starting with `.`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SkillName(String);

impl SkillName {
    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn checked(name: &str) -> Result<Self, SkillNameError> {
        // A dot name is a harness's own (Codex keeps `skills/.system`).
        let usable = !name.is_empty()
            && !name.starts_with('.')
            && !name.contains(['/', '\\'])
            && BundlePath::from_str(&format!("{}/{name}", crate::SKILLS))
                .is_ok_and(|path| path.segments().count() == 2);
        if usable {
            Ok(Self(name.to_owned()))
        } else {
            Err(SkillNameError::NotAName(name.to_owned()))
        }
    }
}

impl fmt::Display for SkillName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl PartialEq<str> for SkillName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

/// Why a `SKILL.md` names no usable skill.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkillNameError {
    /// The file does not start with a `---` line.
    #[error("SKILL.md does not start with `---` frontmatter")]
    NoFrontmatter,
    /// No closing `---`.
    #[error("SKILL.md's frontmatter has no closing `---`")]
    Unterminated,
    /// No top-level `name`.
    #[error("SKILL.md's frontmatter has no `name:`")]
    NoName,
    /// More than one, in any spelling of the key.
    #[error("SKILL.md's frontmatter has more than one `name:`")]
    ManyNames,
    /// A value (or key) a YAML reader could take another way.
    #[error("SKILL.md's `name:` value `{0}` is not a plain or simply quoted name")]
    Unclear(String),
    /// A plain value YAML reads as a null, boolean, number or date.
    #[error("SKILL.md's `name:` value `{0}` is not text in YAML; quote it")]
    NotText(String),
    /// A value that is not usable as a directory name.
    #[error("SKILL.md's name `{0}` cannot be a directory name on every system")]
    NotAName(String),
}

/// The skill name `SKILL.md`'s text declares.
///
/// # Errors
/// See [`SkillNameError`].
pub fn skill_name(text: &str) -> Result<SkillName, SkillNameError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines().map(str::trim_end);
    if lines.next() != Some("---") {
        return Err(SkillNameError::NoFrontmatter);
    }
    let mut name = None;
    let mut closed = false;
    // After the name line: lines a YAML reader would take as its
    // continuation (more indented), until the next top-level line.
    let mut continuing = false;
    for line in lines {
        if line == "---" {
            closed = true;
            break;
        }
        let indented = line.starts_with([' ', '\t']);
        if continuing {
            if indented && !line.trim().is_empty() {
                let value = name.unwrap_or_default();
                return Err(SkillNameError::Unclear(format!("{value} {}", line.trim())));
            }
            continuing = indented || line.is_empty();
        }
        if indented || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('?') {
            return Err(SkillNameError::Unclear(line.to_owned()));
        }
        let Some(value) = name_value(line) else {
            continue;
        };
        if name.is_some() {
            return Err(SkillNameError::ManyNames);
        }
        name = Some(value);
        continuing = true;
    }
    if !closed {
        return Err(SkillNameError::Unterminated);
    }
    let value = name.ok_or(SkillNameError::NoName)?;
    let name = match unquote(value) {
        Some(Quoted::Quoted(inner)) => inner,
        Some(Quoted::Plain(plain)) if not_text(plain) => {
            return Err(SkillNameError::NotText(plain.to_owned()));
        }
        Some(Quoted::Plain(plain)) => plain,
        None => return Err(SkillNameError::Unclear(value.to_owned())),
    };
    SkillName::checked(name)
}

/// The value of a top-level line whose key is `name` (`name: x`,
/// `"name": x`, `'name' : x`); `None` for any other key.
fn name_value(line: &str) -> Option<&str> {
    let (key, rest) = if let Some(quote) = line.chars().next().filter(|c| matches!(c, '"' | '\'')) {
        let inner = line.get(1..)?;
        let end = inner.find(quote)?;
        (inner.get(..end)?, inner.get(end.checked_add(1)?..)?)
    } else {
        // A plain key ends at the first `:` followed by a space or the end.
        let colon = line
            .char_indices()
            .find(|&(index, c)| {
                let after = line.get(index.saturating_add(1)..).unwrap_or_default();
                c == ':' && (after.is_empty() || after.starts_with([' ', '\t']))
            })
            .map(|(index, _c)| index)?;
        (line.get(..colon)?.trim_end(), line.get(colon..)?)
    };
    if key != "name" {
        return None;
    }
    rest.trim_start().strip_prefix(':').map(str::trim)
}

/// A value as written.
enum Quoted<'a> {
    /// In quotes: always text.
    Quoted(&'a str),
    /// Plain: text only if YAML does not read it as something else.
    Plain(&'a str),
}

/// A plain value, or one in single or double quotes with nothing inside
/// that would need unescaping; `None` for anything else.
fn unquote(value: &str) -> Option<Quoted<'_>> {
    for quote in ['"', '\''] {
        if let Some(rest) = value.strip_prefix(quote) {
            let inner = rest.strip_suffix(quote)?;
            let escaping = inner.contains(quote) || (quote == '"' && inner.contains('\\'));
            return (!escaping).then_some(Quoted::Quoted(inner));
        }
    }
    // Plain: no comment, no mapping, no indicator a YAML reader treats
    // specially at the start.
    let special_start = value.starts_with([
        '[', ']', '{', '}', '&', '*', '!', '|', '>', '%', '@', '`', '#', ',', '?', '-', ':', '"',
        '\'',
    ]);
    let ambiguous = value.contains(" #") || value.contains(": ") || value.ends_with(':');
    (!special_start && !ambiguous).then_some(Quoted::Plain(value))
}

/// Whether YAML (1.1, as `PyYAML` reads it, or 1.2) takes a plain value as a
/// null, a boolean, a number or a date rather than text.
fn not_text(value: &str) -> bool {
    const WORDS: [&str; 17] = [
        "", "~", "null", "y", "n", "yes", "no", "true", "false", "on", "off", ".inf", "+.inf",
        "-.inf", ".nan", "<<", "=",
    ];
    let lower = value.to_ascii_lowercase();
    WORDS.contains(&lower.as_str()) || number(&lower) || date(&lower)
}

/// Whether a character is a digit of some radix.
type Digit = fn(&char) -> bool;

/// `12`, `+1_000`, `0x1f`, `0o17`, `0b101`, `1.5`, `1e3`, `1:30`.
fn number(value: &str) -> bool {
    let unsigned = value.strip_prefix(['+', '-']).unwrap_or(value);
    let radixes: [(&str, Digit); 3] = [
        ("0x", char::is_ascii_hexdigit),
        ("0o", |c| matches!(c, '0'..='7')),
        ("0b", |c| matches!(c, '0' | '1')),
    ];
    for (prefix, digit) in radixes {
        if let Some(digits) = unsigned.strip_prefix(prefix) {
            return !digits.is_empty() && digits.chars().all(|c| c == '_' || digit(&c));
        }
    }
    let (mantissa, exponent) = match unsigned.split_once('e') {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (unsigned, None),
    };
    let digits_ok = mantissa.starts_with(|c: char| c.is_ascii_digit() || c == '.')
        && mantissa.chars().any(|c| c.is_ascii_digit())
        && mantissa
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '_' | '.' | ':'))
        && mantissa.matches('.').count() <= 1;
    let exponent_ok = exponent.is_none_or(|exponent| {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
    });
    digits_ok && exponent_ok
}

/// `2001-12-14` and the timestamps that start with one.
fn date(value: &str) -> bool {
    let mut parts = value.splitn(3, '-');
    let year = parts.next().unwrap_or_default();
    let month = parts.next().unwrap_or_default();
    let day: String = parts
        .next()
        .unwrap_or_default()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    year.len() == 4
        && year.chars().all(|c| c.is_ascii_digit())
        && (1..=2).contains(&month.len())
        && month.chars().all(|c| c.is_ascii_digit())
        && (1..=2).contains(&day.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name_line: &str) -> Result<SkillName, SkillNameError> {
        skill_name(&format!(
            "---\n{name_line}\ndescription: >\n  text\n  name: not-this\n---\n# Body\nname: nor-this\n"
        ))
    }

    #[test]
    fn the_top_level_name_is_read_plain_or_quoted() {
        for (line, name) in [
            ("name: node-add-uploader", "node-add-uploader"),
            ("name: \"quoted name\"", "quoted name"),
            ("name: 'single'", "single"),
            ("name:   spaced   ", "spaced"),
            ("\"name\": key-quoted", "key-quoted"),
            ("name : spaced-key", "spaced-key"),
            ("name: \"true\"", "true"),
            ("name: '12'", "12"),
            ("name: 2fa-helper", "2fa-helper"),
            ("name: v1.0.0", "v1.0.0"),
            ("name: 1.0.0", "1.0.0"),
            ("name: deadbeef", "deadbeef"),
        ] {
            assert_eq!(named(line).unwrap(), *name, "{line}");
        }
        let windows = "\u{feff}---\r\nname: crlf\r\n---\r\n";
        assert_eq!(skill_name(windows).unwrap(), *"crlf");
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
            "name: foo\n  bar",
            "name: foo\n\n  bar",
            "name:\n  foo",
            "? name\n: foo",
        ] {
            assert!(
                matches!(named(line), Err(SkillNameError::Unclear(_))),
                "{line}: {:?}",
                named(line)
            );
        }
        for line in [
            "name: ~",
            "name: null",
            "name: Null",
            "name: true",
            "name: False",
            "name: yes",
            "name: off",
            "name: 12",
            "name: +1_000",
            "name: 0x1f",
            "name: 1e3",
            "name: 1.5",
            "name: 1:30",
            "name: 2001-12-14",
            "name: 0o17",
        ] {
            assert!(
                matches!(named(line), Err(SkillNameError::NotText(_))),
                "{line}: {:?}",
                named(line)
            );
        }
        assert_eq!(named("name: a\nname: b"), Err(SkillNameError::ManyNames));
        assert_eq!(
            named("name: a\n\"name\": b"),
            Err(SkillNameError::ManyNames),
            "a quoted key is the same key"
        );
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
            "name: .hidden",
        ] {
            assert!(named(line).is_err(), "{line}");
        }
        assert_eq!(named("name: 노드-추가").unwrap(), *"노드-추가");
    }
}
