//! A member materialising the home's bundle: first copy, updates, local
//! edits kept with the incoming version beside them, local deletions kept
//! until restored, removals, files Vessel never wrote left alone, and bundle
//! paths from later versions ignored.

use std::fs;

use vessel_fleet::{ConflictKind, INCOMING_SUFFIX, Managed, Step};

use crate::support::{Home, Member, canonical, read, skill_md, write};

#[test]
fn a_member_gets_copies_and_a_second_run_changes_nothing() {
    let mut home = Home::new();
    // A harness's own dot directory in the source is never bundled.
    write(&home.codex().join("skills/.own/SKILL.md"), &skill_md("own"));
    let bundled = home.publish();
    assert_eq!(bundled.manifest.generation.get(), 1);
    assert!(bundled.problems.is_empty(), "{:?}", bundled.problems);
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
            "skills/review/SKILL.md",
            "skills/review/notes/how.md"
        ]
    );

    let mut member = Member::new();
    assert_eq!(member.fetch(&bundled.manifest, &home.blobs), 3);
    let claude = member.claude();
    let applied = member.apply(&bundled.manifest, std::slice::from_ref(&claude));
    assert_eq!(applied.written, 3, "{:?}", applied.failures);
    assert!(applied.conflicts.is_empty() && applied.failures.is_empty());
    assert_eq!(read(&claude.root.join("CLAUDE.md")), "instructions v1\n");
    assert_eq!(
        read(&claude.root.join("skills/review/notes/how.md")),
        "how to review\n"
    );

    let again = vessel_fleet::plan(&bundled.manifest, &[claude], &member.managed, &[]).unwrap();
    assert!(again.steps().is_empty(), "{:?}", again.steps());
    assert_eq!(again.unchanged(), 3);
    assert_eq!(
        member.fetch(&bundled.manifest, &home.blobs),
        0,
        "nothing to fetch twice"
    );

    // The same sources bundle to the same generation.
    assert!(!home.publish().changed);
}

#[test]
fn a_local_edit_is_kept_with_the_incoming_version_beside_it() {
    let mut home = Home::new();
    let first = home.publish().manifest;
    let mut member = Member::new();
    let _fetched = member.fetch(&first, &home.blobs);
    let claude = member.claude();
    let _applied = member.apply(&first, std::slice::from_ref(&claude));

    // The member edits its instructions; the home changes them and a skill.
    let instructions = claude.root.join("CLAUDE.md");
    write(&instructions, "my own instructions\n");
    write(&home.codex().join("AGENTS.md"), "instructions v2\n");
    write(
        &home.codex().join("skills/review/notes/how.md"),
        "how to review, v2\n",
    );
    let second = home.publish();
    assert!(second.changed);
    assert_eq!(second.manifest.generation.get(), 2);

    let _fetched = member.fetch(&second.manifest, &home.blobs);
    let applied = member.apply(&second.manifest, std::slice::from_ref(&claude));
    assert_eq!(
        read(&instructions),
        "my own instructions\n",
        "the edit is kept"
    );
    let incoming = claude.root.join(format!("CLAUDE.md{INCOMING_SUFFIX}"));
    assert_eq!(read(&incoming), "instructions v2\n");
    assert_eq!(applied.conflicts[0].kind, ConflictKind::Edited);
    assert_eq!(applied.conflicted().len(), 1);
    assert_eq!(applied.conflicted()[0].as_str(), "instructions.md");
    assert_eq!(
        read(&claude.root.join("skills/review/notes/how.md")),
        "how to review, v2\n",
        "unedited files follow the home"
    );

    // Taking the incoming version settles it; the copy beside goes away.
    let _copied = fs::copy(&incoming, &instructions).unwrap();
    let settled = member.apply(&second.manifest, std::slice::from_ref(&claude));
    assert!(settled.conflicts.is_empty(), "{:?}", settled.conflicts);
    assert_eq!(settled.adopted, 1);
    assert!(!incoming.exists());
}

#[test]
fn files_vessel_did_not_write_are_never_touched() {
    let mut home = Home::new();
    let manifest = home.publish().manifest;
    let mut member = Member::new();
    let _fetched = member.fetch(&manifest, &home.blobs);
    let claude = member.claude();
    // Already there before Vessel: the user's own instructions and skill.
    write(&claude.root.join("CLAUDE.md"), "hand written\n");
    write(&claude.root.join("skills/mine/SKILL.md"), &skill_md("mine"));
    // A file of the incoming name the user made is not Vessel's either.
    write(
        &claude.root.join(format!("CLAUDE.md{INCOMING_SUFFIX}")),
        "also mine\n",
    );

    let applied = member.apply(&manifest, std::slice::from_ref(&claude));
    assert_eq!(read(&claude.root.join("CLAUDE.md")), "hand written\n");
    assert_eq!(
        read(&claude.root.join(format!("CLAUDE.md{INCOMING_SUFFIX}"))),
        "also mine\n"
    );
    let conflict = &applied.conflicts[0];
    assert_eq!(conflict.bundle.as_str(), "instructions.md");
    assert_eq!(conflict.kind, ConflictKind::NotWritten);
    assert_eq!(conflict.incoming, None, "nowhere to put it");
    assert_eq!(
        read(&claude.root.join("skills/mine/SKILL.md")),
        skill_md("mine")
    );
    assert!(
        member
            .managed
            .file(&canonical(&claude.root.join("CLAUDE.md")))
            .is_none()
    );
}

#[test]
fn a_deleted_file_stays_deleted_until_restored() {
    let mut home = Home::new();
    let first = home.publish().manifest;
    let mut member = Member::new();
    let _fetched = member.fetch(&first, &home.blobs);
    let claude = member.claude();
    let _applied = member.apply(&first, std::slice::from_ref(&claude));
    let deleted = claude.root.join("skills/review/notes/how.md");
    let recorded = member.managed.file(&canonical(&deleted)).copied();
    assert!(recorded.is_some());
    fs::remove_file(&deleted).unwrap();

    // The home changes the file too: still not brought back.
    write(
        &home.codex().join("skills/review/notes/how.md"),
        "how to review, v2\n",
    );
    let second = home.publish().manifest;
    let _fetched = member.fetch(&second, &home.blobs);
    for _ in 0..2 {
        let applied = member.apply(&second, std::slice::from_ref(&claude));
        assert!(applied.failures.is_empty(), "{:?}", applied.failures);
        assert_eq!(applied.written, 0);
        assert!(!deleted.exists(), "a deleted file does not come back");
        let conflict = &applied.conflicts[0];
        assert_eq!(applied.conflicts.len(), 1);
        assert_eq!(conflict.kind, ConflictKind::Deleted);
        assert_eq!(conflict.bundle.as_str(), "skills/review/notes/how.md");
        assert_eq!(conflict.incoming, None);
        assert_eq!(applied.conflicted().len(), 1);
        assert!(
            !claude
                .root
                .join(format!("skills/review/notes/how.md{INCOMING_SUFFIX}"))
                .exists(),
            "nothing beside it"
        );
        assert_eq!(
            member.managed.file(&canonical(&deleted)).copied(),
            recorded,
            "the record is kept"
        );
    }

    // Restoring only settles a recorded file that is missing.
    let present = claude.root.join("CLAUDE.md");
    assert_eq!(member.managed.restore(&present), 0, "a file that is there");
    assert_eq!(
        member.managed.restore(&claude.root.join("unrecorded.md")),
        0
    );
    assert_eq!(member.managed.restore(&deleted), 1);
    let restored = member.apply(&second, std::slice::from_ref(&claude));
    assert!(restored.conflicts.is_empty(), "{:?}", restored.conflicts);
    assert_eq!(restored.written, 1);
    assert_eq!(read(&deleted), "how to review, v2\n");
}

#[test]
fn bundle_paths_a_member_does_not_know_are_ignored() {
    let mut home = Home::new();
    let mut manifest = home.publish().manifest;
    let known = manifest.files.len();
    let some_file = manifest.files.values().next().unwrap().clone();
    for later in ["future/x.md", "settings.json"] {
        let _none = manifest
            .files
            .insert(later.parse().unwrap(), some_file.clone());
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
    assert_eq!(plan.steps().len(), known, "{:?}", plan.steps());
    assert!(plan.skips().is_empty(), "{:?}", plan.skips());
    let applied = vessel_fleet::apply(&plan, &member.blobs, &mut member.managed);
    assert!(applied.failures.is_empty() && applied.conflicts.is_empty());
    assert!(!claude.root.join("future").exists());
    assert!(!claude.root.join("settings.json").exists());
}

#[test]
fn files_that_leave_the_bundle_go_unless_edited() {
    let mut home = Home::new();
    write(
        &home.codex().join("skills/extra/SKILL.md"),
        &skill_md("extra"),
    );
    write(&home.codex().join("skills/extra/deep/a.md"), "a\n");
    let first = home.publish().manifest;
    let mut member = Member::new();
    let _fetched = member.fetch(&first, &home.blobs);
    let claude = member.claude();
    let _applied = member.apply(&first, std::slice::from_ref(&claude));
    // The member edits one file of the review skill.
    let edited = claude.root.join("skills/review/notes/how.md");
    write(&edited, "my notes\n");

    fs::remove_dir_all(home.codex().join("skills/extra")).unwrap();
    fs::remove_file(home.codex().join("skills/review/notes/how.md")).unwrap();
    let second = home.publish().manifest;
    let plan =
        vessel_fleet::plan(&second, std::slice::from_ref(&claude), &member.managed, &[]).unwrap();
    let removes = plan
        .steps()
        .iter()
        .filter(|step| matches!(step, Step::Remove { .. }))
        .count();
    assert_eq!(removes, 2, "{:?}", plan.steps());
    let applied = vessel_fleet::apply(&plan, &member.blobs, &mut member.managed);
    assert_eq!(applied.removed, 2);
    assert!(
        !claude.root.join("skills/extra").exists(),
        "empty directories go too"
    );
    assert!(
        claude.root.join("skills").is_dir(),
        "the skills directory stays"
    );
    assert_eq!(read(&edited), "my notes\n", "an edited file stays");
    assert!(
        member.managed.file(&canonical(&edited)).is_none(),
        "and is no longer Vessel's"
    );
}

#[test]
fn the_record_survives_a_restart() {
    let mut home = Home::new();
    let manifest = home.publish().manifest;
    let mut member = Member::new();
    let _fetched = member.fetch(&manifest, &home.blobs);
    let claude = member.claude();
    let _applied = member.apply(&manifest, std::slice::from_ref(&claude));
    let record = member.dir.path().join("data/fleet/managed.json");
    member.managed.save(&record).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(&record).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let loaded = Managed::load(&record).unwrap();
    assert_eq!(loaded, member.managed);
    assert_eq!(
        Managed::load(&member.dir.path().join("none.json"))
            .unwrap()
            .files()
            .count(),
        0
    );
    fs::write(&record, "{\"version\": 9}").unwrap();
    assert!(
        Managed::load(&record).is_err(),
        "a record from elsewhere is not guessed at"
    );
}

#[test]
fn dot_entries_inside_a_skill_are_left_out_and_reported() {
    let mut home = Home::new();
    write(&home.codex().join("skills/review/.env"), "TOKEN=secret\n");
    write(&home.codex().join("skills/review/.git/config"), "[core]\n");
    write(&home.codex().join("skills/review/.cache-me"), "skipped\n");
    home.sources.skip.push(".cache*".parse().unwrap());
    let bundled = home.publish();
    assert!(
        bundled
            .manifest
            .files
            .keys()
            .all(|path| !path.as_str().contains("/.")),
        "{:?}",
        bundled.manifest.files.keys()
    );
    let hidden: Vec<String> = bundled
        .problems
        .iter()
        .filter_map(|problem| {
            if let vessel_fleet::SourceProblem::Hidden(path) = problem {
                Some(path.file_name().unwrap().to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(hidden, [".env", ".git"], "a skipped name is not reported");
}

#[cfg(unix)]
#[test]
fn fleet_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let mut home = Home::new();
    let script = home.codex().join("skills/review/run.sh");
    write(&script, "#!/bin/sh\n");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let readable = home.codex().join("skills/review/notes/how.md");
    fs::set_permissions(&readable, fs::Permissions::from_mode(0o644)).unwrap();
    let manifest = home.publish().manifest;
    let mut member = Member::new();
    let _fetched = member.fetch(&manifest, &home.blobs);
    let claude = member.claude();
    fs::create_dir_all(&claude.root).unwrap();
    let applied = member.apply(&manifest, std::slice::from_ref(&claude));
    assert!(applied.failures.is_empty(), "{:?}", applied.failures);
    assert_eq!(mode(&claude.root.join("CLAUDE.md")), 0o600);
    assert_eq!(mode(&claude.root.join("skills/review/notes/how.md")), 0o600);
    assert_eq!(mode(&claude.root.join("skills/review/run.sh")), 0o700);
    for dir in ["skills", "skills/review", "skills/review/notes"] {
        assert_eq!(mode(&claude.root.join(dir)), 0o700, "{dir}");
    }
}
