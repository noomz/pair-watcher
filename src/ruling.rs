//! Human rulings (V71–V75, V77, I12): the human's say on an Escalation or a live Debate. An
//! explicit `y`/`n`/`d` is a human fact (`ruled`) that pane, CLI and `/pw` append (V43); every
//! fold reads it at once (the Concern closes as `ruled`, NEEDS YOU closes, a Debate's exchange 2
//! is dropped at the gate). The arbiter answers it: `debate end`, the Ruling message for an
//! explicit side-with-Watcher (V74), and on `u` (`ruling undone`) the withdrawal of a queued
//! Ruling and the reopened Debate (V75). The arbiter also writes inferred rulings, `answered`,
//! `closed-stale` and `unruled`. State is a fold over the session event log (V93).
//!
//! The Watcher loop (T15) calls [`Rulings::answer`] every loop, [`Rulings::drop_on_prompt`] and
//! [`Rulings::infer`] on wakes, [`Rulings::answer_blocked`] with `escalation::blocked_answered`,
//! and [`Rulings::unruled`] at session end.

use std::collections::{HashMap, HashSet};
use std::io;

use serde::{Deserialize, Serialize};

use crate::challenge::{Challenges, Op as ChallengeOp, Origin};
use crate::concern::{Change, ConcernList, State};
use crate::debate::{Debate, Op as DebateOp};
use crate::escalation::{Cause, Escalation, Escalations, Stage as EscalationStage};
use crate::event::{Event, EventLog, Kind};
use crate::history;
use crate::human::Act;
use crate::jsonl::Record;
use crate::outbox::{Gate, Item, ItemKind, Outbox, Withdraw};
use crate::output::{self, Line};
use crate::tailer::{Entry, Item as TItem};

/// Letters and digits a `prompt_ref` needs to point at one prompt (as V17's excerpts).
const PROMPT_REF_MIN: usize = 4;
/// Ruling outbox ids: this prefix + the Challenge id.
const ITEM: &str = "ruling/";
/// Why a queued Ruling was withdrawn.
pub const UNDONE: &str = "undone";
pub const PROMPTED_FIRST: &str = "human prompted first";

/// Where the ruled item stood (V71–V73).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// NEEDS YOU: an unresolved Debate's Escalation.
    Escalation,
    /// IN DEBATE: the Debate ends at once (V73).
    Debate,
    /// NEEDS YOU: the Worker waits on the human; only `d` rules it (V72).
    Blocked,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Side {
    Watcher,
    Worker,
    Dismissed,
}

impl Side {
    pub fn name(self) -> &'static str {
        match self {
            Side::Watcher => "watcher",
            Side::Worker => "worker",
            Side::Dismissed => "dismissed",
        }
    }

    /// Ticker words.
    pub fn words(self) -> &'static str {
        match self {
            Side::Watcher => "sided with the Watcher",
            Side::Worker => "sided with the Worker",
            Side::Dismissed => "dismissed",
        }
    }
}

/// How the human ruled (V94 `ruled` via).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Via {
    Key,
    Band,
    /// `/pw rule`.
    Command,
    /// `pair-watcher rule`.
    Cli,
    /// Inferred from the human's next prompt (V71).
    Prompt,
}

/// Which surface it came from (V94 `ruled` surface).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Surface {
    Pane,
    ModPane,
    Band,
    /// The Claude mod's `/pw`.
    Mod,
    Cli,
    /// The human's prompt into the Worker.
    Worker,
}

/// `ruled` event payload (V94).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Ruled {
    /// The Escalation id (NEEDS YOU) or the Challenge id (IN DEBATE).
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concern: Option<String>,
    /// The Challenge sided on: the Ruling message names it (I12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub challenge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
    /// The Watcher that owns the Concern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watcher: Option<String>,
    pub stage: Stage,
    pub side: Side,
    pub via: Via,
    pub surface: Surface,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_ref: Option<String>,
    /// A Ruling message goes to the Worker: explicit side-with-Watcher only (V74).
    pub ruling_sent: bool,
    /// Seconds since the item reached the human (Escalation raised, Challenge sent).
    pub after_s: u64,
}

impl Ruled {
    pub fn event(&self) -> Event {
        Event::typed(Kind::Ruled, self)
    }
}

/// `ruling` event payload (V94): the Ruling message's life, and the human's undo. Each names
/// its ruling as `ruled` (the item id) + `nth` (0 = the first ruling on that id), so a write
/// racing a later ruling on the same item never lands on it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Op {
    /// Outbox item `id` holds the Ruling text.
    Queued {
        id: String,
        ruled: String,
        nth: usize,
    },
    Delivered {
        id: String,
        route: String,
        ruled: String,
        nth: usize,
    },
    /// Never to be delivered; `id` = the outbox item, if one was queued.
    Dropped {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        ruled: String,
        nth: usize,
        why: String,
    },
    /// `u` (V75): the ruling is undone. `delivered` = its Ruling reached the Worker already and
    /// stays sent.
    Undone {
        ruled: String,
        nth: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        concern: Option<String>,
        via: Via,
        surface: Surface,
        delivered: bool,
    },
}

impl Op {
    pub fn event(&self) -> Event {
        Event::typed(Kind::Ruling, self)
    }
}

/// `answered`: the blocked Escalation was answered in the Worker (V72).
pub fn answered(id: &str, after_s: u64) -> Event {
    Event::new(Kind::Answered).with("id", id).with("after_s", after_s)
}

/// `closed-stale`: the Escalation's Concern stopped applying before a ruling (V71).
pub fn closed_stale(id: &str, concern: &str) -> Event {
    Event::new(Kind::ClosedStale).with("id", id).with("concern", concern)
}

/// `unruled`: still open at session end (V71).
pub fn unruled(id: &str) -> Event {
    Event::new(Kind::Unruled).with("id", id)
}

/// The Ruling message (I12).
pub fn text(challenge: &str, alternative: &str) -> String {
    let alternative = alternative.split_whitespace().collect::<Vec<_>>().join(" ");
    let alternative = alternative.trim_end_matches('.');
    format!(
        "[pair-watcher · Ruling {challenge} · from the human] The human sides with the Watcher on Challenge \
         {challenge}: {alternative}. Change course; no verdict line needed."
    )
}

/// Which ruling stands on each item (its Concern, else the Escalation id): the first `ruled`
/// while none stands; an `undone` naming exactly that ruling (id + nth) lifts it. A second
/// ruling racing the first (two surfaces), or a stale undo, changes nothing (V43, V75). Every
/// fold a ruling touches reads rulings through this, so they agree.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Live {
    /// `ruled` events so far per ruled id: the next one's `nth`.
    count: HashMap<String, usize>,
    /// Item key → the standing ruling (ruled id, nth).
    on: HashMap<String, (String, usize)>,
}

/// What one event did to the standing rulings.
#[derive(Clone, Debug, PartialEq)]
pub enum Standing {
    Ruled {
        id: String,
        concern: Option<String>,
        side: Side,
        nth: usize,
    },
    Undone {
        concern: Option<String>,
        ruled: String,
        nth: usize,
    },
    /// A `ruled` on an item already ruled: void.
    Void {
        nth: usize,
    },
}

impl Live {
    pub fn apply(&mut self, event: &Event) -> Option<Standing> {
        match event.k {
            Kind::Ruled => {
                let r = event.parse::<Ruled>()?;
                let count = self.count.entry(r.id.clone()).or_default();
                let nth = *count;
                *count += 1;
                let key = r.concern.clone().unwrap_or_else(|| r.id.clone());
                if self.on.contains_key(&key) {
                    return Some(Standing::Void { nth });
                }
                self.on.insert(key, (r.id.clone(), nth));
                Some(Standing::Ruled { id: r.id, concern: r.concern, side: r.side, nth })
            }
            Kind::Ruling => match event.parse::<Op>()? {
                Op::Undone { ruled, nth, concern, .. } => {
                    let key = concern.clone().unwrap_or_else(|| ruled.clone());
                    if self.on.get(&key) != Some(&(ruled.clone(), nth)) {
                        return None;
                    }
                    self.on.remove(&key);
                    Some(Standing::Undone { concern, ruled, nth })
                }
                _ => None,
            },
            _ => None,
        }
    }
}

/// One ruling as the log has it.
#[derive(Clone, Debug, PartialEq)]
pub struct Ruling {
    pub ruled: Ruled,
    /// Rulings on the same item before this one.
    pub nth: usize,
    pub t: u64,
    pub undone: bool,
    /// Another ruling stood on the item when this one was logged: it counts for nothing.
    pub void: bool,
    /// Its Ruling message's outbox id, once queued.
    pub item: Option<String>,
    pub delivered: bool,
    /// The Ruling message was dropped (undone, stale, the human prompted first).
    pub dropped: bool,
    /// Another Challenge was queued after it: the slot is not this Debate's to reopen (V75).
    slot_lost: bool,
    /// After an undo: the Debate came back (`debate reopened`) or its Challenge was queued again.
    restored: bool,
    /// Log order.
    seq: usize,
}

impl Ruling {
    fn live(&self) -> bool {
        !self.undone && !self.void
    }
}

/// What `s` keeps (V77): the Standing ruling's fields bar id and date (V78).
#[derive(Clone, Debug, PartialEq)]
pub struct Keep {
    pub concern: String,
    pub focus: String,
    pub headline: String,
    pub side: Side,
    pub reason: Option<String>,
}

/// The folds a human surface checks a ruling against.
pub struct Cx<'a> {
    pub concerns: &'a ConcernList,
    pub challenges: &'a Challenges,
    pub escalations: &'a Escalations,
    pub rulings: &'a Rulings,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rulings {
    all: Vec<Ruling>,
    standing: Live,
    /// What the last event applied did to the standing rulings.
    changed: Option<Standing>,
    /// Escalation id → when raised.
    raised: HashMap<String, u64>,
    /// Escalations closed by `answered`, `closed-stale` or `unruled`.
    closed: HashSet<String>,
    /// Challenge id → when queued, when sent, and how it was queued.
    queued: HashMap<String, (u64, String, Origin, Option<String>)>,
    sent: HashMap<String, u64>,
    /// Concern → the Watcher that added it.
    owner: HashMap<String, String>,
    /// The Worker's latest verdict reason per Challenge.
    reasons: HashMap<String, String>,
    /// `human dismiss`: (Concern, seq).
    dismissed: Vec<(String, usize)>,
    /// Concerns kept as Standing rulings this session.
    kept: HashSet<String>,
    seq: usize,
}

impl Rulings {
    pub fn rebuild<'a>(records: impl IntoIterator<Item = &'a Record<Event>>) -> Rulings {
        let mut r = Rulings::default();
        for rec in records {
            r.apply(rec.t, &rec.body);
        }
        r
    }

    pub fn all(&self) -> &[Ruling] {
        &self.all
    }

    /// What the last event applied did to the standing rulings: `None` for a stale undo or any
    /// other event, `Void` for a ruling that came second.
    pub fn changed(&self) -> Option<&Standing> {
        self.changed.as_ref()
    }

    pub fn apply(&mut self, t: u64, event: &Event) {
        self.seq += 1;
        let seq = self.seq;
        let standing = self.standing.apply(event);
        self.changed = standing.clone();
        match event.k {
            Kind::Ruled => {
                if let Some(ruled) = event.parse::<Ruled>() {
                    let (nth, void) = match standing {
                        Some(Standing::Ruled { nth, .. }) => (nth, false),
                        Some(Standing::Void { nth }) => (nth, true),
                        _ => return,
                    };
                    let r = Ruling {
                        ruled,
                        nth,
                        t,
                        undone: false,
                        void,
                        item: None,
                        delivered: false,
                        dropped: false,
                        slot_lost: false,
                        restored: false,
                        seq,
                    };
                    self.all.push(r);
                }
            }
            Kind::Ruling => match event.parse::<Op>() {
                Some(Op::Queued { id, ruled, nth }) => {
                    if let Some(r) = self.at(&ruled, nth) {
                        r.item = Some(id);
                    }
                }
                Some(Op::Delivered { ruled, nth, .. }) => {
                    if let Some(r) = self.at(&ruled, nth) {
                        r.delivered = true;
                    }
                }
                Some(Op::Dropped { ruled, nth, .. }) => {
                    if let Some(r) = self.at(&ruled, nth) {
                        r.dropped = true;
                    }
                }
                Some(Op::Undone { .. }) => {
                    if let Some(Standing::Undone { ruled, nth, .. }) = standing
                        && let Some(r) = self.at(&ruled, nth)
                    {
                        r.undone = true;
                    }
                }
                None => {}
            },
            Kind::Answered | Kind::ClosedStale | Kind::Unruled => {
                if let Some(id) = event.str("id") {
                    self.closed.insert(id.to_owned());
                }
            }
            Kind::Escalation if event.str("op") == Some("raised") => {
                if let Some(id) = event.str("id") {
                    self.raised.insert(id.to_owned(), t);
                }
            }
            Kind::Challenge => match event.parse::<ChallengeOp>() {
                Some(ChallengeOp::Queued { id, concern, origin, remark, .. }) => {
                    for r in self.all.iter_mut().filter(|r| r.ruled.stage == Stage::Debate) {
                        if r.ruled.challenge.as_deref() != Some(id.as_str()) {
                            r.slot_lost = true;
                        }
                        // Queued again after an undo: restored.
                        if r.undone && r.ruled.concern.as_deref() == Some(concern.as_str()) {
                            r.restored = true;
                        }
                    }
                    self.queued.insert(id, (t, concern, origin, remark));
                }
                Some(ChallengeOp::Sent { id, .. }) => {
                    self.sent.insert(id, t);
                }
                _ => {}
            },
            Kind::Debate => match event.parse::<DebateOp>() {
                Some(DebateOp::Exchange { challenge, reason: Some(reason), .. }) => {
                    self.reasons.insert(challenge, reason);
                }
                Some(DebateOp::Reopened { challenge, .. }) => {
                    for r in self.all.iter_mut().filter(|r| r.undone) {
                        if r.ruled.challenge.as_deref() == Some(challenge.as_str()) {
                            r.restored = true;
                        }
                    }
                }
                _ => {}
            },
            Kind::Concern => {
                if let Some(Change::Add { watcher, concern }) = event.parse::<Change>() {
                    self.owner.insert(concern.id, watcher);
                }
            }
            Kind::Human => match event.parse::<Act>() {
                Some(Act::Dismiss { id }) => self.dismissed.push((id, seq)),
                Some(Act::RulingRemembered { id, .. }) => {
                    self.kept.insert(id);
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn at(&mut self, ruled: &str, nth: usize) -> Option<&mut Ruling> {
        self.all.iter_mut().find(|r| r.ruled.id == ruled && r.nth == nth)
    }

    /// The live ruling on `id` (an Escalation or Challenge id).
    pub fn live(&self, id: &str) -> Option<&Ruling> {
        self.all.iter().rev().find(|r| r.live() && r.ruled.id == id)
    }

    /// The live ruling on Concern `concern`.
    pub fn on_concern(&self, concern: &str) -> Option<&Ruling> {
        self.all.iter().rev().find(|r| r.live() && r.ruled.concern.as_deref() == Some(concern))
    }

    /// The last ruling not undone: what `u` undoes (V75).
    pub fn last(&self) -> Option<&Ruling> {
        self.all.iter().rev().find(|r| r.live())
    }

    /// NEEDS YOU: raised, not dropped stale, not ruled, answered, stale or unruled (V71, V72).
    pub fn is_open(&self, e: &Escalation) -> bool {
        e.stage != EscalationStage::Dropped && !self.closed.contains(&e.id) && self.live(&e.id).is_none()
    }

    pub fn kept(&self, concern: &str) -> bool {
        self.kept.contains(concern)
    }

    /// When Escalation `id` was raised.
    pub fn raised(&self, id: &str) -> Option<u64> {
        self.raised.get(id).copied()
    }

    /// The outbox claim's gate for Ruling items: delivered only while its ruling stands (V74,
    /// V75); one this view does not know yet is skipped (it may be behind).
    pub fn gate(&self, item: &Item) -> Gate {
        match self.all.iter().rev().find(|r| r.item.as_deref() == Some(item.id.as_str())) {
            Some(r) if r.undone || r.dropped => Gate::Drop,
            Some(_) => Gate::Deliver,
            None => Gate::Skip,
        }
    }

    /// The arbiter's answer to the human's rulings and undos (V43, V73–V75), and `closed-stale`
    /// for an Escalation whose Concern stopped applying (V71). Run every loop. Every event it
    /// writes is folded into `debate`, `challenges` and `self`. Returns ticker lines.
    #[allow(clippy::too_many_arguments)]
    pub fn answer(
        &mut self,
        log: &EventLog,
        outbox: &Outbox,
        debate: &mut Debate,
        challenges: &mut Challenges,
        list: &ConcernList,
        escalations: &Escalations,
        now: u64,
    ) -> io::Result<Vec<String>> {
        let mut ticker = Vec::new();
        let fold = |event: Option<Event>, debate: &mut Debate, challenges: &mut Challenges, me: &mut Rulings| {
            if let Some(e) = event {
                debate.apply(&e);
                challenges.apply(&e);
                me.apply(now, &e);
            }
        };
        for i in 0..self.all.len() {
            let r = self.all[i].clone();
            let (Some(challenge), Some(concern)) = (r.ruled.challenge.clone(), r.ruled.concern.clone()) else {
                continue;
            };
            if r.live() {
                // IN DEBATE ends at once (V73): the Debate, or the Challenge still waiting.
                if debate.open().is_some_and(|o| o.challenge == challenge) {
                    let e = debate.end_ruled(log, outbox, now)?;
                    fold(e, debate, challenges, self);
                }
                if challenges.slot().is_some_and(|s| s.id == challenge && !s.sent) {
                    let before = challenges.slot().cloned();
                    challenges.drop_stale(log, outbox, now)?;
                    if challenges.slot().is_none()
                        && let Some(s) = before
                    {
                        let e = ChallengeOp::StaleDropped { id: s.id, concern: s.concern }.event();
                        debate.apply(&e);
                        self.apply(now, &e);
                    }
                }
                // Not when a human prompt went first (`drop_on_prompt` may come before the queue).
                if r.ruled.ruling_sent && r.item.is_none() && !r.dropped {
                    let alternative =
                        list.get(&concern).and_then(|c| c.alternative.clone().or(Some(c.headline.clone())));
                    let item = self.queue(log, outbox, &r, &challenge, alternative.as_deref().unwrap_or(""), now)?;
                    ticker.push(format!("Ruling {challenge} queued for the Worker's next idle ({item})"));
                }
            } else {
                if let Some(item) = r.item.clone().filter(|_| !r.delivered && !r.dropped) {
                    // Delivered stays sent (the sync below logs it). A claim in flight: the gate
                    // drops it from now on, and a later loop withdraws it.
                    if outbox.withdraw(&item, UNDONE, now)? != Withdraw::Acked {
                        let (ruled, nth) = (r.ruled.id.clone(), r.nth);
                        let op = Op::Dropped { id: Some(item), ruled, nth, why: UNDONE.to_owned() };
                        self.commit(log, now, &op)?;
                    }
                }
                // Back IN DEBATE only while the slot is free (V75). Undone before the arbiter
                // answered, nothing was withdrawn: the held items go out as they were.
                if r.ruled.stage == Stage::Debate && !r.restored && !r.slot_lost && challenges.slot().is_none() {
                    let open = list.get(&concern).is_some_and(|c| c.state == State::Open);
                    if let Some(e) = debate.reopen(log, outbox, &challenge)? {
                        fold(Some(e), debate, challenges, self);
                        ticker.push(format!("{concern} back in Debate"));
                    } else if open
                        && !self.sent.contains_key(&challenge)
                        && let (Some(c), Some((_, _, origin, remark))) =
                            (list.get(&concern), self.queued.get(&challenge))
                    {
                        // Never delivered: the Challenge is queued again.
                        let (origin, remark) = (*origin, remark.clone());
                        challenges.queue(log, outbox, c, origin, remark.as_deref(), now)?;
                        ticker.push(format!("{concern} Challenge queued again"));
                        // `queue` folds into `challenges`; the rest learn it here.
                        let queued = challenges.slot().map(|s| s.id.clone()).unwrap_or_default();
                        let e = ChallengeOp::Queued {
                            id: queued,
                            concern: concern.clone(),
                            focus: c.focus.clone(),
                            origin,
                            remark,
                        }
                        .event();
                        debate.apply(&e);
                        self.apply(now, &e);
                    }
                }
            }
        }
        // What the outbox did with each Ruling (V28): delivered (even one dropped while a claim
        // was in flight: it went out), or dropped at a claim. A dropped one still pending is
        // withdrawn once no claim holds it.
        let delivered = outbox.delivered()?;
        let pending = outbox.pending()?;
        let items: Vec<(String, String, usize, bool)> = self
            .all
            .iter()
            .filter(|r| !r.delivered)
            .filter_map(|r| Some((r.item.clone()?, r.ruled.id.clone(), r.nth, r.dropped)))
            .collect();
        for (item, ruled, nth, dropped) in items {
            if let Some(d) = delivered.iter().find(|d| d.item.id == item) {
                self.commit(log, now, &Op::Delivered { id: item, route: d.route.clone(), ruled, nth })?;
            } else if !pending.iter().any(|p| p.id == item) {
                if !dropped {
                    self.commit(log, now, &Op::Dropped { id: Some(item), ruled, nth, why: "stale".to_owned() })?;
                }
            } else if dropped {
                let _ = outbox.withdraw(&item, "dropped", now)?;
            }
        }
        // An Escalation whose Concern stopped applying closes stale (V71).
        let stale: Vec<(String, String)> = escalations
            .all()
            .iter()
            .filter(|e| e.cause == Cause::Unresolved && self.is_open(e))
            .filter_map(|e| {
                let c = list.get(e.concern.as_deref()?)?;
                (c.state == State::Resolved).then(|| (e.id.clone(), c.id.clone()))
            })
            .collect();
        for (id, concern) in stale {
            self.write(log, now, closed_stale(&id, &concern))?;
            ticker.push(format!("{id} closed: {concern} no longer applies"));
        }
        Ok(ticker)
    }

    /// Queues the Ruling message (I12) under a fresh outbox id, then logs `ruling queued`.
    fn queue(
        &mut self,
        log: &EventLog,
        outbox: &Outbox,
        r: &Ruling,
        challenge: &str,
        alternative: &str,
        now: u64,
    ) -> io::Result<String> {
        // One id per ruling, never reused: no earlier ruling's state can gate it.
        let id = format!("{ITEM}{}.{}", r.ruled.id, r.nth);
        // Left by a crash before `queued` was logged: no claim delivers an unknown item.
        match outbox.withdraw(&id, "orphan", now)? {
            Withdraw::Acked | Withdraw::Claimed => {
                return Err(io::Error::other(format!("{id} is being delivered outside its ruling")));
            }
            Withdraw::Withdrawn | Withdraw::AlreadyDropped | Withdraw::Missing => {}
        }
        let item = Item { id: id.clone(), kind: ItemKind::Ruling, text: text(challenge, alternative) };
        if !outbox.queue(item, now)? {
            return Err(io::Error::other(format!("outbox already holds {id}")));
        }
        let op = Op::Queued { id: id.clone(), ruled: r.ruled.id.clone(), nth: r.nth };
        if let Err(e) = self.commit(log, now, &op) {
            let _ = outbox.withdraw(&id, "unlogged", now);
            return Err(e);
        }
        Ok(id)
    }

    /// A human prompt that arrived after a ruling goes first: its undelivered Ruling is dropped
    /// (V74). Run on each wake's entries, before delivery. Returns ticker lines.
    pub fn drop_on_prompt(
        &mut self,
        log: &EventLog,
        outbox: &Outbox,
        entries: &[Entry],
        now: u64,
    ) -> io::Result<Vec<String>> {
        let prompted = |after: u64| {
            entries
                .iter()
                .any(|e| e.t > after && e.items.iter().any(|i| matches!(i, TItem::Prompt(p) if !history::is_own(p))))
        };
        // Queued or not yet: once dropped, it is never queued, and the gate stops any claim.
        let due: Vec<Ruling> = self
            .all
            .iter()
            .filter(|r| r.live() && r.ruled.ruling_sent && !r.delivered && !r.dropped && prompted(r.t))
            .cloned()
            .collect();
        let mut ticker = Vec::new();
        for r in due {
            let op = Op::Dropped {
                id: r.item.clone(),
                ruled: r.ruled.id.clone(),
                nth: r.nth,
                why: PROMPTED_FIRST.to_owned(),
            };
            self.commit(log, now, &op)?;
            if let Some(item) = &r.item {
                // A claim in flight is withdrawn by `answer` once it lapses or is released.
                let _ = outbox.withdraw(item, PROMPTED_FIRST, now)?;
            }
            ticker.push(format!("Ruling {} not sent: your prompt went first", r.ruled.challenge.unwrap_or_default()));
        }
        Ok(ticker)
    }

    /// The turn-end wake's `inferred_ruling` lines (V71): a ruling when the human's prompt in
    /// this wake clearly takes a side on an open unresolved Escalation. Code checks that the
    /// `prompt_ref` stands verbatim in a human prompt of `entries`; Watchers that disagree rule
    /// nothing. Nothing is sent to the Worker (V74). Returns lines not taken, as `<watcher>: <why>`.
    #[allow(clippy::too_many_arguments)]
    pub fn infer(
        &mut self,
        log: &EventLog,
        lines: &[(String, Line)],
        turn_end: bool,
        enabled: bool,
        entries: &[Entry],
        escalations: &Escalations,
        list: &ConcernList,
        now: u64,
    ) -> io::Result<Vec<String>> {
        let mut dropped = Vec::new();
        // Human prompts with their arrival: only one after the Escalation answers it (V71).
        let prompts: Vec<(u64, String)> = entries
            .iter()
            .flat_map(|e| e.items.iter().map(move |i| (e.t, i)))
            .filter_map(|(t, i)| match i {
                TItem::Prompt(p) if !history::is_own(p) => Some((t, norm(p))),
                _ => None,
            })
            .collect();
        // One vote per Escalation: side, the first line's ref, its Watchers, whether they disagree.
        let mut votes: Vec<Vote> = Vec::new();
        for (w, line) in lines {
            let Line::InferredRuling { escalation, side, prompt_ref } = line else { continue };
            let open = escalations.get(escalation).filter(|e| e.cause == Cause::Unresolved && self.is_open(e));
            let reference = norm(prompt_ref);
            let why = if !turn_end {
                Some("inferred rulings come from turn-end wakes only".to_owned())
            } else if !enabled {
                Some("infer_rulings is off".to_owned())
            } else if open.is_none() {
                Some(format!("{escalation} is not waiting on a ruling"))
            } else if reference.chars().filter(|c| c.is_alphanumeric()).count() < PROMPT_REF_MIN
                || !prompts.iter().any(|(t, p)| *t > self.raised(escalation).unwrap_or(0) && p.contains(&reference))
            {
                Some(format!("prompt_ref for {escalation} is not in a human prompt since it was raised"))
            } else {
                None
            };
            if let Some(why) = why {
                dropped.push(format!("{w}: {why}"));
                continue;
            }
            match votes.iter_mut().find(|v| v.id == *escalation) {
                Some(v) => {
                    v.split |= v.side != *side;
                    v.watchers.push(w.clone());
                }
                None => votes.push(Vote {
                    id: escalation.clone(),
                    side: *side,
                    prompt_ref: prompt_ref.clone(),
                    watchers: vec![w.clone()],
                    split: false,
                }),
            }
        }
        for Vote { id, side, prompt_ref, watchers, split } in votes {
            if split {
                dropped.extend(watchers.iter().map(|w| format!("{w}: Watchers disagree on {id}")));
                continue;
            }
            let e = escalations.get(&id).expect("checked open above");
            let concern = e.concern.clone();
            let focus = concern.as_deref().and_then(|c| list.get(c)).map(|c| c.focus.clone());
            let ruled = Ruled {
                id: id.clone(),
                watcher: concern.as_deref().and_then(|c| self.owner.get(c).cloned()),
                concern,
                challenge: e.challenge.clone(),
                focus,
                stage: Stage::Escalation,
                side: match side {
                    output::Side::Watcher => Side::Watcher,
                    output::Side::Worker => Side::Worker,
                },
                via: Via::Prompt,
                surface: Surface::Worker,
                prompt_ref: Some(prompt_ref),
                ruling_sent: false,
                after_s: secs(now, self.raised(&id)),
            };
            self.write(log, now, ruled.event())?;
        }
        Ok(dropped)
    }

    /// A blocked Escalation, notified and still open, closes when the human answered in the
    /// Worker (`answered(hook arrival)`, V72).
    pub fn answer_blocked(
        &mut self,
        log: &EventLog,
        escalations: &Escalations,
        is_answered: impl Fn(u64) -> io::Result<bool>,
        now: u64,
    ) -> io::Result<Vec<String>> {
        let mut done = Vec::new();
        for e in escalations.all() {
            let Some(at) = e.hook.filter(|_| e.stage == EscalationStage::Notified && self.is_open(e)) else {
                continue;
            };
            if is_answered(at)? {
                self.write(log, now, answered(&e.id, secs(now, self.raised(&e.id))))?;
                done.push(e.id.clone());
            }
        }
        Ok(done)
    }

    /// Session end: every Escalation still in NEEDS YOU is logged `unruled` (V71).
    pub fn unruled(&mut self, log: &EventLog, escalations: &Escalations, now: u64) -> io::Result<Vec<String>> {
        let open: Vec<String> = escalations.all().iter().filter(|e| self.is_open(e)).map(|e| e.id.clone()).collect();
        for id in &open {
            self.write(log, now, unruled(id))?;
        }
        Ok(open)
    }

    /// The turn-end wake's lines on Escalations waiting for a ruling, so a Watcher can infer one
    /// from the human's prompt (V71); `None` when none waits.
    pub fn awaiting(&self, escalations: &Escalations) -> Option<String> {
        let open: Vec<&Escalation> =
            escalations.all().iter().filter(|e| e.cause == Cause::Unresolved && self.is_open(e)).collect();
        if open.is_empty() {
            return None;
        }
        let mut out = vec!["Awaiting the human's ruling:".to_owned()];
        for e in open {
            out.push(format!("{} {}", e.id, e.title));
            out.extend(e.lines.iter().map(|l| format!("  {l}")));
        }
        out.push(
            "If a human prompt in this turn clearly takes a side on one, add \
             {\"op\":\"inferred_ruling\",\"escalation\":\"<id>\",\"side\":\"watcher|worker\",\"prompt_ref\":\"<words \
             quoted verbatim from that prompt>\"}; otherwise nothing."
                .to_owned(),
        );
        Some(out.join("\n"))
    }

    /// The human's explicit `y`/`n`/`d` on `id` (V71–V73): an open NEEDS YOU Escalation, or the
    /// Concern or Challenge IN DEBATE. `Err` = refused, with why.
    pub fn rule(cx: &Cx, id: &str, side: Side, via: Via, surface: Surface, now: u64) -> Result<Ruled, String> {
        let r = cx.rulings;
        if let Some(e) = cx.escalations.get(id) {
            if !r.is_open(e) {
                return Err(format!("{id} is not waiting on you"));
            }
            if e.cause == Cause::Blocked {
                if side != Side::Dismissed {
                    return Err(format!("{id}: answer in the Worker; d = not worth the ping"));
                }
                let after_s = secs(now, r.raised(id));
                return Ok(Ruled {
                    id: id.to_owned(),
                    concern: None,
                    challenge: None,
                    focus: None,
                    watcher: None,
                    stage: Stage::Blocked,
                    side,
                    via,
                    surface,
                    prompt_ref: None,
                    ruling_sent: false,
                    after_s,
                });
            }
            let concern = e.concern.clone();
            let challenge = e.challenge.clone();
            return Ok(r.ruled(
                cx,
                id,
                concern,
                challenge,
                Stage::Escalation,
                side,
                via,
                surface,
                secs(now, r.raised(id)),
            ));
        }
        let slot = cx.challenges.slot().filter(|s| s.id == id || s.concern == id);
        let Some(s) = slot.filter(|s| cx.concerns.get(&s.concern).is_some_and(|c| c.state == State::Open)) else {
            return Err(format!("{id} is neither in NEEDS YOU nor IN DEBATE"));
        };
        if !s.sent && s.stale {
            return Err(format!("Challenge {} is being dropped stale", s.id));
        }
        let since = r.sent.get(&s.id).or(r.queued.get(&s.id).map(|q| &q.0)).copied();
        let (concern, challenge) = (Some(s.concern.clone()), Some(s.id.clone()));
        Ok(r.ruled(cx, &s.id, concern, challenge, Stage::Debate, side, via, surface, secs(now, since)))
    }

    #[allow(clippy::too_many_arguments)]
    fn ruled(
        &self,
        cx: &Cx,
        id: &str,
        concern: Option<String>,
        challenge: Option<String>,
        stage: Stage,
        side: Side,
        via: Via,
        surface: Surface,
        after_s: u64,
    ) -> Ruled {
        let c = concern.as_deref().and_then(|c| cx.concerns.get(c));
        Ruled {
            id: id.to_owned(),
            focus: c.map(|c| c.focus.clone()),
            watcher: concern.as_deref().and_then(|c| self.owner.get(c).cloned()),
            concern,
            challenge,
            stage,
            side,
            via,
            surface,
            prompt_ref: None,
            ruling_sent: side == Side::Watcher && via != Via::Prompt,
            after_s,
        }
    }

    /// `u` (V75): undoes the last live ruling. `delivered` = its Ruling reached the Worker.
    pub fn undo(&self, outbox: &Outbox, via: Via, surface: Surface) -> Result<Op, String> {
        let r = self.last().ok_or("no ruling to undo")?;
        let delivered = match &r.item {
            Some(item) => outbox.delivered().map_err(|e| e.to_string())?.iter().any(|d| d.item.id == *item),
            None => false,
        };
        Ok(Op::Undone {
            ruled: r.ruled.id.clone(),
            nth: r.nth,
            concern: r.ruled.concern.clone(),
            via,
            surface,
            delivered,
        })
    }

    /// What `s` keeps (V77): the selected Concern if it was ruled or dismissed this session,
    /// else the latest live ruling or dismissal with a Concern. Mutes are not rulings.
    pub fn keep(&self, concerns: &ConcernList, selected: Option<&str>) -> Result<Keep, String> {
        let ruled = |id: &str| self.on_concern(id).map(|r| (r.ruled.side, r.seq));
        let dismissed =
            |id: &str| self.dismissed.iter().rev().find(|(c, _)| c == id).map(|(_, s)| (Side::Dismissed, *s));
        let pick = |id: &str| match (ruled(id), dismissed(id)) {
            (Some(a), Some(b)) => Some(if a.1 > b.1 { a } else { b }),
            (a, b) => a.or(b),
        };
        let chosen = selected.and_then(|id| pick(id).map(|p| (id.to_owned(), p.0))).or_else(|| {
            let last_ruled = self.all.iter().rev().find(|r| r.live() && r.ruled.concern.is_some());
            let last_dismissed = self.dismissed.last();
            match (last_ruled, last_dismissed) {
                (Some(r), Some((c, s))) if *s > r.seq => Some((c.clone(), Side::Dismissed)),
                (Some(r), _) => Some((r.ruled.concern.clone()?, r.ruled.side)),
                (None, Some((c, _))) => Some((c.clone(), Side::Dismissed)),
                (None, None) => None,
            }
        });
        let (id, side) = chosen.ok_or("no ruling or dismissal to keep")?;
        if self.kept(&id) {
            return Err(format!("{id} is already kept for this repo"));
        }
        let c = concerns.get(&id).ok_or_else(|| format!("no Concern {id}"))?;
        let reason = match side {
            Side::Watcher => c.alternative.clone(),
            Side::Worker => self
                .on_concern(&id)
                .and_then(|r| r.ruled.challenge.as_ref())
                .and_then(|ch| self.reasons.get(ch).cloned()),
            Side::Dismissed => None,
        };
        Ok(Keep { concern: id, focus: c.focus.clone(), headline: c.headline.clone(), side, reason })
    }

    fn commit(&mut self, log: &EventLog, now: u64, op: &Op) -> io::Result<()> {
        self.write(log, now, op.event())
    }

    fn write(&mut self, log: &EventLog, now: u64, event: Event) -> io::Result<()> {
        log.append(event.clone())?;
        self.apply(now, &event);
        Ok(())
    }
}

/// Inferred-ruling lines on one Escalation (V71).
struct Vote {
    id: String,
    side: output::Side,
    prompt_ref: String,
    watchers: Vec<String>,
    split: bool,
}

fn secs(now: u64, since: Option<u64>) -> u64 {
    since.map_or(0, |t| now.saturating_sub(t) / 1000)
}

/// Whitespace folded, so a quote across a line break still matches.
fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
