//! Milestone 5, round 3: `Redactor::holds` fails open when `serde_json`
//! cannot decode the text, though a harness's decoder can.
//!
//! Run: copy to crates/vessel-llm/tests/m5_round3_redactor.rs, then
//! `cargo test -p vessel-llm --test m5_round3_redactor`

use std::fmt::Write as _;

use vessel_llm::client::{ApiKey, Redactor};

const KEY: &str = "sk-round3-FAKE-ONLY-6f0c19d2";

fn escaped() -> String {
    KEY.bytes().fold(String::new(), |mut out, byte| {
        let _written = write!(out, "\\u{byte:04x}");
        out
    })
}

/// Each text below is something Node's `JSON.parse` (Claude Code) decodes
/// to a value holding the key; `holds` must say so.
#[test]
fn holds_sees_the_key_in_json_serde_json_refuses() {
    let redactor = Redactor::new(Some(&ApiKey::new(KEY.to_owned())));
    let escaped = escaped();
    let cases = [
        (
            "lone surrogate elsewhere",
            format!(r#"{{"text":"{escaped}","pad":"\ud800"}}"#),
        ),
        (
            "nesting past 128",
            format!(
                r#"{{"pad":{}"{escaped}"{}}}"#,
                "[".repeat(130),
                "]".repeat(130)
            ),
        ),
    ];
    let missed: Vec<&str> = cases
        .iter()
        .filter(|(_name, text)| !redactor.holds(text))
        .map(|(name, _text)| *name)
        .collect();
    assert!(missed.is_empty(), "holds() misses the key: {missed:?}");
    // Not a JSON spelling, recorded for the report: a key cut by the line
    // break between two `data:` lines of one event is not found either
    // (the harness's SSE parser joins the lines with the same break).
    let (head, tail) = KEY.split_at(10);
    assert!(!redactor.holds(&format!("{{\"m\":\"{head}\n{tail}\"}}")));
}
