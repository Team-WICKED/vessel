//! What must never happen to a file that is not Vessel's to touch, however
//! the targets are named and whatever changes between planning and
//! applying (the independent review's cases).

use std::fs;
use std::path::PathBuf;

use vessel_fleet::{SkipReason, Step, Target};

use crate::support::{Home, Member, canonical, read, skill_md, write};

/// `skills/extra` in the bundle, then gone from it.
fn with_and_without_extra() -> (Home, vessel_proto::Manifest, vessel_proto::Manifest) {
    let mut home = Home::new();
    write(
        &home.codex().join("skills/extra/SKILL.md"),
        &skill_md("extra"),
    );
    let first = home.publish().manifest;
    fs::remove_dir_all(home.codex().join("skills/extra")).unwrap();
    let second = home.publish().manifest;
    (home, first, second)
}

#[test]
fn a_target_whose_home_is_missing_is_skipped_and_not_created() {
    let mut home = Home::new();
    let manifest = home.publish().manifest;
    let member = Member::new();
    let missing = Target {
        root: member.dir.path().join("not-installed"),
        ..member.claude()
    };
    let plan = vessel_fleet::plan(
        &manifest,
        std::slice::from_ref(&missing),
        &member.managed,
        &[],
    )
    .unwrap();
    assert!(plan.steps().is_empty());
    assert_eq!(plan.skips().len(), 1);
    assert_eq!(plan.skips()[0].reason, SkipReason::NoHome);
    assert_eq!(plan.skips()[0].bundle, None);
    assert!(!missing.root.exists());
}

#[test]
fn an_adopted_file_is_released_not_removed_when_it_leaves_the_bundle() {
    let (home, first, second) = with_and_without_extra();
    let mut member = Member::new();
    let _fetched = member.fetch(&first, &home.blobs);
    let claude = member.claude();
    // The user had the same skill before Vessel came.
    let own = claude.root.join("skills/extra/SKILL.md");
    write(&own, &skill_md("extra"));
    let applied = member.apply(&first, std::slice::from_ref(&claude));
    assert_eq!(applied.adopted, 1);
    assert!(member.managed.file(&canonical(&own)).unwrap().adopted);

    let applied = member.apply(&second, std::slice::from_ref(&claude));
    assert_eq!(applied.removed, 0);
    assert_eq!(read(&own), skill_md("extra"), "the user's file stays");
    assert!(
        member.managed.file(&canonical(&own)).is_none(),
        "and is theirs again"
    );
}

#[test]
fn a_plan_made_stale_by_an_edit_is_decided_again() {
    let mut home = Home::new();
    let first = home.publish().manifest;
    let mut member = Member::new();
    let _fetched = member.fetch(&first, &home.blobs);
    let claude = member.claude();
    let _applied = member.apply(&first, std::slice::from_ref(&claude));
    write(&home.codex().join("AGENTS.md"), "instructions v2\n");
    let second = home.publish().manifest;
    let _fetched = member.fetch(&second, &home.blobs);
    let instructions = claude.root.join("CLAUDE.md");

    // Planned as an update of Vessel's file; edited before the apply.
    let plan =
        vessel_fleet::plan(&second, std::slice::from_ref(&claude), &member.managed, &[]).unwrap();
    assert!(
        matches!(plan.steps()[0], Step::Write { .. }),
        "{:?}",
        plan.steps()
    );
    write(&instructions, "edited in between\n");
    let applied = vessel_fleet::apply(&plan, &member.blobs, &mut member.managed);
    assert_eq!(
        read(&instructions),
        "edited in between\n",
        "never overwritten"
    );
    assert_eq!(applied.conflicts.len(), 1);

    // Planned as a kept edit; put back to Vessel's content before the
    // apply: it is Vessel's again and updated, with nothing beside it.
    write(&instructions, "my own\n");
    let plan =
        vessel_fleet::plan(&second, std::slice::from_ref(&claude), &member.managed, &[]).unwrap();
    assert!(
        matches!(plan.steps()[0], Step::Keep { .. }),
        "{:?}",
        plan.steps()
    );
    write(&instructions, "instructions v1\n");
    let applied = vessel_fleet::apply(&plan, &member.blobs, &mut member.managed);
    assert!(applied.conflicts.is_empty(), "{:?}", applied.conflicts);
    assert_eq!(read(&instructions), "instructions v2\n");
}

#[test]
fn a_skill_renamed_only_in_case_keeps_its_file_on_every_filesystem() {
    let mut home = Home::new();
    write(
        &home.codex().join("skills/deploy/SKILL.md"),
        &skill_md("Deploy"),
    );
    let first = home.publish().manifest;
    write(
        &home.codex().join("skills/deploy/SKILL.md"),
        &skill_md("deploy"),
    );
    let second = home.publish().manifest;
    let mut member = Member::new();
    let _fetched = member.fetch(&first, &home.blobs);
    let _fetched = member.fetch(&second, &home.blobs);
    let claude = member.claude();
    let _applied = member.apply(&first, std::slice::from_ref(&claude));
    // On a filesystem that ignores case (macOS, Windows) `Deploy` and
    // `deploy` are one file: the old name's record must not remove it.
    let applied = member.apply(&second, std::slice::from_ref(&claude));
    assert!(applied.failures.is_empty(), "{:?}", applied.failures);
    assert!(
        claude.root.join("skills/deploy/SKILL.md").is_file(),
        "{applied:?}"
    );
}

#[test]
fn a_missing_source_is_still_protected_and_a_missing_directory_fails() {
    let mut home = Home::new();
    let manifest = home.publish().manifest;
    let codex = Target {
        root: home.codex(),
        instructions: Some(PathBuf::from("AGENTS.md")),
        skills: Some(PathBuf::from("skills")),
    };
    // An editor replacing AGENTS.md: gone for a moment.
    fs::remove_file(home.codex().join("AGENTS.md")).unwrap();
    let sources = home.sources.paths();
    let mut managed = vessel_fleet::Managed::default();
    let plan =
        vessel_fleet::plan(&manifest, std::slice::from_ref(&codex), &managed, &sources).unwrap();
    assert!(
        plan.skips()
            .iter()
            .any(|skip| skip.path.ends_with("AGENTS.md") && skip.reason == SkipReason::Source),
        "{:?}",
        plan.skips()
    );
    let _applied = vessel_fleet::apply(&plan, &home.blobs, &mut managed);
    assert!(!home.codex().join("AGENTS.md").exists(), "never written");
    assert_eq!(managed.files().count(), 0, "never recorded");

    let gone = vec![home.dir.path().join("nowhere/AGENTS.md")];
    assert!(matches!(
        vessel_fleet::plan(&manifest, &[codex], &managed, &gone),
        Err(vessel_fleet::PlanError::Source(_))
    ));
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _, symlink};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_profile_linked_to_another_home_never_loses_its_files() {
        let mut home = Home::new();
        let manifest = home.publish().manifest;
        let mut member = Member::new();
        let _fetched = member.fetch(&manifest, &home.blobs);
        let claude = member.claude();
        let alt = Target {
            root: member.dir.path().join("claude-alt"),
            ..claude.clone()
        };
        symlink(&claude.root, &alt.root).unwrap();

        let applied = member.apply(&manifest, &[claude.clone(), alt.clone()]);
        assert_eq!(applied.written, 3, "written once: {applied:?}");
        for order in [[alt.clone(), claude.clone()], [claude.clone(), alt.clone()]] {
            let plan = vessel_fleet::plan(&manifest, &order, &member.managed, &[]).unwrap();
            assert!(plan.steps().is_empty(), "{:?}", plan.steps());
            let applied = member.apply(&manifest, &order);
            assert_eq!(applied.removed, 0);
        }
        assert_eq!(read(&claude.root.join("CLAUDE.md")), "instructions v1\n");
        assert!(claude.root.join("skills/review/SKILL.md").is_file());
        // One record per file, under the canonical root.
        assert_eq!(member.managed.files().count(), 3);
    }

    #[test]
    fn a_directory_swapped_for_a_link_before_a_removal_is_not_followed() {
        let (home, first, second) = with_and_without_extra();
        let mut member = Member::new();
        let _fetched = member.fetch(&first, &home.blobs);
        let claude = member.claude();
        let _applied = member.apply(&first, std::slice::from_ref(&claude));
        let plan = vessel_fleet::plan(&second, std::slice::from_ref(&claude), &member.managed, &[])
            .unwrap();
        assert!(
            plan.steps()
                .iter()
                .any(|step| matches!(step, Step::Remove { .. })),
            "{:?}",
            plan.steps()
        );
        // Between the plan and the apply, the directory becomes a link to
        // a copy of the same skill elsewhere (a source, say).
        let elsewhere = member.dir.path().join("elsewhere/extra");
        write(&elsewhere.join("SKILL.md"), &skill_md("extra"));
        fs::remove_dir_all(claude.root.join("skills/extra")).unwrap();
        symlink(&elsewhere, claude.root.join("skills/extra")).unwrap();
        let applied = vessel_fleet::apply(&plan, &member.blobs, &mut member.managed);
        assert_eq!(applied.removed, 0);
        assert_eq!(read(&elsewhere.join("SKILL.md")), skill_md("extra"));
    }

    #[test]
    fn a_fifo_where_a_removal_was_planned_neither_hangs_nor_goes() {
        let (home, first, second) = with_and_without_extra();
        let mut member = Member::new();
        let _fetched = member.fetch(&first, &home.blobs);
        let claude = member.claude();
        let _applied = member.apply(&first, std::slice::from_ref(&claude));
        let plan = vessel_fleet::plan(&second, std::slice::from_ref(&claude), &member.managed, &[])
            .unwrap();
        let place = claude.root.join("skills/extra/SKILL.md");
        fs::remove_file(&place).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(&place)
            .status()
            .unwrap();
        assert!(made.success());
        let (done, finished) = mpsc::channel();
        let mut managed = member.managed.clone();
        let blobs = member.blobs.clone();
        let _thread = std::thread::spawn(move || {
            let applied = vessel_fleet::apply(&plan, &blobs, &mut managed);
            done.send(applied.removed).unwrap();
        });
        let removed = finished
            .recv_timeout(Duration::from_secs(10))
            .expect("apply hung on a FIFO");
        assert_eq!(removed, 0);
        assert!(fs::symlink_metadata(&place).unwrap().file_type().is_fifo());
    }

    #[test]
    fn one_unreadable_target_does_not_stop_the_others() {
        let mut home = Home::new();
        let manifest = home.publish().manifest;
        let mut member = Member::new();
        let _fetched = member.fetch(&manifest, &home.blobs);
        let locked = Target {
            root: member.dir.path().join("locked"),
            ..member.claude()
        };
        let skills = locked.root.join("skills");
        fs::create_dir_all(&skills).unwrap();
        fs::set_permissions(&skills, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&skills).is_ok() {
            // Running as root: permissions do not lock anything.
            fs::set_permissions(&skills, fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let claude = member.claude();
        let plan = vessel_fleet::plan(
            &manifest,
            &[locked.clone(), claude.clone()],
            &member.managed,
            &[],
        )
        .unwrap();
        let unreadable = plan
            .skips()
            .iter()
            .filter(|skip| matches!(skip.reason, SkipReason::Unreadable { .. }))
            .count();
        assert_eq!(
            unreadable,
            2,
            "the locked home's skill files: {:?}",
            plan.skips()
        );
        let applied = vessel_fleet::apply(&plan, &member.blobs, &mut member.managed);
        fs::set_permissions(&skills, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(claude.root.join("skills/review/notes/how.md").is_file());
        assert_eq!(read(&locked.root.join("CLAUDE.md")), "instructions v1\n");
        assert_eq!(applied.written, 4, "{applied:?}");
    }

    #[test]
    fn an_executable_bit_change_reaches_members() {
        let mut home = Home::new();
        let script = home.codex().join("skills/review/run.sh");
        write(&script, "#!/bin/sh\n");
        let first = home.publish().manifest;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let second = home.publish();
        assert!(second.changed, "the mode is part of the bundle");
        let mut member = Member::new();
        let _fetched = member.fetch(&first, &home.blobs);
        let claude = member.claude();
        let _applied = member.apply(&first, std::slice::from_ref(&claude));
        let placed = claude.root.join("skills/review/run.sh");
        let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode(&placed) & 0o777, 0o600);
        let applied = member.apply(&second.manifest, std::slice::from_ref(&claude));
        assert!(applied.conflicts.is_empty(), "{:?}", applied.conflicts);
        assert_eq!(applied.written, 1);
        assert_eq!(mode(&placed) & 0o777, 0o700);
    }
}
