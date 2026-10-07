//! What the human's surfaces (pane, mod, band, `/pw`, CLI) append to the session log: facts only
//! (V43). Code-owned state follows from them in the folds; `promote` is a request the arbiter
//! answers (`Challenges::requests`).

use serde::{Deserialize, Serialize};

use crate::event::{Event, Kind};

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
