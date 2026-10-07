//! Concerns (I10) and their one lifecycle (V13, V18). The list is a fold over the session event
//! log (V93): the arbiter writes `concern` changes, the human writes `human dismiss`, the Debate
//! engine writes `debate end` (V35); anything that reads the log (pane, restart) rebuilds the same
//! list.

use serde::{Deserialize, Serialize};

use crate::event::{Event, Kind};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Note,
    Warn,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Open,
    Dismissed,
    Resolved,
}

/// How a Debate left its Concern open (V35).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Tag {
    /// The Worker conceded: open until the Watcher observes the fix.
    Conceded,
    /// Unresolved and escalated (V36).
    Escalated,
    /// Unresolved below `escalate_unresolved`: no notification (V36).
    UnresolvedQuiet,
}

/// One cited item backing a Challenge; each must exist verbatim in its source (V17).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Evidence {
    /// `at` = `file:line`; `excerpt` = text at that line in the snapshot.
    File {
        #[serde(rename = "ref")]
        at: String,
        excerpt: String,
    },
    Command {
        command: String,
        output: String,
    },
    Transcript {
        excerpt: String,
    },
    /// A repeat hint line from the wake input (V4).
    Hint {
        excerpt: String,
    },
}

/// One Concern; a list of them is oldest first.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Concern {
    pub id: String,
    /// Owner Focus (V44).
    pub focus: String,
    pub severity: Severity,
    pub state: State,
    pub headline: String,
    pub detail: Vec<String>,
    pub refs: Vec<String>,
    /// The Watcher marks it approach-level and challenge-ready (V16).
    pub challenge: bool,
    pub evidence: Vec<Evidence>,
    pub alternative: Option<String>,
    /// Set by `challenge::verify` on the add/update that carries it: the bar holds and every cited
    /// item was found verbatim (V16, V17). Default-deny: absent = not challenge-ready.
    #[serde(default)]
    pub verified: bool,
    /// Set by the `debate end` that left it open (V35).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<Tag>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Resolution {
    Fixed,
    NoLongerApplies,
}

/// Payload of a `concern` event (V94). `watcher` = the Watcher whose output caused it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Change {
    Add {
        watcher: String,
        #[serde(flatten)]
        concern: Concern,
    },
    /// The owner's new evidence; replaces the Concern with the same id.
    Update {
        watcher: String,
        #[serde(flatten)]
        concern: Concern,
    },
    Resolve {
        watcher: String,
        id: String,
        reason: Resolution,
    },
    /// Resolved as a repeat of `of` (V44).
    Duplicate {
        watcher: String,
        id: String,
        of: String,
    },
    /// Over the per-wake cap: never shown, never listed, so a later raise is not a repeat (V14).
    /// `id` is set when an update was trimmed.
    Trimmed {
        watcher: String,
        focus: String,
        severity: Severity,
        headline: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Challenge-flagged, but cited evidence failed the verbatim check (V17): stays a Concern.
    #[serde(rename = "challenge-ready-unverifiable")]
    Unverifiable {
        watcher: String,
        id: String,
        failing: Vec<String>,
    },
}

impl Change {
    pub fn event(&self) -> Event {
        Event::typed(Kind::Concern, self)
    }
}

/// The human's `d` on a Concern (V94 `human dismiss`). Final: it never reopens (V13).
pub fn dismissal(id: &str) -> Event {
    Event::new(Kind::Human).with("action", "dismiss").with("id", id)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConcernList {
    concerns: Vec<Concern>,
}

impl ConcernList {
    pub fn rebuild<'a>(events: impl IntoIterator<Item = &'a Event>) -> ConcernList {
        let mut list = ConcernList::default();
        for e in events {
            list.apply(e);
        }
        list
    }

    /// Oldest first, all Focuses.
    pub fn all(&self) -> &[Concern] {
        &self.concerns
    }

    pub fn get(&self, id: &str) -> Option<&Concern> {
        self.concerns.iter().find(|c| c.id == id)
    }

    /// Id of the `n`-th Concern added after the current list (0 = next).
    pub fn mint(&self, n: usize) -> String {
        format!("c{}", self.concerns.len() + n + 1)
    }

    pub fn apply(&mut self, event: &Event) {
        match event.k {
            Kind::Concern => match event.parse::<Change>() {
                Some(Change::Add { concern, .. }) => self.concerns.push(concern),
                Some(Change::Update { concern, .. }) => {
                    if let Some(c) = self.find(&concern.id) {
                        *c = concern;
                    }
                }
                Some(Change::Resolve { id, .. } | Change::Duplicate { id, .. }) => self.close(&id, State::Resolved),
                // A re-check failed (V19): no longer challenge-ready until its owner updates it.
                Some(Change::Unverifiable { id, .. }) => {
                    if let Some(c) = self.find(&id) {
                        c.verified = false;
                    }
                }
                Some(Change::Trimmed { .. }) | None => {}
            },
            Kind::Human if event.str("action") == Some("dismiss") => {
                if let Some(id) = event.str("id") {
                    self.close(id, State::Dismissed);
                }
            }
            Kind::Debate if event.str("op") == Some("end") => {
                let Some(id) = event.str("concern") else { return };
                let escalated = event.fields.get("escalated").and_then(serde_json::Value::as_bool);
                match event.str("outcome") {
                    Some("worker-concedes") => self.tag(id, Tag::Conceded),
                    Some("watcher-concedes") => self.close(id, State::Resolved),
                    Some("unresolved") if escalated == Some(true) => self.tag(id, Tag::Escalated),
                    Some("unresolved") => self.tag(id, Tag::UnresolvedQuiet),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Only an open Concern carries a Debate tag; a resolved one keeps its end state.
    fn tag(&mut self, id: &str, tag: Tag) {
        if let Some(c) = self.find(id).filter(|c| c.state == State::Open) {
            c.tag = Some(tag);
        }
    }

    fn find(&mut self, id: &str) -> Option<&mut Concern> {
        self.concerns.iter_mut().find(|c| c.id == id)
    }

    /// Only an open Concern closes; a closed one never changes state again.
    fn close(&mut self, id: &str, state: State) {
        if let Some(c) = self.find(id).filter(|c| c.state == State::Open) {
            c.state = state;
        }
    }
}

impl Severity {
    pub fn name(self) -> &'static str {
        match self {
            Severity::Note => "note",
            Severity::Warn => "warn",
        }
    }
}

impl Tag {
    pub fn name(self) -> &'static str {
        match self {
            Tag::Conceded => "conceded",
            Tag::Escalated => "escalated",
            Tag::UnresolvedQuiet => "unresolved-quiet",
        }
    }
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Open => "open",
            State::Dismissed => "dismissed",
            State::Resolved => "resolved",
        }
    }
}
