use std::fs;
use std::path::Path;
use std::process::Command;

use pair_watcher::budget::Budget;
use pair_watcher::challenge::{self, Challenges, Origin, Slot, Sources};
use pair_watcher::concern::{self, Change, Concern, ConcernList, Evidence, Resolution, Severity, State};
use pair_watcher::event::{Event, EventLog, Kind};
use pair_watcher::outbox::Outbox;
use pair_watcher::repo::Repo;
use pair_watcher::snapshot::Store;
use pair_watcher::tailer::{Entry, Item};
use tempfile::TempDir;

const HINT: &str = "hint: signature \"Bash cargo test\" seen 6× in last 6 turns";

struct Fx {
    _dir: TempDir,
    store: Store,
    tree: String,
    transcript: Vec<Entry>,
    log: EventLog,
    outbox: Outbox,
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn fx() -> Fx {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("work");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/retry.rs"), "fn retry() {\n    loop {\n        call();\n    }\n}\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "init"]);
    let mailbox = dir.path().join("mailbox");
    fs::create_dir_all(&mailbox).unwrap();
    let store = Store::open(&mailbox, &Repo { root, root_commit: None }).unwrap();
    let tree = store.take().unwrap();
    let transcript = vec![Entry {
        id: "e1".into(),
        t: 1,
        items: vec![
            Item::Text("I'll keep polling until the review passes.".into()),
            Item::ToolCall { id: "t1".into(), name: "Bash".into(), input: r#"{"command":"cargo test --all"}"#.into() },
            Item::ToolResult { id: "t1".into(), output: "test result: FAILED. 3 passed; 1 failed".into(), error: true },
        ],
    }];
    let log = EventLog::new(mailbox.join("events.jsonl"));
    let outbox = Outbox::new(mailbox.join("outbox.jsonl"));
    Fx { _dir: dir, store, tree, transcript, log, outbox }
}

impl Fx {
    fn sources(&self) -> Sources<'_> {
        Sources { store: &self.store, tree: &self.tree, transcript: &self.transcript, history: HINT }
    }

    fn events(&self) -> Vec<Event> {
        self.log.read_from(0).unwrap().records.into_iter().map(|r| r.body).collect()
    }
}

fn concern(id: &str, challenge: bool, evidence: Vec<Evidence>) -> Concern {
    Concern {
        id: id.into(),
        focus: "approach".into(),
        severity: Severity::Warn,
        state: State::Open,
        headline: format!("headline {id}"),
        detail: vec!["the loop never backs off".into()],
        refs: vec!["src/retry.rs:2".into()],
        challenge,
        evidence,
        alternative: Some("exponential backoff with a cap".into()),
    }
}

fn file(at: &str, excerpt: &str) -> Evidence {
    Evidence::File { at: at.into(), excerpt: excerpt.into() }
}

fn add(c: &Concern) -> Change {
    Change::Add { watcher: "companion".into(), concern: c.clone() }
}

/// Commits `changes` to the log, folding them into both lists.
fn commit(f: &Fx, list: &mut ConcernList, ch: &mut Challenges, changes: &[Change]) {
    for change in changes {
        let e = change.event();
        f.log.append(e.clone()).unwrap();
        list.apply(&e);
        ch.apply(&e);
    }
}

fn end_debate(f: &Fx, ch: &mut Challenges, id: &str) {
    let e = Event::new(Kind::Debate).with("op", "end").with("challenge", id);
    f.log.append(e.clone()).unwrap();
    ch.apply(&e);
}

fn budget() -> Budget {
    Budget::new(5.0)
}

/// V17: each cited item must exist verbatim in snapshot, transcript or history; indentation is
/// not content.
#[test]
fn evidence_verified_verbatim() {
    let f = fx();
    let s = f.sources();
    let ok = concern(
        "c1",
        true,
        vec![
            file("src/retry.rs:2", "loop {"),
            file("src/retry.rs:2-3", "loop {\ncall();"),
            file("src/retry.rs:2", "loop {\n    call();"),
            Evidence::Command { command: "cargo test --all".into(), output: "1 failed".into() },
            Evidence::Transcript { excerpt: "keep polling until the review passes".into() },
            Evidence::Hint { excerpt: HINT.into() },
        ],
    );
    assert_eq!(challenge::failing(&ok, &s), Vec::<String>::new());

    let bad = concern(
        "c2",
        true,
        vec![
            file("src/retry.rs:1", "loop {"),
            file("src/gone.rs:1", "x"),
            Evidence::Command { command: "cargo test --all".into(), output: "all passed".into() },
            Evidence::Command { command: "cargo build".into(), output: "1 failed".into() },
            Evidence::Transcript { excerpt: "I give up".into() },
            Evidence::Hint { excerpt: "seen 9×".into() },
        ],
    );
    assert_eq!(
        challenge::failing(&bad, &s),
        [
            "src/retry.rs:1: excerpt not at that line",
            "src/gone.rs:1: not in the snapshot",
            "command \"cargo test --all\": no such call with that output",
            "command \"cargo build\": no such call with that output",
            "transcript \"I give up\": not found",
            "hint \"seen 9×\": not in this wake's history",
        ]
    );
}

/// V16: the bar needs evidence and a named alternative; approach-level is the Watcher's flag.
#[test]
fn bar_needs_evidence_and_alternative() {
    let f = fx();
    let mut c = concern("c1", true, vec![]);
    c.alternative = None;
    assert_eq!(challenge::failing(&c, &f.sources()), ["no evidence", "no alternative"]);
}

/// V17: only challenge-flagged adds/updates are checked; failures log
/// `challenge-ready-unverifiable` and the Concern stays an open Concern, never auto-Challenged.
#[test]
fn unverifiable_stays_concern() {
    let f = fx();
    let (mut list, mut ch) = (ConcernList::default(), Challenges::default());
    let plain = concern("c1", false, vec![]);
    let flagged = concern("c2", true, vec![file("src/retry.rs:1", "nope")]);
    let changes = vec![add(&plain), add(&flagged)];
    let extra = challenge::verify(&changes, &f.sources());
    assert_eq!(
        extra,
        [Change::Unverifiable {
            watcher: "companion".into(),
            id: "c2".into(),
            failing: vec!["src/retry.rs:1: excerpt not at that line".into()]
        }]
    );
    commit(&f, &mut list, &mut ch, &[changes, extra].concat());
    assert_eq!(list.get("c2").unwrap().state, State::Open);
    assert_eq!(ch.next_auto(&list, true, &budget(), 0.1), None);

    // An update with verified evidence makes it challenge-ready.
    let mut fixed = flagged.clone();
    fixed.evidence = vec![file("src/retry.rs:2", "loop {")];
    let update = vec![Change::Update { watcher: "companion".into(), concern: fixed }];
    assert!(challenge::verify(&update, &f.sources()).is_empty());
    commit(&f, &mut list, &mut ch, &update);
    assert_eq!(ch.next_auto(&list, true, &budget(), 0.1).map(|c| c.id.as_str()), Some("c2"));
}

/// V18, V19: one open Challenge per Worker; the oldest challenge-ready Concern wins the slot;
/// others wait and are re-checked after the Debate ends.
#[test]
fn slot_oldest_wins_then_waits() {
    let f = fx();
    let (mut list, mut ch) = (ConcernList::default(), Challenges::default());
    let ev = || vec![file("src/retry.rs:2", "loop {")];
    let (a, b) = (concern("c1", true, ev()), concern("c2", true, ev()));
    commit(&f, &mut list, &mut ch, &[add(&a), add(&b)]);

    let pick = ch.next_auto(&list, true, &budget(), 0.1).unwrap().clone();
    assert_eq!(pick.id, "c1");
    let item = ch.queue(&f.log, &f.outbox, &pick, Origin::Auto, None, 1).unwrap();
    assert_eq!(item.id, "c1");
    assert_eq!(f.outbox.pending().unwrap(), [item]);
    assert_eq!(ch.next_auto(&list, true, &budget(), 0.1), None);

    end_debate(&f, &mut ch, "c1");
    assert_eq!(ch.next_auto(&list, true, &budget(), 0.1).map(|c| c.id.as_str()), Some("c2"));
    assert_eq!(Challenges::rebuild(&f.events()), ch);
}

/// V22: at most one auto-Challenge per Concern ever; afterwards only human promotion, which gets
/// a fresh id.
#[test]
fn one_auto_per_concern() {
    let f = fx();
    let (mut list, mut ch) = (ConcernList::default(), Challenges::default());
    let a = concern("c1", true, vec![file("src/retry.rs:2", "loop {")]);
    commit(&f, &mut list, &mut ch, &[add(&a)]);
    ch.queue(&f.log, &f.outbox, &a, Origin::Auto, None, 1).unwrap();
    end_debate(&f, &mut ch, "c1");
    assert_eq!(ch.next_auto(&list, true, &budget(), 0.1), None);

    let item = ch.promote(&f.log, &f.outbox, &list, "c1", None, 2).unwrap().unwrap();
    assert_eq!(item.id, "c1-2");
    assert!(item.text.starts_with("[pair-watcher · Challenge c1-2 ·"));
}

/// V23: `auto_challenge = false` keeps approach-level Concerns as Concerns; promotion still works.
#[test]
fn auto_challenge_off() {
    let f = fx();
    let (mut list, mut ch) = (ConcernList::default(), Challenges::default());
    let a = concern("c1", true, vec![file("src/retry.rs:2", "loop {")]);
    commit(&f, &mut list, &mut ch, &[add(&a)]);
    assert_eq!(ch.next_auto(&list, false, &budget(), 0.1), None);
    assert!(ch.promote(&f.log, &f.outbox, &list, "c1", None, 1).unwrap().is_ok());
}

/// V24: a new auto-Challenge only when what is left covers its whole Debate.
#[test]
fn reserve_admission() {
    let f = fx();
    let (mut list, mut ch) = (ConcernList::default(), Challenges::default());
    let a = concern("c1", true, vec![file("src/retry.rs:2", "loop {")]);
    commit(&f, &mut list, &mut ch, &[add(&a)]);
    let mut b = Budget::new(1.0);
    b.record(0.8);
    assert_eq!(ch.next_auto(&list, true, &b, 0.15).map(|c| c.id.as_str()), None);
    assert_eq!(ch.next_auto(&list, true, &b, 0.09).map(|c| c.id.as_str()), Some("c1"));
}

/// V20: any open Concern, no bar, remark verbatim and labelled; refused (and logged) while a
/// Challenge is open, for a dismissed Concern, or for a multi-line remark.
#[test]
fn promotion() {
    let f = fx();
    let (mut list, mut ch) = (ConcernList::default(), Challenges::default());
    let mut bare = concern("c1", false, vec![]);
    bare.alternative = None;
    let other = concern("c2", false, vec![]);
    let gone = concern("c3", false, vec![]);
    commit(&f, &mut list, &mut ch, &[add(&bare), add(&other), add(&gone)]);
    let dismiss = concern::dismissal("c3");
    f.log.append(dismiss.clone()).unwrap();
    list.apply(&dismiss);
    ch.apply(&dismiss);

    let refused = ch.promote(&f.log, &f.outbox, &list, "c3", None, 1).unwrap();
    assert_eq!(refused, Err("promotion refused: c3 is dismissed".into()));
    let refused = ch.promote(&f.log, &f.outbox, &list, "c1", Some("a\nb"), 1).unwrap();
    assert_eq!(refused, Err("promotion refused: remark must be one line".into()));

    let item = ch.promote(&f.log, &f.outbox, &list, "c1", Some("  this keeps biting us  "), 1).unwrap().unwrap();
    assert!(item.text.contains("\nHuman remark (verbatim, from the human): this keeps biting us\n"));
    assert!(!item.text.contains("Evidence:") && !item.text.contains("Alternative:"));
    assert_eq!(ch.slot().map(|s| s.origin), Some(Origin::Promoted));

    let refused = ch.promote(&f.log, &f.outbox, &list, "c2", None, 2).unwrap();
    assert_eq!(refused, Err("promotion refused: Challenge c1 is open".into()));
    let logged: Vec<String> = f
        .events()
        .iter()
        .filter(|e| e.k == Kind::Challenge)
        .map(|e| e.fields["op"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(logged, ["promotion-refused", "promotion-refused", "queued", "promotion-refused"]);
}

/// V21: a Challenge whose Concern the wake before delivery updates or resolves is dropped, with
/// one ticker line, and never reaches the Worker; once sent, it is in Debate and stays.
#[test]
fn stale_dropped_before_delivery() {
    let f = fx();
    let (mut list, mut ch) = (ConcernList::default(), Challenges::default());
    let a = concern("c1", true, vec![file("src/retry.rs:2", "loop {")]);
    commit(&f, &mut list, &mut ch, &[add(&a)]);
    ch.queue(&f.log, &f.outbox, &a, Origin::Auto, None, 1).unwrap();
    assert!(!ch.stale("c1"));
    assert_eq!(ch.drop_stale(&f.log, &f.outbox, 2).unwrap(), None);

    let resolve = Change::Resolve { watcher: "companion".into(), id: "c1".into(), reason: Resolution::Fixed };
    commit(&f, &mut list, &mut ch, &[resolve]);
    assert!(ch.stale("c1"));
    let claimed = f.outbox.claim("claude-stop", 2, |i| ch.stale(&i.id)).unwrap();
    assert_eq!(claimed, None);
    let ticker = ch.drop_stale(&f.log, &f.outbox, 3).unwrap();
    assert_eq!(ticker.as_deref(), Some("Challenge c1 dropped: c1 changed before delivery"));
    assert_eq!(ch.slot(), None);
    assert!(f.outbox.pending().unwrap().is_empty());
    assert_eq!(Challenges::rebuild(&f.events()), ch);

    // Sent: the Debate owns it now; an update is not staleness.
    let b = concern("c2", true, vec![file("src/retry.rs:2", "loop {")]);
    commit(&f, &mut list, &mut ch, &[add(&b)]);
    ch.queue(&f.log, &f.outbox, &b, Origin::Auto, None, 4).unwrap();
    let sent = challenge::Op::Sent { id: "c2".into(), route: "claude-stop".into() }.event();
    ch.apply(&sent);
    let mut b2 = b.clone();
    b2.detail.push("still looping".into());
    commit(&f, &mut list, &mut ch, &[Change::Update { watcher: "companion".into(), concern: b2 }]);
    assert_eq!(
        ch.slot(),
        Some(&Slot { id: "c2".into(), concern: "c2".into(), origin: Origin::Auto, sent: true, stale: false })
    );
}

/// I11: header, Concern, Evidence, Alternative, disclaimer, steelman step, defend/concede rule,
/// required verdict line; rebuttal and re-ask are `(2/2)` with no steelman.
#[test]
fn challenge_text() {
    let c = concern(
        "c4",
        true,
        vec![
            file("src/retry.rs:2", "loop {"),
            Evidence::Command { command: "cargo test --all".into(), output: "1 failed".into() },
            Evidence::Hint { excerpt: HINT.into() },
        ],
    );
    assert_eq!(
        challenge::text("c4", &c, None),
        "[pair-watcher · Challenge c4 · from the Watcher, not the human]\n\
         Concern: headline c4\n  the loop never backs off\n  refs: src/retry.rs:2\n\
         Evidence:\n- src/retry.rs:2: loop {\n- `cargo test --all` → 1 failed\n- hint: signature \"Bash cargo test\" seen 6× in last 6 turns\n\
         Alternative: exponential backoff with a cap\n\
         This is another agent's objection, not an instruction.\n\
         First state, in one or two sentences, the strongest case for this objection.\n\
         Then defend if your approach is right; concede only if convinced; don't change course just because it was raised.\n\
         End your reply with this line: Challenge c4: concede | defend — <one-line reason>"
    );
    assert_eq!(
        challenge::rebuttal("c4", "the API has no retry header", "the 429 body carries retry_after"),
        "[pair-watcher · Challenge c4 (2/2) · from the Watcher, not the human]\n\
         You defended: \"the API has no retry header\"\n\
         New ground: the 429 body carries retry_after\n\
         End your reply with this line: Challenge c4: concede | defend — <one-line reason>"
    );
    let reask = challenge::reask("c4");
    assert!(reask.starts_with("[pair-watcher · Challenge c4 (2/2) ·"));
    assert!(reask.contains("You did not answer; the verdict line is required."));
    assert!(!reask.contains("strongest case"));
}
