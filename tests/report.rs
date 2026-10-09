//! §T18 `report`: H1–H4 over shown items (V95), n guard, inferred split and `--explicit-only`,
//! undone rulings, blocked line, availability vs coverage and audit recall (V97, V98), prune
//! candidates and `--export` (V99).

use std::fs;
use std::path::PathBuf;

use pair_watcher::audit::{Label, Op as AuditOp};
use pair_watcher::challenge::{Op as ChallengeOp, Origin};
use pair_watcher::concern::{Change, Concern, Resolution, Severity, State};
use pair_watcher::debate::{Op as DebateOp, Outcome as DebateOutcome};
use pair_watcher::escalation::{Cause, Op as EscalationOp};
use pair_watcher::event::{Event, EventLog, Kind};
use pair_watcher::home::Home;
use pair_watcher::human::{self, Act};
use pair_watcher::jsonl::Record;
use pair_watcher::report::{self, Filter, Session, Stats};
use pair_watcher::ruling::{self, Ruled, Side, Stage, Surface, Via};
use pair_watcher::status::{Coverage, Op, Wake};
use serde_json::json;
use tempfile::TempDir;

const MIN: u64 = 60_000;
/// 2026-10-01T00:00Z.
const DAY0: u64 = 1_790_812_800_000;

struct Log {
    t: u64,
    recs: Vec<Record<Event>>,
}

impl Log {
    fn new(mode: &str) -> Log {
        let mut l = Log { t: DAY0, recs: vec![] };
        l.push(
            Event::new(Kind::Session)
                .with("phase", "start")
                .with("repo", "/nowhere")
                .with("harness", "claude")
                .with("harness_version", "2.1.289")
                .with("mode", mode)
                .with("ceiling_usd", 5.0)
                .with("focuses", json!(["approach", "correctness", "mine"]))
                .with("version", "0.1.0"),
        );
        l.push(
            Op::Model {
                watcher: "companion".into(),
                model: "gpt-6.1-sol".into(),
                same_vendor: false,
                shares_quota: false,
            }
            .event(),
        );
        l
    }

    fn push(&mut self, e: Event) {
        self.t += 1000;
        self.recs.push(Record::at(self.t, e));
    }

    fn at(&mut self, t: u64, e: Event) {
        self.t = t;
        self.recs.push(Record::at(t, e));
    }

    fn concern(&mut self, id: &str, focus: &str, shown: bool) {
        self.push(Change::Add { watcher: "companion".into(), concern: concern(id, focus) }.event());
        if shown {
            self.push(human::shown(id));
        }
    }

    fn ruled(&mut self, id: &str, concern: Option<&str>, side: Side, via: Via) {
        let r = Ruled {
            id: id.into(),
            concern: concern.map(Into::into),
            challenge: None,
            focus: None,
            watcher: None,
            stage: Stage::Escalation,
            side,
            via,
            surface: if via == Via::Prompt { Surface::Worker } else { Surface::Pane },
            prompt_ref: None,
            ruling_sent: false,
            after_s: 10,
        };
        self.push(r.event());
    }

    fn session(self, id: &str) -> Session {
        Session { id: id.into(), dir: PathBuf::from(format!("/sessions/{id}")), records: self.recs }
    }
}

fn concern(id: &str, focus: &str) -> Concern {
    Concern {
        id: id.into(),
        focus: focus.into(),
        severity: Severity::Warn,
        state: State::Open,
        headline: format!("headline {id}"),
        detail: vec![],
        refs: vec![],
        challenge: false,
        evidence: vec![],
        alternative: None,
        verified: false,
        tag: None,
        debate: None,
    }
}

fn escalation(id: &str, cause: Cause, concern: Option<&str>) -> Vec<Event> {
    vec![
        EscalationOp::Raised {
            id: id.into(),
            cause,
            concern: concern.map(Into::into),
            challenge: None,
            hook: None,
            title: "t".into(),
            lines: vec![],
            rule: "r".into(),
        }
        .event(),
        EscalationOp::Notified { id: id.into(), via: "osascript".into(), error: None }.event(),
    ]
}

fn fold(sessions: &[Session], explicit_only: bool) -> Stats {
    Stats::fold(sessions, &Filter { explicit_only, ..Filter::default() })
}

/// V95: H1 and H2 over shown Concerns only; ratios print from n ≥ 20 per group; a Focus over 25%
/// noise is flagged.
#[test]
fn h1_h2_over_shown_concerns_with_n_guard() {
    let mut l = Log::new("companion");
    for i in 0..20 {
        l.concern(&format!("A{i}"), "approach", true);
    }
    for i in 0..6 {
        l.concern(&format!("C{i}"), "correctness", true);
    }
    l.concern("X", "approach", false); // never shown: in no ratio
    for id in ["A0", "A1", "A2", "X"] {
        l.push(Act::Dismiss { id: id.into() }.event());
    }
    l.push(Act::Mute { focus: "correctness".into() }.event());
    l.push(Act::Helpful { id: "A3".into() }.event());
    l.push(Act::Promote { id: "A4".into(), remark: None }.event());
    let stats = fold(&[l.session("s1")], false);

    let all = stats.total();
    assert_eq!((all.h1.num, all.h1.den), (9, 26), "3 dismissed + 6 muted of 26 shown");
    assert_eq!((all.h2.num, all.h2.den), (2, 26));
    let by_focus = stats.by(|d| &d.focus);
    assert_eq!(by_focus["approach"].h1.value(), Some(0.15));
    assert_eq!(by_focus["correctness"].h1.value(), None, "n<20");

    let lines = report::render(&stats, &[], &Filter::default());
    let line = |p: &str| lines.iter().find(|l| l.trim_start().starts_with(p)).cloned().unwrap_or_default();
    assert!(line("all").contains("H1 noise 0.35 (n 26)"), "{lines:#?}");
    assert!(line("correctness").contains("H1 noise n<20"));
    assert!(!line("approach").contains('⚑'));
}

/// V95: a Focus over 25% noise over ≥20 shown is flagged (flag only).
#[test]
fn noisy_focus_is_flagged() {
    let mut l = Log::new("companion");
    for i in 0..20 {
        l.concern(&format!("A{i}"), "approach", true);
        if i < 6 {
            l.push(Act::Dismiss { id: format!("A{i}") }.event());
        }
    }
    let stats = fold(&[l.session("s1")], false);
    let lines = report::render(&stats, &[], &Filter::default());
    assert!(lines.iter().any(|l| l.contains("approach") && l.contains("0.30 (n 20) ⚑ over 25%")), "{lines:#?}");
}

/// V95: sessions with no pane (nothing ever shown) add nothing to ratios.
#[test]
fn paneless_session_adds_nothing() {
    let mut l = Log::new("companion");
    for i in 0..30 {
        l.concern(&format!("A{i}"), "approach", false);
        l.push(Act::Dismiss { id: format!("A{i}") }.event());
    }
    let stats = fold(&[l.session("s1")], false);
    assert_eq!((stats.sessions, stats.paneless), (1, 1));
    assert_eq!(stats.total().h1.den, 0);
}

/// V95: H3 = side ∈ {watcher, worker} ÷ notified unresolved Escalations − closed-stale; `d`
/// counts as not acted; inferred shown as `(k inferred)`, dropped by `--explicit-only`; an
/// undone ruling never counts. Blocked Escalations: own line.
#[test]
fn h3_escalation_precision_inferred_and_undone() {
    let mut l = Log::new("companion");
    for i in 0..21 {
        let c = format!("A{i}");
        l.concern(&c, "approach", true);
        for e in escalation(&format!("E{i}"), Cause::Unresolved, Some(&c)) {
            l.push(e);
        }
    }
    l.push(Event::new(Kind::ClosedStale).with("id", "E20").with("concern", "A20"));
    for i in 0..10 {
        l.ruled(&format!("E{i}"), Some(&format!("A{i}")), Side::Watcher, Via::Key);
    }
    l.ruled("E10", Some("A10"), Side::Worker, Via::Prompt);
    l.ruled("E11", Some("A11"), Side::Worker, Via::Prompt);
    l.ruled("E12", Some("A12"), Side::Dismissed, Via::Key);
    l.ruled("E13", Some("A13"), Side::Watcher, Via::Key);
    l.push(
        ruling::Op::Undone {
            ruled: "E13".into(),
            nth: 0,
            concern: Some("A13".into()),
            via: Via::Key,
            surface: Surface::Pane,
            delivered: false,
        }
        .event(),
    );
    for (i, e) in ["B0", "B1", "B2"].iter().enumerate() {
        for ev in escalation(e, Cause::Blocked, None) {
            l.push(ev);
        }
        if i == 0 {
            l.push(Act::Helpful { id: (*e).into() }.event());
        }
    }
    l.ruled("B1", None, Side::Dismissed, Via::Key);
    let s = l.session("s1");

    let h3 = fold(std::slice::from_ref(&s), false).total().h3;
    assert_eq!((h3.num, h3.den, h3.inferred), (12, 20, 2));
    let explicit = fold(std::slice::from_ref(&s), true).total().h3;
    assert_eq!((explicit.num, explicit.den), (10, 20));
    // H2 counts side=watcher on the Concern: 10 explicit (the undone one does not).
    assert_eq!(fold(std::slice::from_ref(&s), false).total().h2.num, 10);

    let stats = fold(&[s], false);
    assert_eq!((stats.blocked.shown, stats.blocked.helpful, stats.blocked.dismissed), (3, 1, 1));
    let lines = report::render(&stats, &[], &Filter::default());
    assert!(lines.iter().any(|l| l.contains("H3 Escalation 0.60 (n 20, 2 inferred)")), "{lines:#?}");
    assert!(lines.iter().any(|l| l.starts_with("blocked Escalations: 3 shown · 1 helpful")));
}

/// V95: H4 = (worker conceded and later resolved fixed, or side=watcher) ÷ Challenges sent, beside
/// the watcher-concedes rate.
#[test]
fn h4_challenge_precision() {
    let mut l = Log::new("companion");
    for i in 0..20 {
        let (c, ch) = (format!("A{i}"), format!("K{i}"));
        l.concern(&c, "approach", true);
        l.push(
            ChallengeOp::Queued {
                id: ch.clone(),
                concern: c.clone(),
                focus: "approach".into(),
                origin: Origin::Auto,
                remark: None,
            }
            .event(),
        );
        l.push(ChallengeOp::Sent { id: ch.clone(), route: "stop".into() }.event());
        let outcome = match i {
            0..8 => DebateOutcome::WorkerConcedes,
            8..12 => DebateOutcome::SidedWatcher,
            12..18 => DebateOutcome::WatcherConcedes,
            _ => DebateOutcome::Unresolved,
        };
        l.push(
            DebateOp::End { challenge: ch.clone(), concern: c.clone(), outcome, escalated: None, withdrawn: None }
                .event(),
        );
        if outcome == DebateOutcome::SidedWatcher {
            l.ruled(&ch, Some(&c), Side::Watcher, Via::Key);
        }
        if i < 5 {
            l.push(Change::Resolve { watcher: "companion".into(), id: c, reason: Resolution::Fixed }.event());
        }
    }
    // Queued, never sent: not in H4.
    l.concern("Q", "approach", true);
    l.push(
        ChallengeOp::Queued {
            id: "KQ".into(),
            concern: "Q".into(),
            focus: "approach".into(),
            origin: Origin::Promoted,
            remark: None,
        }
        .event(),
    );
    let h = fold(&[l.session("s1")], false).total();
    assert_eq!((h.h4.num, h.h4.den), (9, 20), "5 conceded + fixed, 4 sided with the Watcher");
    assert_eq!((h.watcher_concedes.num, h.watcher_concedes.den), (6, 20));
}

/// V98: availability (watched / lagging / blind / paused / paused-by-user) split from review
/// coverage (on time / reviewed late / lost); lost spans make silence not certifiable.
#[test]
fn availability_and_coverage() {
    let mut l = Log::new("companion");
    let t0 = l.t;
    l.at(t0 + 10 * MIN, Op::Lagging { focus: None }.event());
    l.at(t0 + 15 * MIN, Op::LaggingEnd { focus: None }.event());
    l.at(t0 + 20 * MIN, Op::Blind { focus: None, reason: "x".into() }.event());
    l.at(t0 + 22 * MIN, Op::BlindEnd { focus: None }.event());
    l.at(t0 + 30 * MIN, Op::UserPause.event());
    l.at(t0 + 40 * MIN, Op::UserResume.event());
    l.at(t0 + 50 * MIN, Wake { n: 1, watcher: "companion".into(), spend: 5.0, ..Wake::default() }.event());
    l.at(t0 + 55 * MIN, Op::Raise.event());
    l.at(t0 + 56 * MIN, Coverage::Delayed { from: t0, to: t0 + 4 * MIN }.event());
    l.at(t0 + 57 * MIN, Coverage::Lost { from: t0 + 5 * MIN, to: t0 + 6 * MIN, cause: "poison-skip".into() }.event());
    l.at(t0 + 60 * MIN, Event::new(Kind::Session).with("phase", "end"));
    let stats = fold(&[l.session("s1")], false);
    let a = stats.availability;
    assert_eq!(
        (a.lagging, a.blind, a.paused_by_user, a.paused, a.watched),
        (5 * MIN, 2 * MIN, 10 * MIN, 5 * MIN, 38 * MIN + 1000)
    );
    let c = stats.coverage;
    assert_eq!((c.late, c.lost, c.on_time), (4 * MIN, MIN, 55 * MIN + 1000));
    assert_eq!(stats.with_lost, 1);
    let lines = report::render(&stats, &[], &Filter::default());
    assert!(lines.iter().any(|l| l.starts_with("silence not certifiable: 1 sessions")));
}

/// V97: recall per Focus from human labels only: already said ÷ (already said + miss).
#[test]
fn audit_labels_give_recall() {
    let mut l = Log::new("companion");
    for i in 0..25 {
        let id = format!("F{i}");
        l.push(
            AuditOp::Finding {
                id: id.clone(),
                wake: 1,
                watcher: "companion".into(),
                focus: "approach".into(),
                headline: "h".into(),
                detail: vec![],
            }
            .event(),
        );
        let label = match i {
            0..16 => Label::AlreadySaid,
            16..20 => Label::Miss,
            _ => Label::NotAMiss,
        };
        if i != 24 {
            l.push(AuditOp::Label { finding: id, label }.event());
        }
    }
    let stats = fold(&[l.session("s1")], false);
    assert_eq!(stats.audit["approach"], (16, 4, 4));
    let lines = report::render(&stats, &[], &Filter::default());
    assert!(lines.iter().any(|l| l.contains("approach") && l.contains("0.80 (n 20) · 4 not a miss")), "{lines:#?}");
}

fn write(home: &Home, id: &str, l: Log) {
    let dir = home.sessions_dir().join(id);
    fs::create_dir_all(&dir).unwrap();
    let log = EventLog::new(dir.join(pair_watcher::mailbox::LOG_FILE));
    for r in l.recs {
        pair_watcher::jsonl::append(log.path(), &r).unwrap();
    }
}

/// V98: `report` scans `sessions/*/` filtering on the `session` start date; uncited Standing
/// rulings (30 days) are prune candidates. V99: `--export` = aggregate + only cited rulings.
#[test]
fn report_cmd_range_prune_and_export() {
    let dir = TempDir::new().unwrap();
    let home = Home::new(dir.path().join("home"));
    home.ensure_layout().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let key = pair_watcher::standing::key(&repo);
    fs::write(
        pair_watcher::standing::path(&home, &key),
        r#"
last_id = 2
[[ruling]]
id = "S1"
focus = "approach"
headline = "keep the retry loop"
verdict = "worker"
date = "2026-08-01"
[[ruling]]
id = "S2"
focus = "security"
headline = "env file is fine"
verdict = "dismissed"
date = "2026-08-01"
"#,
    )
    .unwrap();

    let mut old = Log::new("companion");
    old.recs[0].body.fields.insert("repo".into(), json!(repo.to_string_lossy()));
    old.concern("A0", "approach", true);
    write(&home, "old", old);
    let mut new = Log::new("swarm");
    new.recs[0].body.fields.insert("repo".into(), json!(repo.to_string_lossy()));
    for r in &mut new.recs {
        r.t += 5 * 86_400_000;
    }
    new.t += 5 * 86_400_000;
    new.push(Wake { n: 1, watcher: "companion".into(), rulings: vec!["S1".into()], ..Wake::default() }.event());
    write(&home, "new", new);

    let now = DAY0 + 6 * 86_400_000;
    let filter = Filter { since: Some("2026-10-03".into()), ..Filter::default() };
    let lines = report::report_cmd(&home, &filter, false, now).unwrap();
    assert!(lines[0].contains("2026-10-03 … today · 1 sessions"), "{lines:#?}");
    let prune: Vec<&String> = lines.iter().filter(|l| l.contains(" S")).collect();
    assert_eq!(prune.len(), 1, "{lines:#?}");
    assert!(prune[0].contains("S2 [security] env file is fine"));

    let json: serde_json::Value =
        serde_json::from_str(&report::report_cmd(&home, &Filter::default(), true, now).unwrap()[0]).unwrap();
    assert_eq!(json["sessions"], 2);
    assert_eq!(json["rulings"].as_array().unwrap().len(), 1, "only the cited ruling");
    assert_eq!(json["rulings"][0]["id"], "S1");
    assert_eq!(json["groups"]["mode"]["companion"]["h1_noise"]["n"], 1);
    assert_eq!(json["groups"]["mode"]["companion"]["h1_noise"]["ratio"], serde_json::Value::Null);

    assert!(report::check_date("since", "2026-10-03").is_ok());
    assert!(report::check_date("since", "2026-1-3").is_err());
}
