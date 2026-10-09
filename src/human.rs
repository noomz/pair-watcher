//! What the human's surfaces (pane, mod, band, `/pw`, CLI) append to the session log: facts only
//! (V43). Code-owned state follows from them in the folds; `promote` is a request the arbiter
//! answers (`Challenges::requests`).

use serde::{Deserialize, Serialize};

use crate::event::{Event, Kind};
use crate::ruling::Via;

/// `human` event payload (V94).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Act {
    /// Final: the Concern never reopens (V13).
    Dismiss { id: String },
    /// `P` / `p`: Challenge this Concern, no bar, optional one-line remark (V20).
    Promote {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remark: Option<String>,
    },
    /// `+` / `h`: the Concern or Escalation helped (V95 H2).
    Helpful { id: String },
    /// `m`: this Focus stops for the session (V70).
    Mute { focus: String },
    /// `a`: no auto-Challenges for the session (V23).
    Nochallenge,
    /// `F` / `/pw primary` / `primary` (V51): back to the primary model, after `pair-watcher auth`.
    Primary,
    /// `s` (V77): the ruling or dismissal on Concern `id` kept as Standing ruling `ruling` (V78).
    RulingRemembered {
        id: String,
        ruling: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<Via>,
    },
    /// `x` / `rulings rm` (V78): Standing ruling `ruling` removed.
    RulingRemoved { ruling: String, via: Via },
    /// The Watcher judged the human's prompt a correction of the Worker (V94): arbiter-written
    /// from wake `wake`'s output, at most one per wake. Audit candidates follow from it (V97).
    Correction { prompt_ref: String, watcher: String, wake: u64 },
    /// Session start: the rulings file was edited by hand since pair-watcher last knew it (V78).
    RulingsEdited {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        added: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        removed: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changed: Vec<String>,
    },
}

impl Act {
    pub fn event(&self) -> Event {
        Event::typed(Kind::Human, self)
    }
}

/// `shown <id>`: on screen ≥5 s in a running pane (V69).
pub fn shown(id: &str) -> Event {
    Event::new(Kind::Shown).with("id", id)
}
