use super::*;

fn text(text: &str) -> Part {
    Part::Text(text.to_owned())
}

fn call(id: &str) -> Part {
    Part::ToolCall(ToolCall {
        id: id.to_owned(),
        name: "Write".to_owned(),
        input: ToolInput::Json("{}".to_owned()),
    })
}

fn result(id: &str) -> Part {
    Part::ToolResult(ToolResult {
        call_id: id.to_owned(),
        content: vec![ResultPart::Text("done".to_owned())],
        is_error: false,
    })
}

fn turn(role: Role, parts: Vec<Part>) -> Message {
    Message { role, parts }
}

#[test]
fn repair_answers_every_call_in_the_next_turn() {
    let mut messages = vec![
        turn(Role::User, vec![text("go")]),
        turn(Role::Assistant, vec![text("on it"), call("a"), call("b")]),
        // `b`'s result is missing, and the text comes before `a`'s.
        turn(Role::User, vec![text("also this"), result("a")]),
        turn(Role::Assistant, vec![call("c")]),
        // Interrupted: no turn answers `c`, then the user speaks again.
        turn(Role::Assistant, vec![text("hm")]),
        turn(Role::User, vec![text("stop")]),
    ];
    repair(&mut messages);

    let roles: Vec<Role> = messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            Role::User,
            Role::Assistant,
            Role::User,
            Role::Assistant,
            Role::User
        ]
    );

    let Some(answers) = messages.get(2) else {
        panic!("{messages:?}");
    };
    let shape: Vec<String> = answers
        .parts
        .iter()
        .map(|part| match part {
            Part::ToolResult(result) => format!("result {} {}", result.call_id, result.is_error),
            Part::Text(text) => format!("text {text}"),
            Part::Image(_) | Part::Document(_) | Part::Thinking(_) | Part::ToolCall(_) => {
                format!("{part:?}")
            }
        })
        .collect();
    assert_eq!(shape, ["result a false", "result b true", "text also this"]);

    // `c` and the text after it merged into one assistant turn, and `c`
    // is answered as missing before the user's last words.
    let Some(last) = messages.last() else {
        panic!("{messages:?}");
    };
    assert!(matches!(
        last.parts.as_slice(),
        [Part::ToolResult(missing), Part::Text(stop)]
            if missing.call_id == "c" && missing.is_error && stop == "stop"
    ));
}

#[test]
fn repair_turns_an_orphan_result_into_text() {
    let mut messages = vec![turn(Role::User, vec![result("ghost"), text("hi")])];
    repair(&mut messages);
    let Some(only) = messages.first() else {
        panic!("{messages:?}");
    };
    assert!(matches!(
        only.parts.as_slice(),
        [Part::Text(orphan), Part::Text(hi)] if orphan.contains("ghost") && hi == "hi"
    ));
}

#[test]
fn ids_anthropic_refuses_map_one_to_one() {
    assert_eq!(anthropic_safe_id("toolu_01A-b"), "toolu_01A-b");
    let kimi = anthropic_safe_id("functions.Write:0");
    assert!(kimi.starts_with("toolu_vsl_"));
    assert_eq!(kimi, anthropic_safe_id("functions.Write:0"));
    assert_ne!(kimi, anthropic_safe_id("functions.Write:1"));
    assert_ne!(anthropic_safe_id(&"x".repeat(65)), "x".repeat(65));
    assert_ne!(mint_call_id(), mint_call_id());
}

#[test]
fn budgets_and_efforts_meet_in_the_middle() {
    assert_eq!(Reasoning::Budget(1024).effort(), Some(Effort::Low));
    assert_eq!(Reasoning::Budget(8000).effort(), Some(Effort::Medium));
    assert_eq!(Reasoning::Budget(31_999).effort(), Some(Effort::High));
    assert_eq!(Reasoning::Off.effort(), None);
    assert_eq!(Effort::High.budget(None), 32_000);
    assert_eq!(Effort::High.budget(Some(8192)), 8191);
    assert_eq!(Effort::Max.budget(Some(100)), 1024);
}

#[test]
fn a_freeform_call_travels_as_one_string() {
    let patch = "*** Begin Patch\n\"quoted\"\n*** End Patch\n";
    let arguments = ToolInput::Text(patch.to_owned())
        .to_arguments()
        .into_owned();
    assert_eq!(freeform_text(&arguments).as_deref(), Some(patch));
    assert_eq!(ToolInput::Json(" ".to_owned()).to_arguments(), "{}");

    let grammar = ("lark".to_owned(), "start: x".to_owned());
    let schema = freeform_schema(Some(&grammar)).unwrap();
    assert!(schema.as_text().contains(r#""required":["input"]"#));
    assert!(schema.as_text().contains("lark grammar"));
}

#[test]
fn data_urls_become_inline_images() {
    let image = Image::from_url("data:image/png;base64,iVBORw0KGgo=");
    assert_eq!(
        image,
        Image::Base64 {
            media_type: "image/png".into(),
            data: "iVBORw0KGgo=".into()
        }
    );
    assert_eq!(image.to_url(), "data:image/png;base64,iVBORw0KGgo=");
    assert_eq!(
        Image::from_url("https://example.com/a.png"),
        Image::Url("https://example.com/a.png".into())
    );
}
