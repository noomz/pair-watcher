//! Debate (V30–V39, V100): the bounded exchange a delivered Challenge opens. Code reads the
//! Worker's verdict line (V30), counts exchanges (V31, V33), queues the rebuttal or re-ask (I11)
//! and closes the Debate; the owner Watcher decides only what needs judgment (V32, V39). State is
//! a fold over the session event log (V93), like the Concern list and the Challenge slot.
//!
//! Per wake the Watcher process calls, in order: `Challenges::sync_sent` and
//! [`Debate::sync_sent`] (a delivery acked since the last wake is logged before its reply is
//! read), [`Debate::observe`] on the wake's entries (before the Watchers run, so the input's Open
//! Debate section shows what is owed), the Watchers, the arbiter's commit, then
//! [`Debate::decide`]. Idle routes deliver only after the latest turn end was settled, so every
//! turn end read after `sent` is the reply's or later. The Claude `Stop` hook acks before it
//! appends its record, so the wake that record triggers sees `sent` too; that wake's entries end
//! with the blocked turn's own turn end, which is skipped once (`stop-turn`).

use std::collections::{HashMap, HashSet};
use std::io;

use serde::{Deserialize, Serialize};

use crate::arbiter::Watcher;
use crate::challenge::{self, Op as ChallengeOp};
use crate::concern::{Change, ConcernList, Resolution, Severity};
use crate::event::{Event, EventLog, Kind};
use crate::history;
use crate::outbox::{Gate, Item, ItemKind, Outbox, Withdraw};
use crate::output::{Decision, Line};
use crate::tailer::{Entry, Item as TItem};

/// Exchange 2's outbox id is the Challenge id plus this.
const SECOND: &str = "/2";
/// `delivery::Route::Stop`'s name: the reply continues the blocked turn.
const STOP: &str = "stop";

/// What `decide`'s steps write with.
struct Cx<'a> {
    log: &'a EventLog,
    outbox: &'a Outbox,
    list: &'a ConcernList,
    escalate: EscalateAt,
    now: u64,
}

/// The Worker's verdict line on one exchange (V30, V33).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Concede,
    Defend,
    /// No verdict line: "no answer" (V33).
    None,
}

/// How a Debate ended (V35, V94 `debate end`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    WorkerConcedes,
    WatcherConcedes,
    Unresolved,
    Withdrawn,
    SidedWatcher,
    SidedWorker,
}

/// Why a Debate was withdrawn, not adjudicated (V35, V100).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Withdrawal {
    NoLongerApplies,
    Duplicate,
    Dismissed,
    Budget,
    SessionEnd,
}

/// Exchange 2's message (I11).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Second {
    Rebuttal,
    Reask,
}

/// Payload of a `debate` event (V94).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Op {
    /// The Worker's reply to exchange `n` (V30, V33).
    Exchange {
        challenge: String,
        n: u8,
        verdict: Verdict,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The human prompted the Worker mid-Debate: that turn goes first (V34).
    HumanPrompt { challenge: String },
    /// That turn ended without a verdict line: evidence only, never "no answer" (V34).
    HumanTurn { challenge: String },
    /// The turn a `Stop` block continued ended: its reply is the next turn end.
    StopTurn { challenge: String },
    /// The owner Watcher's move (V31, V32, V100); `defaulted` = it gave no allowed one.
    Decision {
        challenge: String,
        watcher: String,
        decision: Decision,
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        defaulted: bool,
    },
    /// Exchange 2 queued in the outbox as `item`.
    Queued { challenge: String, item: String, second: Second },
    /// Exchange 2 delivered by `route`.
    Sent { challenge: String, item: String, route: String },
    /// Frees the Challenge slot (V19). `escalated` on `unresolved` (V36); `withdrawn` says why.
    End {
        challenge: String,
        concern: String,
        outcome: Outcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        escalated: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        withdrawn: Option<Withdrawal>,
    },
}

impl Op {
    pub fn event(&self) -> Event {
        Event::typed(Kind::Debate, self)
    }
}

/// `escalate_unresolved` (I18): the least severity an unresolved Debate escalates at (V36).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscalateAt {
    Warn,
    Note,
    Off,
}

impl EscalateAt {
    /// The config value; anything else is the default `warn` (V83).
    pub fn from_config(value: &str) -> EscalateAt {
        match value {
            "note" => EscalateAt::Note,
            "off" => EscalateAt::Off,
            _ => EscalateAt::Warn,
        }
    }

    pub fn admits(self, severity: Severity) -> bool {
        match self {
            EscalateAt::Note => true,
            EscalateAt::Warn => severity == Severity::Warn,
            EscalateAt::Off => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Message `n` delivered: waiting for the Worker's reply turn.
    Awaiting,
    /// The Worker answered message `n`: the owner's move is due this wake.
    Owed(Verdict),
    /// The owner's move, logged and not yet carried out (one meaning per write, V93).
    Decided(Decision, String),
    /// Exchange 2 queued as this outbox id, waiting for the Worker's next idle (V34).
    Queued(String),
}

/// The open Debate (one per Worker: it holds the Challenge slot, V19).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Open {
    pub challenge: String,
    pub concern: String,
    /// The challenged Concern's Focus: its Watcher argues the whole Debate (V39).
    pub focus: String,
    /// 1 = the Challenge, 2 = the rebuttal or re-ask.
    pub n: u8,
    pub phase: Phase,
    /// The Worker's latest verdict reason: the rebuttal quotes it (I11), Escalation carries it (I13).
    pub reason: Option<String>,
    /// A human prompt is in the current turn (V34).
    pub human: bool,
    /// Delivered by a `Stop` block: the next turn end is the blocked turn's own.
    pub stop_turn: bool,
    /// One line per step, for the wake input.
    pub log: Vec<String>,
}

/// What one [`Debate::decide`] did.
#[derive(Debug, Default, PartialEq)]
pub struct Step {
    pub ended: Option<Outcome>,
    pub ticker: Vec<String>,
    /// Debate lines not taken, as `<watcher>: <why>`.
    pub dropped: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Debate {
    open: Option<Open>,
    /// Queued Challenges: id → (Concern, Focus); `sent` opens the Debate.
    queued: HashMap<String, (String, String)>,
    /// Concerns closed for good, with what that means for a Debate on them (V100).
    closed: HashMap<String, Closed>,
    ended: HashSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Closed {
    Fixed,
    Withdrawn(Withdrawal),
}

impl Debate {
    pub fn rebuild<'a>(events: impl IntoIterator<Item = &'a Event>) -> Debate {
        let mut d = Debate::default();
        for e in events {
            d.apply(e);
        }
        d
    }

    pub fn open(&self) -> Option<&Open> {
        self.open.as_ref()
    }

    /// The owner Watcher's move is due: the wake runs on the reserve (V48).
    pub fn owes(&self) -> bool {
        self.open.as_ref().is_some_and(|o| matches!(o.phase, Phase::Owed(_)))
    }

    /// Exchange 2's outbox id while it waits for delivery.
    pub fn waiting(&self) -> Option<&str> {
        match &self.open.as_ref()?.phase {
            Phase::Queued(item) => Some(item),
            _ => None,
        }
    }

    pub fn apply(&mut self, event: &Event) {
        match event.k {
            Kind::Challenge => match event.parse::<ChallengeOp>() {
                Some(ChallengeOp::Queued { id, concern, focus, .. }) => {
                    self.queued.insert(id, (concern, focus));
                }
                Some(ChallengeOp::Sent { id, route }) if self.open.is_none() && !self.ended.contains(&id) => {
                    if let Some((concern, focus)) = self.queued.get(&id).cloned() {
                        let log = vec![format!("1. Challenge {id} sent.")];
                        self.open = Some(Open {
                            challenge: id,
                            concern,
                            focus,
                            n: 1,
                            phase: Phase::Awaiting,
                            reason: None,
                            human: false,
                            stop_turn: route == STOP,
                            log,
                        });
                    }
                }
                _ => {}
            },
            Kind::Concern => match event.parse::<Change>() {
                Some(Change::Resolve { id, reason, .. }) => {
                    let closed = match reason {
                        Resolution::Fixed => Closed::Fixed,
                        Resolution::NoLongerApplies => Closed::Withdrawn(Withdrawal::NoLongerApplies),
                    };
                    self.closed.entry(id).or_insert(closed);
                }
                Some(Change::Duplicate { id, .. }) => {
                    self.closed.entry(id).or_insert(Closed::Withdrawn(Withdrawal::Duplicate));
                }
                _ => {}
            },
            Kind::Human if event.str("action") == Some("dismiss") => {
                if let Some(id) = event.str("id") {
                    self.closed.entry(id.to_owned()).or_insert(Closed::Withdrawn(Withdrawal::Dismissed));
                }
            }
            Kind::Debate => {
                if let Some(op) = event.parse::<Op>() {
                    self.apply_op(op);
                }
            }
            _ => {}
        }
    }

    fn apply_op(&mut self, op: Op) {
        if let Op::End { challenge, .. } = &op {
            self.ended.insert(challenge.clone());
        }
        let Some(o) = self.open.as_mut() else { return };
        let ours = |c: &str| c == o.challenge;
        match op {
            Op::Exchange { challenge, n, verdict, reason } if ours(&challenge) => {
                o.log.push(match (&verdict, &reason) {
                    (Verdict::None, _) => "   Worker: no verdict line.".to_owned(),
                    (v, r) => format!("   Worker: {} — {}", name(*v), r.as_deref().unwrap_or("")),
                });
                (o.n, o.phase, o.human) = (n, Phase::Owed(verdict), false);
                if reason.is_some() {
                    o.reason = reason;
                }
            }
            Op::HumanPrompt { challenge } if ours(&challenge) => o.human = true,
            Op::HumanTurn { challenge } if ours(&challenge) => {
                o.human = false;
                o.log.push("   Human turn (evidence only; not an answer).".to_owned());
            }
            Op::StopTurn { challenge } if ours(&challenge) => o.stop_turn = false,
            Op::Decision { challenge, decision, text, defaulted, .. } if ours(&challenge) => {
                let how = if defaulted { " (no allowed move: defaulted)" } else { "" };
                o.log.push(format!("   Watcher: {}{how} — {text}", decision_name(decision)));
                o.phase = Phase::Decided(decision, text);
            }
            Op::Queued { challenge, item, second } if ours(&challenge) => {
                o.log.push(match second {
                    Second::Rebuttal => "2. Rebuttal queued.".to_owned(),
                    Second::Reask => "2. Re-ask queued (verdict line required).".to_owned(),
                });
                (o.n, o.phase) = (2, Phase::Queued(item));
            }
            Op::Sent { challenge, item, route } if ours(&challenge) && o.phase == Phase::Queued(item.clone()) => {
                o.log.push("   Exchange 2 sent.".to_owned());
                (o.phase, o.stop_turn) = (Phase::Awaiting, route == STOP);
            }
            Op::End { challenge, .. } if ours(&challenge) => self.open = None,
            _ => {}
        }
    }

    /// Reads the Worker's turns in a wake's entries (V30, V33, V34): a verdict line for the open
    /// Challenge in a turn's final assistant text is that exchange's answer (`concede` ends the
    /// Debate: the Worker concedes); a turn the human started is evidence only; any other turn
    /// that ends while a reply is awaited is "no answer". Call on every wake, after `sync_sent`
    /// and before the Watchers run.
    pub fn observe(&mut self, log: &EventLog, outbox: &Outbox, entries: &[Entry], now: u64) -> io::Result<()> {
        // The turn's final assistant text: text after its last prompt or tool activity.
        let mut text: Vec<&str> = Vec::new();
        for item in entries.iter().flat_map(|e| &e.items) {
            let Some(open) = self.open.as_ref() else { return Ok(()) };
            match item {
                TItem::Prompt(t) if !history::is_own(t) => {
                    text.clear();
                    if !open.human {
                        let challenge = open.challenge.clone();
                        self.commit(log, &Op::HumanPrompt { challenge })?;
                    }
                }
                // Ours goes in only at idle: a human prompt still flagged had no turn of its own
                // (interrupted, a local command).
                TItem::Prompt(_) => {
                    text.clear();
                    if open.human {
                        let challenge = open.challenge.clone();
                        self.commit(log, &Op::HumanTurn { challenge })?;
                    }
                }
                TItem::ToolCall { .. } | TItem::ToolResult { .. } => text.clear(),
                TItem::Text(t) => text.push(t),
                TItem::TurnEnd => {
                    self.turn_end(log, outbox, &text.join("\n"), now)?;
                    text.clear();
                }
                TItem::ReviewStarted | TItem::ReviewDone { .. } => {}
            }
        }
        Ok(())
    }

    fn turn_end(&mut self, log: &EventLog, outbox: &Outbox, text: &str, now: u64) -> io::Result<()> {
        let Some(open) = self.open.clone() else { return Ok(()) };
        let challenge = open.challenge.clone();
        if open.phase != Phase::Awaiting {
            if open.human {
                self.commit(log, &Op::HumanTurn { challenge })?;
            }
            return Ok(());
        }
        match verdict(text, &open.challenge) {
            None if open.stop_turn => self.commit(log, &Op::StopTurn { challenge })?,
            Some((v, reason)) => {
                self.commit(log, &Op::Exchange { challenge, n: open.n, verdict: v, reason: Some(reason) })?;
                if v == Verdict::Concede {
                    self.end(log, outbox, Outcome::WorkerConcedes, None, None, now)?;
                }
            }
            None if open.human => self.commit(log, &Op::HumanTurn { challenge })?,
            None => self.commit(log, &Op::Exchange { challenge, n: open.n, verdict: Verdict::None, reason: None })?,
        }
        Ok(())
    }

    /// After the arbiter committed this wake: closes the Debate when its Concern closed (V100),
    /// else applies the owner Watcher's Debate line (V39) by phase: after a defence it rebuts
    /// once with new ground, concedes or escalates (V31, V32); after no answer code re-asks, then
    /// calls it unresolved (V33); `switched` and `concede` end it at any point. An owed move not
    /// given, or not allowed, is a concession (V100) — only when the owner's output was accepted
    /// this wake: `ran` = the Watchers whose output the arbiter took (a failed, invalid, skipped
    /// or muted owner owes it still). Unresolved escalates only at or above `escalate` (V36).
    /// Nothing goes back to the Worker on close-out (V37).
    #[allow(clippy::too_many_arguments)]
    pub fn decide(
        &mut self,
        log: &EventLog,
        outbox: &Outbox,
        list: &ConcernList,
        ran: &[Watcher],
        lines: &[(String, Line)],
        escalate: EscalateAt,
        now: u64,
    ) -> io::Result<Step> {
        let cx = Cx { log, outbox, list, escalate, now };
        let mut step = Step::default();
        let debate = lines.iter().filter_map(|(w, l)| match l {
            Line::Debate { decision, text } => Some((w, *decision, text.as_str())),
            _ => None,
        });
        let Some(open) = self.open.clone() else {
            step.dropped.extend(debate.map(|(w, ..)| format!("{w}: no open Debate")));
            return Ok(step);
        };
        let owner = ran.iter().find(|w| w.focuses.contains(&open.focus)).map(|w| w.name.as_str());
        let mut mine = None;
        for (w, decision, text) in debate {
            if Some(w.as_str()) != owner {
                step.dropped.push(format!("{w}: Debate on {} is Focus {}'s", open.challenge, open.focus));
            } else if mine.is_some() {
                step.dropped.push(format!("{w}: one Debate move per wake"));
            } else {
                mine = Some((w.clone(), decision, text.to_owned()));
            }
        }

        if let Some(closed) = self.closed.get(&open.concern).copied() {
            step.dropped.extend(mine.map(|(w, ..)| format!("{w}: {} closed", open.concern)));
            let (outcome, why) = match closed {
                Closed::Fixed => (Outcome::WorkerConcedes, None),
                Closed::Withdrawn(w) => (Outcome::Withdrawn, Some(w)),
            };
            self.conclude(&cx, outcome, why, &mut step)?;
            return Ok(step);
        }
        let owed = match &open.phase {
            // A move logged before a crash is carried out first.
            Phase::Decided(d, text) => {
                step.dropped.extend(mine.map(|(w, ..)| format!("{w}: a move is already logged")));
                let (d, text) = (*d, text.clone());
                self.carry_out(&cx, d, &text, &mut step)?;
                return Ok(step);
            }
            Phase::Owed(v) => Some(*v),
            Phase::Awaiting | Phase::Queued(_) => None,
        };
        let watcher = owner.unwrap_or("arbiter").to_owned();
        let mv = mine.as_ref().map(|(_, d, t)| (*d, t.as_str()));
        let (decision, text, defaulted) = match (owed, mv) {
            // `observe` ends it at once; only a failed write leaves it owed.
            (Some(Verdict::Concede), _) => {
                return self.conclude(&cx, Outcome::WorkerConcedes, None, &mut step).map(|()| step);
            }
            // Actions override words, and the Watcher may drop its objection, at any point.
            (_, Some((d @ (Decision::Switched | Decision::Concede), text))) => (d, text, false),
            (Some(Verdict::Defend), Some((Decision::Rebut, text))) if open.n == 1 => (Decision::Rebut, text, false),
            (Some(Verdict::Defend), Some((Decision::Escalate, text))) => (Decision::Escalate, text, false),
            (Some(Verdict::None), Some((Decision::Escalate, text))) if open.n == 2 => (Decision::Escalate, text, false),
            (Some(Verdict::None), None) if open.n == 1 => {
                self.second(&cx, &open.challenge, Second::Reask, challenge::reask(&open.challenge))?;
                return Ok(step);
            }
            (Some(Verdict::None), None) => {
                return self.conclude(&cx, Outcome::Unresolved, None, &mut step).map(|()| step);
            }
            // The owner did not run (failed, invalid, skipped, muted): the move stays owed.
            (Some(_), None) if owner.is_none() => return Ok(step),
            // Owed and missing, or not allowed here (V100).
            (Some(_), m) => {
                if let Some((d, _)) = m {
                    step.dropped.push(format!("{watcher}: {} not allowed at exchange {}", decision_name(d), open.n));
                }
                (Decision::Concede, m.map_or("", |(_, t)| t), true)
            }
            (None, Some((d, _))) => {
                step.dropped.push(format!("{watcher}: {} with no reply owed", decision_name(d)));
                return Ok(step);
            }
            (None, None) => return Ok(step),
        };
        let op =
            Op::Decision { challenge: open.challenge.clone(), watcher, decision, text: text.to_owned(), defaulted };
        self.commit(log, &op)?;
        self.carry_out(&cx, decision, text, &mut step)?;
        Ok(step)
    }

    /// The logged move's effect: the rebuttal queued, or the end it means.
    fn carry_out(&mut self, cx: &Cx, decision: Decision, text: &str, step: &mut Step) -> io::Result<()> {
        let outcome = match decision {
            Decision::Concede => Outcome::WatcherConcedes,
            Decision::Switched => Outcome::WorkerConcedes,
            Decision::Escalate => Outcome::Unresolved,
            Decision::Rebut => {
                let open = self.open.clone().expect("carry_out runs on an open Debate");
                let worker = open.reason.as_deref().unwrap_or("");
                let text = challenge::rebuttal(&open.challenge, worker, text);
                return self.second(cx, &open.challenge, Second::Rebuttal, text);
            }
        };
        self.conclude(cx, outcome, None, step)
    }

    /// Ends it as `outcome`; unresolved escalates only at or above the gate (V36).
    fn conclude(&mut self, cx: &Cx, outcome: Outcome, why: Option<Withdrawal>, step: &mut Step) -> io::Result<()> {
        let Some(open) = self.open.clone() else { return Ok(()) };
        let escalated = (outcome == Outcome::Unresolved).then(|| {
            let severity = cx.list.get(&open.concern).map_or(Severity::Note, |c| c.severity);
            cx.escalate.admits(severity)
        });
        if escalated == Some(false) {
            step.ticker.push(format!("Challenge {}: unresolved, not escalated", open.challenge));
        }
        self.end(cx.log, cx.outbox, outcome, escalated, why, cx.now)?;
        step.ended = Some(outcome);
        Ok(())
    }

    /// Ends the open Debate without a ruling on it: budget (V35, V48) or session end.
    pub fn withdraw(&mut self, log: &EventLog, outbox: &Outbox, why: Withdrawal, now: u64) -> io::Result<bool> {
        if self.open.is_none() {
            return Ok(false);
        }
        self.end(log, outbox, Outcome::Withdrawn, None, Some(why), now)?;
        Ok(true)
    }

    /// Logs `debate sent {route}` once the outbox has exchange 2 acked (V28, V43). Returns the
    /// route it logged.
    pub fn sync_sent(&mut self, log: &EventLog, outbox: &Outbox) -> io::Result<Option<String>> {
        let (Some(open), Some(item)) = (self.open.as_ref(), self.waiting()) else { return Ok(None) };
        let (challenge, item) = (open.challenge.clone(), item.to_owned());
        let Some(d) = outbox.delivered()?.into_iter().find(|d| d.item.id == item) else { return Ok(None) };
        self.commit(log, &Op::Sent { challenge, item, route: d.route.clone() })?;
        Ok(Some(d.route))
    }

    /// The outbox claim's gate for exchange-2 items; `None` for any other item. One whose Debate
    /// ended, or whose Concern closed since the last wake, is dropped (V34, V37); one this view
    /// has not seen queued is skipped (it may be behind).
    pub fn gate(&self, item: &Item) -> Option<Gate> {
        let challenge = item.id.strip_suffix(SECOND).filter(|_| item.kind == ItemKind::Challenge)?;
        let closed =
            self.open.as_ref().is_some_and(|o| o.challenge == challenge && self.closed.contains_key(&o.concern));
        Some(if self.ended.contains(challenge) || closed {
            Gate::Drop
        } else if self.waiting() == Some(item.id.as_str()) {
            Gate::Deliver
        } else {
            Gate::Skip
        })
    }

    /// The wake input's Open Debate section (V7): the exchanges so far and what is due, the same
    /// for every Watcher (V9); only the owner may reply (V39).
    pub fn render(&self, list: &ConcernList) -> Option<String> {
        let o = self.open.as_ref()?;
        let headline = list.get(&o.concern).map_or("", |c| c.headline.as_str());
        let mut out = vec![format!(
            "Challenge {} on {} \"{headline}\". Only the {} Watcher may reply, with one `debate` line.",
            o.challenge, o.concern, o.focus
        )];
        out.extend(o.log.iter().cloned());
        out.push(
            match (&o.phase, o.n) {
                (Phase::Owed(Verdict::Defend), 1) => {
                    "Due: concede (the defence refutes the evidence or names a missed constraint) | rebut (only \
                     with new ground answering its reason) | escalate (real trade-off only) | switched (its \
                     actions changed approach)."
                }
                (Phase::Owed(Verdict::Defend), _) => {
                    "Due, last word (no third message): concede | escalate (real trade-off only) | switched."
                }
                (Phase::Owed(_), 1) => "No verdict line: code re-asks, unless its actions settle it (switched).",
                (Phase::Owed(_), _) => "Still no verdict line: unresolved, unless its actions settle it (switched).",
                (Phase::Decided(..), _) => "The owner's move is logged; code carries it out.",
                (Phase::Queued(_), _) => "Exchange 2 waits for the Worker's next idle.",
                (Phase::Awaiting, _) => "Waiting for the Worker's verdict line.",
            }
            .to_owned(),
        );
        Some(out.join("\n"))
    }

    /// Queues exchange 2 (I11): an orphan left by a crash before `queued` was logged is withdrawn
    /// first; if the event cannot be logged the item is withdrawn.
    fn second(&mut self, cx: &Cx, challenge: &str, second: Second, text: String) -> io::Result<()> {
        let id = format!("{challenge}{SECOND}");
        match cx.outbox.withdraw(&id, "orphan", cx.now)? {
            Withdraw::Acked | Withdraw::Claimed => {
                return Err(io::Error::other(format!("{id} is being delivered outside the Debate")));
            }
            Withdraw::Withdrawn | Withdraw::AlreadyDropped | Withdraw::Missing => {}
        }
        if !cx.outbox.queue(Item { id: id.clone(), kind: ItemKind::Challenge, text }, cx.now)? {
            return Err(io::Error::other(format!("outbox already holds {id}")));
        }
        let op = Op::Queued { challenge: challenge.to_owned(), item: id.clone(), second };
        if let Err(e) = self.commit(cx.log, &op) {
            let _ = cx.outbox.withdraw(&id, "unlogged", cx.now);
            return Err(e);
        }
        Ok(())
    }

    /// Logs `end`, then withdraws a waiting exchange 2: close-out never reaches the Worker (V37).
    /// The end comes first, so a crash between the two never leaves an open Debate waiting on a
    /// withdrawn item; the gate drops what the withdraw missed.
    fn end(
        &mut self,
        log: &EventLog,
        outbox: &Outbox,
        outcome: Outcome,
        escalated: Option<bool>,
        withdrawn: Option<Withdrawal>,
        now: u64,
    ) -> io::Result<()> {
        let Some(open) = self.open.clone() else { return Ok(()) };
        let item = self.waiting().map(str::to_owned);
        self.commit(log, &Op::End { challenge: open.challenge, concern: open.concern, outcome, escalated, withdrawn })?;
        if let Some(item) = item {
            outbox.withdraw(&item, "debate ended", now)?;
        }
        Ok(())
    }

    fn commit(&mut self, log: &EventLog, op: &Op) -> io::Result<()> {
        let event = op.event();
        log.append(event.clone())?;
        self.apply(&event);
        Ok(())
    }
}

/// The last line in `text` naming `Challenge <id>:` (V30), parsed alone, as its verdict +
/// one-line reason; that line not a verdict = none. Other ids are ignored; markup (`**`,
/// backticks) does not count. A line offering both words (the template echoed back) is no verdict.
pub fn verdict(text: &str, id: &str) -> Option<(Verdict, String)> {
    // ASCII folding keeps byte offsets, so `at` indexes `clean` too.
    let head = format!("challenge {}:", id.to_ascii_lowercase());
    let found = text.lines().rev().find_map(|line| {
        let clean: String = line.chars().filter(|c| !matches!(c, '*' | '`')).collect();
        let lower = clean.to_ascii_lowercase();
        let at = lower
            .match_indices(&head)
            .map(|(i, _)| i)
            .find(|&i| !clean[..i].chars().last().is_some_and(char::is_alphanumeric))?;
        Some(clean[at + head.len()..].trim_start().to_owned())
    })?;
    let word: String = found.chars().take_while(|c| c.is_alphabetic()).collect();
    let v = match word.to_lowercase().as_str() {
        "concede" | "concedes" | "conceded" | "conceding" => Verdict::Concede,
        "defend" | "defends" | "defended" | "defending" => Verdict::Defend,
        _ => return None,
    };
    let after = found[word.len()..].trim_start();
    let next: String = after.chars().take_while(|c| c.is_alphabetic()).collect();
    if after.starts_with(['|', '/']) || next.eq_ignore_ascii_case("or") {
        return None;
    }
    let reason = after.trim_start_matches(['—', '–', '-', ':', ',', ' ']).trim();
    Some((v, reason.to_owned()))
}

fn name(v: Verdict) -> &'static str {
    match v {
        Verdict::Concede => "concede",
        Verdict::Defend => "defend",
        Verdict::None => "none",
    }
}

fn decision_name(d: Decision) -> &'static str {
    match d {
        Decision::Concede => "concede",
        Decision::Rebut => "rebut",
        Decision::Escalate => "escalate",
        Decision::Switched => "switched",
    }
}
