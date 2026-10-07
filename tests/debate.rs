use std::sync::atomic::{AtomicUsize, Ordering};

use pair_watcher::arbiter::Watcher;
use pair_watcher::challenge::Slot;
use pair_watcher::challenge::{Challenges, Op as ChallengeOp, Origin};
use pair_watcher::concern::{self, Change, Concern, ConcernList, Resolution, Severity, State, Tag};
use pair_watcher::debate::{self, Debate, EscalateAt, Outcome, Phase, Verdict, Withdrawal};
use pair_watcher::event::{Event, EventLog, Kind};
use pair_watcher::history;
use pair_watcher::outbox::{Gate, Item, ItemKind, Outbox};
use pair_watcher::output::{Decision, Line};
use pair_watcher::tailer::{Entry, Item as T};
use tempfile::TempDir;

const OWN: &str = "[pair-watcher · Challenge c1 · from the Watcher, not the human]\nConcern: retry loop";

struct Fx {
    _dir: TempDir,
    log: EventLog,
    outbox: Outbox,
    d: Debate,
}

/// Concern `c1` (`approach`, `warn`) challenged as `c1`, delivered: the Debate is open.
fn fx() -> Fx {
    fx_with(Severity::Warn)
}

fn fx_with(severity: Severity) -> Fx {
    fx_route(severity, "mod")
}

fn fx_route(severity: Severity, route: &str) -> Fx {
    let dir = TempDir::new().unwrap();
    let log = EventLog::new(dir.path().join("log.jsonl"));
    let outbox = Outbox::new(dir.path().join("outbox.jsonl"));
    let mut f = Fx { _dir: dir, log, outbox, d: Debate::default() };
    f.log(Change::Add { watcher: "approach".into(), concern: concern(severity) }.event());
    f.log(
        ChallengeOp::Queued {
            id: "c1".into(),
            concern: "c1".into(),
            focus: "approach".into(),
            origin: Origin::Auto,
            remark: None,
        }
        .event(),
    );
    f.log(ChallengeOp::Sent { id: "c1".into(), route: route.into() }.event());
    assert!(f.d.open().is_some(), "challenge sent opens the Debate");
    f
}

fn concern(severity: Severity) -> Concern {
    Concern {
        id: "c1".into(),
        focus: "approach".into(),
        severity,
        state: State::Open,
        headline: "retry loop never converges".into(),
        detail: vec![],
        refs: vec![],
        challenge: true,
        evidence: vec![],
        alternative: Some("stop and bisect".into()),
        verified: true,
        tag: None,
    }
}

fn swarm() -> Vec<Watcher> {
    ["approach", "correctness"]
        .iter()
        .map(|f| Watcher { name: (*f).into(), focuses: vec![(*f).into()], model: None })
        .collect()
}

fn line(decision: Decision, text: &str) -> Line {
    Line::Debate { decision, text: text.into() }
}

/// A Worker turn: optional prompt, then assistant text, then the turn end.
fn turn(prompt: Option<&str>, text: &str) -> Vec<Entry> {
    let mut items: Vec<T> = prompt.map(|p| T::Prompt(p.into())).into_iter().collect();
    items.push(T::Text(text.into()));
    items.push(T::TurnEnd);
    static N: AtomicUsize = AtomicUsize::new(0);
    vec![Entry { id: format!("e{}", N.fetch_add(1, Ordering::Relaxed)), t: 0, items }]
}

impl Fx {
    fn log(&mut self, e: Event) {
        self.log.append(e.clone()).unwrap();
        self.d.apply(&e);
    }

    fn events(&self) -> Vec<Event> {
        self.log.read_from(0).unwrap().records.into_iter().map(|r| r.body).collect()
    }

    fn list(&self) -> ConcernList {
        ConcernList::rebuild(&self.events())
    }

    fn observe(&mut self, entries: &[Entry]) {
        self.d.observe(&self.log, &self.outbox, entries, 1).unwrap();
    }

    fn decide(&mut self, lines: &[(&str, Line)]) -> debate::Step {
        self.decide_at(lines, EscalateAt::Warn)
    }

    fn decide_at(&mut self, lines: &[(&str, Line)], gate: EscalateAt) -> debate::Step {
        let lines: Vec<(String, Line)> = lines.iter().map(|(w, l)| ((*w).to_owned(), l.clone())).collect();
        let list = self.list();
        self.d.decide(&self.log, &self.outbox, &list, &swarm(), &lines, gate, 1).unwrap()
    }

    /// A deliverer claims exchange 2 through the Debate's gate and acks it; the arbiter logs it.
    fn deliver_second(&mut self) {
        let claim = self.outbox.claim("mod", 2, |i| self.d.gate(i).unwrap_or(Gate::Skip)).unwrap().unwrap();
        assert_eq!(claim.item.id, "c1/2");
        assert!(self.outbox.ack(&claim, 2).unwrap());
        assert_eq!(self.d.sync_sent(&self.log, &self.outbox).unwrap().as_deref(), Some("mod"));
        assert_eq!(self.d.open().unwrap().phase, Phase::Awaiting);
    }

    fn ops(&self, op: &str) -> Vec<Event> {
        self.events().into_iter().filter(|e| e.k == Kind::Debate && e.str("op") == Some(op)).collect()
    }

    fn end(&self) -> Event {
        let ends = self.ops("end");
        assert_eq!(ends.len(), 1, "one end per Challenge (V94)");
        ends[0].clone()
    }

    /// V93: the log folds to the same Debate; the slot is free once it ends (V19).
    fn assert_consistent(&self) {
        let events = self.events();
        assert_eq!(Debate::rebuild(&events), self.d);
        if self.d.open().is_none() {
            assert_eq!(Challenges::rebuild(&events).slot(), None);
        }
    }
}

/// V30: the last line matching `Challenge <id>:`; other ids, echoed templates and look-alikes are
/// not verdicts; markup and case do not matter.
#[test]
fn verdict_line_parse() {
    let v = |t: &str| debate::verdict(t, "c1");
    assert_eq!(v("ok\nChallenge c1: defend — the task says so"), Some((Verdict::Defend, "the task says so".into())));
    assert_eq!(
        v("Challenge c1: defend — first\nmore\nChallenge c1: concede - you are right\ntrailing"),
        Some((Verdict::Concede, "you are right".into())),
        "last matching line wins"
    );
    assert_eq!(v("**Challenge C1:** `Defends`: it is cached"), Some((Verdict::Defend, "it is cached".into())));
    assert_eq!(v("> Challenge c1: conceded"), Some((Verdict::Concede, String::new())));
    assert_eq!(v("Challenge c1: concede | defend — <one-line reason>"), None, "template echo");
    assert_eq!(v("Challenge c10: concede — other id"), None);
    assert_eq!(v("Challenge c1-2: concede — other id"), None);
    assert_eq!(v("XChallenge c1: concede"), None);
    assert_eq!(v("Challenge c1: maybe later"), None);
    assert_eq!(v("Challenge c1 concede"), None);
    assert_eq!(
        v("Challenge c1: defend — ok\nChallenge c2: concede — not ours"),
        Some((Verdict::Defend, "ok".into())),
        "other ids ignored"
    );
    assert_eq!(debate::verdict("Challenge c1-2: defend — x", "c1-2"), Some((Verdict::Defend, "x".into())));
    assert_eq!(debate::verdict("Challenge İ: defend", "c1"), None, "non-ASCII never panics");
}

/// V30, V35: "concede" ends it at once: worker concedes, Concern stays open `conceded`; nothing
/// goes back to the Worker (V37).
#[test]
fn worker_concedes_by_verdict() {
    let mut f = fx();
    f.observe(&turn(Some(OWN), "Fair.\nChallenge c1: concede — switching to bisect"));
    assert_eq!(f.end().str("outcome"), Some("worker-concedes"));
    let c = f.list().get("c1").unwrap().clone();
    assert_eq!((c.state, c.tag), (State::Open, Some(Tag::Conceded)), "resolves only on an observed fix");
    assert!(f.outbox.pending().unwrap().is_empty());
    f.assert_consistent();

    // The fix observed later resolves it the normal way.
    f.log(Change::Resolve { watcher: "approach".into(), id: "c1".into(), reason: Resolution::Fixed }.event());
    assert_eq!(f.list().get("c1").unwrap().state, State::Resolved);
    assert_eq!(f.ops("end").len(), 1);
}

/// V30, V100: actions override words: "defend" + a switch of approach = worker concedes.
#[test]
fn defend_with_a_switch_is_worker_concedes() {
    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — polling is fine"));
    assert!(f.d.owes());
    let step = f.decide(&[("approach", line(Decision::Switched, "it replaced polling with a webhook"))]);
    assert_eq!(step.ended, Some(Outcome::WorkerConcedes));
    assert_eq!(f.list().get("c1").unwrap().tag, Some(Tag::Conceded));
    f.assert_consistent();
}

/// V31, I11: defence → one rebuttal with new ground (quotes the reason, `(2/2)`, no steelman);
/// second defence → the Watcher's last word: concede or escalate, never a third message.
#[test]
fn rebuttal_then_last_word() {
    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — the API is idempotent"));
    let step = f.decide(&[("approach", line(Decision::Rebut, "the 429s in the log show it is rate limited"))]);
    assert_eq!(step.ended, None);
    let pending = f.outbox.pending().unwrap();
    assert_eq!(pending.len(), 1);
    let text = &pending[0].text;
    assert!(text.starts_with("[pair-watcher · Challenge c1 (2/2) · from the Watcher, not the human]"), "{text}");
    assert!(text.contains("You defended: \"the API is idempotent\""), "{text}");
    assert!(text.contains("New ground: the 429s in the log show it is rate limited"), "{text}");
    assert!(!text.contains("strongest case"), "no steelman in exchange 2");
    assert_eq!(f.d.waiting(), Some("c1/2"));
    f.deliver_second();

    f.observe(&turn(Some(text), "Challenge c1: defend — limits reset hourly"));
    assert_eq!(f.d.open().unwrap().n, 2);
    // A third message is never allowed: a rebuttal now is a (defaulted) concession.
    let step = f.decide(&[("approach", line(Decision::Rebut, "one more thing"))]);
    assert_eq!(step.ended, Some(Outcome::WatcherConcedes));
    assert_eq!(step.dropped, vec!["approach: rebut not allowed at exchange 2".to_owned()]);
    let decision = &f.ops("decision")[1];
    assert_eq!(decision.fields["defaulted"], true);
    assert_eq!(f.list().get("c1").unwrap().state, State::Resolved, "watcher concedes → resolved");
    assert_eq!(f.outbox.pending().unwrap().len(), 0, "nothing more to the Worker");
    f.assert_consistent();
}

/// V31, V36: escalate after the second defence → unresolved, escalated at `warn`.
#[test]
fn second_defence_escalates() {
    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — idempotent"));
    f.decide(&[("approach", line(Decision::Rebut, "429s"))]);
    f.deliver_second();
    f.observe(&turn(None, "Challenge c1: defend — resets hourly"));
    let step = f.decide(&[("approach", line(Decision::Escalate, "real trade-off: latency vs quota"))]);
    assert_eq!(step.ended, Some(Outcome::Unresolved));
    assert!(step.ticker.is_empty());
    let end = f.end();
    assert_eq!(end.fields["escalated"], true);
    let c = f.list().get("c1").unwrap().clone();
    assert_eq!((c.state, c.tag), (State::Open, Some(Tag::Escalated)));
    assert_eq!(f.d.open(), None);
    f.assert_consistent();
}

/// V31: after the first defence the Watcher may concede or escalate at once.
#[test]
fn first_defence_concede_or_escalate() {
    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — the human asked for polling"));
    let step = f.decide(&[("approach", line(Decision::Concede, "human instruction verified"))]);
    assert_eq!(step.ended, Some(Outcome::WatcherConcedes));
    assert_eq!(f.list().get("c1").unwrap().state, State::Resolved);

    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — trade-off"));
    assert_eq!(f.decide(&[("approach", line(Decision::Escalate, "x"))]).ended, Some(Outcome::Unresolved));
    assert!(f.outbox.pending().unwrap().is_empty());
}

/// V33, I11: no verdict line → re-ask; still nothing → unresolved. A `note` Concern under the
/// default gate stays quiet (V36).
#[test]
fn no_answer_reask_then_unresolved_quiet() {
    let mut f = fx_with(Severity::Note);
    f.observe(&turn(Some(OWN), "I'll keep going."));
    let ex = &f.ops("exchange")[0];
    assert_eq!((ex.str("verdict"), &ex.fields["n"]), (Some("none"), &serde_json::json!(1)));
    let step = f.decide(&[]);
    assert_eq!(step.ended, None);
    let text = f.outbox.pending().unwrap()[0].text.clone();
    assert!(text.contains("(2/2)") && text.contains("You did not answer; the verdict line is required."), "{text}");
    f.deliver_second();

    f.observe(&turn(Some(&text), "Still going."));
    let step = f.decide(&[]);
    assert_eq!(step.ended, Some(Outcome::Unresolved));
    assert_eq!(step.ticker, vec!["Challenge c1: unresolved, not escalated".to_owned()]);
    assert_eq!(f.end().fields["escalated"], false);
    assert_eq!(f.list().get("c1").unwrap().tag, Some(Tag::UnresolvedQuiet));
    f.assert_consistent();
}

/// V33, V100: actions can settle a missing verdict; a move not allowed after no answer
/// (`escalate` before the re-ask) is a defaulted concession.
#[test]
fn no_answer_settled_by_actions_or_defaulted() {
    let mut f = fx();
    f.observe(&turn(None, "Switching to bisect now."));
    assert_eq!(f.decide(&[("approach", line(Decision::Switched, "bisecting"))]).ended, Some(Outcome::WorkerConcedes));

    let mut f = fx();
    f.observe(&turn(None, "no verdict"));
    let step = f.decide(&[("approach", line(Decision::Escalate, "x"))]);
    assert_eq!(step.ended, Some(Outcome::WatcherConcedes));
    assert!(f.outbox.pending().unwrap().is_empty(), "no re-ask after the Debate closed");
}

/// V34: a human prompt mid-Debate goes first: its turn is evidence only, never "no answer".
/// Approach switch = worker concedes.
#[test]
fn human_turn_is_evidence_only() {
    let mut f = fx();
    f.observe(&turn(Some("actually, use the queue instead"), "Sure, switching to the queue."));
    assert!(f.ops("exchange").is_empty(), "never an exchange");
    assert_eq!(f.ops("human-turn").len(), 1);
    assert_eq!(f.d.open().unwrap().phase, Phase::Awaiting);
    // The reply was pre-empted: withdrawn, not adjudicated; the Concern stays open (V34).
    let list = f.list();
    assert!(f.d.render(&list).unwrap().ends_with("unless its actions changed approach (switched)."));
    assert_eq!(f.decide(&[("approach", line(Decision::Rebut, "x"))]).ended, Some(Outcome::Withdrawn));
    assert_eq!(f.end().str("withdrawn"), Some("human-went-first"));
    assert_eq!(f.list().get("c1").unwrap().state, State::Open);
    f.assert_consistent();

    // Human turn whose actions switch approach.
    let mut f = fx();
    f.observe(&turn(Some("drop the retry loop"), "Removed it."));
    let step = f.decide(&[("approach", line(Decision::Switched, "retry loop removed"))]);
    assert_eq!(step.ended, Some(Outcome::WorkerConcedes));
    f.assert_consistent();
}

/// V34, V37, V100: a pending rebuttal waits through a human turn; when that turn makes the Concern
/// no longer apply, the Debate is withdrawn and the rebuttal never goes out.
#[test]
fn pending_rebuttal_waits_and_is_stale_checked() {
    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — fine"));
    f.decide(&[("approach", line(Decision::Rebut, "new ground"))]);
    // The human prompts first; the human prompt may land in a checkpoint wake before its turn end.
    f.observe(&[Entry { id: "h".into(), t: 0, items: vec![T::Prompt("rewrite it as a stream".into())] }]);
    assert!(f.d.open().unwrap().human);
    f.observe(&[Entry { id: "h2".into(), t: 0, items: vec![T::Text("Rewritten.".into()), T::TurnEnd] }]);
    assert_eq!(f.ops("human-turn").len(), 1);
    assert_eq!(f.d.waiting(), Some("c1/2"), "still waits for the next idle");
    assert_eq!(f.decide(&[]).ended, None);

    f.log(Change::Resolve { watcher: "approach".into(), id: "c1".into(), reason: Resolution::NoLongerApplies }.event());
    let step = f.decide(&[]);
    assert_eq!(step.ended, Some(Outcome::Withdrawn));
    assert_eq!(f.end().str("withdrawn"), Some("no-longer-applies"));
    assert!(f.outbox.pending().unwrap().is_empty(), "rebuttal withdrawn");
    f.assert_consistent();
}

/// V37: an exchange-2 item whose Debate ended is dropped at claim, even one the end could not
/// withdraw (under a live claim).
#[test]
fn ended_debate_item_dropped_at_claim() {
    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — fine"));
    f.decide(&[("approach", line(Decision::Rebut, "new ground"))]);
    let claim = f.outbox.claim("mod", 1, |i| f.d.gate(i).unwrap()).unwrap().unwrap();
    f.d.withdraw(&f.log, &f.outbox, Withdrawal::Budget, 1).unwrap();
    assert_eq!(f.outbox.pending().unwrap().len(), 1, "claimed: the end could not withdraw it");
    assert!(f.outbox.release(&claim, 1).unwrap());
    let item = Item { id: "c1/2".into(), kind: ItemKind::Challenge, text: String::new() };
    assert_eq!(f.d.gate(&item), Some(Gate::Drop));
    assert_eq!(f.outbox.claim("mod", 1, |i| f.d.gate(i).unwrap()).unwrap(), None);
    assert!(f.outbox.pending().unwrap().is_empty());
    // Not this module's: a Challenge or a Ruling item.
    assert_eq!(f.d.gate(&Item { id: "c2".into(), kind: ItemKind::Challenge, text: String::new() }), None);
    assert_eq!(f.d.gate(&Item { id: "r/2".into(), kind: ItemKind::Ruling, text: String::new() }), None);
}

/// V35, V48: budget and session end withdraw the Debate, not adjudicated; the Concern stays as it
/// was.
#[test]
fn budget_and_session_end_withdraw() {
    for why in [Withdrawal::Budget, Withdrawal::SessionEnd] {
        let mut f = fx();
        f.observe(&turn(None, "Challenge c1: defend — fine"));
        assert!(f.d.withdraw(&f.log, &f.outbox, why, 1).unwrap());
        let end = f.end();
        assert_eq!(end.str("outcome"), Some("withdrawn"));
        let c = f.list().get("c1").unwrap().clone();
        assert_eq!((c.state, c.tag), (State::Open, None));
        assert!(!f.d.withdraw(&f.log, &f.outbox, why, 1).unwrap(), "nothing open");
        f.assert_consistent();
    }
}

/// V36: `note` escalates every severity, `off` none; unknown config = `warn`.
#[test]
fn escalate_unresolved_gate() {
    assert!(EscalateAt::Warn.admits(Severity::Warn) && !EscalateAt::Warn.admits(Severity::Note));
    assert!(EscalateAt::Note.admits(Severity::Note) && EscalateAt::Note.admits(Severity::Warn));
    assert!(!EscalateAt::Off.admits(Severity::Warn));
    assert_eq!(EscalateAt::from_config("note"), EscalateAt::Note);
    assert_eq!(EscalateAt::from_config("off"), EscalateAt::Off);
    assert_eq!(EscalateAt::from_config("bogus"), EscalateAt::Warn);

    for (gate, escalated) in [(EscalateAt::Note, true), (EscalateAt::Off, false)] {
        let mut f = fx_with(Severity::Note);
        f.observe(&turn(None, "Challenge c1: defend — x"));
        let step = f.decide_at(&[("approach", line(Decision::Escalate, "trade-off"))], gate);
        assert_eq!(f.end().fields["escalated"], escalated);
        assert_eq!(step.ticker.is_empty(), escalated);
    }
}

/// V38: the Worker blocked mid-Debate: no turn end, so the Debate waits.
#[test]
fn blocked_worker_debate_waits() {
    let mut f = fx();
    let before = f.d.clone();
    let tool = T::ToolCall { id: "t1".into(), name: "Bash".into(), input: "{}".into() };
    f.observe(&[Entry { id: "b".into(), t: 0, items: vec![T::Text("Running it.".into()), tool] }]);
    assert_eq!(f.d, before);
    assert_eq!(f.decide(&[]).ended, None);
}

/// V39: the owner Watcher argues the whole Debate; others cannot join; one move per wake.
#[test]
fn swarm_owner_only() {
    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — fine"));
    let step = f.decide(&[
        ("correctness", line(Decision::Concede, "agree with the Worker")),
        ("approach", line(Decision::Escalate, "trade-off")),
        ("approach", line(Decision::Concede, "second thoughts")),
    ]);
    assert_eq!(step.ended, Some(Outcome::Unresolved));
    assert_eq!(
        step.dropped,
        vec![
            "correctness: Debate on c1 is Focus approach's".to_owned(),
            "approach: one Debate move per wake".to_owned()
        ]
    );
    assert_eq!(f.ops("decision")[0].str("watcher"), Some("approach"));

    // Companion holds every Focus: it is the owner.
    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — fine"));
    let companion = [Watcher { name: "companion".into(), focuses: vec!["approach".into()], model: None }];
    let lines = [("companion".to_owned(), line(Decision::Concede, "ok"))];
    let list = f.list();
    let step = f.d.decide(&f.log, &f.outbox, &list, &companion, &lines, EscalateAt::Warn, 1).unwrap();
    assert_eq!(step.ended, Some(Outcome::WatcherConcedes));
}

/// V100: the challenged Concern closed mid-Debate ends it: fixed = worker concedes; no longer
/// applies, duplicate or human dismiss = withdrawn. An owed move missing = defaulted concession.
#[test]
fn concern_closed_mid_debate_and_owed_missing() {
    let cases: [(Event, Outcome, Option<&str>); 4] = [
        (
            Change::Resolve { watcher: "approach".into(), id: "c1".into(), reason: Resolution::Fixed }.event(),
            Outcome::WorkerConcedes,
            None,
        ),
        (
            Change::Resolve { watcher: "approach".into(), id: "c1".into(), reason: Resolution::NoLongerApplies }
                .event(),
            Outcome::Withdrawn,
            Some("no-longer-applies"),
        ),
        (
            Change::Duplicate { watcher: "approach".into(), id: "c1".into(), of: "c0".into() }.event(),
            Outcome::Withdrawn,
            Some("duplicate"),
        ),
        (concern::dismissal("c1"), Outcome::Withdrawn, Some("dismissed")),
    ];
    for (event, outcome, why) in cases {
        let mut f = fx();
        f.observe(&turn(None, "Challenge c1: defend — fine"));
        f.log(event);
        let step = f.decide(&[("approach", line(Decision::Escalate, "x"))]);
        assert_eq!(step.ended, Some(outcome));
        assert_eq!(f.end().str("withdrawn"), why);
        f.assert_consistent();
    }

    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — fine"));
    let step = f.decide(&[]);
    assert_eq!(step.ended, Some(Outcome::WatcherConcedes));
    assert_eq!(f.ops("decision")[0].fields["defaulted"], true);
}

/// Moves with no reply owed change nothing; `concede` / `switched` end it at any point.
#[test]
fn moves_while_awaiting() {
    let mut f = fx();
    let step = f.decide(&[("approach", line(Decision::Rebut, "early"))]);
    assert_eq!((step.ended, step.dropped), (None, vec!["approach: rebut with no reply owed".to_owned()]));
    assert_eq!(f.decide(&[("approach", line(Decision::Concede, "I was wrong"))]).ended, Some(Outcome::WatcherConcedes));
    let step = f.decide(&[("approach", line(Decision::Concede, "late"))]);
    assert_eq!(step.dropped, vec!["approach: no open Debate".to_owned()]);
}

/// A turn that ended before delivery is never read as the reply: `observe` runs before
/// `sync_sent`, and a Debate opens only on `challenge sent`.
#[test]
fn no_debate_before_sent() {
    let dir = TempDir::new().unwrap();
    let log = EventLog::new(dir.path().join("log.jsonl"));
    let outbox = Outbox::new(dir.path().join("outbox.jsonl"));
    let mut d = Debate::default();
    d.observe(&log, &outbox, &turn(None, "Challenge c1: concede — x"), 1).unwrap();
    assert!(log.read_from(0).unwrap().records.is_empty());
}

/// V7, V9, V39: the Open Debate section names the owner and what is due; the reserve runs a wake
/// that owes a move (V48).
#[test]
fn render_and_owes() {
    let mut f = fx();
    let list = f.list();
    let text = f.d.render(&list).unwrap();
    assert!(text.contains("Challenge c1 on c1 \"retry loop never converges\""), "{text}");
    assert!(text.contains("Only the approach Watcher may reply"), "{text}");
    assert!(text.ends_with("Waiting for the Worker's verdict line."), "{text}");
    assert!(!f.d.owes());

    f.observe(&turn(None, "Challenge c1: defend — it is idempotent"));
    assert!(f.d.owes());
    let text = f.d.render(&list).unwrap();
    assert!(text.contains("Worker: defend — it is idempotent"), "{text}");
    assert!(text.contains("rebut (only with new ground answering its reason)"), "{text}");
    f.decide(&[("approach", line(Decision::Rebut, "429s"))]);
    assert!(!f.d.owes());
    assert!(f.d.render(&list).unwrap().ends_with("Exchange 2 waits for the Worker's next idle."));
    f.deliver_second();
    f.observe(&turn(None, "Challenge c1: defend — again"));
    assert!(f.d.render(&list).unwrap().contains("last word (no third message)"));
    f.decide(&[("approach", line(Decision::Concede, "fine"))]);
    assert_eq!(f.d.render(&list), None);
}

/// V30: only the last line naming the Challenge counts; an earlier verdict is no fallback, and
/// template echoes with `/` or `or` are no verdict.
#[test]
fn verdict_is_the_last_naming_line_only() {
    let v = |t: &str| debate::verdict(t, "c1");
    assert_eq!(v("Challenge c1: defend — X\nChallenge c1: on reflection the cache argument is weak"), None);
    assert_eq!(v("Challenge c1: concede / defend — <reason>"), None);
    assert_eq!(v("Challenge c1: concede or defend"), None);
    assert_eq!(v("Challenge c1: defend — or so I think"), Some((Verdict::Defend, "or so I think".into())));
    assert_eq!(v("see XChallenge c1: no, Challenge c1: defend — x"), Some((Verdict::Defend, "x".into())));
}

/// The real call order: delivery acked at idle, then the next wake logs `sent` before it reads
/// the reply turn, which is the exchange (review R1).
#[test]
fn reply_in_the_wake_after_an_idle_delivery_is_read() {
    let dir = TempDir::new().unwrap();
    let log = EventLog::new(dir.path().join("log.jsonl"));
    let outbox = Outbox::new(dir.path().join("outbox.jsonl"));
    log.append(Change::Add { watcher: "approach".into(), concern: concern(Severity::Warn) }.event()).unwrap();
    let events =
        |log: &EventLog| -> Vec<Event> { log.read_from(0).unwrap().records.into_iter().map(|r| r.body).collect() };
    let mut ch = Challenges::rebuild(&events(&log));
    ch.queue(&log, &outbox, &concern(Severity::Warn), Origin::Auto, None, 1).unwrap();
    // W1 ran, settled; the mod delivers at idle.
    let claim = outbox.claim("mod", 2, |i| ch.gate(i)).unwrap().unwrap();
    assert!(outbox.ack(&claim, 2).unwrap());
    // W2, triggered by the reply's turn end: sync first, then observe.
    let mut ch = Challenges::rebuild(&events(&log));
    assert_eq!(ch.sync_sent(&log, &outbox).unwrap().as_deref(), Some("mod"));
    let mut d = Debate::rebuild(&events(&log));
    d.sync_sent(&log, &outbox).unwrap();
    d.observe(&log, &outbox, &turn(Some(OWN), "Challenge c1: defend — idempotent"), 3).unwrap();
    assert!(d.owes(), "the reply is exchange 1's answer");
    assert_eq!(Challenges::rebuild(&events(&log)).slot().map(|s: &Slot| s.sent), Some(true));
}

/// A `Stop` block continues the blocked turn: that turn's own end is skipped once; the reply is
/// the next turn end (review R1).
#[test]
fn stop_route_skips_the_blocked_turns_own_end() {
    let mut f = fx_route(Severity::Warn, "stop");
    f.observe(&turn(None, "Wrote the file. Task complete."));
    assert!(f.ops("exchange").is_empty());
    assert_eq!(f.ops("stop-turn").len(), 1);
    f.observe(&turn(None, "Nothing to add."));
    assert_eq!(f.ops("exchange")[0].str("verdict"), Some("none"));
    f.assert_consistent();

    // A verdict in the first turn end still counts.
    let mut f = fx_route(Severity::Warn, "stop");
    f.observe(&turn(None, "Challenge c1: defend — task said so"));
    assert!(f.d.owes());
}

/// V34, V21: an exchange-2 item whose Concern closed since the last wake is dropped at claim,
/// before any wake decides (review R2).
#[test]
fn rebuttal_dropped_when_its_concern_closes_between_wakes() {
    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — fine"));
    f.decide(&[("approach", line(Decision::Rebut, "new ground"))]);
    f.log(concern::dismissal("c1"));
    assert_eq!(f.outbox.claim("mod", 1, |i| f.d.gate(i).unwrap()).unwrap(), None);
    assert!(f.outbox.pending().unwrap().is_empty());
}

/// V93 crash windows (review R3–R5): every rebuild after a partial step carries the Debate on.
#[test]
fn crash_windows_never_hold_the_slot() {
    // `end` logged, crash before the item was withdrawn: ended, and the gate drops the item.
    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — fine"));
    f.decide(&[("approach", line(Decision::Rebut, "new ground"))]);
    f.log(
        debate::Op::End {
            challenge: "c1".into(),
            concern: "c1".into(),
            outcome: Outcome::Withdrawn,
            escalated: None,
            withdrawn: Some(Withdrawal::Budget),
        }
        .event(),
    );
    let d = Debate::rebuild(&f.events());
    assert_eq!(d.open(), None);
    assert_eq!(f.outbox.claim("mod", 1, |i| d.gate(i).unwrap()).unwrap(), None);

    // Orphan `c1/2` queued, crash before `queued` was logged: the re-ask still goes out.
    let mut f = fx();
    f.observe(&turn(None, "no verdict"));
    f.outbox.queue(Item { id: "c1/2".into(), kind: ItemKind::Challenge, text: "orphan".into() }, 1).unwrap();
    assert_eq!(f.decide(&[]).ended, None);
    let pending = f.outbox.pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].text.contains("You did not answer"));
    assert_eq!(f.d.waiting(), Some("c1/2"));

    // `decision escalate` logged, crash before `end`: the next decide carries it out, with no
    // second (defaulted) decision.
    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — fine"));
    f.log(
        debate::Op::Decision {
            challenge: "c1".into(),
            watcher: "approach".into(),
            decision: Decision::Escalate,
            text: "trade-off".into(),
            defaulted: false,
        }
        .event(),
    );
    f.d = Debate::rebuild(&f.events());
    let step = f.decide(&[]);
    assert_eq!(step.ended, Some(Outcome::Unresolved));
    assert_eq!(f.ops("decision").len(), 1);
    f.assert_consistent();
}

/// V100: the default concession needs the owner to have run; a failed, invalid, skipped or muted
/// owner leaves the move owed (review R6).
#[test]
fn owner_that_did_not_run_still_owes() {
    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — fine"));
    let list = f.list();
    let others = [Watcher { name: "correctness".into(), focuses: vec!["correctness".into()], model: None }];
    let step = f.d.decide(&f.log, &f.outbox, &list, &others, &[], EscalateAt::Warn, 1).unwrap();
    assert_eq!(step, debate::Step::default());
    assert!(f.d.owes() && f.ops("decision").is_empty());
    assert_eq!(f.decide(&[]).ended, Some(Outcome::WatcherConcedes), "ran, silent: defaulted");

    // Never runs: missing after two owed wakes, so the slot is not held for good.
    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — fine"));
    let list = f.list();
    let step = f.d.decide(&f.log, &f.outbox, &list, &others, &[], EscalateAt::Warn, 1).unwrap();
    assert_eq!(step.ended, None);
    let step = f.d.decide(&f.log, &f.outbox, &list, &others, &[], EscalateAt::Warn, 1).unwrap();
    assert_eq!(step.ended, Some(Outcome::WatcherConcedes));
    let d = &f.ops("decision")[0];
    assert_eq!((d.str("watcher"), &d.fields["defaulted"]), (Some("arbiter"), &serde_json::json!(true)));
    f.assert_consistent();
}

/// V34: a human prompt with no turn of its own (interrupted) is cleared by our next delivered
/// prompt, so that turn's missing verdict is "no answer" (review R7).
#[test]
fn stale_human_flag_cleared_by_our_prompt() {
    let mut f = fx();
    f.observe(&[Entry { id: "h".into(), t: 0, items: vec![T::Prompt("[Request interrupted by user]".into())] }]);
    assert!(f.d.open().unwrap().human);
    f.observe(&turn(Some(OWN), "carrying on"));
    assert_eq!(f.ops("exchange")[0].str("verdict"), Some("none"));
}

/// One own-prompt rule everywhere: our header on any line, e.g. under a harness label (review R8).
#[test]
fn own_prompt_predicate() {
    assert!(history::is_own(OWN));
    assert!(history::is_own("Stop hook feedback:\n[pair-watcher · Challenge c1 · from the Watcher, not the human]"));
    assert!(!history::is_own("please look at the pair-watcher · Challenge c1 thing"));
}

/// V35, V100: a Concern resolved fixed mid-Debate keeps its end state; no `conceded` tag (R10).
#[test]
fn resolved_concern_is_not_tagged() {
    let mut f = fx();
    f.log(Change::Resolve { watcher: "approach".into(), id: "c1".into(), reason: Resolution::Fixed }.event());
    assert_eq!(f.decide(&[]).ended, Some(Outcome::WorkerConcedes));
    let c = f.list().get("c1").unwrap().clone();
    assert_eq!((c.state, c.tag), (State::Resolved, None));
}

/// Review round 2: a human prompt that started the `Stop`-blocked turn is not a human turn
/// mid-Debate; a silent reply is "no answer".
#[test]
fn stop_route_blocked_turn_started_by_the_human() {
    let mut f = fx_route(Severity::Warn, "stop");
    f.observe(&turn(Some("write the file"), "Wrote it."));
    assert!(f.ops("human-prompt").is_empty());
    f.observe(&turn(None, "Done."));
    assert_eq!(f.ops("exchange")[0].str("verdict"), Some("none"));
    assert!(f.ops("human-turn").is_empty());
    f.assert_consistent();
}

/// V10, V92: a replayed slice never reads a turn end twice.
#[test]
fn replayed_slice_is_idempotent() {
    let mut f = fx_route(Severity::Warn, "stop");
    let blocked = turn(None, "Wrote it.");
    f.observe(&blocked);
    f.observe(&blocked);
    assert!(f.ops("exchange").is_empty(), "the skipped turn end stays skipped");
    let reply = turn(None, "Challenge c1: defend — fine");
    f.observe(&[blocked[0].clone(), reply[0].clone()]);
    f.observe(&reply);
    assert_eq!(f.ops("exchange").len(), 1);
    f.assert_consistent();
}

/// V37: ending while exchange 2 is queued withdraws it, whatever move ended it.
#[test]
fn switched_while_queued_withdraws() {
    let mut f = fx();
    f.observe(&turn(None, "Challenge c1: defend — fine"));
    f.decide(&[("approach", line(Decision::Rebut, "new ground"))]);
    assert_eq!(f.outbox.pending().unwrap().len(), 1);
    assert_eq!(f.decide(&[("approach", line(Decision::Switched, "moved on"))]).ended, Some(Outcome::WorkerConcedes));
    assert!(f.outbox.pending().unwrap().is_empty());
}

/// Both verdict words before the reason: no verdict; "or" alone is fine.
#[test]
fn verdict_both_words_is_no_verdict() {
    let v = |t: &str| debate::verdict(t, "c1");
    assert_eq!(v("Challenge c1: defend, or concede"), None);
    assert_eq!(
        v("Challenge c1: defend or rather hold — it is cached"),
        Some((Verdict::Defend, "or rather hold — it is cached".into()))
    );
    assert_eq!(
        v("Challenge c1: concede — I will not defend it"),
        Some((Verdict::Concede, "I will not defend it".into()))
    );
}

/// A human prompt quoting our header further down stays the human's.
#[test]
fn quoted_header_stays_human() {
    assert!(!history::is_own("what does this mean?\n\n[pair-watcher · Challenge c1 · from the Watcher]"));
    assert!(!history::is_own("this is a long human sentence that ends in a colon:\n[pair-watcher · Challenge c1"));
}
