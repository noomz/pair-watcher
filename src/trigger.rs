//! Wake triggers (V1) and blocked detection (V56). Pure: the Watcher loop feeds hook events and
//! the clock in, takes due wakes out, and records when it woke.

use serde_json::Value;

use crate::harness::Harness;

/// Checkpoint floor: a mid-turn wake needs this long since the last wake (V1).
pub const CHECKPOINT_FLOOR_MS: u64 = 180_000;
/// Codex `PermissionRequest` counts as blocked once no next hook event follows this long (V56).
pub const CODEX_BLOCKED_MS: u64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    TurnEnd,
    Checkpoint,
    /// Slim wake (V11).
    Blocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    TurnEnd,
    Blocked,
    /// Codex `PermissionRequest`: blocked unless activity follows within `CODEX_BLOCKED_MS`.
    MaybeBlocked,
    /// Codex tool hook, prompt submit, or guardian review start: the Worker is working, not
    /// waiting on the human.
    Activity,
}

/// What a hook event means for wakes; `None` for events that never wake (`SessionStart`).
pub fn signal(harness: Harness, event: &str) -> Option<Signal> {
    match (harness, event) {
        (Harness::Claude | Harness::Codex, "Stop") | (Harness::Pi, "agent_settled") => Some(Signal::TurnEnd),
        (Harness::Claude, "PermissionRequest") | (Harness::Pi, "herdr:blocked") => Some(Signal::Blocked),
        (Harness::Codex, "PermissionRequest") => Some(Signal::MaybeBlocked),
        (Harness::Codex, "PreToolUse" | "PostToolUse" | "UserPromptSubmit") => Some(Signal::Activity),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Due {
    pub trigger: Trigger,
    /// Blocked: the hook payload carrying the question.
    pub payload: Option<Value>,
    /// Arrival of the hook that made it due (latest of a coalesced burst); checkpoint: `now`.
    pub at: u64,
}

/// Signals seen since the last `due`; a burst coalesces to one wake per kind.
#[derive(Clone, Debug, Default)]
pub struct Triggers {
    turn_end: Option<u64>,
    blocked: Option<(u64, Value)>,
    /// Codex `PermissionRequest` arrival time + payload, until activity cancels it.
    maybe_blocked: Option<(u64, Value)>,
}

impl Triggers {
    pub fn observe(&mut self, at: u64, signal: Signal, payload: &Value) {
        if signal != Signal::MaybeBlocked {
            self.maybe_blocked = None;
        }
        match signal {
            Signal::TurnEnd => self.turn_end = Some(at),
            Signal::Blocked => self.blocked = Some((at, payload.clone())),
            Signal::MaybeBlocked => self.maybe_blocked = Some((at, payload.clone())),
            Signal::Activity => {}
        }
    }

    /// Wakes due at `now`: at most one blocked, then at most one normal. Turn end and blocked
    /// bypass the floor; a checkpoint needs `CHECKPOINT_FLOOR_MS` since `last_wake` (session start
    /// before the first wake) and `repo_changed` (asked only then, it snapshots).
    /// `checkpoints = false` drops checkpoints only.
    pub fn due(
        &mut self,
        now: u64,
        last_wake: u64,
        checkpoints: bool,
        repo_changed: impl FnOnce() -> bool,
    ) -> Vec<Due> {
        if let Some((at, _)) = &self.maybe_blocked
            && now.saturating_sub(*at) >= CODEX_BLOCKED_MS
        {
            self.blocked = self.maybe_blocked.take();
        }
        let mut due = Vec::new();
        if let Some((at, payload)) = self.blocked.take() {
            due.push(Due { trigger: Trigger::Blocked, payload: Some(payload), at });
        }
        if let Some(at) = self.turn_end.take() {
            due.push(Due { trigger: Trigger::TurnEnd, payload: None, at });
        } else if checkpoints && now.saturating_sub(last_wake) >= CHECKPOINT_FLOOR_MS && repo_changed() {
            due.push(Due { trigger: Trigger::Checkpoint, payload: None, at: now });
        }
        due
    }
}
