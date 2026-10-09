use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use pair_watcher::arbiter::Watcher;
use pair_watcher::concern::{self, Change, Concern, ConcernList, Evidence, Severity, State};
use pair_watcher::debate::{Op as DebateOp, Outcome, Verdict};
use pair_watcher::escalation::{self, Cause, Escalations, Notifier, Stage, Surfaces};
use pair_watcher::event::{Event, EventLog, Kind};
use pair_watcher::harness::Harness;
use pair_watcher::home::Home;
use pair_watcher::mailbox::{Hook, Mailbox};
use pair_watcher::output::Line;
use serde_json::{Value, json};
use tempfile::TempDir;

const CLAUDE: Surfaces = Surfaces { harness: Some(Harness::Claude), band_digits: true };

struct Fx {
    dir: TempDir,
    log: EventLog,
    e: Escalations,
}

impl Fx {
    fn new() -> Fx {
        let dir = TempDir::new().unwrap();
        let log = EventLog::new(dir.path().join("log.jsonl"));
        let mut f = Fx { dir, log, e: Escalations::default() };
        f.log(Change::Add { watcher: "approach".into(), concern: concern() }.event());
        f
    }

    fn log(&mut self, e: Event) {
        self.log.append(e.clone()).unwrap();
        self.e.apply(&e);
    }

    fn events(&self) -> Vec<Event> {
        self.log.read_from(0).unwrap().records.into_iter().map(|r| r.body).collect()
    }

    fn list(&self) -> ConcernList {
        ConcernList::rebuild(&self.events())
    }

    fn ops(&self, op: &str) -> Vec<Event> {
        self.events().into_iter().filter(|e| e.k == Kind::Escalation && e.str("op") == Some(op)).collect()
    }

    /// Challenge `c1` defended, then ended unresolved with `escalated`.
    fn debate_unresolved(&mut self, escalated: bool) {
        let exchange = DebateOp::Exchange {
            challenge: "c1".into(),
            n: 2,
            verdict: Verdict::Defend,
            reason: Some("the queue is idempotent, retries are safe".into()),
            entry: "e1".into(),
            turn: vec![],
        };
        self.log(exchange.event());
        let end = DebateOp::End {
            challenge: "c1".into(),
            concern: "c1".into(),
            outcome: Outcome::Unresolved,
            escalated: Some(escalated),
            withdrawn: None,
        };
        self.log(end.event());
    }

    fn raise_unresolved(&mut self, swarm: bool) -> Vec<String> {
        let list = self.list();
        self.e.raise_unresolved(&self.log, &list, swarm, CLAUDE).unwrap()
    }

    fn deliver(&mut self, notifier: &Notifier, answered: bool) -> Vec<String> {
        let list = self.list();
        self.e.deliver(&self.log, &list, |_| Ok(answered), notifier).unwrap()
    }

    /// V93: the log folds to the same state.
    fn assert_consistent(&self) {
        assert_eq!(Escalations::rebuild(&self.events()), self.e);
    }
}

fn concern() -> Concern {
    Concern {
        id: "c1".into(),
        focus: "approach".into(),
        severity: Severity::Warn,
        state: State::Open,
        headline: "retry loop never converges".into(),
        detail: vec![],
        refs: vec![],
        challenge: true,
        evidence: vec![Evidence::Command {
            command: "cargo test".into(),
            output: "6 failed\n(same 6 as before)".into(),
        }],
        alternative: Some("stop and bisect the failing test".into()),
        verified: true,
        tag: None,
        debate: None,
    }
}

/// A notifier stand-in: appends its argv, then `---`, to `<dir>/notify.argv`; exits `exit`.
fn fake(dir: &Path, exit: i32) -> PathBuf {
    let path = dir.join("notify");
    let argv = dir.join("notify.argv");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\necho --- >> '{}'\nexit {exit}\n",
        argv.display(),
        argv.display()
    );
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn calls(dir: &Path) -> Vec<String> {
    let text = fs::read_to_string(dir.join("notify.argv")).unwrap_or_default();
    text.split("---\n").filter(|c| !c.is_empty()).map(str::to_owned).collect()
}

fn swarm() -> Vec<Watcher> {
    ["approach", "correctness", "security"]
        .iter()
        .map(|f| Watcher { name: (*f).into(), focuses: vec![(*f).into()], model: None })
        .collect()
}

fn consider(w: &str, text: &str) -> (String, Line) {
    (w.to_owned(), Line::Consider { text: text.into() })
}

fn ask() -> Value {
    json!({"tool_name": "AskUserQuestion", "tool_input": {"questions": [{"question": "Drop the flaky test or fix it?"}]}})
}

/// I13: unresolved text = title + Watcher line (alternative + strongest evidence ≤100) + Worker
/// line (last verdict reason verbatim) + where to rule; no recommendation.
#[test]
fn unresolved_text() {
    let mut f = Fx::new();
    f.debate_unresolved(true);
    assert_eq!(f.raise_unresolved(true), vec!["e1".to_owned()]);
    let e = f.e.get("e1").unwrap().clone();
    assert_eq!(e.cause, Cause::Unresolved);
    assert_eq!(e.title, "Debate unresolved: retry loop never converges (c1)");
    assert!(e.lines[0].starts_with("Watcher (approach): stop and bisect the failing test — `cargo test` → 6 failed"));
    assert!(e.lines[0].len() <= "Watcher (approach): ".len() + 100, "{}", e.lines[0]);
    assert_eq!(e.lines[1], "Worker: the queue is idempotent, retries are safe");
    assert_eq!(e.rule, "Rule: pane y/n/d · band 1/2/3 · /pw rule e1 · pair-watcher rule e1");
    f.assert_consistent();

    // Companion: no Focus label; Codex: no band or `/pw`.
    let mut f = Fx::new();
    f.debate_unresolved(true);
    let list = f.list();
    let codex = Surfaces { harness: Some(Harness::Codex), band_digits: true };
    f.e.raise_unresolved(&f.log, &list, false, codex).unwrap();
    let e = f.e.get("e1").unwrap();
    assert!(e.lines[0].starts_with("Watcher: "), "{}", e.lines[0]);
    assert_eq!(e.rule, "Rule: pane y/n/d · pair-watcher rule e1");
}

/// V36: only `debate end {escalated: true}` raises; once per Challenge, across a rebuild too.
#[test]
fn unresolved_raised_once_and_only_when_escalated() {
    let mut f = Fx::new();
    f.debate_unresolved(false);
    assert!(f.raise_unresolved(true).is_empty(), "unresolved-quiet never escalates");

    let mut f = Fx::new();
    f.debate_unresolved(true);
    assert_eq!(f.raise_unresolved(true).len(), 1);
    assert!(f.raise_unresolved(true).is_empty());
    f.e = Escalations::rebuild(&f.events());
    assert!(f.raise_unresolved(true).is_empty());
    assert_eq!(f.ops("raised").len(), 1);
}

/// V40: exactly one notification per Escalation; I14: argv only.
#[test]
fn notified_exactly_once() {
    let mut f = Fx::new();
    let bin = fake(f.dir.path(), 0);
    f.debate_unresolved(true);
    f.raise_unresolved(true);
    let notifier = Notifier::NotifySend(bin);
    assert_eq!(f.deliver(&notifier, false), vec!["e1".to_owned()]);
    assert!(f.deliver(&notifier, false).is_empty());
    f.e = Escalations::rebuild(&f.events());
    assert!(f.deliver(&notifier, false).is_empty());
    let calls = calls(f.dir.path());
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].starts_with("--\nDebate unresolved: retry loop never converges (c1)\nWatcher (approach): "),
        "{}",
        calls[0]
    );
    assert!(calls[0].contains("Worker: the queue is idempotent"));
    assert_eq!(f.ops("notified")[0].str("via"), Some("notify-send"));
    assert_eq!(f.e.get("e1").unwrap().stage, Stage::Notified);
    f.assert_consistent();
}

/// V40: a failed notifier is logged with its error and never retried.
#[test]
fn failed_notifier_not_retried() {
    let mut f = Fx::new();
    let bin = fake(f.dir.path(), 1);
    f.debate_unresolved(true);
    f.raise_unresolved(true);
    let notifier = Notifier::NotifySend(bin);
    f.deliver(&notifier, false);
    f.deliver(&notifier, false);
    assert_eq!(calls(f.dir.path()).len(), 1);
    assert!(f.ops("notified")[0].str("error").is_some());
}

/// V91: raised but not notified before a restart → re-checked, then notified or dropped stale.
#[test]
fn restart_rechecks_raised() {
    let mut f = Fx::new();
    let bin = fake(f.dir.path(), 0);
    f.debate_unresolved(true);
    f.raise_unresolved(true);
    f.e = Escalations::rebuild(&f.events());
    assert_eq!(f.deliver(&Notifier::NotifySend(bin.clone()), false), vec!["e1".to_owned()]);

    let mut f = Fx::new();
    f.debate_unresolved(true);
    f.raise_unresolved(true);
    f.log(concern::dismissal("c1"));
    f.e = Escalations::rebuild(&f.events());
    assert!(f.deliver(&Notifier::NotifySend(bin), false).is_empty());
    assert_eq!(f.ops("dropped-stale").len(), 1);
    assert_eq!(f.e.get("e1").unwrap().stage, Stage::Dropped);
    f.assert_consistent();
}

/// Stale drop: an unresolved Escalation whose Concern resolved, or a blocked one the human
/// answered first (V11), never notifies.
#[test]
fn stale_escalations_dropped() {
    let mut f = Fx::new();
    let bin = fake(f.dir.path(), 0);
    f.debate_unresolved(true);
    f.raise_unresolved(true);
    f.log(Change::Resolve { watcher: "approach".into(), id: "c1".into(), reason: concern::Resolution::Fixed }.event());
    assert!(f.deliver(&Notifier::NotifySend(bin.clone()), false).is_empty());

    let ran = swarm();
    f.e.raise_blocked(&f.log, 7, &ask(), &ran, &[], true).unwrap();
    assert!(f.deliver(&Notifier::NotifySend(bin), true).is_empty());
    assert_eq!(f.ops("dropped-stale").len(), 2);
    assert!(calls(f.dir.path()).is_empty());
    f.assert_consistent();
}

/// V40, I13: blocked = question + ≤1 labelled consideration per Watcher, in Focus order;
/// silence is fine; one Escalation per blocked hook record.
#[test]
fn blocked_considerations() {
    let mut f = Fx::new();
    let ran = swarm();
    let lines = [
        consider("security", "Dropping it hides a race the security Focus flagged."),
        consider("approach", "The test failed 6 times the same way: fixing beats dropping."),
        consider("approach", "second line from the same Watcher"),
        consider("ghost", "not on the roster"),
    ];
    let step = f.e.raise_blocked(&f.log, 7, &ask(), &ran, &lines, true).unwrap();
    assert_eq!(step.raised.as_deref(), Some("e1"));
    assert_eq!(step.dropped.len(), 2);
    let e = f.e.get("e1").unwrap();
    assert_eq!(e.cause, Cause::Blocked);
    assert_eq!(
        e.lines,
        vec![
            "Drop the flaky test or fix it?".to_owned(),
            "Watcher (approach): The test failed 6 times the same way: fixing beats dropping.".to_owned(),
            "Watcher (security): Dropping it hides a race the security Focus flagged.".to_owned(),
        ]
    );
    let again = f.e.raise_blocked(&f.log, 7, &ask(), &ran, &lines, true).unwrap();
    assert_eq!(again.raised, None);
    assert_eq!(f.ops("raised").len(), 1);

    // Companion: `Watcher (opinion)`; silence = the question alone.
    let companion =
        [Watcher { name: "companion".into(), focuses: vec!["approach".into(), "security".into()], model: None }];
    let step =
        f.e.raise_blocked(&f.log, 8, &ask(), &companion, &[consider("companion", "Fix it.\nmore")], false).unwrap();
    assert_eq!(f.e.get(step.raised.as_deref().unwrap()).unwrap().lines[1], "Watcher (opinion): Fix it.");
    let step = f.e.raise_blocked(&f.log, 9, &ask(), &companion, &[], false).unwrap();
    assert_eq!(f.e.get(step.raised.as_deref().unwrap()).unwrap().lines.len(), 1);
    f.assert_consistent();
}

/// V42: a failed blocked wake (no Watcher output taken) sends no bare notification.
#[test]
fn failed_blocked_wake_ticker_only() {
    let mut f = Fx::new();
    let step = f.e.raise_blocked(&f.log, 7, &ask(), &[], &[], true).unwrap();
    assert_eq!(step.raised, None);
    assert_eq!(step.ticker, vec![escalation::UNAVAILABLE.to_owned()]);
    assert!(f.ops("raised").is_empty());
}

#[test]
fn question_restated() {
    assert_eq!(escalation::question(&ask()), "Drop the flaky test or fix it?");
    let bash = json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf target\n&& cargo build"}});
    assert_eq!(escalation::question(&bash), "Allow Bash: rm -rf target && cargo build?");
    assert_eq!(escalation::question(&json!({"message": "Confirm deploy"})), "Confirm deploy");
    assert_eq!(escalation::question(&json!({})), "The Worker waits on you.");
}

/// V11: answered = any later hook record that wakes or shows activity.
#[test]
fn blocked_answered_by_later_hook() {
    let dir = TempDir::new().unwrap();
    let home = Home::new(dir.path().join(".pair-watcher"));
    home.ensure_layout().unwrap();
    let mb = Mailbox::register(&home, "s1").unwrap();
    mb.append_hook(&Hook { event: "PermissionRequest".into(), payload: ask() }).unwrap();
    let at = mb.read_hooks(0).unwrap().records[0].t;
    assert!(!escalation::blocked_answered(&mb, Harness::Claude, at).unwrap());
    mb.append_hook(&Hook { event: "SessionStart".into(), payload: json!({}) }).unwrap();
    assert!(!escalation::blocked_answered(&mb, Harness::Claude, at).unwrap(), "never wakes: not an answer");
    mb.append_hook(&Hook { event: "Stop".into(), payload: json!({}) }).unwrap();
    assert!(escalation::blocked_answered(&mb, Harness::Claude, at).unwrap());
}

/// I14: Herdr toast inside Herdr, else the desktop notifier; title and body as argv.
#[test]
fn notifier_detect_and_argv() {
    let herdr = Notifier::detect(&|k| (k == "HERDR_PANE_ID").then(|| "p1".to_owned()));
    assert_eq!(herdr, Notifier::Herdr("herdr".into()));
    let desktop = Notifier::detect(&|_| None);
    if cfg!(target_os = "macos") {
        assert_eq!(desktop, Notifier::Osascript("osascript".into()));
    }

    let dir = TempDir::new().unwrap();
    let bin = fake(dir.path(), 0);
    // Herdr 0.9.3 rejects `--` and `--body=` (exit 2, live 2026-10-09): title first, body as a value.
    Notifier::Herdr(bin.clone()).show("-T \"quoted\"", "-line 1\nline 2").unwrap();
    assert_eq!(calls(dir.path())[0], "notification\nshow\n-T \"quoted\"\n--body\n-line 1\nline 2\n--sound\nrequest\n");
    Notifier::Osascript(bin).show("T", "B").unwrap();
    assert!(calls(dir.path())[1].ends_with("end run\n--\nT\nB\n"), "{}", calls(dir.path())[1]);
    assert!(Notifier::None.show("T", "B").is_err());
}

/// Codex review: text starting with `-` never reads as a notifier option.
#[test]
fn dash_text_stays_text() {
    let dir = TempDir::new().unwrap();
    let bin = fake(dir.path(), 0);
    Notifier::NotifySend(bin).show("--icon=x", "-u critical").unwrap();
    assert_eq!(calls(dir.path())[0], "--\n--icon=x\n-u critical\n");
}

/// Codex review (V40): a crash after `sending` and before `notified` never sends again.
#[test]
fn cut_off_send_is_not_resent() {
    let mut f = Fx::new();
    let bin = fake(f.dir.path(), 0);
    f.debate_unresolved(true);
    f.raise_unresolved(true);
    f.log(escalation::Op::Sending { id: "e1".into(), via: "herdr".into() }.event());
    f.e = Escalations::rebuild(&f.events());
    assert_eq!(f.e.get("e1").unwrap().stage, Stage::Sending);
    assert!(f.deliver(&Notifier::NotifySend(bin), false).is_empty());
    assert!(calls(f.dir.path()).is_empty());
    assert!(f.ops("notified")[0].str("error").unwrap().starts_with("unconfirmed"));
    assert_eq!(f.ops("notified")[0].str("via"), Some("herdr"), "the notifier it went to, not today's");
    assert_eq!(f.e.get("e1").unwrap().stage, Stage::Notified);
    f.assert_consistent();
}

/// Codex review (I13): a long alternative never clips the evidence away.
#[test]
fn long_alternative_keeps_evidence() {
    let mut f = Fx::new();
    let mut c = concern();
    c.alternative = Some("a".repeat(300));
    f.log(Change::Update { watcher: "approach".into(), concern: c }.event());
    f.debate_unresolved(true);
    f.raise_unresolved(true);
    let line = &f.e.get("e1").unwrap().lines[0];
    let text = line.strip_prefix("Watcher (approach): ").unwrap();
    assert!(text.len() <= 100, "{}", text.len());
    assert!(text.contains(" — `cargo test` → 6 failed"), "{text}");
}
