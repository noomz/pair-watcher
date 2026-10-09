//! §T18 retention (V96) and telemetry (V99): records age out, pinned ones stay, the cap drops
//! oldest unpinned first, stores go with their last record, logs and rulings are never touched;
//! telemetry is opt-in, aggregate-only and drops a failed send.

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime};

use pair_watcher::audit::{Label, Op as AuditOp};
use pair_watcher::concern::{Change, Concern, Severity, State};
use pair_watcher::event::{Event, EventLog, Kind};
use pair_watcher::home::Home;
use pair_watcher::human;
use pair_watcher::mailbox::LOG_FILE;
use pair_watcher::record::{self, WakeRecord};
use pair_watcher::retention::{self, Policy};
use pair_watcher::status::{Outcome, Wake};
use pair_watcher::telemetry::{self, Sent};
use pair_watcher::wake::Input;
use serde_json::json;
use tempfile::TempDir;

const DAY: u64 = 86_400_000;

fn now() -> u64 {
    pair_watcher::jsonl::now_ms()
}

fn home() -> (TempDir, Home) {
    let dir = TempDir::new().unwrap();
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    (dir, home)
}

fn age(path: &Path, days: u64) {
    let t = SystemTime::now() - Duration::from_millis(days * DAY);
    File::open(path).unwrap().set_modified(t).unwrap();
}

/// Session `id` with wake records `(n, outcome, age days)`, a snapshot store aged `store_days`.
fn session(home: &Home, id: &str, live: bool, wakes: &[(u64, Outcome, u64)], store_days: u64) -> std::path::PathBuf {
    let dir = home.sessions_dir().join(id);
    fs::create_dir_all(dir.join("snapshots.git/objects")).unwrap();
    fs::write(dir.join("snapshots.git/objects/blob"), vec![0u8; 4096]).unwrap();
    fs::write(dir.join("snapshots.index"), b"idx").unwrap();
    age(&dir.join("snapshots.git"), store_days);
    if live {
        fs::write(dir.join("registered"), b"").unwrap();
    }
    let log = EventLog::new(dir.join(LOG_FILE));
    log.append(Event::new(Kind::Session).with("phase", "start")).unwrap();
    for &(n, outcome, days) in wakes {
        let rec = WakeRecord {
            n,
            watcher: "companion".into(),
            trigger: "turn-end".into(),
            prompt: String::new(),
            briefs: vec![],
            input: Input { sections: vec![], diff_bytes: 0 },
            tree: None,
            head: None,
            output: None,
        };
        let rel = record::write(&dir, &rec).unwrap();
        age(&dir.join(&rel), days);
        log.append(Wake { n, watcher: "companion".into(), outcome, record: Some(rel), ..Wake::default() }.event())
            .unwrap();
    }
    dir
}

fn has(dir: &Path, n: u64) -> bool {
    dir.join(record::rel(n, "companion")).is_file()
}

const POLICY: Policy = Policy { retention_days: 30, cap_gb: 2.0 };

/// V96: unpinned records older than `retention_days` go; Challenge/Escalation and audit-labelled
/// wakes stay; a store goes only with an ended session's last record; logs and rulings stay.
#[test]
fn retention_keeps_pinned_and_live_stores() {
    let (_t, home) = home();
    let s1 = session(
        &home,
        "s1",
        false,
        &[(1, Outcome::Silent, 40), (2, Outcome::Challenge, 40), (3, Outcome::Silent, 40), (4, Outcome::Concern, 1)],
        40,
    );
    // Wake 3 audit-labelled: pinned.
    let log = EventLog::new(s1.join(LOG_FILE));
    let finding = AuditOp::Finding {
        id: "F1".into(),
        wake: 3,
        watcher: "companion".into(),
        focus: "approach".into(),
        headline: "h".into(),
        detail: vec![],
    };
    log.append(finding.event()).unwrap();
    log.append(AuditOp::Label { finding: "F1".into(), label: Label::Miss }.event()).unwrap();
    let s2 = session(&home, "s2", false, &[(1, Outcome::Escalation, 0), (2, Outcome::Silent, 45)], 45);
    let s3 = session(&home, "s3", false, &[(1, Outcome::Silent, 50)], 50);
    let s4 = session(&home, "s4", true, &[(1, Outcome::Silent, 50)], 50);
    let rulings = home.repos_dir().join("abc.rulings.toml");
    fs::write(&rulings, "last_id = 0\n").unwrap();

    let p = retention::prune(&home, POLICY, now()).unwrap();
    assert_eq!((p.records, p.stores), (4, 1));
    assert!(!has(&s1, 1) && has(&s1, 2) && has(&s1, 3) && has(&s1, 4));
    assert!(s1.join("snapshots.git").is_dir(), "records remain: store kept");
    assert!(has(&s2, 1) && !has(&s2, 2));
    assert!(!has(&s3, 1) && !s3.join("snapshots.git").exists() && !s3.join("snapshots.index").exists());
    assert!(!has(&s4, 1) && s4.join("snapshots.git").is_dir(), "live session keeps its store");
    for s in [&s1, &s2, &s3, &s4] {
        assert!(s.join(LOG_FILE).is_file(), "event logs stay");
    }
    assert!(rulings.is_file());
}

/// V96: over `cap_gb`, the oldest unpinned records go first, young ones too; pinned never.
#[test]
fn cap_drops_oldest_unpinned_first() {
    let (_t, home) = home();
    let s1 =
        session(&home, "s1", true, &[(1, Outcome::Silent, 3), (2, Outcome::Silent, 2), (3, Outcome::Escalation, 5)], 0);
    let size = |n| fs::metadata(s1.join(record::rel(n, "companion"))).unwrap().len();
    // Room for the store, the pinned record and one more.
    let total = 4096 + 3 + size(3) + size(2);
    let policy = Policy { retention_days: 30, cap_gb: total as f64 / (1024.0 * 1024.0 * 1024.0) };
    let p = retention::prune(&home, policy, now()).unwrap();
    assert_eq!(p.records, 1);
    assert!(!has(&s1, 1) && has(&s1, 2) && has(&s1, 3));

    let p = retention::prune(&home, Policy { retention_days: 30, cap_gb: 0.0 }, now()).unwrap();
    assert_eq!(p.records, 1);
    assert!(has(&s1, 3), "pinned stays over the cap");
}

/// V99: `telemetry on` sets the global key, keeping the rest; the weekly send goes after 7 days,
/// aggregate only, custom Focus names → `custom`; a failed send is dropped, not retried.
#[test]
fn telemetry_weekly_aggregate_only() {
    let (_t, home) = home();
    fs::write(home.config_file(), "# mine\nmodel = \"auto\"\n").unwrap();
    let t0 = now() - 8 * DAY;
    telemetry::set(&home, true, t0).unwrap();
    let cfg = fs::read_to_string(home.config_file()).unwrap();
    assert!(cfg.contains("# mine") && cfg.contains("model = \"auto\"") && cfg.contains("telemetry = true"));

    let dir = home.sessions_dir().join("s1");
    fs::create_dir_all(&dir).unwrap();
    let log = EventLog::new(dir.join(LOG_FILE));
    log.append(
        Event::new(Kind::Session)
            .with("phase", "start")
            .with("repo", "/secret/repo")
            .with("harness", "codex")
            .with("mode", "swarm")
            .with("focuses", json!(["approach", "payments-team"])),
    )
    .unwrap();
    let c = Concern {
        id: "A1".into(),
        focus: "payments-team".into(),
        severity: Severity::Warn,
        state: State::Open,
        headline: "SECRET HEADLINE".into(),
        detail: vec![],
        refs: vec![],
        challenge: false,
        evidence: vec![],
        alternative: None,
        verified: false,
        tag: None,
        debate: None,
    };
    log.append(Change::Add { watcher: "payments-team".into(), concern: c }.event()).unwrap();
    log.append(human::shown("A1")).unwrap();

    let sent = std::cell::RefCell::new(None);
    let ok = |v: &serde_json::Value| {
        *sent.borrow_mut() = Some(v.clone());
        Ok(())
    };
    assert_eq!(telemetry::weekly(&home, now(), &ok).unwrap(), Sent::Sent);
    let body = sent.borrow().clone().unwrap();
    let text = body.to_string();
    assert!(!text.contains("secret") && !text.contains("SECRET") && !text.contains("payments"), "{text}");
    assert_eq!(body["groups"]["focus"]["custom"]["h1_noise"]["n"], 1);
    assert_eq!(body["focuses"], json!(["approach", "custom"]));
    assert_eq!(body["rulings"]["held"], 0);

    // Within the week: not due. After it: due, and a failure is dropped, window closed anyway.
    assert_eq!(telemetry::weekly(&home, now(), &ok).unwrap(), Sent::NotDue);
    let fail = |_: &serde_json::Value| Err("offline".to_string());
    assert_eq!(telemetry::weekly(&home, now() + 8 * DAY, &fail).unwrap(), Sent::Dropped("offline".into()));
    assert_eq!(telemetry::weekly(&home, now() + 9 * DAY, &fail).unwrap(), Sent::NotDue, "no retry");

    telemetry::set(&home, false, now()).unwrap();
    assert!(fs::read_to_string(home.config_file()).unwrap().contains("telemetry = false"));
}
