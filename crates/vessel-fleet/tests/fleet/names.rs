//! Names that are one file on macOS or Windows (letter case, Unicode
//! normalisation), and the names members keep for incoming versions.

use std::fs;

use vessel_fleet::{SkipReason, SourceProblem};

use crate::support::{Home, Member, skill_md, write};

#[test]
fn names_that_would_be_one_file_somewhere_are_bundled_once_and_reported() {
    let mut home = Home::new();
    let review = home.codex().join("skills/review");
    write(&review.join("README.md"), "upper\n");
    write(&review.join("readme.md"), "lower\n");
    write(&review.join("caf\u{e9}.md"), "composed\n");
    write(&review.join("cafe\u{301}.md"), "decomposed\n");
    write(&review.join("x.md.vessel-incoming"), "reserved\n");
    write(&home.codex().join("skills/a/SKILL.md"), &skill_md("Deploy"));
    write(&home.codex().join("skills/b/SKILL.md"), &skill_md("deploy"));
    // On a filesystem that already treats a pair as one file there is one
    // entry of it, and nothing to report for it.
    let names: Vec<String> = fs::read_dir(&review)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    let apart = [
        ["README.md", "readme.md"],
        ["caf\u{e9}.md", "cafe\u{301}.md"],
    ]
    .iter()
    .filter(|pair| {
        pair.iter()
            .all(|name| names.iter().any(|found| found == name))
    })
    .count();
    let bundled = home.publish();

    let files: Vec<String> = bundled
        .manifest
        .files
        .keys()
        .map(|path| path.as_str().to_lowercase())
        .collect();
    let readme = files
        .iter()
        .filter(|path| path.ends_with("/readme.md"))
        .count();
    assert_eq!(readme, 1, "{files:?}");
    assert!(!files.iter().any(|path| path.ends_with(".vessel-incoming")));
    assert_eq!(
        files
            .iter()
            .filter(|path| path.starts_with("skills/deploy/"))
            .count(),
        1,
        "{files:?}"
    );
    let collisions = bundled
        .problems
        .iter()
        .filter(|problem| matches!(problem, SourceProblem::NameCollision { .. }))
        .count();
    assert_eq!(collisions, apart, "{:?}", bundled.problems);
    assert!(
        bundled
            .problems
            .iter()
            .any(|problem| matches!(problem, SourceProblem::ReservedName(_)))
    );
    assert!(bundled.problems.iter().any(
        |problem| matches!(problem, SourceProblem::DuplicateSkill { name, .. } if name == "deploy")
    ));
}

#[test]
fn a_member_places_only_the_first_of_colliding_paths() {
    let mut home = Home::new();
    let mut manifest = home.publish().manifest;
    let some_file = manifest.files.values().next().unwrap().clone();
    for added in [
        "skills/review/NOTES.md",
        "skills/review/notes.md",
        "skills/review/how.md.vessel-incoming",
    ] {
        let _none = manifest
            .files
            .insert(added.parse().unwrap(), some_file.clone());
    }
    let mut member = Member::new();
    let _fetched = member.fetch(&manifest, &home.blobs);
    let claude = member.claude();
    let plan = vessel_fleet::plan(
        &manifest,
        std::slice::from_ref(&claude),
        &member.managed,
        &[],
    )
    .unwrap();
    let reasons: Vec<&SkipReason> = plan.skips().iter().map(|skip| &skip.reason).collect();
    assert_eq!(
        reasons,
        // In manifest order: `NOTES.md`, `how.md.vessel-incoming`,
        // `notes.md`.
        [&SkipReason::Unplaced, &SkipReason::Collision],
        "{:?}",
        plan.skips()
    );
    let applied = vessel_fleet::apply(&plan, &member.blobs, &mut member.managed);
    assert!(applied.failures.is_empty(), "{:?}", applied.failures);
    assert!(
        !claude
            .root
            .join("skills/review/how.md.vessel-incoming")
            .exists()
    );
}
