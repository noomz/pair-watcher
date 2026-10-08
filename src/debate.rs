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
use crate::ruling::{Live, Side, Standing};
use crate::tailer::{Entry, Item as TItem};

/// Exchange 2's outbox id is the Challenge id plus this.
const SECOND: &str = "/2";
/// `delivery::Route::Stop`'s name: the reply continues the blocked turn.
const STOP: &str = "stop";
/// Owed wakes the owner may miss before its move counts as missing (V100; as V88's two strikes).
const OWNER_WAKES: u8 = 2;

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
    /// A human turn came while the Worker's reply was awaited (V34).
    HumanWentFirst,
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
    /// The Worker's reply to exchange `n` (V30, V33). `entry` = the transcript entry holding the
    /// turn end it read, so a replayed slice never reads it twice (V10, V92).
    Exchange {
        challenge: String,
        n: u8,
        verdict: Verdict,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default)]
        entry: String,
        /// The turn's earlier entries in the same slice: a replay skips them too.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        turn: Vec<String>,
    },
    /// The human prompted the Worker mid-Debate: that turn goes first (V34).
    HumanPrompt { challenge: String },
    /// That turn ended without a verdict line: evidence only, never "no answer" (V34). `entry`
    /// empty = a human prompt with no turn of its own.
    HumanTurn {
        challenge: String,
        #[serde(default)]
        entry: String,
        /// The turn's earlier entries in the same slice: a replay skips them too.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        turn: Vec<String>,
    },
    /// The turn a `Stop` block continued ended: its reply is the next turn end.
    StopTurn {
        challenge: String,
        #[serde(default)]
        entry: String,
        /// The turn's earlier entries in the same slice: a replay skips them too.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        turn: Vec<String>,
    },
    /// A slice ended mid-turn on assistant text holding a verdict line: the turn end in a later
    /// slice reads it (empty = no such text pending). `turn` = the entries read for it.
    Partial {
        challenge: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        turn: Vec<String>,
    },
    /// The owner Watcher owed a move and did not run this wake (failed, invalid, skipped, muted).
    OwnerAbsent { challenge: String },
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
    /// The human undid the ruling that ended it (V75): owed again on the Worker's last answer,
    /// unless its exchange 2 was never withdrawn (`kept`: still queued, its `sent` follows).
    Reopened {
        challenge: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        kept: bool,
    },
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
    /// Owed wakes in a row the owner did not run (V100).
    pub absent: u8,
    /// A human turn ended while the reply was awaited: withdrawn unless `switched` (V34).
    pub preempted: bool,
    /// Entries of turns already read.
    pub seen: HashSet<String>,
    /// Verdict-bearing text of the turn in progress, read in an earlier slice.
    pub partial: String,
    /// One line per step, for the wake input.
    pub log: Vec<String>,
    /// The Worker's last answer: exchange and verdict. A reopened Debate is owed on it (V75).
    pub last: Option<(u8, Verdict)>,
}

impl Open {
    /// A turn end was read: its entries are never read again, and its text is spent.
    fn read(&mut self, entry: String, turn: Vec<String>) {
        self.seen.insert(entry);
        self.seen.extend(turn);
        self.partial.clear();
    }
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
    /// The Debate a ruling ended, until another Challenge is queued: `u` may reopen it (V75).
    ruled_end: Option<Open>,
    rulings: Live,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Closed {
    Fixed,
    Withdrawn(Withdrawal),
    /// The human ruled (V73); undone, it is no longer closed (V75).
    Ruled(Side),
}

impl Closed {
    /// How a Debate on a Concern closed this way ends (V35, V73, V100).
    fn outcome(self) -> (Outcome, Option<Withdrawal>) {
        match self {
            Closed::Fixed => (Outcome::WorkerConcedes, None),
            Closed::Withdrawn(w) => (Outcome::Withdrawn, Some(w)),
            Closed::Ruled(Side::Watcher) => (Outcome::SidedWatcher, None),
            Closed::Ruled(Side::Worker) => (Outcome::SidedWorker, None),
            Closed::Ruled(Side::Dismissed) => (Outcome::Withdrawn, Some(Withdrawal::Dismissed)),
        }
    }
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

    /// The owner Watcher's move is due, or a logged one is still to carry out: the wake runs on
    /// the reserve (V48), and `decide` must run even when no Watcher may.
    pub fn owes(&self) -> bool {
        self.open.as_ref().is_some_and(|o| matches!(o.phase, Phase::Owed(_) | Phase::Decided(..)))
    }

    /// Exchange 2's outbox id while it waits for delivery.
    pub fn waiting(&self) -> Option<&str> {
        match &self.open.as_ref()?.phase {
            Phase::Queued(item) => Some(item),
            _ => None,
        }
    }

    pub fn apply(&mut self, event: &Event) {
        match self.rulings.apply(event) {
            Some(Standing::Ruled { concern: Some(c), side, .. }) => {
                self.closed.entry(c).or_insert(Closed::Ruled(side));
            }
            Some(Standing::Undone { concern: Some(c), .. }) => {
                if matches!(self.closed.get(&c), Some(Closed::Ruled(_))) {
                    self.closed.remove(&c);
                }
            }
            _ => {}
        }
        match event.k {
            Kind::Challenge => match event.parse::<ChallengeOp>() {
                Some(ChallengeOp::Queued { id, concern, focus, .. }) => {
                    // The slot moved on: a ruled Debate can no longer come back.
                    self.ruled_end = None;
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
                            absent: 0,
                            preempted: false,
                            seen: HashSet::new(),
                            partial: String::new(),
                            log,
                            last: None,
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
        if let Op::Reopened { challenge, kept } = &op {
            self.reopen_from_ruling(challenge, *kept);
            return;
        }
        let Some(o) = self.open.as_mut() else { return };
        let ours = |c: &str| c == o.challenge;
        match op {
            Op::Exchange { challenge, n, verdict, reason, entry, turn } if ours(&challenge) => {
                o.read(entry, turn);
                o.log.push(match (&verdict, &reason) {
                    (Verdict::None, _) => "   Worker: no verdict line.".to_owned(),
                    (v, r) => format!("   Worker: {} — {}", name(*v), r.as_deref().unwrap_or("")),
                });
                // An answer came after all: adjudicated.
                (o.n, o.phase, o.human, o.preempted) = (n, Phase::Owed(verdict), false, false);
                o.last = Some((n, verdict));
                if reason.is_some() {
                    o.reason = reason;
                }
            }
            Op::HumanPrompt { challenge } if ours(&challenge) => o.human = true,
            Op::HumanTurn { challenge, entry, turn } if ours(&challenge) => {
                // A turn of its own ended while the reply was awaited; a pending exchange 2 waits.
                o.preempted |= !entry.is_empty() && o.phase == Phase::Awaiting;
                o.read(entry, turn);
                o.human = false;
                o.log.push("   Human turn (evidence only; not an answer).".to_owned());
            }
            // The blocked turn's own prompt is not a human turn mid-Debate.
            Op::StopTurn { challenge, entry, turn } if ours(&challenge) => {
                o.read(entry, turn);
                (o.stop_turn, o.human) = (false, false);
            }
            Op::Partial { challenge, text, turn } if ours(&challenge) => {
                o.seen.extend(turn);
                o.partial = text;
            }
            Op::OwnerAbsent { challenge } if ours(&challenge) => o.absent += 1,
            Op::Decision { challenge, decision, text, defaulted, .. } if ours(&challenge) => {
                o.absent = 0;
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
            // Only a ruling ends it sided or dismissed (V73): kept for `u`, even when the undo
            // was logged before this end (V75).
            Op::End { challenge, outcome, withdrawn, .. } if ours(&challenge) => {
                let ruled = matches!(outcome, Outcome::SidedWatcher | Outcome::SidedWorker)
                    || withdrawn == Some(Withdrawal::Dismissed);
                let open = self.open.take();
                self.ruled_end = if ruled { open } else { None };
            }
            _ => {}
        }
    }

    /// Reads the Worker's turns in a wake's entries (V30, V33, V34): a verdict line for the open
    /// Challenge in a turn's final assistant text is that exchange's answer (`concede` ends the
    /// Debate: the Worker concedes); a turn the human started is evidence only; any other turn
    /// that ends while a reply is awaited is "no answer". Call on every wake, after `sync_sent`
    /// and before the Watchers run.
    pub fn observe(&mut self, log: &EventLog, outbox: &Outbox, entries: &[Entry], now: u64) -> io::Result<()> {
        // The turn's final assistant text: text after its last prompt or tool activity, part of
        // it maybe read in an earlier slice.
        let seed = self.open.as_ref().map(|o| o.partial.clone()).unwrap_or_default();
        let mut text: Vec<&str> = Some(seed.as_str()).filter(|t| !t.is_empty()).into_iter().collect();
        // The turn's entries so far in this slice.
        let mut turn: Vec<String> = Vec::new();
        for entry in entries {
            // Read once: a replayed slice holds turns already read.
            if self.open.as_ref().is_some_and(|o| o.seen.contains(&entry.id)) {
                continue;
            }
            for item in &entry.items {
                let Some(open) = self.open.as_ref() else { return Ok(()) };
                let challenge = open.challenge.clone();
                match item {
                    TItem::Prompt(t) if !history::is_own(t) => {
                        text.clear();
                        // A blocked turn's own prompt came before the delivery.
                        if !open.human && !open.stop_turn {
                            self.commit(log, &Op::HumanPrompt { challenge })?;
                        }
                    }
                    // Ours goes in only at idle: a human prompt still flagged had no turn of its
                    // own (interrupted, a local command).
                    TItem::Prompt(_) => {
                        text.clear();
                        if open.human {
                            self.commit(log, &Op::HumanTurn { challenge, entry: String::new(), turn: vec![] })?;
                        }
                    }
                    TItem::ToolCall { .. } | TItem::ToolResult { .. } => text.clear(),
                    TItem::Text(t) => text.push(t),
                    TItem::TurnEnd => {
                        let turn = std::mem::take(&mut turn);
                        self.turn_end(log, outbox, &text.join("\n"), &entry.id, turn, now)?;
                        text.clear();
                    }
                    TItem::ReviewStarted | TItem::ReviewDone { .. } => {}
                }
            }
            if !entry.items.contains(&TItem::TurnEnd) {
                turn.push(entry.id.clone());
            }
        }
        // A slice cut mid-turn keeps a verdict line for the turn end in a later slice, and the
        // entries it read, so a replay never reads them again.
        let Some(open) = self.open.as_ref() else { return Ok(()) };
        let joined = text.join("\n");
        let partial = if open.phase == Phase::Awaiting && verdict(&joined, &open.challenge).is_some() {
            joined
        } else {
            String::new()
        };
        if partial != open.partial || !turn.is_empty() {
            let challenge = open.challenge.clone();
            self.commit(log, &Op::Partial { challenge, text: partial, turn })?;
        }
        Ok(())
    }

    fn turn_end(
        &mut self,
        log: &EventLog,
        outbox: &Outbox,
        text: &str,
        entry: &str,
        turn: Vec<String>,
        now: u64,
    ) -> io::Result<()> {
        let Some(open) = self.open.clone() else { return Ok(()) };
        let (challenge, entry) = (open.challenge.clone(), entry.to_owned());
        if open.phase != Phase::Awaiting {
            if open.human {
                self.commit(log, &Op::HumanTurn { challenge, entry, turn })?;
            }
            return Ok(());
        }
        match verdict(text, &open.challenge) {
            None if open.stop_turn => self.commit(log, &Op::StopTurn { challenge, entry, turn })?,
            Some((v, reason)) => {
                let n = open.n;
                self.commit(log, &Op::Exchange { challenge, n, verdict: v, reason: Some(reason), entry, turn })?;
                if v == Verdict::Concede {
                    self.end(log, outbox, Outcome::WorkerConcedes, None, None, now)?;
                }
            }
            None if open.human => self.commit(log, &Op::HumanTurn { challenge, entry, turn })?,
            None => {
                let n = open.n;
                self.commit(log, &Op::Exchange { challenge, n, verdict: Verdict::None, reason: None, entry, turn })?
            }
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
            let (outcome, why) = closed.outcome();
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
            // The human went first: withdrawn, not adjudicated, unless the owner judged a switch
            // (V34).
            (None, m)
                if open.preempted && open.phase == Phase::Awaiting && !matches!(m, Some((Decision::Switched, _))) =>
            {
                if let Some((d, _)) = m {
                    step.dropped.push(format!("{watcher}: {} after the human went first", decision_name(d)));
                }
                self.conclude(&cx, Outcome::Withdrawn, Some(Withdrawal::HumanWentFirst), &mut step)?;
                return Ok(step);
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
            // The owner did not run (failed, invalid, skipped, muted): the move stays owed, for at
            // most OWNER_WAKES owed wakes; then it is missing (V100).
            (Some(_), None) if owner.is_none() && open.absent + 1 < OWNER_WAKES => {
                self.commit(log, &Op::OwnerAbsent { challenge: open.challenge.clone() })?;
                return Ok(step);
            }
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

    /// The human ruled on the open Debate's Concern: it ends at once, sided or withdrawn, and a
    /// queued exchange 2 is withdrawn (V73). Returns the `end` event written.
    pub fn end_ruled(&mut self, log: &EventLog, outbox: &Outbox, now: u64) -> io::Result<Option<Event>> {
        let Some(open) = self.open.as_ref() else { return Ok(None) };
        let Some(closed @ Closed::Ruled(_)) = self.closed.get(&open.concern).copied() else { return Ok(None) };
        let (outcome, why) = closed.outcome();
        self.end(log, outbox, outcome, None, why, now).map(Some)
    }

    /// `u` undid the ruling on `challenge`'s Debate (V75): reopens the Debate the ruling ended,
    /// unless another Challenge was queued since; or, when the undo came before the end, owes the
    /// still-open Debate its move again if the ruling cost it its queued exchange 2 (the gate
    /// dropped it). Returns the `reopened` event written.
    pub fn reopen(&mut self, log: &EventLog, outbox: &Outbox, challenge: &str) -> io::Result<Option<Event>> {
        let Some(o) = self
            .ruled_end
            .as_ref()
            .filter(|o| self.open.is_none() && o.challenge == challenge && !self.closed.contains_key(&o.concern))
        else {
            return Ok(None);
        };
        // The end could not withdraw exchange 2 under a live claim: still pending or delivered,
        // it stays out; only a withdrawn one is re-decided.
        let kept = match &o.phase {
            Phase::Queued(item) => {
                outbox.pending()?.iter().any(|i| i.id == *item)
                    || outbox.delivered()?.iter().any(|d| d.item.id == *item)
            }
            _ => false,
        };
        let event = Op::Reopened { challenge: challenge.to_owned(), kept }.event();
        log.append(event.clone())?;
        self.apply(&event);
        Ok(Some(event))
    }

    /// Restores the Debate a ruling ended, owed again on the Worker's last answer: a logged move
    /// or a withdrawn exchange 2 was cancelled with the ruling, so the owner decides again. An
    /// exchange 2 `kept` stays queued.
    fn reopen_from_ruling(&mut self, challenge: &str, kept: bool) {
        if self.open.is_some() {
            return;
        }
        let Some(mut o) = self.ruled_end.take().filter(|o| o.challenge == challenge) else { return };
        let cancelled = match o.phase {
            Phase::Queued(_) => !kept,
            Phase::Decided(..) => true,
            _ => false,
        };
        if cancelled {
            (o.n, o.phase) = match o.last {
                Some((n, v)) => (n, Phase::Owed(v)),
                None => (o.n, Phase::Awaiting),
            };
        }
        o.absent = 0;
        o.log.push("   Ruling undone: the Debate resumes.".to_owned());
        self.ended.remove(challenge);
        self.open = Some(o);
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
        let closed = self.open.as_ref().filter(|o| o.challenge == challenge).and_then(|o| self.closed.get(&o.concern));
        Some(if self.ended.contains(challenge) || matches!(closed, Some(Closed::Fixed | Closed::Withdrawn(_))) {
            Gate::Drop
        } else if closed.is_some() {
            // A ruling stands: held until the arbiter ends the Debate, or `u` lifts it (V73, V75).
            Gate::Skip
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
                (Phase::Awaiting, _) if o.preempted => {
                    "The human went first: withdrawn this wake, unless its actions changed approach (switched)."
                }
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

    /// Logs `end`, then withdraws any undelivered exchange 2: close-out never reaches the Worker
    /// (V37). The end comes first, so a crash between the two never leaves an open Debate waiting
    /// on a withdrawn item.
    fn end(
        &mut self,
        log: &EventLog,
        outbox: &Outbox,
        outcome: Outcome,
        escalated: Option<bool>,
        withdrawn: Option<Withdrawal>,
        now: u64,
    ) -> io::Result<Event> {
        let open = self.open.clone().expect("end runs on an open Debate");
        let item = format!("{}{SECOND}", open.challenge);
        let op = Op::End { challenge: open.challenge, concern: open.concern, outcome, escalated, withdrawn };
        self.commit(log, &op)?;
        // Best effort, whatever the phase (a logged move may sit over a queued item): the end
        // stands, and the gate drops what this misses.
        let _ = outbox.withdraw(&item, "debate ended", now);
        Ok(op.event())
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
/// backticks) does not count. A line offering both words before its reason (the template echoed
/// back) is no verdict.
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
    // Both words offered before the reason (`concede | defend`, `defend, or concede`): no verdict.
    let head = after.split(['—', '–', ':']).next().unwrap_or("").split(" - ").next().unwrap_or("");
    let other = if v == Verdict::Concede { "defend" } else { "concede" };
    if after.starts_with('|') || head.to_ascii_lowercase().contains(other) {
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
