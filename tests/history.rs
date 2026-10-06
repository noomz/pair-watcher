use pair_watcher::action::{Outcome, error_line, exit_code, signature};
use pair_watcher::history::History;
use pair_watcher::tailer::{Entry, Item};

fn entry(items: Vec<Item>) -> Entry {
    Entry { id: "e".into(), t: 0, items }
}

fn bash(id: &str, cmd: &str, output: &str, error: bool) -> Vec<Item> {
    vec![
        Item::ToolCall { id: id.into(), name: "Bash".into(), input: format!(r#"{{"command":"{cmd}"}}"#) },
        Item::ToolResult { id: id.into(), output: output.into(), error },
    ]
}

fn turn(text: &str, mut items: Vec<Item>) -> Entry {
    items.push(Item::Text(text.into()));
    items.push(Item::TurnEnd);
    entry(items)
}

/// V4: signature = tool + command; ids, hashes, temp paths stripped; numbers kept.
#[test]
fn signature_strips_ids_hashes_temp_paths() {
    assert_eq!(
        signature("Bash", r#"{"command":"gh pr comment 412 --body '@reviewbot review'"}"#),
        "Bash gh pr comment 412 --body '@reviewbot review'"
    );
    assert_eq!(
        signature("Bash", r#"{"command":"git show 3f9c2ab1 && cat /tmp/x/run-77.log"}"#),
        "Bash git show <hash> && cat <tmp>"
    );
    assert_eq!(
        signature("Bash", r#"{"command":"curl api/jobs/0b5e9c2a-1d3f-4e6a-9b8c-7d6e5f4a3b2c"}"#),
        "Bash curl api/jobs/<id>"
    );
    assert_eq!(signature("Read", r#"{"file_path":"/private/var/folders/ab/T/s.txt"}"#), "Read <tmp>");
    assert_eq!(signature("Read", r#"{"file_path":"src/decade.rs"}"#), "Read src/decade.rs", "words stay");
    assert_eq!(signature("TodoWrite", r#"{"todos":[]}"#), "TodoWrite");
}

/// V4: Codex `exec` JS and `apply_patch` inputs.
#[test]
fn signature_reads_codex_inputs() {
    let exec = r#"text(await tools.exec_command({cmd:"cargo test -q",sandbox_permissions:"x"}));"#;
    assert_eq!(signature("exec", exec), "exec cargo test -q");
    let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: src/b.rs\n*** End Patch";
    assert_eq!(signature("apply_patch", patch), "apply_patch src/a.rs src/b.rs");
}

/// V3: exit status as harnesses print it; error-looking line.
#[test]
fn outcome_reads_exit_status() {
    assert_eq!(exit_code(r#"Script completed\n{"chunk_id":"x","exit_code":2,"output":""}"#), Some(2));
    assert_eq!(exit_code("Exit code: 0\nWall time: 1s"), Some(0));
    assert_eq!(exit_code("Exit code 101\nerror[E0308]"), Some(101));
    assert_eq!(exit_code("Process exited with code -1"), Some(-1));
    assert_eq!(exit_code(&format!("{}exit code 3", "x".repeat(500))), None, "head only");
    assert_eq!(Outcome::of("Exit code 1\nboom", true), Outcome::Exit(1));
    assert_eq!(Outcome::of("Access blocked", true), Outcome::Error);
    assert_eq!(Outcome::of("fine", false), Outcome::Ok);
    assert_eq!(error_line("ok\nerror[E0308]: mismatched types\n").as_deref(), Some("error[E0308]: mismatched types"));
}

/// V4: one line per turn with signatures → outcome; identical actions merge.
#[test]
fn digest_lines_per_turn() {
    let mut items = bash("1", "cargo test", "Exit code 101\nerror: x", true);
    items.extend(bash("2", "cargo test", "Exit code 101\nerror: x", true));
    let h = History::default().ingest(&[turn("Tests still fail.\nmore", items)]);
    assert_eq!(h.digest(4096), "T1 \"Tests still fail.\" · Bash cargo test → exit 101 ×2");
}

/// V4: a checkpoint cut mid-turn shows the turn so far.
#[test]
fn open_turn_shows_so_far() {
    let h = History::default().ingest(&[entry(bash("1", "ls", "a", false))]);
    assert_eq!(h.digest(4096), "T1 (so far) \"\" · Bash ls → ok");
    let h = h.ingest(&[entry(vec![Item::Text("done".into()), Item::TurnEnd])]);
    assert_eq!(h.digest(4096), "T1 \"done\" · Bash ls → ok");
}

/// V4: older turns fold into counts; the digest never passes 4 KB however long the session.
#[test]
fn digest_folds_and_stays_bounded() {
    let mut h = History::default();
    for i in 0..300 {
        h = h.ingest(&[turn(
            &format!("turn {i} {}", "w".repeat(80)),
            bash(&i.to_string(), &format!("make step{i}"), "ok", false),
        )]);
    }
    let d = h.digest(4096);
    assert!(d.len() <= 4096, "{}", d.len());
    let first = d.lines().next().unwrap();
    assert!(first.starts_with("T1–T") && first.contains("turns folded") && first.contains("(Bash "), "{first}");
    assert!(d.lines().last().unwrap().starts_with("T300 "), "newest kept");
    assert!(serde_json::to_string(&h).unwrap().len() < 64 * 1024, "state bounded");
}

/// V4: repeat hint when one signature recurs ≥3× within the last 10 turns.
#[test]
fn repeat_hints() {
    let review = "gh pr comment 412 --body review";
    let mut h = History::default();
    for i in 0..2 {
        h = h.ingest(&[turn("r", bash(&i.to_string(), review, "ok", false))]);
    }
    assert!(h.hints().is_empty(), "2× is no hint");
    h = h.ingest(&[turn("r", bash("2", review, "ok", false))]);
    assert_eq!(h.hints(), [format!("hint: signature \"Bash {review}\" seen 3× in last 3 turns")]);
    for i in 0..9 {
        h = h.ingest(&[turn("other", bash(&format!("o{i}"), &format!("ls {i}"), "ok", false))]);
    }
    assert!(h.hints().is_empty(), "out of the 10-turn window");
}

/// V6: human prompts and blocked answers; adapter-delivered Challenge text is not the human's.
#[test]
fn asks_keep_human_prompts_and_answers() {
    let h = History::default().ingest(&[entry(vec![
        Item::Prompt("fix the bug".into()),
        Item::Prompt("[pair-watcher · Challenge c2 · from the Watcher, not the human] ...".into()),
        Item::ToolCall { id: "q".into(), name: "AskUserQuestion".into(), input: "{}".into() },
        Item::ToolResult { id: "q".into(), output: "User chose: option B".into(), error: false },
    ])]);
    let asks: Vec<(bool, &str)> = h.asks().iter().map(|a| (a.answer, a.text.as_str())).collect();
    assert_eq!(asks, [(false, "fix the bug"), (true, "User chose: option B")]);
}

/// V6: each ≤1.5 KB; past ≈6 KB the middle folds to first lines, first + latest 3 stay full.
#[test]
fn asks_fold_middle_past_cap() {
    let big = |i: usize| format!("prompt {i}\n{}", "x".repeat(2000));
    let prompts: Vec<Item> = (0..8).map(|i| Item::Prompt(big(i))).collect();
    let h = History::default().ingest(&[entry(prompts)]);
    let asks = h.asks();
    assert!(asks.iter().all(|a| a.text.len() <= 1536));
    assert!(!asks[0].folded && asks[0].text.contains("[clipped"));
    assert!(asks[1..5].iter().all(|a| a.folded && !a.text.contains('\n')));
    assert!(asks[5..].iter().all(|a| !a.folded));
}

/// V2: ingest is pure; the committed state is unchanged until the caller replaces it.
#[test]
fn ingest_is_pure() {
    let h = History::default().ingest(&[turn("a", bash("1", "ls", "x", false))]);
    let before = h.clone();
    let _ = h.ingest(&[turn("b", bash("2", "ls", "x", false))]);
    assert_eq!(h, before);
}
