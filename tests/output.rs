use pair_watcher::concern::{Evidence, Severity};
use pair_watcher::output::{self, Decision, Line, Reason, Side};

#[test]
fn every_line_kind_parses() {
    let text = r#"
{"op":"add","severity":"warn","focus":"approach","headline":"Round 3 review requested though round 2 found nothing","detail":["repeat hint: 3x"],"refs":["src/a.rs:12","src/b.rs:3-9"],"challenge":true,"evidence":[{"kind":"file","ref":"src/a.rs:12","excerpt":"retry()"},{"kind":"command","command":"gh pr view","output":"No new issues"},{"kind":"transcript","excerpt":"round 3"},{"kind":"hint","excerpt":"seen 3x"}],"alternative":"stop requesting reviews"}
{"op":"update","id":"c1","severity":"note"}
{"op":"resolve","id":"c1","reason":"fixed"}
{"op":"resolve","id":"c2","reason":"no-longer-applies"}
{"op":"resolve","id":"c3","reason":"duplicate","of":"c1"}
{"op":"consider","text":"approving deletes the cache"}
{"op":"debate","decision":"rebut","text":"the test still fails"}
{"op":"inferred_ruling","escalation":"c4","side":"worker","prompt_ref":"prompt 3"}
{"op":"held_back","ruling":"r2"}
"#;
    let lines = output::parse(text).unwrap();
    assert_eq!(lines.len(), 9);
    let Line::Add { severity, focus, refs, challenge, evidence, alternative, .. } = &lines[0] else { panic!() };
    assert_eq!((*severity, focus.as_deref(), *challenge), (Severity::Warn, Some("approach"), true));
    assert_eq!(refs.len(), 2);
    assert_eq!(evidence[0], Evidence::File { at: "src/a.rs:12".into(), excerpt: "retry()".into() });
    assert_eq!(evidence.len(), 4);
    assert_eq!(alternative.as_deref(), Some("stop requesting reviews"));
    assert!(matches!(&lines[1], Line::Update { severity: Some(Severity::Note), headline: None, .. }));
    assert!(matches!(&lines[3], Line::Resolve { reason: Reason::NoLongerApplies, of: None, .. }));
    assert!(matches!(&lines[4], Line::Resolve { reason: Reason::Duplicate, of: Some(o), .. } if o == "c1"));
    assert!(matches!(&lines[6], Line::Debate { decision: Decision::Rebut, .. }));
    assert!(matches!(&lines[7], Line::InferredRuling { side: Side::Worker, .. }));
}

#[test]
fn empty_output_is_a_silent_wake() {
    assert_eq!(output::parse("").unwrap(), vec![]);
    assert_eq!(output::parse("\n  \n").unwrap(), vec![]);
}

#[test]
fn add_defaults_optional_fields() {
    let lines = output::parse(r#"{"op":"add","severity":"note","headline":"h"}"#).unwrap();
    assert_eq!(
        lines[0],
        Line::Add {
            severity: Severity::Note,
            focus: None,
            headline: "h".into(),
            detail: vec![],
            refs: vec![],
            challenge: false,
            evidence: vec![],
            alternative: None,
        }
    );
}

/// I10: anything off-shape invalidates the whole output; the diagnostic names the line.
#[test]
fn strict_shape_invalidates_whole_output() {
    let good = r#"{"op":"add","severity":"note","headline":"fine"}"#;
    let long = "x".repeat(101);
    let bad = [
        ("Here are my concerns:".to_owned(), "line 2"),
        (r#"{"op":"add","severity":"note","headline":"h","extra":1}"#.to_owned(), "unknown field"),
        (r#"{"op":"praise","text":"nice"}"#.to_owned(), "unknown variant"),
        (r#"{"op":"add","severity":"high","headline":"h"}"#.to_owned(), "unknown variant"),
        (format!(r#"{{"op":"add","severity":"note","headline":"{long}"}}"#), "over 100 chars"),
        (r#"{"op":"add","severity":"note","headline":"a\nb"}"#.to_owned(), "spans lines"),
        (r#"{"op":"add","severity":"note","headline":"  "}"#.to_owned(), "empty"),
        (r#"{"op":"add","severity":"note","headline":"h","detail":["1","2","3","4"]}"#.to_owned(), "over 3 lines"),
        (r#"{"op":"add","severity":"note","headline":"h","refs":["src/a.rs"]}"#.to_owned(), "not file:line"),
        (r#"{"op":"add","severity":"note","headline":"h","refs":["src/a.rs:x"]}"#.to_owned(), "not file:line"),
        (r#"{"op":"add","severity":"note","headline":"h","evidence":[{"kind":"vibe","excerpt":"x"}]}"#.to_owned(), "unknown variant"),
        (
            r#"{"op":"add","severity":"note","headline":"h","evidence":[{"kind":"file","ref":"a:1","excerpt":"x","n":1}]}"#
                .to_owned(),
            "unknown field",
        ),
        (r#"{"op":"resolve","id":"c1","reason":"duplicate"}"#.to_owned(), "needs `of`"),
        (r#"{"op":"resolve","id":"c1","reason":"fixed","of":"c2"}"#.to_owned(), "only with reason duplicate"),
        (r#"{"op":"debate","decision":"concede","text":""}"#.to_owned(), "empty"),
    ];
    for (line, want) in bad {
        let err = output::parse(&format!("{good}\n{line}\n{good}")).unwrap_err();
        assert!(err.starts_with("line 2: "), "{line}: {err}");
        assert!(err.contains(want), "{line}: want {want:?}, got {err}");
    }
}
