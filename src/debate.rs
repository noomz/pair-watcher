//! Debate (V30–V39, V100): the bounded exchange a delivered Challenge opens. Code reads the
//! Worker's verdict line (V30), counts exchanges (V31, V33), queues the rebuttal or re-ask (I11)
//! and closes the Debate; the owner Watcher decides only what needs judgment (V32, V39). State is
//! a fold over the session event log (V93), like the Concern list and the Challenge slot.
//!
//! Per turn-end wake the Watcher process calls, in order: [`Debate::observe`] on the wake's
//! entries (before the wake, so the input's Open Debate section shows what is owed; before
//! `sync_sent`, so a turn that ended before delivery is never read as the reply), the Watchers,
//! the arbiter's commit, [`Debate::decide`], then [`Debate::sync_sent`] with the Challenge's.

use std::collections::{HashMap, HashSet};
use std::io;

use serde::{Deserialize, Serialize};

use crate::arbiter::Watcher;
use crate::challenge::{self, Op as ChallengeOp};
use crate::concern::{Change, ConcernList, Resolution, Severity};
use crate::event::{Event, EventLog, Kind};
use crate::history::OWN_PREFIX;
use crate::outbox::{Gate, Item, ItemKind, Outbox};
use crate::output::{Decision, Line};
use crate::tailer::{Entry, Item as TItem};

/// Exchange 2's outbox id is the Challenge id plus this.
const SECOND: &str = "/2";

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
                Some(ChallengeOp::Sent { id, .. }) if self.open.is_none() && !self.ended.contains(&id) => {
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
            Op::Decision { challenge, decision, text, defaulted, .. } if ours(&challenge) => {
                let how = if defaulted { " (no allowed move: defaulted)" } else { "" };
                o.log.push(format!("   Watcher: {}{how} — {text}", decision_name(decision)));
            }
            Op::Queued { challenge, item, second } if ours(&challenge) => {
                o.log.push(match second {
                    Second::Rebuttal => "2. Rebuttal queued.".to_owned(),
                    Second::Reask => "2. Re-ask queued (verdict line required).".to_owned(),
                });
                (o.n, o.phase) = (2, Phase::Queued(item));
            }
            Op::Sent { challenge, item, .. } if ours(&challenge) && o.phase == Phase::Queued(item.clone()) => {
                o.log.push("   Exchange 2 sent.".to_owned());
                o.phase = Phase::Awaiting;
            }
            Op::End { challenge, .. } if ours(&challenge) => self.open = None,
            _ => {}
        }
    }

    /// Reads the Worker's turns in a wake's entries (V30, V33, V34): a verdict line for the open
    /// Challenge in a turn's final assistant text is that exchange's answer (`concede` ends the
    /// Debate: the Worker concedes); a turn the human started is evidence only; any other turn
    /// that ends while a reply is awaited is "no answer". Call on every wake, before it runs.
    pub fn observe(&mut self, log: &EventLog, outbox: &Outbox, entries: &[Entry], now: u64) -> io::Result<()> {
        // The turn's final assistant text: text after its last prompt or tool activity.
        let mut text: Vec<&str> = Vec::new();
        for item in entries.iter().flat_map(|e| &e.items) {
            let Some(open) = self.open.as_ref() else { return Ok(()) };
            match item {
                // Delivered Challenges, Rulings and Stop-block feedback carry our header.
                TItem::Prompt(t) if !t.contains(OWN_PREFIX) => {
                    text.clear();
                    if !open.human {
                        let challenge = open.challenge.clone();
                        self.commit(log, &Op::HumanPrompt { challenge })?;
                    }
                }
                TItem::Prompt(_) | TItem::ToolCall { .. } | TItem::ToolResult { .. } => text.clear(),
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
    /// given, or not allowed, is a concession (V100). Unresolved escalates only at or above
    /// `escalate` (V36). Nothing goes back to the Worker on close-out (V37).
    #[allow(clippy::too_many_arguments)]
    pub fn decide(
        &mut self,
        log: &EventLog,
        outbox: &Outbox,
        list: &ConcernList,
        roster: &[Watcher],
        lines: &[(String, Line)],
        escalate: EscalateAt,
        now: u64,
    ) -> io::Result<Step> {
        let mut step = Step::default();
        let debate = lines.iter().filter_map(|(w, l)| match l {
            Line::Debate { decision, text } => Some((w, *decision, text.as_str())),
            _ => None,
        });
        let Some(open) = self.open.clone() else {
            step.dropped.extend(debate.map(|(w, ..)| format!("{w}: no open Debate")));
            return Ok(step);
        };
        let owner = roster.iter().find(|w| w.focuses.contains(&open.focus)).map(|w| w.name.as_str());
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
            let (outcome, why) = match closed {
                Closed::Fixed => (Outcome::WorkerConcedes, None),
                Closed::Withdrawn(w) => (Outcome::Withdrawn, Some(w)),
            };
            step.dropped.extend(mine.map(|(w, ..)| format!("{w}: {} closed", open.concern)));
            self.end(log, outbox, outcome, None, why, now)?;
            step.ended = Some(outcome);
            return Ok(step);
        }

        let owed = match &open.phase {
            Phase::Owed(v) => Some(*v),
            Phase::Awaiting | Phase::Queued(_) => None,
        };
        let watcher = owner.unwrap_or("arbiter").to_owned();
        let challenge = open.challenge.clone();
        let decided = |log: &EventLog, d: &mut Debate, decision, text: &str, defaulted| {
            let op = Op::Decision {
                challenge: challenge.clone(),
                watcher: watcher.clone(),
                decision,
                text: text.to_owned(),
                defaulted,
            };
            d.commit(log, &op)
        };
        let outcome = match (owed, mine.as_ref().map(|(_, d, t)| (*d, t.as_str()))) {
            // `observe` ends it at once; only a failed write leaves it owed.
            (Some(Verdict::Concede), _) => Some(Outcome::WorkerConcedes),
            // Actions override words, and the Watcher may drop its objection, at any point.
            (_, Some((d @ (Decision::Switched | Decision::Concede), text))) => {
                decided(log, self, d, text, false)?;
                Some(if d == Decision::Switched { Outcome::WorkerConcedes } else { Outcome::WatcherConcedes })
            }
            (Some(Verdict::Defend), Some((Decision::Rebut, text))) if open.n == 1 => {
                decided(log, self, Decision::Rebut, text, false)?;
                let worker = open.reason.as_deref().unwrap_or("");
                self.second(log, outbox, Second::Rebuttal, challenge::rebuttal(&open.challenge, worker, text), now)?;
                None
            }
            (Some(Verdict::Defend), Some((Decision::Escalate, text))) => {
                decided(log, self, Decision::Escalate, text, false)?;
                Some(Outcome::Unresolved)
            }
            (Some(Verdict::None), None) if open.n == 1 => {
                self.second(log, outbox, Second::Reask, challenge::reask(&open.challenge), now)?;
                None
            }
            (Some(Verdict::None), None) => Some(Outcome::Unresolved),
            (Some(Verdict::None), Some((Decision::Escalate, text))) if open.n == 2 => {
                decided(log, self, Decision::Escalate, text, false)?;
                Some(Outcome::Unresolved)
            }
            // Owed and missing, or not allowed here (V100).
            (Some(_), m) => {
                if let Some((w, d, _)) = &mine {
                    step.dropped.push(format!("{w}: {} not allowed at exchange {}", decision_name(*d), open.n));
                }
                decided(log, self, Decision::Concede, m.map_or("", |(_, t)| t), true)?;
                Some(Outcome::WatcherConcedes)
            }
            (None, Some((d, _))) => {
                step.dropped.push(format!("{watcher}: {} with no reply owed", decision_name(d)));
                None
            }
            (None, None) => None,
        };
        if let Some(outcome) = outcome {
            let escalated = (outcome == Outcome::Unresolved).then(|| {
                let severity = list.get(&open.concern).map_or(Severity::Note, |c| c.severity);
                escalate.admits(severity)
            });
            if escalated == Some(false) {
                step.ticker.push(format!("Challenge {}: unresolved, not escalated", open.challenge));
            }
            self.end(log, outbox, outcome, escalated, None, now)?;
            step.ended = Some(outcome);
        }
        Ok(step)
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
    /// ended is dropped (V37); one this view has not seen queued is skipped (it may be behind).
    pub fn gate(&self, item: &Item) -> Option<Gate> {
        let challenge = item.id.strip_suffix(SECOND).filter(|_| item.kind == ItemKind::Challenge)?;
        Some(if self.ended.contains(challenge) {
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
                (Phase::Queued(_), _) => "Exchange 2 waits for the Worker's next idle.",
                (Phase::Awaiting, _) => "Waiting for the Worker's verdict line.",
            }
            .to_owned(),
        );
        Some(out.join("\n"))
    }

    fn second(&mut self, log: &EventLog, outbox: &Outbox, second: Second, text: String, now: u64) -> io::Result<()> {
        let challenge = self.open.as_ref().map(|o| o.challenge.clone()).unwrap_or_default();
        let id = format!("{challenge}{SECOND}");
        if !outbox.queue(Item { id: id.clone(), kind: ItemKind::Challenge, text }, now)? {
            return Err(io::Error::other(format!("outbox already holds {id}")));
        }
        if let Err(e) = self.commit(log, &Op::Queued { challenge, item: id.clone(), second }) {
            let _ = outbox.withdraw(&id, "unlogged", now);
            return Err(e);
        }
        Ok(())
    }

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
        // Close-out never reaches the Worker (V37); one under a live claim is dropped by the gate.
        if let Some(item) = self.waiting().map(str::to_owned) {
            outbox.withdraw(&item, "debate ended", now)?;
        }
        self.commit(log, &Op::End { challenge: open.challenge, concern: open.concern, outcome, escalated, withdrawn })
    }

    fn commit(&mut self, log: &EventLog, op: &Op) -> io::Result<()> {
        let event = op.event();
        log.append(event.clone())?;
        self.apply(&event);
        Ok(())
    }
}

/// The last line in `text` matching `Challenge <id>:` (V30), as its verdict + one-line reason.
/// Other ids are ignored; markup around the line (`**`, backticks, quote marks) does not count. A
/// line naming both words (the template echoed back) is no verdict.
pub fn verdict(text: &str, id: &str) -> Option<(Verdict, String)> {
    // ASCII folding keeps byte offsets, so `at` indexes `clean` too.
    let head = format!("challenge {}:", id.to_ascii_lowercase());
    text.lines().rev().find_map(|line| {
        let clean: String = line.chars().filter(|c| !matches!(c, '*' | '`')).collect();
        let at = clean.to_ascii_lowercase().find(&head)?;
        if clean[..at].chars().last().is_some_and(char::is_alphanumeric) {
            return None;
        }
        let rest = clean[at + head.len()..].trim_start();
        let word: String = rest.chars().take_while(|c| c.is_alphabetic()).collect();
        let v = match word.to_lowercase().as_str() {
            "concede" | "concedes" | "conceded" | "conceding" => Verdict::Concede,
            "defend" | "defends" | "defended" | "defending" => Verdict::Defend,
            _ => return None,
        };
        let after = rest[word.len()..].trim_start();
        if after.starts_with('|') {
            return None;
        }
        let reason = after.trim_start_matches(['—', '–', '-', ':', ',', ' ']).trim();
        Some((v, reason.to_owned()))
    })
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
