//! The standalone pane (V63–V70, I17): a fold over the session event log (V93) plus the live
//! pending line from the outbox, drawn by the pure [`crate::layout`]. The pane writes only human
//! facts and `shown` (V43, V69).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::budget::{self, Budget, Stage};
use crate::challenge::{Challenges, Op as ChallengeOp, Origin};
use crate::concern::{Change, ConcernList, Resolution, State, Tag};
use crate::debate::{Debate, Op as DebateOp, Outcome as DebateOutcome, Second, Verdict};
use crate::delivery::{self, Pending};
use crate::escalation::{self, Escalations, Op as EscalationOp};
use crate::event::{Event, Kind};
use crate::human::{self, Act};
use crate::jsonl::Record;
use crate::layout::{Frame, Input, Keys, Overlay, Span, Ui, layout};
use crate::mailbox::Mailbox;
use crate::outbox::Outbox;
use crate::ruling::{self, Cx, Op as RulingOp, Ruled, Rulings, Side, Stage as RulingStage, Standing, Surface, Via};
use crate::standing;
use crate::status::{Coverage, Op as StatusOp, Outcome, Wake, WatcherOp};
use crate::term::{self, Key};

/// Ticker lines kept; the Board shows the last 2 (V63).
const TICKER_KEEP: usize = 50;

/// One Debate log entry under its Concern: time · event · reason (V63).
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub t: u64,
    pub event: String,
    pub reason: String,
}

/// What the pane knows of one Concern beyond the Concern list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Info {
    pub added: u64,
    /// Debate log, all Challenges on it, oldest first.
    pub entries: Vec<Entry>,
    /// How it closed, for its CLOSED row.
    pub end: Option<String>,
    pub helpful: bool,
}

/// How a ticker line is coloured: red only for refusals (V64), magenta for diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Diag,
    Refusal,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tick {
    pub t: u64,
    pub text: String,
    pub tone: Tone,
}

/// One Stream overlay line (V68).
#[derive(Clone, Debug, PartialEq)]
pub struct StreamLine {
    pub t: u64,
    pub focus: Option<String>,
    /// A Debate turn, indented under its Concern.
    pub indent: bool,
    pub sym: char,
    pub text: String,
}

/// One wake strip cell (V63).
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    /// Wake number; `None` = a lost span.
    pub n: Option<u64>,
    pub glyph: char,
}

/// Strip glyphs by precedence when Watchers of one wake differ.
const GLYPHS: [char; 6] = ['▲', '✗', '■', '↻', '·', '░'];

/// Everything the Board draws, folded from the log.
#[derive(Clone, Debug)]
pub struct Board {
    /// Local time offset, seconds east of UTC, for `HH:MM`.
    pub tz: i64,
    pub started: Option<u64>,
    pub swarm: bool,
    pub focuses: Vec<String>,
    pub observe_only: bool,
    pub ended: bool,
    /// Standing rulings held: the count at session start (V78) + kept − removed since (V77).
    pub rulings_held: usize,
    /// The Standing rulings, read live from the store while the `r` overlay is open (V78).
    pub held: Vec<standing::Standing>,
    pub budget: Budget,
    pub concerns: ConcernList,
    pub challenges: Challenges,
    pub debate: Debate,
    pub escalations: Escalations,
    pub info: HashMap<String, Info>,
    /// When each Escalation was raised.
    pub raised: HashMap<String, u64>,
    /// Rulings, undos, and what closed each Escalation (V71–V75).
    pub rulings: Rulings,
    /// Model per Watcher, with its notes.
    pub models: BTreeMap<String, (String, bool, bool)>,
    pub user_paused: bool,
    /// Lagging / BLIND per Focus; key `None` = every Watcher.
    pub lagging: BTreeMap<Option<String>, u64>,
    pub blind: BTreeMap<Option<String>, (u64, String)>,
    pub fallback: Option<String>,
    /// `primary` requests so far (V51); the Watcher answers each once.
    pub primary_asked: usize,
    /// The session's one BLIND notification went out (V41).
    pub blind_notified: bool,
    pub nochallenge: bool,
    pub muted: Vec<String>,
    pub lost_ms: u64,
    pub cells: Vec<Cell>,
    /// Latest wake: time, outcome word, wake number.
    pub last_wake: Option<(u64, &'static str, u64)>,
    /// When the Watcher last spoke (Concern or Escalation).
    pub last_spoke: Option<u64>,
    pub ticker: Vec<Tick>,
    pub stream: Vec<StreamLine>,
    pub shown: HashSet<String>,
    /// The item waiting for delivery, read live from the outbox (V26, V65).
    pub pending: Option<Pending>,
    /// A Concern, Escalation or BLIND was logged: the pane may open (V67).
    pub opened: bool,
    /// Challenge id → Concern id.
    challenged: HashMap<String, String>,
}

impl Board {
    pub fn new(tz: i64) -> Board {
        Board {
            tz,
            started: None,
            swarm: false,
            focuses: Vec::new(),
            observe_only: false,
            ended: false,
            rulings_held: 0,
            held: Vec::new(),
            budget: Budget::new(budget::RAISE_USD),
            concerns: ConcernList::default(),
            challenges: Challenges::default(),
            debate: Debate::default(),
            escalations: Escalations::default(),
            info: HashMap::new(),
            raised: HashMap::new(),
            rulings: Rulings::default(),
            models: BTreeMap::new(),
            user_paused: false,
            lagging: BTreeMap::new(),
            blind: BTreeMap::new(),
            fallback: None,
            primary_asked: 0,
            blind_notified: false,
            nochallenge: false,
            muted: Vec::new(),
            lost_ms: 0,
            cells: Vec::new(),
            last_wake: None,
            last_spoke: None,
            ticker: Vec::new(),
            stream: Vec::new(),
            shown: HashSet::new(),
            pending: None,
            opened: false,
            challenged: HashMap::new(),
        }
    }

    pub fn rebuild<'a>(tz: i64, records: impl IntoIterator<Item = &'a Record<Event>>) -> Board {
        let mut b = Board::new(tz);
        for r in records {
            b.apply(r);
        }
        b
    }

    pub fn time(&self, t: u64) -> String {
        hhmm(t, self.tz)
    }

    /// The Escalations still waiting on the human (NEEDS YOU), oldest first.
    /// A Watcher is BLIND on a model key failure (V86): `F` may bring it back after `auth`.
    pub fn key_blind(&self) -> bool {
        self.blind.values().any(|(_, reason)| reason.starts_with(crate::fail::KEY_BLIND))
    }

    pub fn needs_you(&self) -> impl Iterator<Item = &escalation::Escalation> {
        self.escalations.all().iter().filter(|e| self.rulings.is_open(e))
    }

    /// The open Concern holding the Challenge slot (IN DEBATE); a ruled one has left it (V73).
    pub fn in_debate(&self) -> Option<&str> {
        self.challenges
            .slot()
            .filter(|s| (!s.stale && !s.ruled) || s.sent)
            .filter(|s| self.concerns.get(&s.concern).is_some_and(|c| c.state == State::Open))
            .map(|s| s.concern.as_str())
    }

    /// The folds a ruling is checked against.
    pub fn cx(&self) -> Cx<'_> {
        Cx {
            concerns: &self.concerns,
            challenges: &self.challenges,
            escalations: &self.escalations,
            rulings: &self.rulings,
        }
    }

    pub fn apply(&mut self, r: &Record<Event>) {
        let (t, e) = (r.t, &r.body);
        self.opened |= opens(e);
        let stage = self.budget.stage();
        self.concerns.apply(e);
        self.challenges.apply(e);
        self.debate.apply(e);
        self.escalations.apply(e);
        self.rulings.apply(t, e);
        match e.k {
            Kind::Session => self.session(t, e),
            Kind::Concern => self.concern(t, e),
            Kind::Human => self.human(t, e),
            Kind::Shown => {
                if let Some(id) = e.str("id") {
                    self.shown.insert(id.to_owned());
                }
            }
            Kind::Challenge => self.challenge(t, e),
            Kind::Debate => self.debate_op(t, e),
            Kind::Escalation => self.escalation(t, e),
            Kind::Ruled => self.ruled(t, e),
            Kind::Ruling => self.ruling(t, e),
            Kind::Answered => {
                let id = e.str("id").unwrap_or_default();
                self.tick(t, format!("{id} closed: you answered in the Worker"), Tone::Plain);
            }
            Kind::ClosedStale => {
                let (id, concern) = (e.str("id").unwrap_or_default(), e.str("concern").unwrap_or_default());
                self.tick(t, format!("{id} closed: {concern} no longer applies"), Tone::Plain);
            }
            Kind::Unruled => {}
            Kind::Wake => self.wake(t, e),
            Kind::Status => self.status(t, e),
            Kind::Coverage => self.coverage(t, e),
            Kind::Watcher => self.watcher(t, e),
            Kind::Audit => {}
        }
        let now = self.budget.stage();
        if now != stage {
            let text = match now {
                Stage::Watching => "budget: watching again".to_owned(),
                Stage::Thinned => "budget 60%: checkpoints off".to_owned(),
                Stage::Collapsed => "budget 75%: Swarm runs as one Companion".to_owned(),
                Stage::Reserve => "budget 90%: normal wakes stop; reserve only".to_owned(),
                Stage::Paused => {
                    format!("paused {}/{}: no model calls", money(&self.budget, true), money(&self.budget, false))
                }
            };
            self.tick(t, text, Tone::Plain);
        }
    }

    fn tick(&mut self, t: u64, text: String, tone: Tone) {
        self.ticker.push(Tick { t, text, tone });
        if self.ticker.len() > TICKER_KEEP {
            self.ticker.remove(0);
        }
    }

    fn stream(&mut self, t: u64, focus: Option<&str>, indent: bool, sym: char, text: String) {
        self.stream.push(StreamLine { t, focus: focus.map(str::to_owned), indent, sym, text });
    }

    fn focus_of(&self, concern: &str) -> Option<String> {
        self.concerns.get(concern).map(|c| c.focus.clone())
    }

    /// A Debate entry under the Concern a Challenge is on; also a Stream line.
    fn entry(&mut self, t: u64, challenge: &str, event: String, reason: String) {
        let Some(concern) = self.challenged.get(challenge).cloned() else { return };
        let focus = self.focus_of(&concern);
        let text = if reason.is_empty() { event.clone() } else { format!("{event} — {reason}") };
        self.stream(t, focus.as_deref(), true, '│', text);
        self.info.entry(concern).or_default().entries.push(Entry { t, event, reason });
    }

    fn end(&mut self, id: &str, end: String) {
        let info = self.info.entry(id.to_owned()).or_default();
        if info.end.is_none() {
            info.end = Some(end);
        }
    }

    fn ruled(&mut self, t: u64, e: &Event) {
        let Some(r) = e.parse::<Ruled>() else { return };
        // Another surface ruled first: this one counts for nothing.
        if let Some(Standing::Void { .. }) = self.rulings.changed() {
            self.tick(t, format!("{} was already ruled: {} ignored", r.id, r.side.words()), Tone::Plain);
            return;
        }
        let inferred = r.via == Via::Prompt;
        let words = r.side.words();
        if let Some(concern) = &r.concern {
            let how = if inferred { " (inferred)" } else { "" };
            let end = match r.stage {
                RulingStage::Escalation => format!("escalated → ruled: {words}{how}"),
                _ => format!("ruled: {words}{how}"),
            };
            self.end(concern, end);
            let focus = self.focus_of(concern);
            self.stream(t, focus.as_deref(), false, '⚖', format!("{} {words}{how}", r.id));
        }
        if r.stage == RulingStage::Debate {
            self.entry(t, &r.id, "human ruled".to_owned(), words.to_owned());
        }
        let text = match (r.stage, inferred) {
            (RulingStage::Blocked, _) => {
                format!("{} dismissed (the Worker still waits on its question) · u undo", r.id)
            }
            (_, true) => format!("{} ruled by your prompt (inferred): {words} · u undo · s keep for this repo", r.id),
            (_, false) => format!("{} {words} · u undo · s keep for this repo", r.id),
        };
        self.tick(t, text, Tone::Plain);
    }

    fn ruling(&mut self, t: u64, e: &Event) {
        match e.parse::<RulingOp>() {
            // A stale undo (its ruling already undone) changes nothing.
            Some(RulingOp::Undone { .. }) if !matches!(self.rulings.changed(), Some(Standing::Undone { .. })) => {}
            Some(RulingOp::Undone { ruled, concern, delivered, .. }) => {
                if let Some(c) = &concern {
                    self.info.entry(c.clone()).or_default().end = None;
                }
                let stage =
                    self.rulings.all().iter().rev().find(|r| r.undone && r.ruled.id == ruled).map(|r| r.ruled.stage);
                let back = match stage {
                    Some(RulingStage::Debate) => "back IN DEBATE",
                    _ => "back in NEEDS YOU",
                };
                let sent = if delivered { " (its Ruling was already delivered)" } else { "" };
                if stage == Some(RulingStage::Debate) {
                    self.entry(t, &ruled, "ruling undone".to_owned(), String::new());
                }
                self.tick(t, format!("{ruled} ruling undone: {back}{sent}"), Tone::Plain);
            }
            Some(RulingOp::Delivered { id, route, .. }) => {
                self.tick(t, format!("{id} delivered via {route}"), Tone::Plain);
            }
            Some(RulingOp::Dropped { why, .. }) if why == ruling::PROMPTED_FIRST => {
                self.tick(t, "Ruling not sent: your prompt went first".to_owned(), Tone::Plain);
            }
            _ => {}
        }
    }

    fn session(&mut self, t: u64, e: &Event) {
        match e.str("phase") {
            Some("start") => {
                self.started = Some(t);
                self.swarm = e.str("mode") == Some("swarm");
                self.focuses = strings(e.fields.get("focuses"));
                self.observe_only = e.fields.get("observe_only").and_then(|v| v.as_bool()).unwrap_or(false);
                self.rulings_held = e.fields.get("rulings").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let ceiling = e.fields.get("ceiling_usd").and_then(|v| v.as_f64()).unwrap_or(budget::RAISE_USD);
                self.budget = Budget::new(ceiling);
                for note in strings(e.fields.get("notes")) {
                    self.tick(t, note, Tone::Plain);
                }
            }
            Some("end") => {
                self.ended = true;
                let open = self.concerns.all().iter().filter(|c| c.state == State::Open).count();
                self.tick(t, format!("session ended; {open} open Concerns closed"), Tone::Plain);
            }
            _ => {}
        }
    }

    fn concern(&mut self, t: u64, e: &Event) {
        match e.parse::<Change>() {
            Some(Change::Add { concern: c, .. }) => {
                self.last_spoke = Some(t);
                self.info.entry(c.id.clone()).or_default().added = t;
                let text = format!("{} {} [{}] {}", c.id, c.severity.name(), c.focus, c.headline);
                self.stream(t, Some(&c.focus), false, '+', text);
            }
            Some(Change::Update { concern: c, .. }) => {
                self.last_spoke = Some(t);
                let text = format!("{} {} [{}] {}", c.id, c.severity.name(), c.focus, c.headline);
                self.stream(t, Some(&c.focus), false, '↑', text);
            }
            Some(Change::Resolve { id, reason, .. }) => {
                let why = match reason {
                    Resolution::Fixed => "fixed",
                    Resolution::NoLongerApplies => "no longer applies",
                };
                self.end(&id, format!("resolved: {why}"));
                let focus = self.focus_of(&id);
                self.stream(t, focus.as_deref(), false, '✓', format!("{id} resolved: {why}"));
            }
            Some(Change::Duplicate { id, of, .. }) => {
                self.end(&id, format!("resolved: duplicate of {of}"));
                let focus = self.focus_of(&id);
                self.stream(t, focus.as_deref(), false, '✓', format!("{id} duplicate of {of}"));
            }
            Some(Change::Unverifiable { id, .. }) => {
                self.tick(t, format!("{id} challenge-ready, evidence unverifiable: stays a Concern"), Tone::Diag);
            }
            Some(Change::Trimmed { .. }) | None => {}
        }
    }

    fn human(&mut self, t: u64, e: &Event) {
        match e.parse::<Act>() {
            Some(Act::Dismiss { id }) => {
                self.end(&id, "dismissed".to_owned());
                let focus = self.focus_of(&id);
                self.stream(t, focus.as_deref(), false, '✗', format!("{id} dismissed by you (final)"));
                self.tick(t, format!("{id} dismissed · s keep for this repo"), Tone::Plain);
            }
            Some(Act::Promote { id, remark }) => {
                let focus = self.focus_of(&id);
                let remark = remark.map(|r| format!(" + \"{r}\"")).unwrap_or_default();
                self.stream(t, focus.as_deref(), false, '⇡', format!("{id} promoted by you{remark}"));
                self.tick(t, format!("{id} promoted{remark}"), Tone::Plain);
            }
            Some(Act::Helpful { id }) => {
                self.info.entry(id).or_default().helpful = true;
            }
            Some(Act::Mute { focus }) => {
                if !self.muted.contains(&focus) {
                    self.muted.push(focus.clone());
                }
                self.tick(t, format!("muted: {focus} (this session)"), Tone::Plain);
            }
            Some(Act::Primary) => {
                self.primary_asked += 1;
                self.tick(t, "primary model requested".to_owned(), Tone::Plain);
            }
            Some(Act::Nochallenge) => {
                self.nochallenge = true;
                self.tick(t, "no auto-Challenge (this session)".to_owned(), Tone::Plain);
            }
            Some(Act::RulingRemembered { id, ruling, .. }) => {
                self.rulings_held += 1;
                self.tick(t, format!("{id} kept for this repo as Standing ruling {ruling}"), Tone::Plain);
            }
            Some(Act::RulingRemoved { ruling, .. }) => {
                self.rulings_held = self.rulings_held.saturating_sub(1);
                self.tick(t, format!("Standing ruling {ruling} removed"), Tone::Plain);
            }
            // Already in the session start count.
            Some(Act::Correction { .. }) => {}
            Some(Act::RulingsEdited { added, removed, changed }) => {
                let mut parts = Vec::new();
                for (sign, ids) in [("+", added), ("−", removed), ("~", changed)] {
                    if !ids.is_empty() {
                        parts.push(format!("{sign}{}", ids.join(" ")));
                    }
                }
                self.tick(t, format!("rulings file edited by hand: {}", parts.join(" · ")), Tone::Plain);
            }
            None => {}
        }
    }

    fn challenge(&mut self, t: u64, e: &Event) {
        match e.parse::<ChallengeOp>() {
            Some(ChallengeOp::Queued { id, concern, origin, remark, .. }) => {
                self.challenged.insert(id.clone(), concern);
                let reason = match (origin, remark) {
                    (Origin::Auto, _) => "approach-level · evidence ✓".to_owned(),
                    (Origin::Promoted, Some(r)) => format!("promoted · \"{r}\""),
                    (Origin::Promoted, None) => "promoted".to_owned(),
                };
                self.entry(t, &id, "Challenge queued".to_owned(), reason);
            }
            Some(ChallengeOp::Sent { id, route }) => self.entry(t, &id, "1/2 sent".to_owned(), format!("via {route}")),
            Some(ChallengeOp::StaleDropped { id, concern }) => {
                self.entry(t, &id, "dropped stale".to_owned(), format!("{concern} changed before delivery"));
                self.tick(t, format!("Challenge {id} dropped: {concern} changed before delivery"), Tone::Plain);
            }
            Some(ChallengeOp::PromotionRefused { concern, reason }) => {
                self.tick(t, format!("promote {concern} refused: {reason}"), Tone::Refusal);
            }
            None => {}
        }
    }

    fn debate_op(&mut self, t: u64, e: &Event) {
        let Some(op) = e.parse::<DebateOp>() else { return };
        match op {
            DebateOp::Exchange { challenge, verdict, reason, .. } => {
                let (event, reason) = match verdict {
                    Verdict::Concede => ("Worker concede", reason.unwrap_or_default()),
                    Verdict::Defend => ("Worker defend", reason.unwrap_or_default()),
                    Verdict::None => ("Worker: no answer", "no verdict line".to_owned()),
                };
                self.entry(t, &challenge, event.to_owned(), reason);
            }
            DebateOp::HumanTurn { challenge, entry, .. } if !entry.is_empty() => {
                self.entry(t, &challenge, "human turn".to_owned(), "evidence only".to_owned());
            }
            DebateOp::Decision { challenge, decision, text, defaulted, .. } => {
                let name = serde_json::to_value(decision).ok().and_then(|v| v.as_str().map(str::to_owned));
                let text = if defaulted { format!("defaulted: {text}") } else { text };
                self.entry(t, &challenge, format!("Watcher {}", name.unwrap_or_default()), text);
            }
            DebateOp::Queued { challenge, second, .. } => {
                let event = match second {
                    Second::Rebuttal => "Rebuttal queued",
                    Second::Reask => "Re-ask queued",
                };
                self.entry(t, &challenge, event.to_owned(), String::new());
            }
            DebateOp::Sent { challenge, route, .. } => {
                self.entry(t, &challenge, "2/2 sent".to_owned(), format!("via {route}"));
            }
            DebateOp::Reopened { challenge, .. } => {
                self.entry(t, &challenge, "reopened".to_owned(), "owed again on the Worker's last answer".to_owned());
            }
            DebateOp::End { challenge, concern, outcome, escalated, withdrawn } => {
                let words = |v: serde_json::Value| v.as_str().unwrap_or_default().replace('-', " ");
                let outcome_words = words(serde_json::to_value(outcome).unwrap_or_default());
                let reason = match (outcome, escalated, withdrawn) {
                    (DebateOutcome::Unresolved, Some(true), _) => "unresolved → Escalation".to_owned(),
                    (DebateOutcome::Unresolved, _, _) => "unresolved, not escalated".to_owned(),
                    (DebateOutcome::Withdrawn, _, Some(w)) => {
                        format!("withdrawn — {}", words(serde_json::to_value(w).unwrap_or_default()))
                    }
                    _ => outcome_words,
                };
                self.entry(t, &challenge, "ended".to_owned(), reason.clone());
                if outcome == DebateOutcome::Unresolved && escalated != Some(true) {
                    self.tick(t, format!("Challenge {challenge}: unresolved, not escalated"), Tone::Plain);
                } else {
                    self.tick(t, format!("{concern} Debate ended: {reason}"), Tone::Plain);
                }
                if outcome == DebateOutcome::Withdrawn {
                    self.end(&concern, "withdrawn".to_owned());
                }
            }
            _ => {}
        }
    }

    fn escalation(&mut self, t: u64, e: &Event) {
        match e.parse::<EscalationOp>() {
            Some(EscalationOp::Raised { id, title, .. }) => {
                self.last_spoke = Some(t);
                self.raised.insert(id.clone(), t);
                self.stream(t, None, false, '▲', format!("{id} {title}"));
            }
            Some(EscalationOp::Notified { id, error: Some(error), .. }) => {
                self.tick(t, format!("{id} notification failed: {error}"), Tone::Diag);
            }
            Some(EscalationOp::DroppedStale { id, why }) => {
                self.stream(t, None, false, '■', format!("{id} Escalation dropped: {why}"));
                self.tick(t, format!("{id} Escalation dropped: {why}"), Tone::Plain);
            }
            _ => {}
        }
    }

    fn wake(&mut self, t: u64, e: &Event) {
        let Some(w) = e.parse::<Wake>() else { return };
        self.budget.record(w.spend);
        let (glyph, word) = match w.outcome {
            Outcome::Escalation => ('▲', "escalation"),
            Outcome::Failed | Outcome::Invalid | Outcome::Interrupted => ('✗', "failed"),
            Outcome::Concern | Outcome::Challenge => ('■', "concern"),
            Outcome::Silent if w.replayed => ('↻', "replayed"),
            Outcome::Silent => ('·', "silent"),
            Outcome::Skipped => ('·', "skipped"),
        };
        if matches!(w.outcome, Outcome::Concern | Outcome::Challenge | Outcome::Escalation) {
            self.last_spoke = Some(t);
        }
        match self.cells.iter_mut().rev().find(|c| c.n == Some(w.n)) {
            Some(c) => {
                if rank(glyph) < rank(c.glyph) {
                    c.glyph = glyph;
                }
            }
            None => self.cells.push(Cell { n: Some(w.n), glyph }),
        }
        // Watchers of one wake share `n`: the loudest outcome speaks for it.
        let word = match self.last_wake {
            Some((_, prev, n)) if n == w.n && rank_word(prev) < rank_word(word) => prev,
            _ => word,
        };
        self.last_wake = Some((t, word, w.n));
        if w.outcome == Outcome::Failed && w.trigger == "blocked" {
            self.tick(t, escalation::UNAVAILABLE.to_owned(), Tone::Diag);
        }
    }

    fn status(&mut self, t: u64, e: &Event) {
        let Some(op) = e.parse::<StatusOp>() else { return };
        match op {
            StatusOp::Model { watcher, model, same_vendor, shares_quota } => {
                self.models.insert(watcher, (model, same_vendor, shares_quota));
            }
            StatusOp::Raise => {
                self.budget.raise();
                let text =
                    format!("+{} → ceiling {}; resuming", budget::usd(budget::RAISE_USD), money(&self.budget, false));
                self.tick(t, text, Tone::Plain);
            }
            StatusOp::UserPause => {
                self.user_paused = true;
                self.tick(t, "paused by you: no wakes, no spend (Z resumes)".to_owned(), Tone::Plain);
            }
            StatusOp::UserResume => {
                self.user_paused = false;
                self.tick(t, "resumed: one wake on the current state".to_owned(), Tone::Plain);
            }
            StatusOp::Lagging { focus } => {
                self.lagging.entry(focus).or_insert(t);
            }
            StatusOp::LaggingEnd { focus } => {
                self.lagging.remove(&focus);
            }
            StatusOp::Blind { focus, reason } => {
                let who = focus.as_deref().map(|f| format!(" ({f})")).unwrap_or_default();
                self.tick(t, format!("BLIND{who}: {reason}"), Tone::Refusal);
                self.blind.entry(focus).or_insert((t, reason));
            }
            StatusOp::BlindEnd { focus } => {
                if let Some((from, _)) = self.blind.remove(&focus) {
                    let mins = t.saturating_sub(from) / 60_000;
                    let text = format!("blind {}–{} ({mins} min)", self.time(from), self.time(t));
                    self.tick(t, text, Tone::Plain);
                }
            }
            StatusOp::Fallback { model, .. } => {
                self.tick(t, format!("fallback {model}: primary failing"), Tone::Plain);
                self.fallback = Some(model);
            }
            StatusOp::Primary => {
                self.fallback = None;
                self.tick(t, "back on the primary model".to_owned(), Tone::Plain);
            }
            StatusOp::BlindNotified { .. } => self.blind_notified = true,
        }
    }

    fn coverage(&mut self, t: u64, e: &Event) {
        match e.parse::<Coverage>() {
            Some(Coverage::Lost { from, to, cause }) => {
                self.lost_ms += to.saturating_sub(from);
                self.cells.push(Cell { n: None, glyph: '░' });
                let text = format!("lost {}–{} ({cause}): not reviewed", self.time(from), self.time(to));
                self.tick(t, text, Tone::Diag);
            }
            Some(Coverage::Delayed { from, to }) => {
                self.tick(t, format!("reviewed late {}–{}", self.time(from), self.time(to)), Tone::Plain);
            }
            Some(Coverage::BacklogPaused { .. }) => self.tick(t, "backlog paused (budget)".to_owned(), Tone::Plain),
            None => {}
        }
    }

    fn watcher(&mut self, t: u64, e: &Event) {
        let text = match e.parse::<WatcherOp>() {
            Some(WatcherOp::Restart { why }) => format!("Watcher restarted: {why}"),
            Some(WatcherOp::Crash { focus: Some(f), why }) => format!("{f} Watcher crashed: {why}"),
            Some(WatcherOp::Crash { focus: None, why }) => format!("Watcher crashed: {why}"),
            Some(WatcherOp::HangKill) => "Watcher hung: killed and restarted".to_owned(),
            Some(WatcherOp::CrashLoop { focus: Some(f) }) => format!("{f} Watcher crash-looping: stopped"),
            Some(WatcherOp::CrashLoop { focus: None }) => "Watcher crash-looping: stopped".to_owned(),
            Some(WatcherOp::SupervisorGone) => "supervisor gone".to_owned(),
            None => return,
        };
        self.tick(t, text, Tone::Diag);
    }

    /// The CLOSED end state of a Concern, or `None` if it belongs in OPEN / IN DEBATE.
    pub fn closed_as(&self, id: &str) -> Option<String> {
        let c = self.concerns.get(id)?;
        let info = self.info.get(id);
        if c.state != State::Open {
            let end = info.and_then(|i| i.end.clone());
            return Some(end.unwrap_or_else(|| c.state.name().to_owned()));
        }
        if self.in_debate() == Some(id) {
            return None;
        }
        if self.muted.contains(&c.focus) {
            return Some("muted".to_owned());
        }
        if self.ended {
            return Some("closed: session end".to_owned());
        }
        match c.tag {
            Some(Tag::UnresolvedQuiet) => Some("unresolved quiet".to_owned()),
            // Waiting in NEEDS YOU, else closed without a ruling (answered stale, never ruled).
            Some(Tag::Escalated) if self.needs_you().any(|e| e.concern.as_deref() == Some(id)) => None,
            Some(Tag::Escalated) => Some("escalated".to_owned()),
            _ => None,
        }
    }
}

/// The first Concern, Escalation or BLIND opens the pane (V67); the Watcher process calls
/// `placement` on the first event this holds for.
pub fn opens(e: &Event) -> bool {
    match e.k {
        Kind::Concern => e.str("op") == Some("add"),
        Kind::Escalation => e.str("op") == Some("raised"),
        Kind::Status => e.str("op") == Some("blind"),
        _ => false,
    }
}

fn rank(glyph: char) -> usize {
    GLYPHS.iter().position(|g| *g == glyph).unwrap_or(GLYPHS.len())
}

fn rank_word(word: &str) -> usize {
    ["escalation", "failed", "concern", "replayed", "silent", "skipped"].iter().position(|w| *w == word).unwrap_or(9)
}

fn strings(v: Option<&serde_json::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// `$x` spent or the ceiling.
pub fn money(b: &Budget, spent: bool) -> String {
    budget::usd(if spent { b.spent() } else { b.ceiling() })
}

/// `HH:MM` at `tz` seconds east of UTC.
pub fn hhmm(t_ms: u64, tz: i64) -> String {
    let secs = (t_ms / 1000) as i64 + tz;
    let day = secs.rem_euclid(86_400);
    format!("{:02}:{:02}", day / 3600, day / 60 % 60)
}

/// What a key asks of the runtime.
#[derive(Debug, PartialEq)]
pub enum Effect {
    None,
    Quit,
    /// Human facts to append to the session log (V43).
    Append(Vec<Event>),
    /// `u`: undo the last ruling (V75); the runtime reads the outbox.
    Undo,
    /// `s`: keep a ruling or dismissal for this repo (V77): the selected row, else the last.
    Keep(Option<String>),
    /// `x` in the `r` overlay: remove this Standing ruling (V78).
    Remove(String),
}

/// One key on the Board (I17). Pure: `ui` changes, events are returned for the caller to log.
/// `now` stamps a ruling's `after_s`.
pub fn key(b: &Board, ui: &mut Ui, frame: &Frame, k: Key, now: u64) -> Effect {
    ui.note = None;
    if let Some(input) = ui.input.as_mut() {
        match k {
            Key::Char(c) => input.text.push(c),
            Key::Backspace => {
                input.text.pop();
            }
            Key::Esc => ui.input = None,
            Key::CtrlC => return Effect::Quit,
            Key::Enter => {
                let Input { concern, text } = ui.input.take().expect("in input");
                let remark = Some(text.trim().to_owned()).filter(|r| !r.is_empty());
                return Effect::Append(vec![Act::Promote { id: concern, remark }.event()]);
            }
            Key::Up | Key::Down => {}
        }
        return Effect::None;
    }
    let selected = frame.selected.clone();
    // Over the rulings the selection is a ruling id: only moving, `x` and closing apply.
    let over_rulings =
        matches!(k, Key::Char('j' | 'k' | 'r' | 'x' | 'l' | '?' | 'q') | Key::Up | Key::Down | Key::Esc | Key::CtrlC);
    if ui.overlay == Overlay::Rulings && !over_rulings {
        ui.note = Some("r closes the rulings first".to_owned());
        return Effect::None;
    }
    let concern = selected.as_deref().and_then(|id| b.concerns.get(id));
    let append = |act: Act| Effect::Append(vec![act.event()]);
    match k {
        Key::Char('q') | Key::CtrlC => return Effect::Quit,
        Key::Esc => ui.overlay = Overlay::None,
        Key::Char('j') | Key::Down => ui.selected = step(&frame.order, selected.as_deref(), 1),
        Key::Char('k') | Key::Up => ui.selected = step(&frame.order, selected.as_deref(), -1),
        Key::Enter => {
            if let Some(id) = selected.filter(|id| !ui.expanded.remove(id)) {
                ui.expanded.insert(id);
            }
        }
        Key::Char('c') => ui.closed = !ui.closed,
        Key::Char('l') => ui.overlay = toggle(ui.overlay, Overlay::Stream),
        Key::Char('?') => ui.overlay = toggle(ui.overlay, Overlay::Help),
        Key::Char('r') => ui.overlay = toggle(ui.overlay, Overlay::Rulings),
        Key::Char('x') => match selected {
            Some(id) if ui.overlay == Overlay::Rulings => return Effect::Remove(id),
            _ if ui.overlay == Overlay::Rulings => ui.note = Some("no Standing ruling selected".to_owned()),
            _ => ui.note = Some("x removes a Standing ruling: open them with r".to_owned()),
        },
        Key::Char('f') => {
            let mut all = b.focuses.clone();
            for c in b.concerns.all() {
                if !all.contains(&c.focus) {
                    all.push(c.focus.clone());
                }
            }
            let next = match &ui.filter {
                None => all.first(),
                Some(f) => all.iter().skip_while(|x| *x != f).nth(1),
            };
            ui.filter = next.cloned();
        }
        // On NEEDS YOU or IN DEBATE `d` is a ruling (V72, V73); on OPEN, the final dismissal.
        Key::Char('d') if selected.as_deref().is_some_and(|id| ruled_here(b, id)) => {
            return rule(b, ui, selected.as_deref(), Side::Dismissed, now);
        }
        Key::Char('d') => match concern {
            Some(c) if c.state == State::Open => return append(Act::Dismiss { id: c.id.clone() }),
            Some(c) => ui.note = Some(format!("{} is already {}", c.id, c.state.name())),
            None => ui.note = Some("nothing selected".to_owned()),
        },
        Key::Char('y') => return rule(b, ui, selected.as_deref(), Side::Watcher, now),
        Key::Char('n') => return rule(b, ui, selected.as_deref(), Side::Worker, now),
        Key::Char('u') => return Effect::Undo,
        Key::Char('s') => return Effect::Keep(selected),
        Key::Char('P') => match concern {
            Some(c) if c.state == State::Open => ui.input = Some(Input { concern: c.id.clone(), text: String::new() }),
            Some(c) => ui.note = Some(format!("{} is {}: cannot be promoted", c.id, c.state.name())),
            None => ui.note = Some("select an open Concern to promote".to_owned()),
        },
        Key::Char('+') => {
            if let Some(id) = selected.filter(|id| !b.info.get(id).is_some_and(|i| i.helpful)) {
                return append(Act::Helpful { id });
            }
        }
        Key::Char('m') => match concern.map(|c| c.focus.clone()).or_else(|| ui.filter.clone()) {
            Some(f) if b.muted.contains(&f) => ui.note = Some(format!("{f} is already muted")),
            Some(focus) => return append(Act::Mute { focus }),
            None => ui.note = Some("select a Concern (or filter a Focus) to mute".to_owned()),
        },
        Key::Char('a') if !b.nochallenge => return append(Act::Nochallenge),
        Key::Char('F') if b.fallback.is_some() || b.key_blind() => return append(Act::Primary),
        Key::Char('F') => ui.note = Some("already on the primary model".to_owned()),
        Key::Char('Z') => {
            let op = if b.user_paused { StatusOp::UserResume } else { StatusOp::UserPause };
            return Effect::Append(vec![op.event()]);
        }
        Key::Char('$') => return Effect::Append(vec![StatusOp::Raise.event()]),
        _ => {}
    }
    Effect::None
}

/// A NEEDS YOU Escalation or the IN DEBATE Concern: where `y`/`n`/`d` rule.
fn ruled_here(b: &Board, id: &str) -> bool {
    b.needs_you().any(|e| e.id == id) || b.in_debate() == Some(id)
}

fn rule(b: &Board, ui: &mut Ui, selected: Option<&str>, side: Side, now: u64) -> Effect {
    let Some(id) = selected.filter(|id| ruled_here(b, id)) else {
        ui.note = Some("y / n / d rule on a NEEDS YOU or IN DEBATE item".to_owned());
        return Effect::None;
    };
    match Rulings::rule(&b.cx(), id, side, Via::Key, surface(ui.keys), now) {
        Ok(r) => Effect::Append(vec![r.event()]),
        Err(why) => {
            ui.note = Some(why);
            Effect::None
        }
    }
}

/// Where a pane key was pressed, for the ruling record.
pub fn surface(keys: Keys) -> Surface {
    match keys {
        Keys::Standalone => Surface::Pane,
        Keys::Mod => Surface::ModPane,
    }
}

/// A Claude mod pane key (I17) as the standalone key it stands for: `b` = `$`, `h` = `+`;
/// `y n d s u` are the same. `p` (promote) goes through [`promote`] with its remark.
pub fn mod_key(c: char) -> Option<Key> {
    match c {
        'b' => Some(Key::Char('$')),
        'h' => Some(Key::Char('+')),
        'y' | 'n' | 'd' | 's' | 'u' => Some(Key::Char(c)),
        _ => None,
    }
}

/// Mod pane `p` (V20): promotes the selected Concern with an optional remark, as `P`, the
/// remark and Enter would.
pub fn promote(b: &Board, ui: &mut Ui, frame: &Frame, remark: &str, now: u64) -> Effect {
    match key(b, ui, frame, Key::Char('P'), now) {
        Effect::None if ui.input.is_some() => {
            ui.input.as_mut().expect("in input").text = remark.to_owned();
            key(b, ui, frame, Key::Enter, now)
        }
        other => other,
    }
}

/// Band Buttons (I17, V76): the item the band asks about, the first NEEDS YOU Escalation.
pub fn band_item(b: &Board) -> Option<&escalation::Escalation> {
    b.needs_you().next()
}

/// Band digit `1` Watcher / `2` Worker / `3` dismiss on the band's item (V71, V72).
pub fn band(b: &Board, digit: char, now: u64) -> Result<Event, String> {
    let side = match digit {
        '1' => Side::Watcher,
        '2' => Side::Worker,
        '3' => Side::Dismissed,
        _ => return Err(format!("band keys are 1 2 3 u, not {digit}")),
    };
    let Some(e) = band_item(b) else { return Err("nothing waits for a ruling".to_owned()) };
    Rulings::rule(&b.cx(), &e.id, side, Via::Band, Surface::Band, now).map(|r| r.event())
}

/// Runs a key's [`Effect`] against the session: the events to append, or why it was refused.
/// `Quit` is the caller's.
pub fn perform(
    b: &Board,
    mailbox: &Mailbox,
    store: Option<&Path>,
    effect: Effect,
    via: Via,
    surface: Surface,
    now: u64,
) -> Result<Vec<Event>, String> {
    match effect {
        Effect::Quit | Effect::None => Ok(vec![]),
        Effect::Append(events) => Ok(events),
        Effect::Undo => undo(b, &mailbox.outbox(), via, surface).map(|e| vec![e]),
        Effect::Keep(selected) => match store {
            Some(store) => keep(b, store, selected.as_deref(), now, via).map(|e| vec![e]),
            None => Err("keep: the session's repo is unknown".to_owned()),
        },
        Effect::Remove(id) => match store {
            Some(store) => remove(store, &id, via).map(|e| vec![e]),
            None => Err("remove: the session's repo is unknown".to_owned()),
        },
    }
}

/// `u` / `/pw undo` / `pair-watcher undo` (V75): the `ruling undone` fact.
pub fn undo(b: &Board, outbox: &Outbox, via: Via, surface: Surface) -> Result<Event, String> {
    b.rulings.undo(outbox, via, surface).map(|op| op.event())
}

/// `s` / `/pw keep` / `pair-watcher keep` (V77): writes the Standing ruling to `store` (V78),
/// returns the `human ruling-remembered` fact; refused at the cap.
pub fn keep(b: &Board, store: &Path, selected: Option<&str>, now: u64, via: Via) -> Result<Event, String> {
    let keep = b.rulings.keep(&b.concerns, selected)?;
    match standing::remember(store, &keep, &standing::date(now, b.tz), true) {
        Ok(Ok(ruling)) => Ok(Act::RulingRemembered { id: keep.concern, ruling, via: Some(via) }.event()),
        Ok(Err(why)) => Err(format!("keep refused: {why}")),
        Err(e) => Err(format!("keep failed: {e}")),
    }
}

/// `x` / `/pw`-less `pair-watcher rulings rm` (V78): removes the Standing ruling from `store`,
/// returns the `human ruling-removed` fact.
pub fn remove(store: &Path, id: &str, via: Via) -> Result<Event, String> {
    match standing::remove(store, id, true) {
        Ok(Ok(gone)) => Ok(Act::RulingRemoved { ruling: gone.id, via }.event()),
        Ok(Err(why)) => Err(why),
        Err(e) => Err(format!("remove failed: {e}")),
    }
}

fn toggle(now: Overlay, to: Overlay) -> Overlay {
    if now == to { Overlay::None } else { to }
}

fn step(order: &[String], selected: Option<&str>, by: isize) -> Option<String> {
    if order.is_empty() {
        return None;
    }
    let i = selected.and_then(|s| order.iter().position(|x| x == s)).unwrap_or(0) as isize;
    let j = (i + by).clamp(0, order.len() as isize - 1);
    order.get(j as usize).cloned()
}

/// `shown` (V69): a Concern row continuously on screen this long in a running pane.
pub const SHOWN_AFTER: Duration = Duration::from_secs(5);

/// When each visible Concern came on screen; ids due a `shown` event.
#[derive(Default)]
pub struct Shown {
    since: HashMap<String, Instant>,
    sent: HashSet<String>,
}

impl Shown {
    /// The visible ids that have now been on screen ≥5 s and are not yet logged as shown.
    pub fn due(&mut self, b: &Board, visible: &[String], now: Instant) -> Vec<String> {
        self.since.retain(|id, _| visible.contains(id));
        let mut due = Vec::new();
        for id in visible {
            let since = *self.since.entry(id.clone()).or_insert(now);
            if now.duration_since(since) >= SHOWN_AFTER && !b.shown.contains(id) && self.sent.insert(id.clone()) {
                due.push(id.clone());
            }
        }
        due
    }
}

/// A dead supervisor (V90): its heartbeat silent on a live session. The pane shows `BLIND
/// supervisor gone` and sends the session's one BLIND notification if none went out (V41).
/// Returns whether it logged it now.
pub fn supervisor_gone(
    mailbox: &Mailbox,
    board: &Board,
    now: u64,
    notifier: &escalation::Notifier,
) -> io::Result<bool> {
    let beat = crate::watch::beat_of(&mailbox.dir().join(crate::supervisor::HEARTBEAT_FILE_SUPERVISOR));
    let gone = mailbox.is_live() && beat.is_some_and(|b| now.saturating_sub(b) >= crate::supervisor::SILENT_MS);
    let logged = board.blind.get(&None).is_some_and(|(_, r)| r == crate::supervisor::GONE);
    if !gone || logged {
        return Ok(false);
    }
    let log = mailbox.log();
    log.append(WatcherOp::SupervisorGone.event())?;
    log.append(StatusOp::Blind { focus: None, reason: crate::supervisor::GONE.to_owned() }.event())?;
    let now_board = Board::rebuild(board.tz, &log.read_from(0)?.records);
    crate::supervisor::notify_first_blind(&now_board, &log, notifier, "pane")?;
    Ok(true)
}

const TICK: Duration = Duration::from_millis(250);
/// A local note (refused key) stays this long.
const NOTE_FOR: Duration = Duration::from_secs(4);

/// `pair-watcher pane`: tails the session log and draws the Board until `q`.
pub fn run(mailbox: &Mailbox) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other("the pane needs a terminal"));
    }
    let log = mailbox.log();
    let store = standing::session_path(mailbox);
    let mut board = Board::new(term::local_offset());
    let mut ui = Ui::default();
    let mut offset = 0;
    let mut shown = Shown::default();
    let mut drawn: Vec<String> = Vec::new();
    let mut note_at = Instant::now();
    term::stop_on_signals();
    let _raw = term::Raw::enter()?;
    let mut out = io::stdout();
    let tail = |board: &mut Board, offset: &mut u64| -> io::Result<()> {
        let batch = log.read_from(*offset)?;
        *offset = batch.next;
        for r in &batch.records {
            board.apply(r);
        }
        Ok(())
    };
    let env = |k: &str| std::env::var(k).ok();
    let notifier = escalation::Notifier::detect(&env);
    while !term::stopping() {
        tail(&mut board, &mut offset)?;
        if supervisor_gone(mailbox, &board, crate::jsonl::now_ms(), &notifier)? {
            tail(&mut board, &mut offset)?;
        }
        board.pending =
            delivery::pending(&board.challenges, &board.debate, board.observe_only, mailbox).unwrap_or(None);
        if ui.overlay == Overlay::Rulings {
            board.held = match store.as_deref().map(standing::load) {
                Some(Ok(held)) => held,
                Some(Err(e)) => {
                    ui.note = Some(format!("rulings unreadable: {e}"));
                    Vec::new()
                }
                None => Vec::new(),
            };
        }
        if ui.note.is_some() && note_at.elapsed() > NOTE_FOR {
            ui.note = None;
        }
        let (w, h) = term::size();
        let frame = layout(&board, &ui, w, h);
        ui.selected = frame.selected.clone();
        let lines: Vec<String> = frame.lines.iter().map(|l| l.ansi()).collect();
        if lines != drawn {
            let mut screen = String::from("\x1b[H");
            for (i, l) in lines.iter().enumerate() {
                screen.push_str(l);
                screen.push_str("\x1b[K");
                if i + 1 < lines.len() {
                    screen.push_str("\r\n");
                }
            }
            screen.push_str("\x1b[J");
            out.write_all(screen.as_bytes())?;
            out.flush()?;
            drawn = lines;
        }
        for id in shown.due(&board, &frame.visible, Instant::now()) {
            log.append(human::shown(&id))?;
        }
        for k in term::keys(TICK)? {
            let frame = layout(&board, &ui, w, h);
            let now = crate::jsonl::now_ms();
            let events = match key(&board, &mut ui, &frame, k, now) {
                Effect::Quit => return Ok(()),
                effect => perform(&board, mailbox, store.as_deref(), effect, Via::Key, Surface::Pane, now),
            };
            match events {
                Ok(events) if !events.is_empty() => {
                    for e in events {
                        log.append(e)?;
                    }
                    // The next key sees what this one wrote (`ZZ` = pause, then resume).
                    tail(&mut board, &mut offset)?;
                }
                Ok(_) => {}
                Err(why) => ui.note = Some(why),
            }
            if ui.note.is_some() {
                note_at = Instant::now();
            }
        }
    }
    Ok(())
}

/// What the Claude mod draws (I8): the band and, while placed, the mod pane. Rebuilt from the log
/// each poll; the mod holds only the focused row and the expanded logs.
#[derive(Serialize, Debug, PartialEq)]
pub struct ModView {
    /// A Concern, Escalation or BLIND has been logged: the pane may open (V67).
    pub opens: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub band: Option<BandView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane: Option<PaneView>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct BandView {
    pub mini: Vec<Span>,
    /// The NEEDS YOU item the digit Buttons rule on, when `band_digits` is on (V76).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask: Option<Ask>,
    /// `<id> → <side>`: the last live ruling; the mod adds `· u undo` as a Button (V76).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ruled: Option<String>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Ask {
    pub id: String,
    pub title: String,
    /// `1`/`2` apply: an unresolved Debate. A blocked Escalation is answered in the Worker (V72).
    pub sides: bool,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct PaneView {
    pub lines: Vec<Vec<Span>>,
    pub items: Vec<Option<String>>,
    pub order: Vec<String>,
    pub selected: Option<String>,
}

/// The mod's view of `b`: `band` at that width, `pane` at (width, rows) with the mod's selection.
pub fn mod_view(
    b: &Board,
    band: Option<(usize, bool)>,
    pane: Option<(usize, usize)>,
    selected: Option<String>,
    expanded: &[String],
) -> ModView {
    let ui = Ui { selected, expanded: expanded.iter().cloned().collect(), keys: Keys::Mod, ..Ui::default() };
    let band = band.map(|(width, digits)| BandView {
        mini: layout(b, &ui, width, 1).lines.into_iter().next().map(|l| l.0).unwrap_or_default(),
        ask: band_item(b).filter(|_| digits).map(|e| Ask {
            id: e.id.clone(),
            title: e.title.clone(),
            sides: e.cause == escalation::Cause::Unresolved,
        }),
        ruled: b.rulings.last().map(|r| {
            let side = match r.ruled.side {
                Side::Watcher => "Watcher",
                Side::Worker => "Worker",
                Side::Dismissed => "dismissed",
            };
            format!("{} → {side}", r.ruled.id)
        }),
    });
    let pane = pane.map(|(width, rows)| {
        let frame = layout(b, &ui, width, rows);
        PaneView {
            lines: frame.lines.into_iter().map(|l| l.0).collect(),
            items: frame.items,
            order: frame.order,
            selected: frame.selected,
        }
    });
    ModView { opens: b.opened, band, pane }
}

/// A mod pane key, or (`on_band`) a band Button `1 2 3 u`, on the session (I17, V76): the events
/// it appended, or why it was refused. `selected` is the mod's focused row; `remark` goes with
/// `p` (V20).
pub fn mod_press(
    mailbox: &Mailbox,
    on_band: bool,
    pressed: char,
    selected: Option<String>,
    remark: &str,
    now: u64,
) -> Result<Vec<Event>, String> {
    let log = mailbox.log();
    let records = log.read_from(0).map_err(|e| format!("event log: {e}"))?.records;
    let mut b = Board::rebuild(term::local_offset(), &records);
    b.pending = delivery::pending(&b.challenges, &b.debate, b.observe_only, mailbox).unwrap_or(None);
    let store = standing::session_path(mailbox);
    let events = if on_band {
        match pressed {
            'u' => vec![undo(&b, &mailbox.outbox(), Via::Band, Surface::Band)?],
            digit => vec![band(&b, digit, now)?],
        }
    } else {
        let mut ui = Ui { selected, keys: Keys::Mod, ..Ui::default() };
        // Only the row the human focused: a gone or absent one is never swapped for the first.
        let frame = layout(&b, &ui, 80, 200);
        let effect = match pressed {
            'p' => promote(&b, &mut ui, &frame, remark, now),
            c => match mod_key(c) {
                Some(k) => key(&b, &mut ui, &frame, k, now),
                None => return Err(format!("mod pane keys are p b h y n d s u, not {c}")),
            },
        };
        if let Some(note) = ui.note.take() {
            return Err(note);
        }
        perform(&b, mailbox, store.as_deref(), effect, Via::Key, Surface::ModPane, now)?
    };
    for e in &events {
        log.append(e.clone()).map_err(|e| format!("event log: {e}"))?;
    }
    Ok(events)
}
