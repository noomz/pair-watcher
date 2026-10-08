//! Payloads of the `wake`, `status`, `coverage` and `watcher` events (V94, I20): what the pane
//! shows as the header, wake strip and ticker (V63, V64). The Watcher loop and supervisor write
//! them (T15); human surfaces write only `raise` and the user pause (V43). Field names follow
//! V94; anything a reader does not know is ignored. Ladder steps are not logged: spend (`wake`)
//! and ceiling (`session` start + `raise`) give them (V48).

use serde::{Deserialize, Serialize};

use crate::event::{Event, Kind};

/// What one Watcher's wake ended as (V94 `wake` outcome).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Silent,
    Concern,
    Challenge,
    Escalation,
    Invalid,
    Failed,
    Interrupted,
}

/// Failure class (V86).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    Service,
    Slice,
    Key,
}

/// `wake` event body, one per Watcher per wake. `n` numbers the wake: every Watcher of one wake
/// shares it, so the pane draws one strip cell per wake.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Wake {
    pub n: u64,
    pub watcher: String,
    /// `turn-end`, `checkpoint`, `blocked`, `catch-up`.
    pub trigger: String,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<Class>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub model: String,
    /// USD this wake cost (V47).
    #[serde(default)]
    pub spend: f64,
    /// Recovery replay of a held slice: reviewed late (V92).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replayed: bool,
    /// Standing rulings the Watcher held a Concern back for (`held_back`, V78, V94).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rulings: Vec<String>,
}

impl Wake {
    pub fn event(&self) -> Event {
        Event::typed(Kind::Wake, self)
    }
}

/// `status` event payload (V94). `focus` set = that Focus's Watcher only (Swarm); unset = all.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Op {
    /// The model a Watcher runs (V46) and its pane notes.
    Model {
        watcher: String,
        model: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        same_vendor: bool,
        /// Subscription backend (V52).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        shares_quota: bool,
    },
    /// `$` / `b`: the ceiling grows by $5 and the Watcher resumes (V50). Human-written.
    Raise,
    /// `Z`: no wakes, no spend, not BLIND (V70). Human-written.
    UserPause,
    UserResume,
    Lagging {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focus: Option<String>,
    },
    LaggingEnd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focus: Option<String>,
    },
    Blind {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focus: Option<String>,
        reason: String,
    },
    BlindEnd {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focus: Option<String>,
    },
    /// `fallback_model` took over (V51).
    Fallback {
        model: String,
        class: Class,
    },
    Primary,
    /// The first BLIND's one notification (V41); `by` = supervisor | pane.
    BlindNotified {
        by: String,
    },
}

impl Op {
    pub fn event(&self) -> Event {
        Event::typed(Kind::Status, self)
    }
}

/// `coverage` event payload (V94); times are unix ms.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Coverage {
    Delayed {
        from: u64,
        to: u64,
    },
    /// Activity never reviewed (V88, V92): unknown, never successful silence.
    Lost {
        from: u64,
        to: u64,
        /// `poison-skip`, `transcript-unrecoverable`, `merge-omitted`.
        cause: String,
    },
    BacklogPaused {
        from: u64,
        to: u64,
    },
}

impl Coverage {
    pub fn event(&self) -> Event {
        Event::typed(Kind::Coverage, self)
    }
}

/// `watcher` event payload (V90, V94).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum WatcherOp {
    Restart {
        why: String,
    },
    Crash {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focus: Option<String>,
        why: String,
    },
    HangKill,
    CrashLoop {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focus: Option<String>,
    },
    SupervisorGone,
}

impl WatcherOp {
    pub fn event(&self) -> Event {
        Event::typed(Kind::Watcher, self)
    }
}
