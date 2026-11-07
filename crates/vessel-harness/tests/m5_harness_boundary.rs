//! Run: copy to crates/vessel-harness/tests/m5_harness_boundary.rs, then
//! `cargo test -p vessel-harness --test m5_harness_boundary -- --nocapture`
//! Checks the actual Launch argv with a fake-tui executable; never runs Codex.

use std::collections::BTreeMap;

use vessel_harness::fakes::{Role, install};
use vessel_harness::{HarnessEnv, Lookup, ModelProxy, launch, parse_profiles};
use vessel_proto::{
    HarnessKind, ModelId, ResolvedMcpServers, SessionMode, SessionSpec, TerminalSize,
};

#[test]
fn profile_and_session_overrides_must_not_replace_the_routed_provider() {
    let temp = tempfile::tempdir().unwrap();
    let fake = vessel_testkit::Fake::Tui.path().unwrap();
    let _installed = install(temp.path(), HarnessKind::Codex, Role::Terminal, &fake).unwrap();
    let lookup = Lookup::new([temp.path().to_owned()]);
    let proxy = ModelProxy {
        base_url: "http://127.0.0.1:4321".to_owned(),
        token: "vslm-FAKE-review-token".to_owned(),
        model: ModelId::new("allowed"),
        small_model: None,
    };
    let mut failures = Vec::new();
    for in_profile in [true, false] {
        let text = format!(
            "[profiles.routed]\nharness = \"codex\"\nmodel = \"local\"\n{}",
            if in_profile {
                "args = ['-c', 'model_providers.vessel.base_url=\"http://127.0.0.1:9/v1\"']\n"
            } else {
                ""
            }
        );
        // Refusing the conflicting profile would also satisfy the boundary.
        let Ok(profiles) = parse_profiles(&text, Some(temp.path())) else {
            continue;
        };
        let profile = profiles.get(&"routed".parse().unwrap()).unwrap();
        let args = if in_profile {
            Vec::new()
        } else {
            vec![
                "-c".to_owned(),
                "model_providers.vessel.base_url=\"http://127.0.0.1:9/v1\"".to_owned(),
            ]
        };
        let spec = SessionSpec::new(
            HarnessKind::Codex,
            SessionMode::Terminal {
                args,
                size: TerminalSize::default(),
            },
        )
        .with_cwd(temp.path())
        .with_profile(profile.name().clone());
        let Ok(launched) = launch(
            &lookup,
            &HarnessEnv::new(temp.path().to_owned()),
            &spec,
            profile,
            Some(&proxy),
            &ResolvedMcpServers::new(),
            None,
        ) else {
            continue;
        };
        // Codex applies -c overrides in command-line order. This isolates
        // the collision in Vessel's output, without depending on a real account.
        let args: Vec<String> = launched
            .command
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let mut settings = BTreeMap::new();
        for pair in args.chunks_exact(2) {
            assert_eq!(pair[0], "-c");
            let (key, value) = pair[1].split_once('=').unwrap();
            let doc: toml_edit::DocumentMut = format!("v = {value}").parse().unwrap();
            if key == "model_providers.vessel" {
                let _previous = settings.insert(
                    "base_url",
                    doc["v"]["base_url"].as_str().unwrap().to_owned(),
                );
            } else if key == "model_providers.vessel.base_url" {
                let _previous = settings.insert("base_url", doc["v"].as_str().unwrap().to_owned());
            }
        }
        let base = &settings["base_url"];
        if base != "http://127.0.0.1:4321/v1" {
            failures.push(format!(
                "{} override wins: {base}",
                if in_profile { "profile" } else { "session" }
            ));
        }
        assert!(!args.iter().any(|arg| arg.contains(&proxy.token)));
    }
    assert!(failures.is_empty(), "routed provider bypass: {failures:?}");
}
