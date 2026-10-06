//! Concern record shape (I10) as wake input lists it (V7, V44). Lifecycle and ids: T7 arbiter.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Note,
    Warn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Open,
    Dismissed,
    Resolved,
}

/// One Concern; a list of them is oldest first.
#[derive(Clone, Debug, PartialEq)]
pub struct Concern {
    pub id: String,
    /// Owner Focus (V44).
    pub focus: String,
    pub severity: Severity,
    pub state: State,
    pub headline: String,
    pub detail: Vec<String>,
    pub refs: Vec<String>,
}

impl Severity {
    pub fn name(self) -> &'static str {
        match self {
            Severity::Note => "note",
            Severity::Warn => "warn",
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
