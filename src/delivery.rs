//! Delivery of Challenges (with a Debate's exchange 2) and Rulings to the Worker (V25–V29, I7–I9).
//!
//! Every route is a turn-end block or a harness-native follow-up at idle; nothing is steered,
//! sent mid-tool or typed into a prompt box (V25). Deliverers touch only the outbox: the Claude
//! `Stop` hook, the Claude mod and the pi extension (through `pw-hook adapter`), and the Watcher's
//! `codex queue` call. The arbiter alone logs `challenge sent` from the outbox's acks (V43).

use std::ffi::OsStr;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::challenge::Challenges;
use crate::concern::{ConcernList, Severity, State};
use crate::debate::Debate;
use crate::event::{Event, Kind};
use crate::harness::Harness;
use crate::jsonl::Record;
use crate::mailbox::{Hook, Mailbox};
use crate::outbox::{Claim, Gate, Item};
use crate::trigger::{self, Signal};

/// Held reason a deliverer records while the human has a draft in the prompt box (V26).
pub const DRAFT: &str = "draft";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Claude `Stop` hook block (I7).
    Stop,
    /// Claude mod `$.prompt.submit` at idle with an empty box (I8).
    Mod,
    /// `codex queue` after `Stop` (V27).
    CodexQueue,
    /// pi extension `sendUserMessage(…, {deliverAs: "followUp"})` (I9).
    PiFollowUp,
}

impl Route {
    pub fn name(self) -> &'static str {
        match self {
            Route::Stop => "stop",
            Route::Mod => "mod",
            Route::CodexQueue => "codex-queue",
            Route::PiFollowUp => "pi-followup",
        }
    }

    /// Starts a turn at idle, so it waits for the stale check of the latest turn end (V21). The
    /// `Stop` block carries a Challenge that was ready before that `Stop` (V26).
    fn idle(self) -> bool {
        self != Route::Stop
    }
}

/// The session as its event log has it: what deliverers need to pick and gate a route.
pub struct View {
    /// From `session` start; `None` (no start logged) delivers nothing.
    pub harness: Option<Harness>,
    /// No hooks: Challenges stay pending, never delivered (V29).
    pub observe_only: bool,
    pub challenges: Challenges,
    pub debate: Debate,
    pub concerns: ConcernList,
}

impl View {
    pub fn read(mailbox: &Mailbox) -> io::Result<View> {
        let events: Vec<Event> = mailbox.log().read_from(0)?.records.into_iter().map(|r| r.body).collect();
        let start = events.iter().find(|e| e.k == Kind::Session && e.str("phase") == Some("start"));
        Ok(View {
            harness: start.and_then(|e| e.str("harness")).and_then(|h| Harness::from_program(OsStr::new(h))),
            observe_only: start.and_then(|e| e.fields.get("observe_only")).and_then(Value::as_bool).unwrap_or(false),
            challenges: Challenges::rebuild(&events),
            debate: Debate::rebuild(&events),
            concerns: ConcernList::rebuild(&events),
        })
    }

    /// The outbox claim's gate: the Debate's exchange 2, else the Challenge slot (V19, V21, V37).
    pub fn gate(&self, item: &Item) -> Gate {
        self.debate.gate(item).unwrap_or_else(|| self.challenges.gate(item))
    }

    /// The item waiting for delivery: the slot's Challenge, else the Debate's exchange 2 (V34).
    fn waiting(&self) -> Option<&str> {
        match self.challenges.slot() {
            Some(s) if !s.sent && !s.stale => Some(&s.id),
            _ => self.debate.waiting(),
        }
    }

    /// The item waiting for delivery: id + pane line (V26, V29).
    pub fn pending(&self, mailbox: &Mailbox) -> io::Result<Option<(String, String)>> {
        let Some(id) = self.waiting() else { return Ok(None) };
        let line = if self.observe_only {
            "pending: observe-only, not delivered"
        } else if mailbox.outbox().held(id)?.as_deref() == Some(DRAFT) {
            "pending: human draft in prompt"
        } else {
            "pending: Worker idle"
        };
        Ok(Some((id.to_owned(), line.to_owned())))
    }

    /// Claude mod status-line field (I8): non-zero parts only; `None` clears the line.
    pub fn status_line(&self) -> Option<String> {
        let open = self.concerns.all().iter().filter(|c| c.state == State::Open).count();
        let mut parts = Vec::new();
        if self.observe_only {
            parts.push("observe-only".to_owned());
        }
        if open > 0 {
            parts.push(format!("{open} open"));
        }
        if let Some(s) = self.challenges.slot().filter(|s| !s.stale) {
            parts.push(format!("Challenge {} {}", s.id, if s.sent { "in debate" } else { "pending" }));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// `/commentary` lines: open Concerns, `warn` first, then the waiting Challenge.
    pub fn commentary(&self, pending: Option<&str>) -> Vec<String> {
        let mut open: Vec<_> = self.concerns.all().iter().filter(|c| c.state == State::Open).collect();
        open.sort_by_key(|c| c.severity != Severity::Warn);
        let mut lines: Vec<String> =
            open.iter().map(|c| format!("{} {} [{}] {}", c.severity.name(), c.id, c.focus, c.headline)).collect();
        lines.extend(pending.map(str::to_owned));
        lines
    }
}

/// Claims the next item `route` may deliver now (V28), or `None`. The stale check reads the log
/// again under the outbox lock: a dismissal logged while this deliverer waited for the lock counts.
pub fn claim(mailbox: &Mailbox, view: &View, route: Route, now: u64) -> io::Result<Option<Claim>> {
    let Some(harness) = view.harness else { return Ok(None) };
    if view.observe_only || (route.idle() && !settled(mailbox, harness)?) {
        return Ok(None);
    }
    let mut fresh = None;
    let claim =
        mailbox.outbox().claim(route.name(), now, |item| match fresh.get_or_insert_with(|| View::read(mailbox)) {
            Ok(view) => view.gate(item),
            Err(_) => Gate::Skip,
        })?;
    match fresh {
        Some(Err(e)) => Err(e),
        _ => Ok(claim),
    }
}

/// No turn end in the hook inbox past the arbiter's settled offset (V21).
fn settled(mailbox: &Mailbox, harness: Harness) -> io::Result<bool> {
    let hooks = mailbox.read_hooks(mailbox.settled()?)?.records;
    Ok(!hooks.iter().any(|r| trigger::signal(harness, &r.body.event) == Some(Signal::TurnEnd)))
}

/// The Claude `Stop` hook's answer (I7, V26): a ready item blocks the stop with its text as
/// `reason`, written to `out`. Never while `stop_hook_active` (Claude is already continuing for a
/// block). Returns whether it blocked.
pub fn stop_block(mailbox: &Mailbox, payload: &Value, out: &mut impl Write, now: u64) -> io::Result<bool> {
    if payload.get("stop_hook_active").and_then(Value::as_bool) == Some(true) {
        return Ok(false);
    }
    let view = View::read(mailbox)?;
    if view.harness != Some(Harness::Claude) {
        return Ok(false);
    }
    let Some(claim) = claim(mailbox, &view, Route::Stop, now)? else { return Ok(false) };
    let mut line = serde_json::to_vec(&json!({"decision": "block", "reason": claim.item.text}))?;
    line.push(b'\n');
    out.write_all(&line)?;
    out.flush()?;
    mailbox.outbox().ack(&claim, now)?;
    Ok(true)
}

/// After the turn-end wake (V27): `<codex> queue --thread <id> --message <text>` for the next
/// deliverable item, while the Worker is still idle. Returns the delivered id. A failed call
/// leaves the claim to lapse, so the item is tried again after the lease; a timed-out one counts
/// as sent.
pub fn codex_queue(mailbox: &Mailbox, codex: &OsStr, timeout: Duration, now: u64) -> io::Result<Option<String>> {
    let view = View::read(mailbox)?;
    if view.harness != Some(Harness::Codex) || !idle_since_turn_end(mailbox, Harness::Codex)? {
        return Ok(None);
    }
    let Some(thread) = worker_session(mailbox)? else { return Ok(None) };
    let Some(claim) = claim(mailbox, &view, Route::CodexQueue, now)? else { return Ok(None) };
    let mut child = Command::new(codex)
        .args(["queue", "--thread", &thread, "--message", &claim.item.text])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // It may have queued before hanging: count it sent rather than risk a second copy. A
            // lost one shows as no verdict, and the Debate re-asks (V33).
            mailbox.outbox().ack(&claim, now)?;
            return Err(io::Error::new(io::ErrorKind::TimedOut, "codex queue timed out; counted as sent"));
        }
        thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        return Err(io::Error::other(format!("codex queue: {status}")));
    }
    mailbox.outbox().ack(&claim, now)?;
    Ok(Some(claim.item.id))
}

/// No Worker activity (tool or permission hook) after the latest turn end: a human turn that
/// started since would have the item queued behind it, past that turn end's stale check (V21).
fn idle_since_turn_end(mailbox: &Mailbox, harness: Harness) -> io::Result<bool> {
    let hooks = mailbox.read_hooks(0)?.records;
    let mut signals = hooks.iter().filter_map(|r| trigger::signal(harness, &r.body.event));
    Ok(signals.next_back().is_none_or(|s| s == Signal::TurnEnd))
}

/// The Worker's own session id (V58): from the latest `Stop` or `SessionStart` payload, else the
/// latest payload carrying one.
pub fn worker_session(mailbox: &Mailbox) -> io::Result<Option<String>> {
    let hooks = mailbox.read_hooks(0)?.records;
    let id = |r: &&Record<Hook>| r.body.payload.get("session_id")?.as_str().map(str::to_owned);
    let own =
        hooks.iter().rev().filter(|r| matches!(r.body.event.as_str(), "Stop" | "SessionStart")).find_map(|r| id(&r));
    Ok(own.or_else(|| hooks.iter().rev().find_map(|r| id(&r))))
}

/// What an adapter's deliverer reports about its Worker each poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Worker {
    /// No turn running, prompt box empty: it may deliver now.
    Idle,
    /// A turn is running: hold for the turn end.
    Busy,
    /// Idle, but the human has a draft in the prompt box (V26).
    Draft,
}

/// One adapter poll's answer (`pw-hook adapter poll`).
#[derive(Serialize, Debug, PartialEq)]
pub struct Poll {
    /// Status-line field; absent clears it.
    pub status: Option<String>,
    pub commentary: Vec<String>,
    /// Claimed for delivery: send `text`, then ack with `token`.
    pub item: Option<PollItem>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct PollItem {
    pub id: String,
    pub text: String,
    pub token: String,
}

/// An adapter route's poll: claims only at `Worker::Idle`; otherwise records why the waiting
/// Challenge is held, for the pane.
pub fn poll(mailbox: &Mailbox, route: Route, worker: Worker, now: u64) -> io::Result<Poll> {
    let view = View::read(mailbox)?;
    let item = match worker {
        Worker::Idle => claim(mailbox, &view, route, now)?,
        Worker::Busy | Worker::Draft => None,
    };
    // Why the waiting item is held, for the pane; a claim clears it, an empty box too.
    if let Some(id) = view.waiting()
        && item.is_none()
    {
        let reason = (worker == Worker::Draft).then_some(DRAFT);
        mailbox.outbox().hold(id, route.name(), reason, now)?;
    }
    let pending = view.pending(mailbox)?;
    Ok(Poll {
        status: view.status_line(),
        commentary: view.commentary(pending.as_ref().map(|(_, line)| line.as_str())),
        item: item.map(|c| PollItem { id: c.item.id, text: c.item.text, token: c.token }),
    })
}
