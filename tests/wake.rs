mod common;

use std::fs;
use std::path::Path;

use pair_watcher::concern::{Concern, Severity, State};
use pair_watcher::history::History;
use pair_watcher::snapshot::FileDiff;
use pair_watcher::tailer::{self, Cursor, Entry, Format, Item};
use pair_watcher::trigger::Trigger;
use pair_watcher::wake::{self, Context, Diff, Input, TURN_CAP, TURN_MAX};
use serde_json::json;

const PROMPT: &str = "WATCHER PROMPT (T7 owns the text)";

fn golden(name: &str, actual: &str) {
    let path = Path::new("tests/fixtures/wake").join(name);
    if std::env::var_os("PW_BLESS").is_some() {
        fs::write(&path, actual).unwrap();
    }
    let want = fs::read_to_string(&path).unwrap_or_default();
    assert!(want == actual, "{name} differs; PW_BLESS=1 cargo test --test wake rewrites it\n--- actual ---\n{actual}");
}

fn concern(id: &str, focus: &str, state: State, detail: usize) -> Concern {
    Concern {
        id: id.into(),
        focus: focus.into(),
        severity: Severity::Note,
        state,
        headline: format!("headline of {id}"),
        detail: (0..detail).map(|i| format!("detail {i} {}", "d".repeat(100))).collect(),
        refs: vec!["src/client.ts:57".into()],
        challenge: false,
        evidence: vec![],
        alternative: None,
        verified: false,
        tag: None,
    }
}

fn ctx<'a>(
    concerns: &'a [Concern],
    debate: Option<&'a str>,
    rulings: &'a str,
    instructions: &'a [String],
) -> Context<'a> {
    Context { concerns, debate, rulings, instructions }
}

fn entry(items: Vec<Item>) -> Entry {
    Entry { id: "e".into(), t: 0, items }
}

fn call(id: &str, tool: &str, input: &str, output: &str, error: bool) -> Vec<Item> {
    vec![
        Item::ToolCall { id: id.into(), name: tool.into(), input: input.into() },
        Item::ToolResult { id: id.into(), output: output.into(), error },
    ]
}

fn patch(path: &str, hunks: usize, lines: usize) -> FileDiff {
    let mut p = format!("diff --git a/{path} b/{path}\nindex 1..2 100644\n--- a/{path}\n+++ b/{path}\n");
    for h in 0..hunks {
        p.push_str(&format!("@@ -{0},{1} +{0},{1} @@\n", h * 100 + 1, lines));
        for l in 0..lines {
            p.push_str(&format!("+{path} hunk {h} line {l}\n"));
        }
    }
    FileDiff { path: path.into(), added: Some((hunks * lines) as u64), removed: Some(0), patch: p }
}

/// The repetition fixture: turns 1–5 committed in history, turn 6 is this wake's entries.
fn repetition() -> (History, Vec<Entry>) {
    let tail =
        tailer::read(Format::Claude, Path::new("tests/fixtures/wake/repetition.jsonl"), &Cursor::default()).unwrap();
    let ends: Vec<usize> =
        tail.entries.iter().enumerate().filter(|(_, e)| e.items.contains(&Item::TurnEnd)).map(|(i, _)| i).collect();
    assert_eq!(ends.len(), 6);
    let (before, window) = tail.entries.split_at(ends[4] + 1);
    (History::default().ingest(before), window.to_vec())
}

fn repetition_input() -> (Input, History) {
    let (history, window) = repetition();
    let client = "diff --git a/src/client.ts b/src/client.ts\nindex 1..2 100644\n--- a/src/client.ts\n+++ b/src/client.ts\n@@ -56 +56 @@\n-// retry with capped backoff (30 s)\n+// Retry with capped exponential backoff (30 s).\n";
    let wake_diff = [
        FileDiff { path: "src/client.ts".into(), added: Some(1), removed: Some(1), patch: client.into() },
        patch("README.md", 1, 2),
    ];
    let session = [
        FileDiff { path: "src/client.ts".into(), added: Some(1), removed: Some(1), patch: String::new() },
        FileDiff { path: "README.md".into(), added: Some(2), removed: Some(0), patch: String::new() },
    ];
    let concerns = [
        Concern {
            id: "c1".into(),
            focus: "correctness".into(),
            severity: Severity::Note,
            state: State::Resolved,
            headline: "Retry change in client.ts:57 has no test".into(),
            detail: vec![],
            refs: vec![],
            challenge: false,
            evidence: vec![],
            alternative: None,
            verified: false,
            tag: None,
        },
        Concern {
            id: "c2".into(),
            focus: "approach".into(),
            severity: Severity::Note,
            state: State::Open,
            headline: "Review round 3 requested though round 2 found nothing new".into(),
            detail: vec!["repeat hint: gh pr comment 412 seen 3×, outcome unchanged".into()],
            refs: vec![],
            challenge: false,
            evidence: vec![],
            alternative: None,
            verified: false,
            tag: None,
        },
    ];
    let instructions = ["AGENTS.md".to_owned()];
    wake::normal(
        Trigger::TurnEnd,
        &history,
        &window,
        Some(&Diff { wake: &wake_diff, session: &session }),
        &ctx(&concerns, None, "", &instructions),
    )
}

/// Golden: the unproductive-repetition fixture at the end of turn 6 (V3–V7).
#[test]
fn golden_repetition_turn_end() {
    let (input, next) = repetition_input();
    let text = input.render(PROMPT, &[]);
    golden("repetition.turn-end.golden", &text);
    let review = "Bash gh pr comment 412 --body '@reviewbot review'";
    assert!(text.contains(&format!("hint: signature \"{review}\" seen 6× in last 6 turns")), "V4 hint");
    assert!(text.contains("hint: signature \"Bash gh pr view 412 --comments\" seen 5× in last 5 turns"));
    let history = input.section("History").unwrap();
    assert!(history.contains("T1 \"Fixed 3 review comments") && history.contains("T5 \"Round 5"));
    assert!(!history.contains("T6"), "digest covers earlier turns only");
    assert!(next.digest(4096).contains("T6 \"Round 6 requested."), "next history carries turn 6");
    let turn = input.section("Turn").unwrap();
    assert!(turn.contains("→ exit 1 · ") && turn.contains("  | reviewbot/approved\tpending"), "failed body kept");
    let diff = input.section("Diff").unwrap();
    assert!(diff.find("=== src/client.ts").unwrap() < diff.find("=== README.md").unwrap(), "touched file first");
    assert!(input.diff_bytes > 0);
}

/// Golden: blocked slim wake on the same history (V11).
#[test]
fn golden_repetition_blocked() {
    let (history, window) = repetition();
    let history = history.ingest(&window);
    let payload = json!({
        "session_id": "s", "transcript_path": "/x", "cwd": "/w", "hook_event_name": "PermissionRequest",
        "tool_name": "AskUserQuestion",
        "tool_input": {"questions": [{"question": "Keep requesting reviews until reviewbot approves?", "options": ["yes", "no"]}]}
    });
    let concerns = [concern("c1", "correctness", State::Resolved, 0), concern("c2", "approach", State::Open, 1)];
    let input = wake::blocked(&history, &payload, &ctx(&concerns, Some("debate"), "", &[]));
    let text = input.render(PROMPT, &[]);
    golden("repetition.blocked.golden", &text);
    let titles: Vec<&str> = input.sections.iter().map(|s| s.title).collect();
    assert_eq!(titles, ["Wake", "Blocked question", "Task", "Concerns", "History"]);
    let q = input.section("Blocked question").unwrap();
    assert!(!q.contains("session_id") && !q.contains("transcript_path") && q.contains("Keep requesting"));
    assert!(!input.section("Concerns").unwrap().contains("c1"), "open Concerns only");
}

/// V11: blocked payload ≤2 K; whole slim input ≈22 K at most even when every part is full.
#[test]
fn blocked_input_is_slim() {
    let mut h = History::default();
    for i in 0..200 {
        h = h.ingest(&[entry(vec![
            Item::Prompt(format!("p{i} {}", "x".repeat(3000))),
            Item::Text("t".repeat(500)),
            Item::ToolCall {
                id: i.to_string(),
                name: "Bash".into(),
                input: format!(r#"{{"command":"make {i} {}"}}"#, "y".repeat(200)),
            },
            Item::TurnEnd,
        ])]);
    }
    let concerns: Vec<Concern> = (0..100).map(|i| concern(&format!("c{i}"), "approach", State::Open, 3)).collect();
    let payload = json!({"tool_input": "q".repeat(10_000)});
    let input = wake::blocked(&h, &payload, &ctx(&concerns, None, "", &[]));
    assert!(input.section("Blocked question").unwrap().len() <= 2048);
    let shared: usize = input.sections.iter().map(|s| s.body.len()).sum();
    assert!(shared <= 22 * 1024, "{shared}");
}

/// V3: inputs ≤2 K; every result an outcome line; head 600 + tail 400 bodies only for failed
/// results and the last result per tool.
#[test]
fn turn_content_clipping() {
    let long = format!("{}{}", "h".repeat(700), "t".repeat(700));
    let mut items = call("1", "Bash", &"i".repeat(5000), &long, false);
    items.extend(call(
        "2",
        "Bash",
        r#"{"command":"cargo test"}"#,
        &format!("Exit code 101\nerror[E0308]: x\n{long}"),
        true,
    ));
    items.extend(call("3", "Bash", r#"{"command":"ls"}"#, "a\nb", false));
    items.extend(call("4", "Read", r#"{"file_path":"a"}"#, "content", false));
    let (input, _) =
        wake::normal(Trigger::Checkpoint, &History::default(), &[entry(items)], None, &ctx(&[], None, "", &[]));
    let turn = input.section("Turn").unwrap();
    let calls: Vec<&str> = turn.split("▸ ").skip(1).collect();
    assert_eq!(calls.len(), 4);
    assert!(calls[0].len() < 2048 + 100 && calls[0].contains('…'), "input clipped 2 K");
    assert!(calls[0].contains("→ ok · 1400 B\n") && !calls[0].contains("  | "), "earlier ok result: outcome line only");
    assert!(calls[1].contains("→ exit 101 · ") && calls[1].contains("\"error[E0308]: x\""), "{}", calls[1]);
    assert!(calls[1].contains("[clipped "), "failed body head+tail");
    assert!(calls[2].contains("  | a\n  | b"), "last Bash result kept");
    assert!(calls[3].contains("  | content"), "last Read result kept");
    assert_eq!(input.section("Diff"), Some("(not a git repo: no diff)"));
}

/// V7: past the spec'd trims the turn stays within its cap: inputs to first line, then the
/// oldest calls fold into a count.
#[test]
fn turn_content_bounded_with_fold() {
    let items: Vec<Item> = (0..600)
        .flat_map(|i| {
            call(
                &i.to_string(),
                "Write",
                &format!("{{\"file_path\":\"f{i}\",\"content\":\"{}\"}}", "c".repeat(3000)),
                "ok",
                i % 7 == 0,
            )
        })
        .collect();
    let (input, _) =
        wake::normal(Trigger::TurnEnd, &History::default(), &[entry(items)], None, &ctx(&[], None, "", &[]));
    let turn = input.section("Turn").unwrap();
    assert!(turn.len() <= TURN_MAX, "{}", turn.len());
    assert!(turn.starts_with('[') && turn.lines().next().unwrap().contains("earlier calls: ok "), "fold line first");
    assert!(turn.contains("f599"), "newest call kept");
    assert!(
        turn.lines().filter(|l| l.starts_with("▸ ")).all(|l| l.len() <= "▸ Write ".len() + 200),
        "inputs cut to ≤200 B"
    );
}

/// V7: turn content borrows unused space only up to 24 K; with every lender full it gets 16 K.
#[test]
fn turn_borrows_unused_space() {
    let items: Vec<Item> = (0..200).flat_map(|i| [Item::Text(format!("{i} {}", "w".repeat(400)))]).collect();
    let es = [entry(items)];
    let (lean, _) = wake::normal(Trigger::TurnEnd, &History::default(), &es, None, &ctx(&[], None, "", &[]));
    let lean = lean.section("Turn").unwrap().len();
    assert!(lean > TURN_CAP && lean <= TURN_MAX, "{lean}");

    let full = full_session();
    let concerns: Vec<Concern> = (0..100).map(|i| concern(&format!("c{i}"), "approach", State::Open, 3)).collect();
    let debate = "d".repeat(10_000);
    let rulings = "r".repeat(2048);
    let diff = [patch("a", 50, 20), patch("b", 50, 20)];
    let session: Vec<FileDiff> = (0..200).map(|i| patch(&format!("dir/file{i}.rs"), 0, 0)).collect();
    let (input, _) = wake::normal(
        Trigger::TurnEnd,
        &full,
        &es,
        Some(&Diff { wake: &diff, session: &session }),
        &ctx(&concerns, Some(&debate), &rulings, &[]),
    );
    let caps: [(&str, usize); 6] =
        [("Task", 6), ("Standing rulings", 2), ("Concerns", 8), ("Open Debate", 4), ("History", 4), ("Diff", 10)];
    let lend: usize = caps.iter().map(|(t, k)| (k * 1024).saturating_sub(input.section(t).unwrap().len())).sum();
    assert!(lend < 1024, "lenders nearly full: {lend}");
    let turn = input.section("Turn").unwrap().len();
    assert!(turn <= TURN_CAP + lend, "{turn} > 16 K + {lend}");
}

fn full_session() -> History {
    let mut h = History::default();
    for i in 0..300 {
        h = h.ingest(&[entry(vec![
            Item::Prompt(format!("p{i} {}", "x".repeat(3000))),
            Item::ToolCall {
                id: i.to_string(),
                name: "Bash".into(),
                input: format!(r#"{{"command":"make {i} {}"}}"#, "y".repeat(200)),
            },
            Item::Text("t".repeat(500)),
            Item::TurnEnd,
        ])]);
    }
    h
}

/// V7: a long session never grows the input past ≈56 K plus the prompt section.
#[test]
fn long_session_input_bounded() {
    let h = full_session();
    let items: Vec<Item> = (0..400)
        .flat_map(|i| {
            let mut v = vec![Item::Text("w".repeat(2000))];
            v.extend(call(&i.to_string(), "Bash", &"c".repeat(3000), &"o".repeat(5000), i % 3 == 0));
            v
        })
        .collect();
    let concerns: Vec<Concern> = (0..200)
        .map(|i| concern(&format!("c{i}"), "approach", [State::Open, State::Dismissed, State::Resolved][i % 3], 3))
        .collect();
    let debate = "d".repeat(20_000);
    let diff: Vec<FileDiff> = (0..100).map(|i| patch(&format!("f{i}"), 5, 30)).collect();
    let session: Vec<FileDiff> = (0..500).map(|i| patch(&format!("s{i}"), 0, 0)).collect();
    let instructions = ["AGENTS.md".to_owned()];
    let (input, _) = wake::normal(
        Trigger::TurnEnd,
        &h,
        &[entry(items)],
        Some(&Diff { wake: &diff, session: &session }),
        &ctx(&concerns, Some(&debate), "", &instructions),
    );
    let text = input.render(PROMPT, &[]);
    assert!(text.len() <= 56 * 1024, "{}", text.len());
    for (title, cap) in [("Task", 6), ("Concerns", 8), ("Open Debate", 4), ("History", 4), ("Diff", 10)] {
        let len = input.section(title).unwrap().len();
        assert!(len <= cap * 1024, "{title} {len}");
    }
}

/// V7: Concern list trims oldest resolved first, then collapses oldest open to id + headline,
/// dismissed kept longest.
#[test]
fn concern_list_trim_order() {
    let mut concerns: Vec<Concern> =
        (0..40).map(|i| concern(&format!("r{i}"), "approach", State::Resolved, 0)).collect();
    concerns.extend((0..10).map(|i| concern(&format!("o{i}"), "correctness", State::Open, 3)));
    concerns.extend((0..10).map(|i| concern(&format!("d{i}"), "security", State::Dismissed, 0)));
    let (input, _) = wake::normal(Trigger::TurnEnd, &History::default(), &[], None, &ctx(&concerns, None, "", &[]));
    let list = input.section("Concerns").unwrap();
    assert!(list.contains("o0 [note] (correctness) headline of o0\n  detail 0"), "open full while room");
    assert!(list.contains("d9 (security) dismissed: headline of d9"));
    assert!(list.contains("r39 (approach) resolved") && list.len() <= 8 * 1024);

    concerns.extend((10..40).map(|i| concern(&format!("o{i}"), "correctness", State::Open, 3)));
    let (input, _) = wake::normal(Trigger::TurnEnd, &History::default(), &[], None, &ctx(&concerns, None, "", &[]));
    let list = input.section("Concerns").unwrap();
    assert!(list.len() <= 8 * 1024, "{}", list.len());
    assert!(!list.contains("r0 ") && !list.contains("r39 "), "resolved dropped first");
    assert!(list.contains("o0 [note] (correctness) headline of o0\no1 "), "oldest open collapsed");
    assert!(list.contains("o39 [note] (correctness) headline of o39\n  detail"), "newest open full");
    assert!(list.contains("d0 (security) dismissed"), "dismissed kept");
}

/// V5: hunks of touched files first; untouched files as stat lines once the 8 K is spent;
/// session stat ≤2 K.
#[test]
fn diff_touched_first_rest_stat() {
    let wake_files = [patch("big_untouched.rs", 20, 20), patch("src/edited.rs", 2, 3)];
    let session: Vec<FileDiff> = (0..300).map(|i| patch(&format!("dir/file{i}.rs"), 0, 0)).collect();
    let items = call("1", "Edit", r#"{"file_path":"/w/src/edited.rs"}"#, "ok", false);
    let (input, _) = wake::normal(
        Trigger::TurnEnd,
        &History::default(),
        &[entry(items)],
        Some(&Diff { wake: &wake_files, session: &session }),
        &ctx(&[], None, "", &[]),
    );
    let diff = input.section("Diff").unwrap();
    assert!(diff.starts_with("=== src/edited.rs | +6 -0\n@@"), "{diff}");
    assert!(diff.contains("=== big_untouched.rs") && diff.contains("more hunks]"), "untouched hunks fill the rest");
    let session_part = &diff[diff.find("Session since start: 300 files").unwrap()..];
    assert!(session_part.len() <= 2048 && session_part.contains("more files]"));
    assert_eq!(input.diff_bytes, wake_files.iter().map(|f| f.patch.len()).sum::<usize>());
}

/// V6: repo instruction files listed by path, prompts numbered, answers labelled.
#[test]
fn task_section_lists_instructions_and_prompts() {
    let dir = tempfile::TempDir::new().unwrap();
    fs::write(dir.path().join("AGENTS.md"), "x").unwrap();
    fs::create_dir_all(dir.path().join(".claude")).unwrap();
    fs::write(dir.path().join(".claude/CLAUDE.md"), "x").unwrap();
    let files = wake::instruction_files(dir.path());
    assert_eq!(files, ["AGENTS.md", ".claude/CLAUDE.md"]);
    let items = vec![Item::Prompt("fix it".into()), Item::Prompt("and test".into())];
    let (input, _) =
        wake::normal(Trigger::TurnEnd, &History::default(), &[entry(items)], None, &ctx(&[], None, "", &files));
    assert_eq!(
        input.section("Task").unwrap(),
        "Repo instructions (read on demand): AGENTS.md, .claude/CLAUDE.md\n[prompt 1] fix it\n[prompt 2] and test"
    );
}

/// V9: every Watcher gets identical input; only its brief differs. Brief size never moves the
/// shared sections (no lending from the prompt section).
#[test]
fn identical_input_across_watchers() {
    let world = common::World::new();
    let set = world.focuses(&[]);
    let (input, _) = repetition_input();
    let mut renders = Vec::new();
    for f in &set.enabled {
        renders.push(input.render(PROMPT, &[f]));
    }
    let shared = |r: &str| r[r.find("\n\n## Wake\n").unwrap()..].to_owned();
    assert!(renders.windows(2).all(|w| shared(&w[0]) == shared(&w[1])));
    assert!(renders[0].contains(&format!("## Focus: {}", set.enabled[0].name)));
    let companion = input.render(PROMPT, &set.enabled.iter().collect::<Vec<_>>());
    assert_eq!(shared(&companion), shared(&renders[0]));
}

/// V2: stateless: same committed state + entries → same input; nothing carried between builds.
#[test]
fn stateless_rebuild() {
    let (a, ha) = repetition_input();
    let (b, hb) = repetition_input();
    assert_eq!(a, b);
    assert_eq!(ha, hb);
}

/// V7: ≤4 prompts at the 1.5 KB item cap still fit the 6 K Task cap.
#[test]
fn task_cap_holds_with_few_items() {
    let prompts: Vec<Item> = (0..4).map(|i| Item::Prompt(format!("{i}{}", "x".repeat(3000)))).collect();
    let instructions = ["AGENTS.md".to_owned(), "CLAUDE.md".to_owned()];
    let (input, _) = wake::normal(
        Trigger::TurnEnd,
        &History::default(),
        &[entry(prompts)],
        None,
        &ctx(&[], None, "", &instructions),
    );
    let task = input.section("Task").unwrap();
    assert!(task.len() <= 6 * 1024, "{}", task.len());
}

/// V7: `[N more hunks]` lines and a long last stat line stay inside the 10 K Diff cap.
#[test]
fn diff_cap_holds_with_partial_files() {
    let wake_files: Vec<FileDiff> = (0..40).map(|i| patch(&format!("f{i}"), 3, 25)).collect();
    let mut session: Vec<FileDiff> = (0..60).map(|i| patch(&format!("s{i}"), 0, 0)).collect();
    session.push(patch(&"long/".repeat(500), 0, 0));
    let items: Vec<Item> = (0..40)
        .flat_map(|i| call(&i.to_string(), "Edit", &format!("{{\"file_path\":\"f{i}\"}}"), "ok", false))
        .collect();
    let (input, _) = wake::normal(
        Trigger::TurnEnd,
        &History::default(),
        &[entry(items)],
        Some(&Diff { wake: &wake_files, session: &session }),
        &ctx(&[], None, "", &[]),
    );
    let diff = input.section("Diff").unwrap();
    assert!(diff.contains("more hunks]") || diff.contains("Other changes:"));
    assert!(diff.len() <= 10 * 1024, "{}", diff.len());
    let session_part = &diff[diff.find("Session since start").unwrap()..];
    assert!(session_part.len() <= 2048 && !session_part.contains("long/long/"), "{}", session_part.len());
}
