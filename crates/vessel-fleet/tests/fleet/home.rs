//! The operator's shape (Unix: it is made of symlinks): one source in
//! `~/.codex`, Claude homes linking to it (the instructions file and one
//! link per skill, named by `SKILL.md`), a skill directory named
//! differently from its skill, skipped globs, and symlinks that lead out.

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::PathBuf;

use vessel_fleet::{SkipReason, SourceProblem, Target};

use crate::support::{Home, Member, read, skill_md, write};

/// The home's sources, as the operator keeps them, with the extras the
/// bundle must leave out; the Claude home linked to them.
fn operator_home() -> (Home, Target) {
    let home = Home::new();
    let codex = home.codex();
    // A directory named in another script, installed under its `name`.
    write(
        &codex.join("skills/노드추가시쓸것/SKILL.md"),
        &skill_md("node-add-uploader"),
    );
    write(&codex.join("skills/노드추가시쓸것/ref.md"), "reference\n");
    let script = codex.join("skills/review/run.sh");
    write(&script, "#!/bin/sh\necho review\n");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    // Managed by someone else (skip globs), or not a skill at all.
    write(&codex.join("skills/.system/SKILL.md"), &skill_md("system"));
    write(
        &codex.join("skills/paseo-advisor/SKILL.md"),
        &skill_md("paseo-advisor"),
    );
    write(&codex.join("skills/broken/SKILL.md"), "no frontmatter\n");
    write(&codex.join("skills/README.md"), "not a skill\n");
    // A link inside a skill that leads out of the skills directory.
    write(&home.dir.path().join("private/id_key"), "secret material\n");
    symlink(
        home.dir.path().join("private/id_key"),
        codex.join("skills/review/leak"),
    )
    .unwrap();

    let claude = Target {
        root: home.dir.path().join("claude"),
        instructions: Some(PathBuf::from("CLAUDE.md")),
        skills: Some(PathBuf::from("skills")),
    };
    fs::create_dir_all(claude.root.join("skills")).unwrap();
    symlink(codex.join("AGENTS.md"), claude.root.join("CLAUDE.md")).unwrap();
    symlink(
        codex.join("skills/review"),
        claude.root.join("skills/review"),
    )
    .unwrap();
    symlink(
        codex.join("skills/노드추가시쓸것"),
        claude.root.join("skills/node-add-uploader"),
    )
    .unwrap();
    (home, claude)
}

#[test]
fn the_bundle_names_skills_by_skill_md_and_leaves_out_what_it_must() {
    let (mut home, _claude) = operator_home();
    let bundled = home.publish();
    let paths: Vec<&str> = bundled
        .manifest
        .files
        .keys()
        .map(vessel_proto::BundlePath::as_str)
        .collect();
    assert_eq!(
        paths,
        [
            "instructions.md",
            "skills/node-add-uploader/SKILL.md",
            "skills/node-add-uploader/ref.md",
            "skills/review/SKILL.md",
            "skills/review/notes/how.md",
            "skills/review/run.sh",
        ]
    );
    let script = &bundled.manifest.files[&"skills/review/run.sh".parse().unwrap()];
    assert!(script.executable);
    assert!(bundled.problems.iter().any(
        |problem| matches!(problem, SourceProblem::SkillName { dir, .. } if dir.ends_with("broken"))
    ));
    assert!(
        bundled.problems.iter().any(
            |problem| matches!(problem, SourceProblem::Symlink(path) if path.ends_with("leak"))
        )
    );
    assert_eq!(bundled.problems.len(), 2, "{:?}", bundled.problems);
}

#[test]
fn on_the_home_linked_homes_and_the_source_itself_are_left_alone() {
    let (mut home, claude) = operator_home();
    let manifest = home.publish().manifest;
    let codex = Target {
        root: home.codex(),
        instructions: Some(PathBuf::from("AGENTS.md")),
        skills: Some(PathBuf::from("skills")),
    };
    // A third home on the same machine with no links: it gets copies.
    let alt = Target {
        root: home.dir.path().join("claude-alt"),
        ..claude.clone()
    };
    fs::create_dir(&alt.root).unwrap();
    let source_paths = home.sources.paths();
    let mut managed = vessel_fleet::Managed::default();
    let plan = vessel_fleet::plan(
        &manifest,
        &[claude.clone(), codex, alt.clone()],
        &managed,
        &source_paths,
    )
    .unwrap();
    // Every place in the linked home and in the source is a skip...
    let sourced = plan
        .skips()
        .iter()
        .filter(|skip| skip.reason == SkipReason::Source)
        .count();
    assert_eq!(sourced, 2 * manifest.files.len(), "{:?}", plan.skips());
    // ...and the unlinked home gets every file.
    let applied = vessel_fleet::apply(&plan, &home.blobs, &mut managed);
    assert_eq!(
        applied.written,
        manifest.files.len(),
        "{:?}",
        applied.failures
    );
    assert_eq!(
        read(&alt.root.join("skills/node-add-uploader/ref.md")),
        "reference\n"
    );
    // The sources are exactly as they were: no copy under the skill's
    // English name, nothing beside the instructions.
    assert!(!home.codex().join("skills/node-add-uploader").exists());
    assert!(!home.codex().join("AGENTS.md.vessel-incoming").exists());
    assert_eq!(
        fs::read_link(claude.root.join("CLAUDE.md")).unwrap(),
        home.codex().join("AGENTS.md")
    );
}

#[test]
fn a_member_never_follows_a_symlink_out_of_its_home() {
    let (mut home, _claude) = operator_home();
    let manifest = home.publish().manifest;
    let mut member = Member::new();
    let _fetched = member.fetch(&manifest, &home.blobs);
    let claude = member.claude();
    // The member's skills directory has a link to somewhere else.
    let elsewhere = member.dir.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::create_dir_all(claude.root.join("skills")).unwrap();
    symlink(&elsewhere, claude.root.join("skills/review")).unwrap();

    let applied = member.apply(&manifest, std::slice::from_ref(&claude));
    assert!(applied.failures.is_empty(), "{:?}", applied.failures);
    assert_eq!(
        fs::read_dir(&elsewhere).unwrap().count(),
        0,
        "nothing written through the link"
    );
    assert_eq!(
        read(&claude.root.join("skills/node-add-uploader/SKILL.md")),
        skill_md("node-add-uploader")
    );
    let mode = fs::metadata(claude.root.join("skills/node-add-uploader/ref.md"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0, "not executable");
    let plan = vessel_fleet::plan(
        &manifest,
        std::slice::from_ref(&claude),
        &member.managed,
        &[],
    )
    .unwrap();
    assert!(
        plan.skips()
            .iter()
            .all(|skip| matches!(skip.reason, SkipReason::Symlink(_)))
    );
    assert_eq!(
        plan.skips().len(),
        3,
        "the review skill's files: {:?}",
        plan.skips()
    );
}
