//! Escalation (V40–V42, I13, I14): the only speech that notifies. Two causes: an unresolved
//! Debate at or above `escalate_unresolved` (V36: `debate end {escalated: true}`), and a Worker
//! blocked on the human, where each Watcher adds at most one consideration line (V40). State is
//! a fold over the session event log (V93): `raised` holds the full text, so a restart re-checks
//! and notifies what was raised but never notified (V91).
//!
//! Per wake the Watcher process calls [`Escalations::raise_unresolved`] after `Debate::decide`,
//! [`Escalations::raise_blocked`] after a blocked wake's arbiter commit, then
//! [`Escalations::deliver`]. Nothing here answers for the human (V40).

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::arbiter::Watcher;
use crate::clip;
use crate::concern::{ConcernList, Evidence, State};
use crate::debate::{Op as DebateOp, Outcome};
use crate::event::{Event, EventLog, Kind};
use crate::harness::Harness;
use crate::mailbox::Mailbox;
use crate::output::Line;
use crate::trigger::{self, Signal};

/// The Watcher line of an unresolved Escalation: alternative + strongest evidence (I13).
const WATCHER_MAX: usize = 100;
/// The restated blocked question.
const QUESTION_MAX: usize = 200;
/// One consideration line.
const CONSIDER_MAX: usize = 200;
/// A notifier call that has not returned by then is killed (and counts as failed).
pub const NOTIFY_TIMEOUT: Duration = Duration::from_secs(5);
/// V42.
pub const UNAVAILABLE: &str = "blocked: considerations unavailable";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Cause {
    Unresolved,
    Blocked,
}

/// `escalation` event payload (V94).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Op {
    Raised {
        id: String,
        cause: Cause,
        /// Unresolved: the challenged Concern and its Challenge.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        concern: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        challenge: Option<String>,
        /// Blocked: arrival time of the blocked hook record; one Escalation per record.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hook: Option<u64>,
        title: String,
        lines: Vec<String>,
        /// Where to rule (I13).
        rule: String,
    },
    /// About to call the notifier. A crash before `notified` leaves it unconfirmed: never sent
    /// again (at most one; a second interruption costs more than a lost toast, C1).
    Sending {
        id: String,
        via: String,
    },
    Notified {
        id: String,
        via: String,
        /// The notifier failed or was cut off: not retried, the pane still shows it (V40).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    DroppedStale {
        id: String,
        why: String,
    },
}

impl Op {
    pub fn event(&self) -> Event {
        Event::typed(Kind::Escalation, self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Raised,
    Sending,
    Notified,
    Dropped,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Escalation {
    pub id: String,
    pub cause: Cause,
    pub concern: Option<String>,
    pub challenge: Option<String>,
    pub hook: Option<u64>,
    pub title: String,
    pub lines: Vec<String>,
    pub rule: String,
    pub stage: Stage,
    /// The notifier it went to, once `sending`.
    pub via: Option<String>,
}

impl Escalation {
    /// The notification body: both lines (or question + considerations), then where to rule.
    pub fn body(&self) -> String {
        let mut out = self.lines.clone();
        out.push(self.rule.clone());
        out.join("\n")
    }
}

/// Where a session's human can rule (I13): band digits and `/pw` exist on the Claude mod only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Surfaces {
    pub harness: Option<Harness>,
    pub band_digits: bool,
}

/// What one [`Escalations::raise_blocked`] did.
#[derive(Debug, Default, PartialEq)]
pub struct Blocked {
    pub raised: Option<String>,
    pub ticker: Vec<String>,
    /// Consideration lines not taken, as `<watcher>: <why>`.
    pub dropped: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Escalations {
    all: Vec<Escalation>,
    /// The Worker's latest verdict reason per Challenge (I13 `Worker:` line).
    reasons: HashMap<String, String>,
    /// Debates that ended unresolved and escalated: Challenge → Concern, in end order.
    unresolved: Vec<(String, String)>,
    /// Challenges and blocked hooks already raised.
    challenges: HashSet<String>,
    hooks: HashSet<u64>,
}

impl Escalations {
    pub fn rebuild<'a>(events: impl IntoIterator<Item = &'a Event>) -> Escalations {
        let mut e = Escalations::default();
        for event in events {
            e.apply(event);
        }
        e
    }

    pub fn all(&self) -> &[Escalation] {
        &self.all
    }

    pub fn get(&self, id: &str) -> Option<&Escalation> {
        self.all.iter().find(|e| e.id == id)
    }

    pub fn apply(&mut self, event: &Event) {
        match event.k {
            Kind::Debate => match event.parse::<DebateOp>() {
                Some(DebateOp::Exchange { challenge, reason: Some(reason), .. }) => {
                    self.reasons.insert(challenge, reason);
                }
                Some(DebateOp::End {
                    challenge, concern, outcome: Outcome::Unresolved, escalated: Some(true), ..
                }) => {
                    self.unresolved.push((challenge, concern));
                }
                _ => {}
            },
            Kind::Escalation => match event.parse::<Op>() {
                Some(Op::Raised { id, cause, concern, challenge, hook, title, lines, rule }) => {
                    self.challenges.extend(challenge.clone());
                    self.hooks.extend(hook);
                    let (stage, via) = (Stage::Raised, None);
                    self.all.push(Escalation { id, cause, concern, challenge, hook, title, lines, rule, stage, via });
                }
                Some(Op::Sending { id, via }) => {
                    self.stage(&id, Stage::Sending);
                    if let Some(e) = self.all.iter_mut().find(|e| e.id == id) {
                        e.via = Some(via);
                    }
                }
                Some(Op::Notified { id, .. }) => self.stage(&id, Stage::Notified),
                Some(Op::DroppedStale { id, .. }) => self.stage(&id, Stage::Dropped),
                None => {}
            },
            _ => {}
        }
    }

    fn stage(&mut self, id: &str, stage: Stage) {
        let open = |s: Stage| matches!(s, Stage::Raised | Stage::Sending);
        if let Some(e) = self.all.iter_mut().find(|e| e.id == id && open(e.stage)) {
            e.stage = stage;
        }
    }

    fn next_id(&self) -> String {
        format!("e{}", self.all.len() + 1)
    }

    /// Raises one Escalation per Debate that ended unresolved and escalated (V36), once each.
    /// `swarm` names the Focus on the Watcher line. Returns the ids raised.
    pub fn raise_unresolved(
        &mut self,
        log: &EventLog,
        list: &ConcernList,
        swarm: bool,
        surfaces: Surfaces,
    ) -> io::Result<Vec<String>> {
        let due: Vec<(String, String)> =
            self.unresolved.iter().filter(|(ch, _)| !self.challenges.contains(ch)).cloned().collect();
        let mut raised = Vec::new();
        for (challenge, concern) in due {
            let id = self.next_id();
            let c = list.get(&concern);
            let headline = c.map_or("", |c| c.headline.as_str());
            let label = match c {
                Some(c) if swarm => format!("Watcher ({})", c.focus),
                _ => "Watcher".to_owned(),
            };
            let argument = c.map(|c| argument(c.alternative.as_deref(), c.evidence.first().map(evidence)));
            let worker = self.reasons.get(&challenge).map_or("no verdict line (no answer)", String::as_str);
            let op = Op::Raised {
                title: format!("Debate unresolved: {headline} ({concern})"),
                lines: vec![format!("{label}: {}", argument.unwrap_or_default()), format!("Worker: {worker}")],
                rule: rule_unresolved(&id, surfaces),
                id: id.clone(),
                cause: Cause::Unresolved,
                concern: Some(concern),
                challenge: Some(challenge),
                hook: None,
            };
            self.commit(log, &op)?;
            raised.push(id);
        }
        Ok(raised)
    }

    /// A blocked wake's Escalation (V40): the question restated from the blocked hook's
    /// `payload`, then at most one `consider` line per Watcher in roster (Focus) order; silence
    /// is fine. `ran` = the Watchers whose output the arbiter took; none = the wake failed: no
    /// bare notification, a ticker line instead (V42). One per blocked hook record (`at`).
    pub fn raise_blocked(
        &mut self,
        log: &EventLog,
        at: u64,
        payload: &Value,
        ran: &[Watcher],
        lines: &[(String, Line)],
        swarm: bool,
    ) -> io::Result<Blocked> {
        let mut step = Blocked::default();
        let considerations = lines.iter().filter_map(|(w, l)| match l {
            Line::Consider { text } => Some((w, text)),
            _ => None,
        });
        if self.hooks.contains(&at) {
            step.dropped.extend(considerations.map(|(w, _)| format!("{w}: blocked Escalation already raised")));
            return Ok(step);
        }
        if ran.is_empty() {
            step.ticker.push(UNAVAILABLE.to_owned());
            return Ok(step);
        }
        let mut taken: HashMap<&str, String> = HashMap::new();
        for (w, text) in considerations {
            let line = clip::first_line(text, CONSIDER_MAX);
            if !ran.iter().any(|r| r.name == *w) {
                step.dropped.push(format!("{w}: did not run this wake"));
            } else if taken.contains_key(w.as_str()) {
                step.dropped.push(format!("{w}: one consideration line per Watcher"));
            } else if line.is_empty() {
                step.dropped.push(format!("{w}: empty consideration"));
            } else {
                taken.insert(w, line);
            }
        }
        let mut body = vec![question(payload)];
        for w in ran {
            if let Some(text) = taken.remove(w.name.as_str()) {
                let label = match w.focuses.as_slice() {
                    [focus] if swarm => format!("Watcher ({focus})"),
                    _ => "Watcher (opinion)".to_owned(),
                };
                body.push(format!("{label}: {text}"));
            }
        }
        let id = self.next_id();
        let op = Op::Raised {
            id: id.clone(),
            cause: Cause::Blocked,
            concern: None,
            challenge: None,
            hook: Some(at),
            title: "Worker blocked: waiting on you".to_owned(),
            lines: body,
            rule: "Answer in the Worker; d in the pane = not worth the ping.".to_owned(),
        };
        self.commit(log, &op)?;
        step.raised = Some(id);
        Ok(step)
    }

    /// Notifies each Escalation raised and not yet notified, once (V40), after re-checking it
    /// (V91, V92): an unresolved one whose Concern is no longer open, or a blocked one the human
    /// answered since (`answered(at)`, V11), is dropped stale instead. Returns the ids notified.
    pub fn deliver(
        &mut self,
        log: &EventLog,
        list: &ConcernList,
        answered: impl Fn(u64) -> io::Result<bool>,
        notifier: &Notifier,
    ) -> io::Result<Vec<String>> {
        // Cut off mid-send by a crash: it may have gone out, so it is never sent again.
        let cut: Vec<(String, String)> = self
            .all
            .iter()
            .filter(|e| e.stage == Stage::Sending)
            .map(|e| (e.id.clone(), e.via.clone().unwrap_or_default()))
            .collect();
        for (id, via) in cut {
            let error = Some("unconfirmed: interrupted while sending".to_owned());
            self.commit(log, &Op::Notified { id, via, error })?;
        }
        let pending: Vec<Escalation> = self.all.iter().filter(|e| e.stage == Stage::Raised).cloned().collect();
        let mut notified = Vec::new();
        for e in pending {
            let stale = match (e.cause, &e.concern, e.hook) {
                (Cause::Unresolved, Some(c), _) if list.get(c).is_none_or(|c| c.state != State::Open) => {
                    Some(format!("{c} no longer open"))
                }
                (Cause::Blocked, _, Some(at)) if answered(at)? => Some("the human answered first".to_owned()),
                _ => None,
            };
            if let Some(why) = stale {
                // The human ruled before it went out: no ping, and kept for `u` (V75).
                let ruled = e.concern.as_deref().and_then(|c| list.get(c)).is_some_and(|c| c.state == State::Ruled);
                if !ruled {
                    self.commit(log, &Op::DroppedStale { id: e.id, why })?;
                }
                continue;
            }
            let via = notifier.name().to_owned();
            self.commit(log, &Op::Sending { id: e.id.clone(), via: via.clone() })?;
            let error = notifier.show(&e.title, &e.body()).err().map(|err| err.to_string());
            self.commit(log, &Op::Notified { id: e.id.clone(), via, error })?;
            notified.push(e.id);
        }
        Ok(notified)
    }

    fn commit(&mut self, log: &EventLog, op: &Op) -> io::Result<()> {
        let event = op.event();
        log.append(event.clone())?;
        self.apply(&event);
        Ok(())
    }
}

/// Whether the Worker moved on since the blocked hook record that arrived at `at`: any later
/// hook record that wakes or shows activity (the human answered, V11).
pub fn blocked_answered(mailbox: &Mailbox, harness: Harness, at: u64) -> io::Result<bool> {
    let hooks = mailbox.read_hooks(0)?.records;
    let signal = |r: &crate::jsonl::Record<crate::mailbox::Hook>| trigger::signal(harness, &r.body.event);
    let blocked = |r| matches!(signal(r), Some(Signal::Blocked | Signal::MaybeBlocked));
    let start = match hooks.iter().position(|r| r.t == at && blocked(r)) {
        Some(i) => i + 1,
        None => hooks.iter().position(|r| r.t > at).unwrap_or(hooks.len()),
    };
    Ok(hooks[start..].iter().any(|r| signal(r).is_some()))
}

/// The blocked question in one line (V40: the arbiter takes it from the blocked event):
/// `AskUserQuestion` questions, else the tool and what it would run, else a text field.
pub fn question(payload: &Value) -> String {
    let input = &payload["tool_input"];
    let asked: Vec<&str> =
        input["questions"].as_array().into_iter().flatten().filter_map(|q| q["question"].as_str()).collect();
    let text = if !asked.is_empty() {
        asked.join(" / ")
    } else if let Some(tool) = payload["tool_name"].as_str() {
        let what = ["command", "description", "file_path", "url"].iter().find_map(|k| input[*k].as_str());
        match what {
            Some(w) => format!("Allow {tool}: {w}?"),
            None => format!("Allow {tool}?"),
        }
    } else {
        ["question", "message", "title"]
            .iter()
            .find_map(|k| payload[*k].as_str())
            .unwrap_or("The Worker waits on you.")
            .to_owned()
    };
    clip::first_line(&text.split_whitespace().collect::<Vec<_>>().join(" "), QUESTION_MAX)
}

/// Alternative + strongest evidence in [`WATCHER_MAX`] bytes; when both do not fit, each keeps
/// at least half the room the other leaves, so the evidence is never clipped away (I13).
fn argument(alternative: Option<&str>, evidence: Option<String>) -> String {
    const SEP: &str = " — ";
    match (alternative, evidence) {
        (Some(a), Some(e)) if a.len() + SEP.len() + e.len() > WATCHER_MAX => {
            let room = WATCHER_MAX - SEP.len();
            let evidence = e.len().min(room - a.len().min(room / 2));
            format!("{}{SEP}{}", clip::head(a, room - evidence), clip::head(&e, evidence))
        }
        (Some(a), Some(e)) => format!("{a}{SEP}{e}"),
        (Some(a), None) => clip::head(a, WATCHER_MAX),
        (None, Some(e)) => clip::head(&e, WATCHER_MAX),
        (None, None) => String::new(),
    }
}

fn evidence(e: &Evidence) -> String {
    let one = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    match e {
        Evidence::File { at, excerpt } => format!("{at}: {}", one(excerpt)),
        Evidence::Command { command, output } => format!("`{}` → {}", one(command), one(output)),
        Evidence::Transcript { excerpt } | Evidence::Hint { excerpt } => one(excerpt),
    }
}

fn rule_unresolved(id: &str, s: Surfaces) -> String {
    let mut at = vec!["pane y/n/d".to_owned()];
    if s.harness == Some(Harness::Claude) {
        if s.band_digits {
            at.push("band 1/2/3".to_owned());
        }
        at.push(format!("/pw rule {id}"));
    }
    at.push(format!("pair-watcher rule {id}"));
    format!("Rule: {}", at.join(" · "))
}

/// The OS notifier (I14): Herdr's toast inside Herdr, else the desktop's. Never a mod toast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notifier {
    Herdr(PathBuf),
    Osascript(PathBuf),
    NotifySend(PathBuf),
    /// No notifier on this platform: Escalations still show in the pane.
    None,
}

impl Notifier {
    pub fn detect(env: &dyn Fn(&str) -> Option<String>) -> Notifier {
        if env("HERDR_PANE_ID").is_some() {
            return Notifier::Herdr(env("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into()).into());
        }
        if cfg!(target_os = "macos") {
            Notifier::Osascript("osascript".into())
        } else if cfg!(target_os = "linux") {
            Notifier::NotifySend("notify-send".into())
        } else {
            Notifier::None
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Notifier::Herdr(_) => "herdr",
            Notifier::Osascript(_) => "osascript",
            Notifier::NotifySend(_) => "notify-send",
            Notifier::None => "none",
        }
    }

    /// Shows one notification; title and body go as argv, never through a shell or script text,
    /// and never where they could read as options (text may start with `-`).
    pub fn show(&self, title: &str, body: &str) -> io::Result<()> {
        let body_opt = format!("--body={body}");
        let (bin, args): (&PathBuf, Vec<&str>) = match self {
            Notifier::Herdr(bin) => (bin, vec!["notification", "show", &body_opt, "--sound", "request", "--", title]),
            Notifier::Osascript(bin) => (
                bin,
                vec![
                    "-e",
                    "on run argv",
                    "-e",
                    "display notification (item 2 of argv) with title (item 1 of argv)",
                    "-e",
                    "end run",
                    "--",
                    title,
                    body,
                ],
            ),
            Notifier::NotifySend(bin) => (bin, vec!["--", title, body]),
            Notifier::None => return Err(io::Error::new(io::ErrorKind::Unsupported, "no notifier")),
        };
        let mut child =
            Command::new(bin).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        let deadline = Instant::now() + NOTIFY_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait()? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!("{}: {status}", self.name())))
                };
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(io::ErrorKind::TimedOut, format!("{} timed out", self.name())));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
