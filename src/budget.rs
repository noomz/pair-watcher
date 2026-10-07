//! Session spend against the ceiling (V47): ladder and reserve (V48), status words (V49), `+$5`
//! (V50). Thresholds are constants, not config (I18).

use std::fmt;

/// What `$` / `b` adds.
pub const RAISE_USD: f64 = 5.0;
/// Checkpoint wakes off.
const THIN: f64 = 0.60;
/// Swarm collapses to Companion on the default model.
const COLLAPSE: f64 = 0.75;
/// Normal wakes stop; the last 10% is the reserve.
const RESERVE: f64 = 0.90;

/// Ladder step by share of the ceiling spent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    Watching,
    Thinned,
    Collapsed,
    Reserve,
    Paused,
}

/// What a model call is for, i.e. how far down the ladder it still runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    Checkpoint,
    /// Turn-end wake.
    Normal,
    /// Blocked wake, or finishing an open Debate.
    Reserve,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Budget {
    ceiling: f64,
    spent: f64,
}

impl Budget {
    /// `ceiling` = `ceiling_usd`; all Watchers + fallback share it.
    pub fn new(ceiling: f64) -> Budget {
        Budget { ceiling, spent: 0.0 }
    }

    pub fn spent(&self) -> f64 {
        self.spent
    }

    pub fn ceiling(&self) -> f64 {
        self.ceiling
    }

    pub fn record(&mut self, usd: f64) {
        self.spent += usd.max(0.0);
    }

    /// `$` / `b`: +$5 and resume.
    pub fn raise(&mut self) {
        self.ceiling += RAISE_USD;
    }

    pub fn stage(&self) -> Stage {
        let share = |t: f64| self.spent >= self.ceiling * t;
        if share(1.0) {
            Stage::Paused
        } else if share(RESERVE) {
            Stage::Reserve
        } else if share(COLLAPSE) {
            Stage::Collapsed
        } else if share(THIN) {
            Stage::Thinned
        } else {
            Stage::Watching
        }
    }

    pub fn admits(&self, need: Need) -> bool {
        let stage = self.stage();
        match need {
            Need::Checkpoint => stage < Stage::Thinned,
            Need::Normal => stage < Stage::Reserve,
            Need::Reserve => stage < Stage::Paused,
        }
    }

    /// From 75% a Swarm runs as one Companion on the default model.
    pub fn swarm_collapsed(&self) -> bool {
        self.stage() >= Stage::Collapsed
    }

    /// USD left before every model call stops; T8 admits a new auto-Challenge only if this covers
    /// its whole Debate.
    pub fn left(&self) -> f64 {
        (self.ceiling - self.spent).max(0.0)
    }

    /// The budget's part of the status; `lagging`, `PAUSED (you)` and `BLIND` come from elsewhere.
    pub fn status(&self) -> Status {
        match self.stage() {
            Stage::Watching => Status::Watching,
            Stage::Paused => Status::Paused { spent: self.spent, ceiling: self.ceiling },
            _ => Status::Thinned,
        }
    }
}

/// Always-visible Watcher status, pane only (V49).
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Watching,
    Thinned,
    Lagging,
    Paused { spent: f64, ceiling: f64 },
    PausedByYou,
    Blind,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Watching => f.write_str("watching"),
            Status::Thinned => f.write_str("thinned"),
            Status::Lagging => f.write_str("lagging"),
            Status::Paused { spent, ceiling } => write!(f, "paused {}/{}", usd(*spent), usd(*ceiling)),
            Status::PausedByYou => f.write_str("PAUSED (you)"),
            Status::Blind => f.write_str("BLIND"),
        }
    }
}

/// `$4.52`; whole dollars drop the cents (`$5`).
pub fn usd(x: f64) -> String {
    let cents = (x * 100.0).round();
    if cents % 100.0 == 0.0 { format!("${}", cents / 100.0) } else { format!("${:.2}", cents / 100.0) }
}
