//! Run: copy to crates/vessel-harness/tests/m5_verify_args.rs, then
//! `cargo test -p vessel-harness --test m5_verify_args`
#![cfg(test)]
use vessel_harness::fakes::{Role, install};
use vessel_harness::{HarnessEnv, Lookup, ModelProxy, launch, parse_profiles};
use vessel_proto::{
    HarnessKind, ModelId, ResolvedMcpServers, SessionMode, SessionSpec, TerminalSize,
};
#[test]
fn verify_codex_config_spellings_and_legitimate_separator() {
    let temp = tempfile::tempdir().unwrap();
    let fake = vessel_testkit::Fake::Tui.path().unwrap();
    let _installed = install(temp.path(), HarnessKind::Codex, Role::Terminal, &fake).unwrap();
    let lookup = Lookup::new([temp.path().to_owned()]);
    let proxy = ModelProxy {
        base_url: "http://127.0.0.1:4321".into(),
        token: "vslm-FAKE".into(),
        model: ModelId::new("allowed"),
        small_model: None,
    };
    let profiles = parse_profiles(
        "[profiles.routed]\nharness='codex'\nmodel='routed'",
        Some(temp.path()),
    )
    .unwrap();
    let profile = profiles.get(&"routed".parse().unwrap()).unwrap();
    let try_args = |args: Vec<String>| {
        launch(
            &lookup,
            &HarnessEnv::new(temp.path().to_owned()),
            &SessionSpec::new(
                HarnessKind::Codex,
                SessionMode::Terminal {
                    args,
                    size: TerminalSize::default(),
                },
            )
            .with_cwd(temp.path())
            .with_profile(profile.name().clone()),
            profile,
            Some(&proxy),
            &ResolvedMcpServers::new(),
            None,
        )
    };
    let mut count = 0;
    for key in [
        "model",
        "model_provider",
        "model_providers.vessel.base_url",
        "\"model_providers\" . 'vessel' . base_url",
        "'model'",
        " profile ",
        "profiles.other.model",
        "oss_provider",
    ] {
        for style in 0..5 {
            let value = format!("{key}=\"other\"");
            let args = match style {
                0 => vec!["-c".into(), value],
                1 => vec!["--config".into(), value],
                2 => vec![format!("--config={value}")],
                3 => vec![format!("-c{value}")],
                _ => vec![format!("-c={value}")],
            };
            assert!(try_args(args.clone()).is_err(), "accepted {args:?}");
            count += 1;
        }
    }
    for args in [
        vec!["-mother"],
        vec!["-m", "other"],
        vec!["--model=other"],
        vec!["-pother"],
        vec!["--profile", "other"],
        vec!["--oss"],
        vec!["--local-provider=ollama"],
    ] {
        assert!(try_args(args.into_iter().map(str::to_owned).collect()).is_err());
        count += 1;
    }
    let safe = try_args(vec![
        "--config".into(),
        "approval_policy=\"never\"".into(),
        "--".into(),
        "--config=model_provider=other".into(),
    ])
    .unwrap();
    let args: Vec<_> = safe
        .command
        .args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let marker = args.iter().position(|arg| arg == "--").unwrap();
    assert_eq!(args[marker - 1], "model=\"allowed\"");
    assert_eq!(args[marker + 1], "--config=model_provider=other");
    assert_eq!(count, 47);
}
