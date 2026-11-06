use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::profile::parse_profiles;

/// A fresh directory, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("vessel-harness-setup-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn home(&self, name: &str, files: &[&str]) {
        let dir = self.0.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for file in files {
            std::fs::write(dir.join(file), "").unwrap();
        }
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.0));
    }
}

#[test]
fn everything_is_missing_from_an_empty_search_path() {
    let empty = Dir::new();
    let missing = missing(&Lookup::new([empty.0.clone()]));
    let harnesses: Vec<HarnessKind> = missing.iter().map(|install| install.harness).collect();
    assert_eq!(harnesses, HarnessKind::ALL);
    for install in &missing {
        assert_eq!(install.part, InstallPart::Harness);
        assert_eq!(install.program, "npm");
        assert_eq!(install.args().first(), Some(&"install"), "{install:?}");
    }
}

#[test]
fn an_installed_harness_without_its_adapter_is_offered_the_adapter() {
    let bin = Dir::new();
    let tui = vessel_testkit::Fake::Tui.path().unwrap();
    drop(
        crate::fakes::install(
            &bin.0,
            HarnessKind::ClaudeCode,
            crate::fakes::Role::Terminal,
            &tui,
        )
        .unwrap(),
    );
    let missing = missing(&Lookup::new([bin.0.clone()]));
    let claude = missing
        .iter()
        .find(|install| install.harness == HarnessKind::ClaudeCode)
        .unwrap();
    assert_eq!(claude.part, InstallPart::Adapter);
    assert!(claude.command.contains("claude-agent-acp"), "{claude:?}");
}

#[test]
fn other_accounts_homes_are_found_by_name_and_login_file() {
    let home = Dir::new();
    home.home(".claude", &[".credentials.json"]);
    home.home(".claude-work", &[".credentials.json"]);
    home.home(".claude-side", &[".claude.json"]);
    home.home(".claude-empty", &["settings.json"]);
    home.home(".claude-code", &[".credentials.json"]);
    home.home(".codex-review", &["auth.json"]);
    home.home(".gemini-other", &["oauth_creds.json"]);
    std::fs::write(home.0.join(".claude.json"), "").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(home.0.join(".claude-work"), home.0.join(".claude-link")).unwrap();

    let found = account_homes(&home.0).unwrap();
    let names: Vec<(&str, HarnessKind)> = found
        .iter()
        .map(|account| (account.name.as_str(), account.harness))
        .collect();
    assert_eq!(
        names,
        [
            ("claude-code", HarnessKind::ClaudeCode),
            ("claude-side", HarnessKind::ClaudeCode),
            ("claude-work", HarnessKind::ClaudeCode),
            ("codex-review", HarnessKind::Codex),
        ]
    );
    // A directory named after a harness cannot be a profile's name.
    let reserved = found.first().unwrap();
    assert_eq!(reserved.profile_name(), None);
    assert_eq!(found[2].profile_name().unwrap().as_str(), "claude-work");
}

#[test]
fn a_profile_is_added_and_the_file_keeps_its_own_text() {
    let name: ProfileName = "claude-work".parse().unwrap();
    let fresh = add_profile(
        "",
        &name,
        HarnessKind::ClaudeCode,
        "CLAUDE_CONFIG_DIR",
        "~/.claude-work",
    )
    .unwrap();
    let home = Dir::new();
    let profiles = parse_profiles(&fresh, Some(&home.0)).unwrap();
    let profile = profiles.get(&name).unwrap();
    assert_eq!(profile.harness(), HarnessKind::ClaudeCode);
    assert_eq!(
        crate::home(profile, &home.0, |_| None),
        Some(home.0.join(".claude-work"))
    );

    let existing = "# my accounts\n[profiles.codex-side]\nharness = \"codex\" # kept\n";
    let added = add_profile(
        existing,
        &name,
        HarnessKind::ClaudeCode,
        "CLAUDE_CONFIG_DIR",
        "~/.claude-work",
    )
    .unwrap();
    assert!(added.starts_with(existing), "{added}");
    assert_eq!(
        parse_profiles(&added, Some(&home.0))
            .unwrap()
            .defined()
            .count(),
        2
    );

    let taken: ProfileName = "Codex-Side".parse().unwrap();
    assert!(matches!(
        add_profile(
            existing,
            &taken,
            HarnessKind::Codex,
            "CODEX_HOME",
            "~/.codex-side"
        ),
        Err(AddProfileError::Taken(_))
    ));
    assert!(matches!(
        add_profile(
            "profiles = 3\n",
            &name,
            HarnessKind::ClaudeCode,
            "CLAUDE_CONFIG_DIR",
            "x"
        ),
        Err(AddProfileError::ProfilesNotATable)
    ));
}
