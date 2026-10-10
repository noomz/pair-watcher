mod common;

use std::fs;
use std::io::Cursor;

use common::{ROOT_COMMIT, World, names, write};
use pair_watcher::approval::{self, Answer};

const PERF: &str = "Attend to: slow loops.\n";

fn ask(w: &World, input: &str) -> (Answer, String) {
    let set = w.focuses(&[]);
    let mut out = Vec::new();
    let answer = approval::ask(&set.pending[0], &mut Cursor::new(input), &mut out).unwrap();
    (answer, String::from_utf8(out).unwrap())
}

#[test]
fn prompt_offers_view_then_takes_an_answer() {
    let w = World::new();
    w.repo_focus("perf", PERF);
    let (answer, out) = ask(&w, "v\na\n");
    assert_eq!(answer, Answer::Approve);
    assert!(out.contains("[v]iew [a]pprove [s]kip this session [n]ever for this file"), "{out}");
    assert!(out.contains("Attend to: slow loops."), "view prints the file");
    assert_eq!(out.matches("[v]iew").count(), 2, "asks again after view");

    assert_eq!(ask(&w, "s\n").0, Answer::Skip);
    assert_eq!(ask(&w, "n\n").0, Answer::Never);
    assert_eq!(ask(&w, "what\nN\n").0, Answer::Never, "unknown input re-asks");
    assert_eq!(ask(&w, "").0, Answer::Skip, "end of input skips");
}

#[test]
fn approval_enables_exactly_this_file() {
    let w = World::new();
    w.repo_focus("perf", PERF);
    let pending = w.focuses(&[]).pending[0].clone();
    approval::record(&w.home, ROOT_COMMIT, &pending, true).unwrap();
    let set = w.focuses(&[]);
    assert!(names(&set).contains(&"perf"));
    assert!(set.pending.is_empty());

    w.repo_focus("perf", "Attend to: everything.\n");
    let set = w.focuses(&[]);
    assert!(!names(&set).contains(&"perf"), "V82: changed file is not approved");
    assert_eq!(set.pending.len(), 1, "changed file re-asks");
}

#[test]
fn never_keeps_this_file_off_without_asking_again() {
    let w = World::new();
    w.repo_focus("perf", PERF);
    let pending = w.focuses(&[]).pending[0].clone();
    approval::record(&w.home, ROOT_COMMIT, &pending, false).unwrap();
    let set = w.focuses(&[]);
    assert!(!names(&set).contains(&"perf"));
    assert!(set.pending.is_empty(), "refused file is not re-asked");
}

#[test]
fn record_keeps_human_edits_in_the_per_repo_file() {
    let w = World::new();
    w.user_repo("# my overrides\nceiling_usd = 3 # tighter here\n\n[approved_focus]\nold = { sha256 = \"x\", approved = false }\n");
    w.repo_focus("perf", PERF);
    let pending = w.focuses(&[]).pending[0].clone();
    approval::record(&w.home, ROOT_COMMIT, &pending, true).unwrap();
    let text = fs::read_to_string(w.home.repo_config(ROOT_COMMIT)).unwrap();
    assert!(text.starts_with("# my overrides\nceiling_usd = 3 # tighter here\n"), "{text}");
    assert!(text.contains("old = { sha256 = \"x\", approved = false }"), "{text}");
    assert!(text.contains(&format!("perf = {{ sha256 = \"{}\", approved = true }}", pending.sha256)), "{text}");
    assert_eq!(w.resolve().get("ceiling_usd").and_then(toml::Value::as_float), Some(3.0));
}

#[test]
fn ticker_names_the_approve_command() {
    let w = World::new();
    w.repo_focus("perf", PERF);
    let line = approval::ticker_line(&w.focuses(&[]).pending[0]);
    assert_eq!(line, "repo Focus `perf` not approved — pair-watcher focus approve perf");
}

#[test]
fn record_refuses_a_non_table_approved_focus() {
    let w = World::new();
    write(&w.home.repo_config(ROOT_COMMIT), "approved_focus = 3\n");
    w.repo_focus("perf", PERF);
    // The resolver reads the user file first; a bad `approved_focus` type just means no approvals.
    let pending = w.focuses(&[]).pending[0].clone();
    let err = approval::record(&w.home, ROOT_COMMIT, &pending, true).unwrap_err();
    assert!(err.to_string().contains("not a table"), "{err}");
}
