use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Barrier};
use std::thread;

use pair_watcher::event::{Event, Kind};
use pair_watcher::home::Home;
use pair_watcher::jsonl::{self, Record, SCHEMA};
use pair_watcher::mailbox::{Hook, Mailbox, valid_session_id};
use pair_watcher::outbox::{Item, ItemKind, LEASE_MS, Outbox};
use serde_json::{Value, json};
use tempfile::TempDir;

fn home() -> (TempDir, Home) {
    let dir = TempDir::new().unwrap();
    let home = Home::new(dir.path().join(".pair-watcher"));
    home.ensure_layout().unwrap();
    (dir, home)
}

fn item(id: &str) -> Item {
    Item { id: id.into(), kind: ItemKind::Challenge, text: format!("challenge {id}") }
}

fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// --- jsonl ---

#[test]
fn every_record_carries_schema_version_and_time() {
    let (_d, home) = home();
    let log = Mailbox::register(&home, "s1").unwrap().log();
    log.append(Event::new(Kind::Session).with("op", "start")).unwrap();
    let line = fs::read_to_string(log.path()).unwrap();
    let v: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["v"], json!(SCHEMA));
    assert!(v["t"].as_u64().unwrap() > 1_700_000_000_000);
    assert_eq!(v["k"], "session");
    assert_eq!(v["op"], "start");
}

#[test]
fn reader_leaves_torn_tail_unread_and_resumes_from_offset() {
    let (d, _home) = home();
    let path = d.path().join("x.jsonl");
    jsonl::append(&path, &Record::at(1, json!({"a": 1}))).unwrap();
    fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(br#"{"v":1,"t":2,"a""#).unwrap();

    let first = jsonl::read_from::<Record<Value>>(&path, 0).unwrap();
    assert_eq!(first.records.len(), 1);
    assert!(first.invalid.is_empty());

    // The writer finishes the line; the reader picks it up from its offset.
    fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b":2}\n").unwrap();
    let second = jsonl::read_from::<Record<Value>>(&path, first.next).unwrap();
    assert_eq!(second.records, vec![Record::at(2, json!({"a": 2}))]);
    assert_eq!(second.next, fs::metadata(&path).unwrap().len());
}

#[test]
fn append_after_crashed_writer_keeps_new_line_intact() {
    let (d, _home) = home();
    let path = d.path().join("x.jsonl");
    fs::write(&path, br#"{"v":1,"t":1,"tor"#).unwrap();
    jsonl::append(&path, &Record::at(2, json!({"ok": true}))).unwrap();
    let batch = jsonl::read_from::<Record<Value>>(&path, 0).unwrap();
    assert_eq!(batch.records, vec![Record::at(2, json!({"ok": true}))]);
    assert_eq!(batch.invalid.len(), 1);
}

#[test]
fn unknown_schema_major_is_flagged_not_parsed() {
    let (d, _home) = home();
    let path = d.path().join("x.jsonl");
    fs::write(&path, "{\"v\":2,\"t\":1,\"k\":\"session\"}\n{\"t\":1}\nnot json\n").unwrap();
    let batch = jsonl::read_from::<Record<Event>>(&path, 0).unwrap();
    assert!(batch.records.is_empty());
    assert_eq!(batch.invalid.len(), 3);
    assert!(batch.invalid[0].contains("schema v2 unsupported"), "{:?}", batch.invalid);
}

#[test]
fn missing_file_reads_empty() {
    let (d, _home) = home();
    let batch = jsonl::read_from::<Record<Value>>(&d.path().join("none.jsonl"), 0).unwrap();
    assert!(batch.records.is_empty() && batch.next == 0);
}

// --- event log ---

#[test]
fn event_kinds_use_i20_names() {
    let kinds = [
        (Kind::Session, "session"),
        (Kind::Wake, "wake"),
        (Kind::Concern, "concern"),
        (Kind::Shown, "shown"),
        (Kind::Human, "human"),
        (Kind::Challenge, "challenge"),
        (Kind::Debate, "debate"),
        (Kind::Escalation, "escalation"),
        (Kind::Ruled, "ruled"),
        (Kind::Answered, "answered"),
        (Kind::Ruling, "ruling"),
        (Kind::Unruled, "unruled"),
        (Kind::ClosedStale, "closed-stale"),
        (Kind::Status, "status"),
        (Kind::Watcher, "watcher"),
        (Kind::Coverage, "coverage"),
        (Kind::Audit, "audit"),
    ];
    for (kind, name) in kinds {
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(name));
    }
}

#[test]
fn event_log_round_trips_and_tails() {
    let (_d, home) = home();
    let log = Mailbox::register(&home, "s1").unwrap().log();
    let ruled = Event::new(Kind::Ruled).with("id", "e1").with("side", "watcher").with("ruling_sent", true);
    log.append(ruled.clone()).unwrap();
    let first = log.read_from(0).unwrap();
    assert_eq!(first.records.iter().map(|r| &r.body).collect::<Vec<_>>(), vec![&ruled]);

    log.append(Event::new(Kind::ClosedStale).with("id", "e2")).unwrap();
    let next = log.read_from(first.next).unwrap();
    assert_eq!(next.records.len(), 1);
    assert_eq!(next.records[0].body.k, Kind::ClosedStale);
    assert_eq!(mode(log.path()), 0o600);
}

// --- mailbox ---

#[test]
fn register_creates_private_mailbox_and_unregister_closes_it() {
    let (_d, home) = home();
    assert!(Mailbox::open_registered(&home, "abc-1").is_none());
    let mb = Mailbox::register(&home, "abc-1").unwrap();
    assert_eq!(mode(mb.dir()), 0o700);
    assert!(Mailbox::open_registered(&home, "abc-1").is_some());

    mb.append_hook(&Hook { event: "Stop".into(), payload: json!({"session_id": "abc-1"}) }).unwrap();
    mb.unregister().unwrap();
    mb.unregister().unwrap();
    assert!(Mailbox::open_registered(&home, "abc-1").is_none());
    // Files stay until pruned.
    assert_eq!(mb.read_hooks(0).unwrap().records.len(), 1);
}

#[test]
fn session_ids_are_one_plain_segment() {
    for ok in ["0199c2d4-7f3e-7a51-b8a5-1f2e3d4c5b6a", "abc_1.2"] {
        assert!(valid_session_id(ok), "{ok}");
    }
    for bad in ["", ".", "..", "../x", "a/b", "a b", "/abs", "a\0b", &"x".repeat(129)] {
        assert!(!valid_session_id(bad), "{bad:?}");
    }
    let (_d, home) = home();
    assert!(Mailbox::register(&home, "../escape").is_err());
    assert!(Mailbox::open_registered(&home, "..").is_none());
}

// --- outbox (V28) ---

#[test]
fn claim_ack_delivers_each_item_once_in_queue_order() {
    let (_d, home) = home();
    let outbox = Mailbox::register(&home, "s1").unwrap().outbox();
    assert!(outbox.queue(item("c1"), 10).unwrap());
    assert!(outbox.queue(item("c2"), 11).unwrap());
    assert!(!outbox.queue(item("c1"), 12).unwrap(), "duplicate id refused");

    let a = outbox.claim("stop-block", 20, |_| false).unwrap().unwrap();
    assert_eq!(a.item.id, "c1");
    // c1 is under a live claim: the next claimer gets c2.
    let b = outbox.claim("stop-block", 21, |_| false).unwrap().unwrap();
    assert_eq!(b.item.id, "c2");
    assert!(outbox.claim("stop-block", 22, |_| false).unwrap().is_none());

    assert!(outbox.ack(&a, 23).unwrap());
    assert!(!outbox.ack(&a, 24).unwrap(), "second ack is a no-op");
    assert_eq!(outbox.pending().unwrap(), vec![item("c2")]);
    assert!(outbox.ack(&b, 25).unwrap());
    assert!(outbox.pending().unwrap().is_empty());
    assert!(outbox.claim("stop-block", 20 + 10 * LEASE_MS, |_| false).unwrap().is_none());
}

#[test]
fn unacked_claim_expires_and_is_reclaimed() {
    let (_d, home) = home();
    let outbox = Mailbox::register(&home, "s1").unwrap().outbox();
    outbox.queue(item("c1"), 0).unwrap();
    let dead = outbox.claim("stop-block", 100, |_| false).unwrap().unwrap();
    assert!(outbox.claim("mod-submit", 100 + LEASE_MS - 1, |_| false).unwrap().is_none());

    let retry = outbox.claim("mod-submit", 100 + LEASE_MS, |_| false).unwrap().unwrap();
    assert_eq!(retry.item.id, "c1");
    assert!(!outbox.ack(&dead, 100 + LEASE_MS + 1).unwrap(), "expired claim cannot ack");
    assert!(outbox.ack(&retry, 100 + LEASE_MS + 2).unwrap());
}

#[test]
fn stale_items_are_dropped_at_claim() {
    let (_d, home) = home();
    let outbox = Mailbox::register(&home, "s1").unwrap().outbox();
    outbox.queue(item("c1"), 0).unwrap();
    outbox.queue(item("c2"), 0).unwrap();
    let mut checked = Vec::new();
    let got = outbox
        .claim("stop-block", 5, |i| {
            checked.push(i.id.clone());
            i.id == "c1"
        })
        .unwrap()
        .unwrap();
    assert_eq!(got.item.id, "c2");
    assert_eq!(checked, ["c1", "c2"]);
    assert_eq!(outbox.pending().unwrap(), vec![item("c2")]);

    let text = fs::read_to_string(home.sessions_dir().join("s1/outbox.jsonl")).unwrap();
    assert!(text.contains(r#""k":"dropped","id":"c1","reason":"stale""#), "{text}");
}

#[test]
fn dropped_item_is_never_claimed() {
    let (_d, home) = home();
    let outbox = Mailbox::register(&home, "s1").unwrap().outbox();
    outbox.queue(Item { id: "r1".into(), kind: ItemKind::Ruling, text: "ruling".into() }, 0).unwrap();
    assert!(outbox.drop_item("r1", "superseded", 1).unwrap());
    assert!(!outbox.drop_item("r1", "superseded", 2).unwrap());
    assert!(outbox.claim("stop-block", 3, |_| false).unwrap().is_none());
}

#[test]
fn concurrent_claimers_split_items_exactly_once() {
    let (_d, home) = home();
    let mb = Mailbox::register(&home, "s1").unwrap();
    let ids: Vec<String> = (0..20).map(|i| format!("c{i}")).collect();
    for id in &ids {
        mb.outbox().queue(item(id), 0).unwrap();
    }
    let path = mb.dir().join("outbox.jsonl");
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (path, barrier) = (path.clone(), barrier.clone());
            thread::spawn(move || {
                // Separate handles, like separate hook processes.
                let outbox = Outbox::new(path);
                barrier.wait();
                let mut got = Vec::new();
                while let Some(c) = outbox.claim("stop-block", 1, |_| false).unwrap() {
                    assert!(outbox.ack(&c, 2).unwrap());
                    got.push(c.item.id);
                }
                got
            })
        })
        .collect();
    let mut all: Vec<String> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
    all.sort_by_key(|id| id[1..].parse::<u32>().unwrap());
    assert_eq!(all, ids);
}
