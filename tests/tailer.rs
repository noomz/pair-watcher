use std::fs;
use std::path::{Path, PathBuf};

use pair_watcher::jsonl::now_ms;
use pair_watcher::tailer::{self, Cursor, Format, Item, Reanchor, Tail, find_guardian};
use tempfile::TempDir;

const WORKER: &str = "rollout-2026-10-05T16-46-20-01a10b75-04dc-7bd1-b3af-3f0785e5bc6e.jsonl";
const GUARDIAN: &str = "rollout-2026-10-05T16-46-20-01a10b75-053f-74b1-a40c-45ecec61ef58.jsonl";
const WORKER_ID: &str = "01a10b75-04dc-7bd1-b3af-3f0785e5bc6e";

const ALL: [(Format, &str); 4] =
    [(Format::Claude, "claude.jsonl"), (Format::Codex, WORKER), (Format::Guardian, GUARDIAN), (Format::Pi, "pi.jsonl")];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transcripts").join(name)
}

fn lines(name: &str) -> Vec<Vec<u8>> {
    fs::read(fixture(name)).unwrap().split_inclusive(|&b| b == b'\n').map(<[u8]>::to_vec).collect()
}

fn read(format: Format, path: &Path, cursor: &Cursor) -> Tail {
    tailer::read(format, path, cursor).unwrap()
}

fn items(tail: &Tail) -> Vec<Item> {
    tail.entries.iter().flat_map(|e| e.items.clone()).collect()
}

fn read_items(format: Format, path: &Path) -> Vec<Item> {
    items(&read(format, path, &Cursor::default()))
}

#[derive(Default, Debug, PartialEq)]
struct Counts {
    prompts: usize,
    texts: usize,
    calls: usize,
    results: usize,
    turn_ends: usize,
}

fn count(items: &[Item]) -> Counts {
    let mut c = Counts::default();
    for item in items {
        match item {
            Item::Prompt(_) => c.prompts += 1,
            Item::Text(_) => c.texts += 1,
            Item::ToolCall { .. } => c.calls += 1,
            Item::ToolResult { .. } => c.results += 1,
            Item::TurnEnd => c.turn_ends += 1,
            Item::ReviewStarted | Item::ReviewDone { .. } => {}
        }
    }
    c
}

fn call_ids(items: &[Item]) -> (Vec<&str>, Vec<&str>) {
    let calls = items.iter().filter_map(|i| if let Item::ToolCall { id, .. } = i { Some(id.as_str()) } else { None });
    let results =
        items.iter().filter_map(|i| if let Item::ToolResult { id, .. } = i { Some(id.as_str()) } else { None });
    (calls.collect(), results.collect())
}

// --- per-format parse, checked against the capture (counts computed independently with jq) ---

#[test]
fn claude_capture() {
    let items = read_items(Format::Claude, &fixture("claude.jsonl"));
    // 11 turns: one turn end per end_turn message although thinking + text span two lines each;
    // matches the 11 `turn_duration` lines and 11 `Stop` hooks of the capture.
    assert_eq!(count(&items), Counts { prompts: 11, texts: 11, calls: 4, results: 4, turn_ends: 11 });
    let Item::Prompt(first) = &items[0] else { panic!("first item {:?}", items[0]) };
    assert!(first.starts_with("The pw-idle-probe plugin sent a message:"), "{first}");
    let (calls, results) = call_ids(&items);
    assert_eq!(calls, results);
    // The turn ends after its reply, not on the thinking line that comes first.
    assert!(matches!(items.last(), Some(Item::TurnEnd)));
    for pair in items.windows(2) {
        if pair[1] == Item::TurnEnd {
            assert!(matches!(pair[0], Item::Text(_)), "turn end after {:?}", pair[0]);
        }
    }
}

#[test]
fn codex_capture() {
    let items = read_items(Format::Codex, &fixture(WORKER));
    assert_eq!(count(&items), Counts { prompts: 4, texts: 6, calls: 2, results: 2, turn_ends: 4 });
    let prompts: Vec<&str> =
        items.iter().filter_map(|i| if let Item::Prompt(p) = i { Some(p.as_str()) } else { None }).collect();
    assert!(prompts[0].starts_with("Run exactly this one shell command"), "{}", prompts[0]);
    assert!(prompts.iter().all(|p| !p.contains("<environment_context>")), "harness context is not a prompt");
    for item in &items {
        if let Item::ToolCall { name, input, .. } = item {
            assert_eq!(name, "exec");
            assert!(input.contains("sleep 15"), "{input}");
        }
    }
    let (calls, results) = call_ids(&items);
    assert_eq!(calls, results);
}

#[test]
fn guardian_capture() {
    let allow = Item::ReviewDone { outcome: Some("allow".into()) };
    assert_eq!(
        read_items(Format::Guardian, &fixture(GUARDIAN)),
        [Item::ReviewStarted, allow.clone(), Item::ReviewStarted, allow]
    );
}

#[test]
fn pi_capture() {
    let items = read_items(Format::Pi, &fixture("pi.jsonl"));
    assert_eq!(count(&items), Counts { prompts: 1, texts: 1, calls: 3, results: 3, turn_ends: 1 });
    let names: Vec<&str> = items
        .iter()
        .filter_map(|i| if let Item::ToolCall { name, .. } = i { Some(name.as_str()) } else { None })
        .collect();
    assert_eq!(names, ["read", "read", "bash"]);
    let errors: Vec<bool> =
        items.iter().filter_map(|i| if let Item::ToolResult { error, .. } = i { Some(*error) } else { None }).collect();
    assert_eq!(errors, [false, false, true]);
    let (calls, results) = call_ids(&items);
    assert_eq!(calls, results);
}

// --- guardian sibling discovery (V56) ---

#[test]
fn finds_guardian_sibling_by_parent_thread_id() {
    let dir = TempDir::new().unwrap();
    for name in [WORKER, GUARDIAN] {
        fs::copy(fixture(name), dir.path().join(name)).unwrap();
    }
    // The Worker's own rollout shares `session_id` with the guardian; only `parent_thread_id` counts.
    fs::copy(fixture(WORKER), dir.path().join("rollout-2026-10-05T17-00-00-other.jsonl")).unwrap();
    fs::write(dir.path().join("rollout-torn.jsonl"), b"{\"type\":\"session_me").unwrap();
    fs::copy(fixture(GUARDIAN), dir.path().join("notes.jsonl")).unwrap();

    assert_eq!(find_guardian(dir.path(), WORKER_ID).unwrap(), Some(dir.path().join(GUARDIAN)));
    assert_eq!(find_guardian(dir.path(), "01a10b75-053f-74b1-a40c-45ecec61ef58").unwrap(), None);
}

// --- cursor (V10) ---

#[test]
fn entries_carry_arrival_time_not_transcript_time() {
    for (format, name) in ALL {
        let before = now_ms();
        let tail = read(format, &fixture(name), &Cursor::default());
        let after = now_ms();
        assert!(!tail.entries.is_empty(), "{name}");
        assert!(tail.entries.iter().all(|e| (before..=after).contains(&e.t)), "{name}");
    }
}

/// Every split point (each line boundary and each line's middle): a read of the prefix, then a
/// read of the rest from the returned cursor, yields exactly the items of one full read. Covers
/// the torn last line and a Claude message whose lines straddle two reads.
#[test]
fn split_reads_equal_one_read() {
    let dir = TempDir::new().unwrap();
    for (format, name) in ALL {
        let full = read_items(format, &fixture(name));
        let bytes = fs::read(fixture(name)).unwrap();
        let mut splits = Vec::new();
        let mut at = 0;
        for line in lines(name) {
            splits.extend([at, at + line.len() / 2]);
            at += line.len();
        }
        let path = dir.path().join(name);
        for split in splits {
            fs::write(&path, &bytes[..split]).unwrap();
            let first = read(format, &path, &Cursor::default());
            assert!(first.cursor.at <= split as u64);
            fs::write(&path, &bytes).unwrap();
            let second = read(format, &path, &first.cursor);
            assert_eq!(second.reanchor, None, "{name} split {split}");
            assert!(first.invalid.is_empty() && second.invalid.is_empty(), "{name} split {split}: torn line read");
            assert_eq!([items(&first), items(&second)].concat(), full, "{name} split {split}");
        }
    }
}

#[test]
fn caught_up_read_is_empty_and_keeps_cursor() {
    for (format, name) in ALL {
        let tail = read(format, &fixture(name), &Cursor::default());
        let again = read(format, &fixture(name), &tail.cursor);
        assert!(again.entries.is_empty() && again.reanchor.is_none(), "{name}");
        assert_eq!(again.cursor, tail.cursor, "{name}");
    }
}

/// The file is rewritten so offsets shift (lines dropped in front): the cursor's id re-anchors
/// and only the entries after it are read. Claude re-anchors by `uuid`, Codex by line hash.
#[test]
fn id_reanchors_after_rewrite() {
    let dir = TempDir::new().unwrap();
    for (format, name) in [(Format::Claude, "claude.jsonl"), (Format::Codex, WORKER), (Format::Pi, "pi.jsonl")] {
        let all = lines(name);
        let (cut, mid) = (all.len() / 4, all.len() / 2);
        let path = dir.path().join(name);
        fs::write(&path, all[..mid].concat()).unwrap();
        let first = read(format, &path, &Cursor::default());

        fs::write(&path, all[cut..].concat()).unwrap();
        let second = read(format, &path, &first.cursor);
        assert_eq!(second.reanchor, Some(Reanchor::Found), "{name}");
        fs::write(&path, all[mid..].concat()).unwrap();
        assert_eq!(items(&second), read_items(format, &path), "{name}");

        // The re-anchored cursor is a plain cursor again.
        fs::write(&path, all[cut..].concat()).unwrap();
        let caught_up = read(format, &path, &second.cursor);
        assert!(caught_up.entries.is_empty() && caught_up.reanchor.is_none(), "{name}");
    }
}

/// The cursor's entry is gone (new file, history dropped): read everything rather than skip.
#[test]
fn lost_anchor_reads_whole_file() {
    let dir = TempDir::new().unwrap();
    let all = lines("claude.jsonl");
    let path = dir.path().join("t.jsonl");
    fs::write(&path, all[..100].concat()).unwrap();
    let first = read(Format::Claude, &path, &Cursor::default());

    fs::write(&path, all[150..].concat()).unwrap();
    let second = read(Format::Claude, &path, &first.cursor);
    assert_eq!(second.reanchor, Some(Reanchor::Lost));
    assert_eq!(items(&second), read_items(Format::Claude, &path));

    fs::write(&path, b"").unwrap();
    let emptied = read(Format::Claude, &path, &second.cursor);
    assert_eq!(emptied.reanchor, Some(Reanchor::Lost));
    assert_eq!((emptied.cursor.id, emptied.cursor.at), (None, 0));
}

#[test]
fn invalid_line_reported_once_and_skipped() {
    let dir = TempDir::new().unwrap();
    let all = lines("pi.jsonl");
    let path = dir.path().join("t.jsonl");
    let junk = b"not json\n";
    fs::write(&path, [all[..10].concat(), junk.to_vec(), all[10..].concat()].concat()).unwrap();
    let tail = read(Format::Pi, &path, &Cursor::default());
    let junk_at = all[..10].concat().len();
    assert_eq!(tail.invalid.len(), 1);
    assert!(tail.invalid[0].contains(&format!("@{junk_at}:")), "{}", tail.invalid[0]);
    assert_eq!(items(&tail), read_items(Format::Pi, &fixture("pi.jsonl")));
    assert!(read(Format::Pi, &path, &tail.cursor).invalid.is_empty());
}

#[test]
fn missing_transcript_is_an_error() {
    let dir = TempDir::new().unwrap();
    let err = tailer::read(Format::Claude, &dir.path().join("gone.jsonl"), &Cursor::default()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn cursor_persists_as_json() {
    let tail = read(Format::Claude, &fixture("claude.jsonl"), &Cursor::default());
    let json = serde_json::to_string(&tail.cursor).unwrap();
    assert_eq!(serde_json::from_str::<Cursor>(&json).unwrap(), tail.cursor);
    let fresh = serde_json::to_value(Cursor::default()).unwrap();
    assert_eq!(fresh, serde_json::json!({"id": null, "at": 0}));
}
