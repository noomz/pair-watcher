//! §T13 human rulings: `y`/`n`/`d` on NEEDS YOU and IN DEBATE (V71–V73), the Ruling message
//! (V74, I12), `u` (V75), `s` keep (V77), inference from the human's prompt (V71), `answered`,
//! `closed-stale`, `unruled`.

use std::fs;

use pair_watcher::challenge::{Challenges, Op as ChallengeOp, Origin};
use pair_watcher::concern::{Change, Concern, ConcernList, Resolution, Severity, State};
use pair_watcher::debate::{Debate, Op as DebateOp, Outcome, Phase, Second, Verdict};
use pair_watcher::delivery::{self, Route};
use pair_watcher::escalation::{Cause, Escalations, Op as EscalationOp};
use pair_watcher::event::{Event, Kind};
use pair_watcher::home::Home;
use pair_watcher::human::Act;
use pair_watcher::jsonl::{Record, now_ms};
use pair_watcher::layout::{Ui, layout};
use pair_watcher::mailbox::Mailbox;
use pair_watcher::outbox::{Gate, Item, ItemKind};
use pair_watcher::output::{Decision, Line, Side as OutSide};
use pair_watcher::pane::{self, Board, Effect};
use pair_watcher::ruling::{self, Rulings, Side, Stage, Surface, Via};
use pair_watcher::standing;
use pair_watcher::tailer::{Entry, Item as TItem};
use pair_watcher::term::Key;
use pair_watcher::wake::{self, Context};
use tempfile::TempDir;

const ALT: &str = "stop requesting reviews; ask the human to merge";

struct Fx {
    dir: TempDir,
    home: Home,
    mb: Mailbox,
}

fn concern(id: &str) -> Concern {
    Concern {
        id: id.into(),
        focus: "approach".into(),
        severity: Severity::Warn,
        state: State::Open,
        headline: "5th review round, 0 new findings".into(),
        detail: vec![],
        refs: vec![],
        challenge: true,
        evidence: vec![],
        alternative: Some(ALT.into()),
        verified: true,
        tag: None,
    }
}

impl Fx {
    /// A Claude session with Concern `c2` open, owned by the Companion.
    fn new() -> Fx {
        let dir = TempDir::new().unwrap();
        let home = Home::new(dir.path().join(".pair-watcher"));
        home.ensure_layout().unwrap();
        let mb = Mailbox::register(&home, "s1").unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let fx = Fx { dir, home, mb };
        fx.log(
            Event::new(Kind::Session)
                .with("phase", "start")
                .with("harness", "claude")
                .with("repo", repo.to_string_lossy().as_ref()),
        );
        fx.log(Change::Add { watcher: "companion".into(), concern: concern("c2") }.event());
        fx
    }

    fn log(&self, e: Event) {
        self.mb.log().append(e).unwrap();
    }

    fn records(&self) -> Vec<Record<Event>> {
        self.mb.log().read_from(0).unwrap().records
    }

    fn events(&self) -> Vec<Event> {
        self.records().into_iter().map(|r| r.body).collect()
    }

    fn kind(&self, k: Kind) -> Vec<Event> {
        self.events().into_iter().filter(|e| e.k == k).collect()
    }

    fn op(&self, k: Kind, op: &str) -> Vec<Event> {
        self.kind(k).into_iter().filter(|e| e.str("op") == Some(op)).collect()
    }

    fn board(&self) -> Board {
        Board::rebuild(0, &self.records())
    }

    fn rulings(&self) -> Rulings {
        Rulings::rebuild(&self.records())
    }

    fn debate(&self) -> Debate {
        Debate::rebuild(&self.events())
    }

    fn list(&self) -> ConcernList {
        ConcernList::rebuild(&self.events())
    }

    /// The arbiter's answer, on folds rebuilt from the log.
    fn answer(&self) -> Vec<String> {
        let events = self.events();
        let (mut d, mut c) = (Debate::rebuild(&events), Challenges::rebuild(&events));
        let (list, esc) = (ConcernList::rebuild(&events), Escalations::rebuild(&events));
        self.rulings().answer(&self.mb.log(), &self.mb.outbox(), &mut d, &mut c, &list, &esc, now_ms()).unwrap()
    }

    fn rule(&self, id: &str, side: Side) -> Result<ruling::Ruled, String> {
        let b = self.board();
        let r = Rulings::rule(&b.cx(), id, side, Via::Key, Surface::Pane, now_ms())?;
        self.log(r.event());
        Ok(r)
    }

    fn undo(&self) {
        let e = pane::undo(&self.board(), &self.mb.outbox(), Via::Key, Surface::Pane).unwrap();
        self.log(e);
    }

    /// Challenge `c2` on `c2` queued (outbox item) and delivered.
    fn challenge(&self) {
        let mut ch = Challenges::rebuild(&self.events());
        ch.queue(&self.mb.log(), &self.mb.outbox(), &concern("c2"), Origin::Auto, None, now_ms()).unwrap();
        self.ack("c2");
        self.log(ChallengeOp::Sent { id: "c2".into(), route: "mod".into() }.event());
    }

    fn ack(&self, id: &str) {
        let outbox = self.mb.outbox();
        let claim = outbox
            .claim("mod", now_ms(), |i| if i.id == id { Gate::Deliver } else { Gate::Skip })
            .unwrap()
            .expect("claimable");
        assert!(outbox.ack(&claim, now_ms()).unwrap());
    }

    fn exchange(&self, n: u8, verdict: Verdict, entry: &str) {
        let reason = Some("only reviewbot can approve; branch protection".to_owned());
        let e = DebateOp::Exchange { challenge: "c2".into(), n, verdict, reason, entry: entry.into(), turn: vec![] };
        self.log(e.event());
    }

    /// IN DEBATE: the Worker defended exchange 1, the owner rebutted, `c2/2` waits in the outbox.
    fn rebuttal_queued(&self) {
        self.challenge();
        self.exchange(1, Verdict::Defend, "t1");
        let decision = DebateOp::Decision {
            challenge: "c2".into(),
            watcher: "companion".into(),
            decision: Decision::Rebut,
            text: "COMMENTED x4, never APPROVED".into(),
            defaulted: false,
        };
        self.log(decision.event());
        let item = Item { id: "c2/2".into(), kind: ItemKind::Challenge, text: "rebuttal".into() };
        assert!(self.mb.outbox().queue(item, now_ms()).unwrap());
        self.log(DebateOp::Queued { challenge: "c2".into(), item: "c2/2".into(), second: Second::Rebuttal }.event());
    }

    /// NEEDS YOU: the Debate on `c2` ended unresolved, Escalation `e1` raised and notified.
    fn escalated(&self) {
        self.challenge();
        self.exchange(2, Verdict::Defend, "t2");
        let end = DebateOp::End {
            challenge: "c2".into(),
            concern: "c2".into(),
            outcome: Outcome::Unresolved,
            escalated: Some(true),
            withdrawn: None,
        };
        self.log(end.event());
        self.raise("e1", Cause::Unresolved, None);
    }

    fn raise(&self, id: &str, cause: Cause, hook: Option<u64>) {
        let unresolved = cause == Cause::Unresolved;
        let raised = EscalationOp::Raised {
            id: id.into(),
            cause,
            concern: unresolved.then(|| "c2".into()),
            challenge: unresolved.then(|| "c2".into()),
            hook,
            title: if unresolved { "Debate unresolved: 5th review round (c2)" } else { "Worker blocked" }.into(),
            lines: vec!["Watcher: stop".into(), "Worker: only reviewbot".into()],
            rule: "Rule: pane y/n/d".into(),
        };
        self.log(raised.event());
        self.log(EscalationOp::Notified { id: id.into(), via: "none".into(), error: None }.event());
    }

    fn ruling_items(&self) -> Vec<Item> {
        self.mb.outbox().pending().unwrap().into_iter().filter(|i| i.kind == ItemKind::Ruling).collect()
    }
}

fn prompt(id: &str, t: u64, text: &str) -> Entry {
    Entry { id: id.into(), t, items: vec![TItem::Prompt(text.into()), TItem::TurnEnd] }
}

#[test]
fn ruling_text_is_i12() {
    assert_eq!(
        ruling::text("c2", "stop requesting reviews;\n  ask the human to merge."),
        "[pair-watcher · Ruling c2 · from the human] The human sides with the Watcher on Challenge c2: stop \
         requesting reviews; ask the human to merge. Change course; no verdict line needed."
    );
}

/// V71, V74: `y` on NEEDS YOU closes it at once; the arbiter queues the Ruling, which only an
/// idle route delivers, then logs it delivered.
#[test]
fn y_on_needs_you_closes_it_and_sends_a_ruling_at_idle_only() {
    let fx = Fx::new();
    fx.escalated();
    assert_eq!(fx.board().needs_you().count(), 1);
    let r = fx.rule("e1", Side::Watcher).unwrap();
    assert_eq!((r.stage, r.concern.as_deref(), r.challenge.as_deref()), (Stage::Escalation, Some("c2"), Some("c2")));
    assert!(r.ruling_sent);
    assert_eq!(r.watcher.as_deref(), Some("companion"));

    let b = fx.board();
    assert_eq!(b.needs_you().count(), 0, "closed at once");
    assert_eq!(b.concerns.get("c2").unwrap().state, State::Ruled);
    assert_eq!(b.closed_as("c2").as_deref(), Some("escalated → ruled: sided with the Watcher"));
    assert!(b.ticker.last().unwrap().text.contains("u undo · s keep for this repo"));
    assert!(fx.rule("e1", Side::Worker).is_err(), "no second ruling on a closed item");

    fx.answer();
    let items = fx.ruling_items();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].text, ruling::text("c2", ALT));
    assert_eq!(fx.op(Kind::Ruling, "queued").len(), 1);
    fx.answer();
    assert_eq!(fx.op(Kind::Ruling, "queued").len(), 1, "answered once");

    let view = delivery::View::read(&fx.mb).unwrap();
    assert!(delivery::claim(&fx.mb, &view, Route::Stop, now_ms()).unwrap().is_none(), "never by the Stop block");
    let claim = delivery::claim(&fx.mb, &view, Route::Mod, now_ms()).unwrap().expect("idle route");
    assert_eq!(claim.item.kind, ItemKind::Ruling);
    fx.mb.outbox().ack(&claim, now_ms()).unwrap();
    fx.answer();
    assert_eq!(fx.op(Kind::Ruling, "delivered")[0].str("route"), Some("mod"));
}

/// V74: siding with the Worker, dismissing, and inferred rulings send nothing.
#[test]
fn only_an_explicit_side_with_the_watcher_sends_a_ruling() {
    for side in [Side::Worker, Side::Dismissed] {
        let fx = Fx::new();
        fx.escalated();
        assert!(!fx.rule("e1", side).unwrap().ruling_sent);
        fx.answer();
        assert!(fx.ruling_items().is_empty());
        assert!(fx.kind(Kind::Ruling).is_empty());
    }
}

/// V72: a blocked Escalation takes `d` only; answering in the Worker closes it.
#[test]
fn blocked_takes_only_d_and_closes_when_answered() {
    let fx = Fx::new();
    fx.raise("e1", Cause::Blocked, Some(41));
    for side in [Side::Watcher, Side::Worker] {
        assert!(fx.rule("e1", side).unwrap_err().contains("answer in the Worker"));
    }
    let r = fx.rule("e1", Side::Dismissed).unwrap();
    assert_eq!((r.stage, r.ruling_sent), (Stage::Blocked, false));
    assert_eq!(fx.board().needs_you().count(), 0);

    fx.raise("e2", Cause::Blocked, Some(42));
    let mut rulings = fx.rulings();
    let esc = Escalations::rebuild(&fx.events());
    let not_yet = rulings.answer_blocked(&fx.mb.log(), &esc, |_| Ok(false), now_ms()).unwrap();
    assert!(not_yet.is_empty());
    let done = rulings.answer_blocked(&fx.mb.log(), &esc, |at| Ok(at == 42), now_ms()).unwrap();
    assert_eq!(done, ["e2"]);
    assert_eq!(fx.kind(Kind::Answered).len(), 1);
    assert_eq!(fx.board().needs_you().count(), 0);
    let again = fx.rulings().answer_blocked(&fx.mb.log(), &esc, |_| Ok(true), now_ms()).unwrap();
    assert!(again.is_empty(), "answered once");
}

/// V73: `y`/`n`/`d` on IN DEBATE end it at once: the waiting rebuttal is cancelled, no
/// Escalation; the arbiter logs the outcome.
#[test]
fn ruling_in_debate_ends_it_at_once() {
    for (side, outcome, withdrawn) in [
        (Side::Watcher, Outcome::SidedWatcher, None),
        (Side::Worker, Outcome::SidedWorker, None),
        (Side::Dismissed, Outcome::Withdrawn, Some("dismissed")),
    ] {
        let fx = Fx::new();
        fx.rebuttal_queued();
        assert_eq!(fx.board().in_debate(), Some("c2"));
        let r = fx.rule("c2", side).unwrap();
        assert_eq!(r.stage, Stage::Debate);

        // Before the arbiter answers: gone from IN DEBATE, the rebuttal never goes out.
        assert_eq!(fx.board().in_debate(), None);
        let view = delivery::View::read(&fx.mb).unwrap();
        let rebuttal = Item { id: "c2/2".into(), kind: ItemKind::Challenge, text: String::new() };
        assert_eq!(view.gate(&rebuttal), Gate::Drop);

        fx.answer();
        let end = &fx.op(Kind::Debate, "end")[0];
        assert_eq!(end.str("outcome"), serde_json::to_value(outcome).unwrap().as_str());
        assert_eq!(end.str("withdrawn"), withdrawn);
        assert!(!fx.mb.outbox().pending().unwrap().iter().any(|i| i.id == "c2/2"), "rebuttal withdrawn");
        let mut esc = Escalations::rebuild(&fx.events());
        let raised = esc
            .raise_unresolved(
                &fx.mb.log(),
                &fx.list(),
                false,
                pair_watcher::escalation::Surfaces { harness: None, band_digits: false },
            )
            .unwrap();
        assert!(raised.is_empty(), "no Escalation");
        assert_eq!(fx.ruling_items().len(), usize::from(side == Side::Watcher));
        fx.answer();
        assert_eq!(fx.op(Kind::Debate, "end").len(), 1, "ended once");
    }
}

/// V73: a ruling on a Challenge still waiting for delivery drops it.
#[test]
fn ruling_on_an_unsent_challenge_drops_it() {
    let fx = Fx::new();
    let mut ch = Challenges::rebuild(&fx.events());
    ch.queue(&fx.mb.log(), &fx.mb.outbox(), &concern("c2"), Origin::Promoted, Some("look"), now_ms()).unwrap();
    assert_eq!(fx.board().in_debate(), Some("c2"));
    fx.rule("c2", Side::Worker).unwrap();
    let view = delivery::View::read(&fx.mb).unwrap();
    assert!(delivery::claim(&fx.mb, &view, Route::Stop, now_ms()).unwrap().is_none());
    fx.answer();
    assert_eq!(fx.op(Kind::Challenge, "stale-dropped").len(), 1);
    assert!(Challenges::rebuild(&fx.events()).slot().is_none());

    // Undone: queued again, origin and remark kept (V75).
    fx.undo();
    fx.answer();
    let queued = fx.op(Kind::Challenge, "queued");
    assert_eq!(queued.len(), 2);
    assert_eq!((queued[1].str("origin"), queued[1].str("remark")), (Some("promoted"), Some("look")));
    assert_eq!(fx.board().in_debate(), Some("c2"));
}

/// V71, V74: an inferred ruling needs a turn-end wake, `infer_rulings`, an open unresolved
/// Escalation, and a `prompt_ref` found verbatim in a human prompt; disagreeing Watchers rule
/// nothing; nothing is sent.
#[test]
fn inferred_ruling_is_checked_by_code() {
    let line = |side, r: &str| Line::InferredRuling { escalation: "e1".into(), side, prompt_ref: r.into() };
    let entries =
        [prompt("p1", now_ms() + 60_000, "reviewbot is broken, I'll merge it myself.\nDraft the squash message")];
    let infer = |fx: &Fx, lines: &[(String, Line)], turn_end: bool, enabled: bool| {
        let (esc, list) = (Escalations::rebuild(&fx.events()), fx.list());
        fx.rulings().infer(&fx.mb.log(), lines, turn_end, enabled, &entries, &esc, &list, now_ms()).unwrap()
    };
    let good = vec![("companion".to_owned(), line(OutSide::Watcher, "I'll merge it myself. Draft"))];

    for (turn_end, enabled, why) in [(false, true, "turn-end"), (true, false, "infer_rulings is off")] {
        let fx = Fx::new();
        fx.escalated();
        let dropped = infer(&fx, &good, turn_end, enabled);
        assert!(dropped[0].contains(why), "{dropped:?}");
        assert!(fx.kind(Kind::Ruled).is_empty());
    }

    let fx = Fx::new();
    fx.escalated();
    let made_up = vec![("companion".to_owned(), line(OutSide::Watcher, "keep requesting reviews"))];
    assert!(infer(&fx, &made_up, true, true)[0].contains("not in a human prompt since it was raised"));
    let split = vec![
        ("approach".to_owned(), line(OutSide::Watcher, "merge it myself")),
        ("correctness".to_owned(), line(OutSide::Worker, "reviewbot is broken")),
    ];
    assert_eq!(infer(&fx, &split, true, true).len(), 2);
    assert!(fx.kind(Kind::Ruled).is_empty());

    assert!(infer(&fx, &good, true, true).is_empty());
    let ruled = &fx.kind(Kind::Ruled)[0];
    assert_eq!((ruled.str("via"), ruled.str("side")), (Some("prompt"), Some("watcher")));
    assert_eq!(ruled.fields["ruling_sent"], false);
    let b = fx.board();
    assert_eq!(b.needs_you().count(), 0);
    assert!(b.ticker.last().unwrap().text.contains("(inferred)"));
    assert!(b.ticker.last().unwrap().text.contains("u undo"));
    fx.answer();
    assert!(fx.ruling_items().is_empty());
    assert!(infer(&fx, &good, true, true)[0].contains("not waiting on a ruling"));
}

/// V75: `u` on NEEDS YOU: back in NEEDS YOU, the queued Ruling withdrawn and gated off.
#[test]
fn undo_reopens_needs_you_and_withdraws_the_queued_ruling() {
    let fx = Fx::new();
    fx.escalated();
    fx.rule("e1", Side::Watcher).unwrap();
    fx.answer();
    let item = fx.ruling_items().remove(0);
    fx.undo();
    let undone = &fx.op(Kind::Ruling, "undone")[0];
    assert_eq!(undone.fields["delivered"], false);

    let b = fx.board();
    assert_eq!(b.needs_you().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["e1"]);
    assert_eq!(b.concerns.get("c2").unwrap().state, State::Open);
    assert_eq!(b.closed_as("c2"), None);
    assert_eq!(delivery::View::read(&fx.mb).unwrap().gate(&item), Gate::Drop);

    fx.answer();
    assert_eq!(fx.op(Kind::Ruling, "dropped")[0].str("why"), Some(ruling::UNDONE));
    assert!(fx.ruling_items().is_empty());
    assert!(pane::undo(&fx.board(), &fx.mb.outbox(), Via::Key, Surface::Pane).is_err(), "nothing left to undo");

    // Ruled again: a fresh Ruling under its own outbox id; the undone one's state never gates it.
    fx.rule("e1", Side::Watcher).unwrap();
    fx.answer();
    let fresh = fx.ruling_items().remove(0);
    assert_eq!((item.id.as_str(), fresh.id.as_str()), ("ruling/e1.0", "ruling/e1.1"));
    assert_eq!(delivery::View::read(&fx.mb).unwrap().gate(&fresh), Gate::Deliver);
}

/// V75: a delivered Ruling stays sent; the undo record says so.
#[test]
fn undo_after_delivery_keeps_the_ruling_sent() {
    let fx = Fx::new();
    fx.escalated();
    fx.rule("e1", Side::Watcher).unwrap();
    fx.answer();
    let id = fx.ruling_items()[0].id.clone();
    fx.ack(&id);
    fx.undo();
    assert_eq!(fx.op(Kind::Ruling, "undone")[0].fields["delivered"], true);
    fx.answer();
    assert_eq!(fx.op(Kind::Ruling, "delivered").len(), 1);
    assert!(fx.op(Kind::Ruling, "dropped").is_empty());
}

/// V75: `u` on an IN DEBATE ruling reopens the Debate, owed again on the Worker's last answer,
/// while the slot is free; once another Challenge took it, the Concern stays open.
#[test]
fn undo_in_debate_reopens_the_debate_only_while_the_slot_is_free() {
    let fx = Fx::new();
    fx.rebuttal_queued();
    fx.rule("c2", Side::Watcher).unwrap();
    fx.answer();
    assert!(fx.debate().open().is_none());
    fx.undo();
    fx.answer();
    assert_eq!(fx.op(Kind::Debate, "reopened").len(), 1);
    assert_eq!(fx.op(Kind::Ruling, "dropped").len(), 1, "the Ruling never went out");
    let d = fx.debate();
    let open = d.open().expect("reopened");
    assert_eq!((open.n, &open.phase), (1, &Phase::Owed(Verdict::Defend)));
    assert!(d.owes(), "the owner decides again");
    assert_eq!(fx.board().in_debate(), Some("c2"));
    assert!(Challenges::rebuild(&fx.events()).slot().is_some_and(|s| s.id == "c2" && s.sent));
    fx.answer();
    assert_eq!(fx.op(Kind::Debate, "reopened").len(), 1, "once");

    // Another Challenge took the slot before the undo: no reopen.
    let fx = Fx::new();
    fx.log(Change::Add { watcher: "companion".into(), concern: concern("c3") }.event());
    fx.rebuttal_queued();
    fx.rule("c2", Side::Worker).unwrap();
    fx.answer();
    let mut ch = Challenges::rebuild(&fx.events());
    ch.queue(&fx.mb.log(), &fx.mb.outbox(), &concern("c3"), Origin::Auto, None, now_ms()).unwrap();
    fx.undo();
    fx.answer();
    assert!(fx.op(Kind::Debate, "reopened").is_empty());
    assert_eq!(fx.list().get("c2").unwrap().state, State::Open, "an open Concern again");
    assert_eq!(fx.board().in_debate(), Some("c3"));
}

/// V74: a human prompt that arrived after the ruling goes first: the Ruling is dropped.
#[test]
fn human_prompt_first_drops_the_ruling() {
    let fx = Fx::new();
    fx.escalated();
    fx.rule("e1", Side::Watcher).unwrap();
    fx.answer();
    let t = fx.rulings().last().unwrap().t;
    let mut r = fx.rulings();
    let before = [prompt("p0", t - 1, "unrelated earlier prompt")];
    let own = [prompt("p1", t + 1, &ruling::text("c2", ALT))];
    for entries in [&before[..], &own[..]] {
        assert!(r.drop_on_prompt(&fx.mb.log(), &fx.mb.outbox(), entries, now_ms()).unwrap().is_empty());
    }
    let after = [prompt("p2", t + 1, "also bump the version")];
    assert_eq!(r.drop_on_prompt(&fx.mb.log(), &fx.mb.outbox(), &after, now_ms()).unwrap().len(), 1);
    assert_eq!(fx.op(Kind::Ruling, "dropped")[0].str("why"), Some(ruling::PROMPTED_FIRST));
    assert!(fx.ruling_items().is_empty());
}

/// V71: closed stale when the Concern stops applying; `unruled` at session end.
#[test]
fn escalation_closes_stale_or_unruled() {
    let fx = Fx::new();
    fx.escalated();
    fx.log(
        Change::Resolve { watcher: "companion".into(), id: "c2".into(), reason: Resolution::NoLongerApplies }.event(),
    );
    assert_eq!(fx.answer(), ["e1 closed: c2 no longer applies"]);
    assert_eq!(fx.kind(Kind::ClosedStale).len(), 1);
    assert_eq!(fx.board().needs_you().count(), 0);

    let fx = Fx::new();
    fx.escalated();
    fx.raise("e2", Cause::Blocked, Some(7));
    let esc = Escalations::rebuild(&fx.events());
    assert_eq!(fx.rulings().unruled(&fx.mb.log(), &esc, now_ms()).unwrap(), ["e1", "e2"]);
    assert_eq!(fx.board().needs_you().count(), 0);
}

/// V77: `s` keeps the last ruling or dismissal, or the selected CLOSED row, once; the store
/// refuses past the cap (V78); a mute is not a ruling.
#[test]
fn keep_writes_a_standing_ruling() {
    let fx = Fx::new();
    let store = standing::path(&fx.home, &standing::key(&fx.dir.path().join("repo")));
    fx.log(Act::Mute { focus: "approach".into() }.event());
    assert!(pane::keep(&fx.board(), &store, None, now_ms()).unwrap_err().contains("no ruling or dismissal"));

    fx.log(Change::Add { watcher: "companion".into(), concern: concern("c3") }.event());
    fx.log(Act::Dismiss { id: "c3".into() }.event());
    fx.escalated();
    fx.rule("e1", Side::Worker).unwrap();

    // The latest: the ruling on c2, with the Worker's reason.
    let e = pane::keep(&fx.board(), &store, None, now_ms()).unwrap();
    assert_eq!((e.str("id"), e.str("ruling")), (Some("c2"), Some("r1")));
    fx.log(e);
    // The selected CLOSED row: the dismissal of c3.
    let e = pane::keep(&fx.board(), &store, Some("c3"), now_ms()).unwrap();
    fx.log(e);
    assert!(pane::keep(&fx.board(), &store, Some("c3"), now_ms()).unwrap_err().contains("already kept"));
    assert_eq!(fx.board().rulings_held, 2);

    let held = standing::load(&store).unwrap();
    assert_eq!(held.len(), 2);
    assert_eq!(
        (held[0].verdict.as_str(), held[0].reason.as_deref()),
        ("worker", Some("only reviewbot can approve; branch protection"))
    );
    assert_eq!((held[1].id.as_str(), held[1].verdict.as_str(), held[1].reason.as_deref()), ("r2", "dismissed", None));
    assert_eq!(held[0].date.len(), 10);

    let keep = ruling::Keep {
        concern: "c9".into(),
        focus: "approach".into(),
        headline: "h".into(),
        side: Side::Dismissed,
        reason: None,
    };
    for _ in 2..standing::CAP {
        standing::remember(&store, &keep, "2026-10-08").unwrap().unwrap();
    }
    assert!(standing::remember(&store, &keep, "2026-10-08").unwrap().unwrap_err().contains("30 Standing rulings"));
    assert_eq!(standing::date(0, 0), "1970-01-01");
    assert_eq!(standing::date(1_791_417_600_000, 0), "2026-10-08");
}

/// I17, V72, V73: the pane's `y`/`n`/`d`/`u`/`s`.
#[test]
fn pane_keys_rule_undo_keep() {
    let fx = Fx::new();
    fx.escalated();
    fx.raise("e2", Cause::Blocked, Some(9));
    let b = fx.board();
    let press = |ui: &mut Ui, k: char| {
        let f = layout(&b, ui, 64, 40);
        pane::key(&b, ui, &f, Key::Char(k), now_ms())
    };
    let mut ui = Ui { selected: Some("e1".into()), ..Ui::default() };
    let Effect::Append(events) = press(&mut ui, 'y') else { panic!("y rules") };
    assert_eq!((events[0].k, events[0].str("side")), (Kind::Ruled, Some("watcher")));
    assert_eq!(press(&mut ui, 'u'), Effect::Undo);
    assert_eq!(press(&mut ui, 's'), Effect::Keep(Some("e1".into())));

    let mut ui = Ui { selected: Some("e2".into()), ..Ui::default() };
    assert_eq!(press(&mut ui, 'n'), Effect::None);
    assert!(ui.note.as_deref().unwrap().contains("answer in the Worker"));
    let Effect::Append(events) = press(&mut ui, 'd') else { panic!("d rules a blocked one") };
    assert_eq!(events[0].str("stage"), Some("blocked"));

    let fx = Fx::new();
    fx.log(Change::Add { watcher: "companion".into(), concern: concern("c3") }.event());
    fx.rebuttal_queued();
    let b = fx.board();
    let press = |ui: &mut Ui, k: char| {
        let f = layout(&b, ui, 64, 40);
        pane::key(&b, ui, &f, Key::Char(k), now_ms())
    };
    let mut ui = Ui { selected: Some("c2".into()), ..Ui::default() };
    let Effect::Append(events) = press(&mut ui, 'd') else { panic!("d rules IN DEBATE") };
    assert_eq!(
        (events[0].k, events[0].str("side"), events[0].str("stage")),
        (Kind::Ruled, Some("dismissed"), Some("debate"))
    );
    let mut ui = Ui { selected: Some("c3".into()), ..Ui::default() };
    let Effect::Append(events) = press(&mut ui, 'd') else { panic!("d dismisses OPEN") };
    assert_eq!(events[0].k, Kind::Human);
    assert_eq!(press(&mut ui, 'y'), Effect::None);
    assert!(ui.note.is_some());
}

/// V71: a turn-end wake shows the Escalations awaiting a ruling; other wakes do not.
#[test]
fn turn_end_input_shows_escalations_awaiting_a_ruling() {
    let fx = Fx::new();
    fx.escalated();
    let awaiting = fx.rulings().awaiting(&Escalations::rebuild(&fx.events())).unwrap();
    assert!(awaiting.contains("e1 Debate unresolved") && awaiting.contains("inferred_ruling"));
    let concerns = [concern("c2")];
    let ctx = Context { concerns: &concerns, debate: None, awaiting: Some(&awaiting), rulings: "", instructions: &[] };
    let history = pair_watcher::history::History::default();
    let entries = [prompt("p1", 1, "hi")];
    let (turn_end, _) = wake::normal(pair_watcher::trigger::Trigger::TurnEnd, &history, &entries, None, &ctx);
    assert!(turn_end.section("Open Debate").unwrap().contains("Awaiting the human's ruling"));
    let (checkpoint, _) = wake::normal(pair_watcher::trigger::Trigger::Checkpoint, &history, &entries, None, &ctx);
    assert!(checkpoint.section("Open Debate").is_none());

    fx.rule("e1", Side::Dismissed).unwrap();
    assert!(fx.rulings().awaiting(&Escalations::rebuild(&fx.events())).is_none());
}

/// Review #1: an arbiter answer from a view older than `u` + `n` queues the Watcher Ruling for
/// the undone `y`, never for the later `n`; it is dropped, not delivered.
#[test]
fn a_stale_answer_never_sends_a_ruling_for_a_later_worker_ruling() {
    let fx = Fx::new();
    fx.escalated();
    fx.rule("e1", Side::Watcher).unwrap();
    let events = fx.events();
    let (mut stale, mut d, mut c) = (fx.rulings(), Debate::rebuild(&events), Challenges::rebuild(&events));
    let (list, esc) = (ConcernList::rebuild(&events), Escalations::rebuild(&events));
    fx.undo();
    fx.rule("e1", Side::Worker).unwrap();
    stale.answer(&fx.mb.log(), &fx.mb.outbox(), &mut d, &mut c, &list, &esc, now_ms()).unwrap();
    let queued = &fx.op(Kind::Ruling, "queued")[0];
    assert_eq!((queued.str("id"), queued.fields["nth"].as_u64()), (Some("ruling/e1.0"), Some(0)));

    let item = fx.ruling_items().remove(0);
    assert_eq!(delivery::View::read(&fx.mb).unwrap().gate(&item), Gate::Drop);
    let view = delivery::View::read(&fx.mb).unwrap();
    assert!(delivery::claim(&fx.mb, &view, Route::Mod, now_ms()).unwrap().is_none());
    fx.answer();
    assert!(fx.ruling_items().is_empty());
    let r = fx.rulings();
    assert!(r.all()[0].undone && r.all()[0].dropped && !r.all()[1].ruled.ruling_sent);
}

/// Review #2: a prompt first drops the Ruling even under a live claim (the release does not
/// bring it back) and before the arbiter queued it.
#[test]
fn prompt_first_holds_through_a_claim_and_before_the_queue() {
    let fx = Fx::new();
    fx.escalated();
    fx.rule("e1", Side::Watcher).unwrap();
    fx.answer();
    let view = delivery::View::read(&fx.mb).unwrap();
    let claim = delivery::claim(&fx.mb, &view, Route::Mod, now_ms()).unwrap().expect("claimed");
    let t = fx.rulings().last().unwrap().t;
    let after = [prompt("p2", t + 1, "also bump the version")];
    let ticker = fx.rulings().drop_on_prompt(&fx.mb.log(), &fx.mb.outbox(), &after, now_ms()).unwrap();
    assert_eq!(ticker.len(), 1);
    assert!(fx.mb.outbox().release(&claim, now_ms()).unwrap());
    let view = delivery::View::read(&fx.mb).unwrap();
    assert!(delivery::claim(&fx.mb, &view, Route::Mod, now_ms()).unwrap().is_none(), "never after the prompt");
    fx.answer();
    assert!(fx.ruling_items().is_empty());

    // Not yet queued: dropped, and never queued.
    let fx = Fx::new();
    fx.escalated();
    fx.rule("e1", Side::Watcher).unwrap();
    let t = fx.rulings().last().unwrap().t;
    let after = [prompt("p2", t + 1, "also bump the version")];
    fx.rulings().drop_on_prompt(&fx.mb.log(), &fx.mb.outbox(), &after, now_ms()).unwrap();
    let dropped = &fx.op(Kind::Ruling, "dropped")[0];
    assert!(dropped.str("id").is_none());
    fx.answer();
    assert!(fx.ruling_items().is_empty() && fx.op(Kind::Ruling, "queued").is_empty());
}

/// Review #3: `y` IN DEBATE, a claim drops the waiting rebuttal, `u` before the arbiter
/// answers: the still-open Debate is owed its move again, not stuck on a dropped item.
#[test]
fn undo_before_the_end_recovers_a_dropped_rebuttal() {
    let fx = Fx::new();
    fx.rebuttal_queued();
    fx.rule("c2", Side::Watcher).unwrap();
    let view = delivery::View::read(&fx.mb).unwrap();
    assert!(delivery::claim(&fx.mb, &view, Route::Mod, now_ms()).unwrap().is_none());
    assert!(!fx.mb.outbox().pending().unwrap().iter().any(|i| i.id == "c2/2"), "gate dropped it");
    fx.undo();
    fx.answer();
    assert!(fx.op(Kind::Debate, "end").is_empty());
    assert_eq!(fx.op(Kind::Debate, "reopened").len(), 1);
    let d = fx.debate();
    assert_eq!(d.open().map(|o| (o.n, o.phase.clone())), Some((1, Phase::Owed(Verdict::Defend))));
    assert!(fx.ruling_items().is_empty(), "the undone Ruling is never queued");
}

/// Review #4: the undo lands between the arbiter reading the ruling and writing `debate end`;
/// replay still keeps the ended Debate for `u` and reopens it.
#[test]
fn undo_racing_the_end_still_reopens() {
    let fx = Fx::new();
    fx.rebuttal_queued();
    fx.rule("c2", Side::Worker).unwrap();
    let events = fx.events();
    let (mut d, mut c) = (Debate::rebuild(&events), Challenges::rebuild(&events));
    let (list, esc) = (ConcernList::rebuild(&events), Escalations::rebuild(&events));
    let mut stale = fx.rulings();
    fx.undo();
    stale.answer(&fx.mb.log(), &fx.mb.outbox(), &mut d, &mut c, &list, &esc, now_ms()).unwrap();
    assert_eq!(fx.op(Kind::Debate, "end").len(), 1);
    fx.answer();
    assert_eq!(fx.op(Kind::Debate, "reopened").len(), 1);
    assert_eq!(fx.board().in_debate(), Some("c2"));
}

/// Review #6: a prompt that arrived before the Escalation was raised cannot rule on it.
#[test]
fn inference_ignores_prompts_from_before_the_escalation() {
    let fx = Fx::new();
    fx.escalated();
    let raised = fx.rulings().raised("e1").unwrap();
    let entries = [prompt("p0", raised - 1, "reviewbot is broken, I'll merge it myself")];
    let lines = vec![(
        "companion".to_owned(),
        Line::InferredRuling { escalation: "e1".into(), side: OutSide::Watcher, prompt_ref: "merge it myself".into() },
    )];
    let (esc, list) = (Escalations::rebuild(&fx.events()), fx.list());
    let dropped = fx.rulings().infer(&fx.mb.log(), &lines, true, true, &entries, &esc, &list, now_ms()).unwrap();
    assert!(dropped[0].contains("since it was raised"), "{dropped:?}");
    assert!(fx.kind(Kind::Ruled).is_empty());
}
