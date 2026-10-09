//! Silent-session audit (V97): candidates, an auditor model's findings, the human's labels. Only
//! the labels count; `report` turns them into recall per Focus.

use serde::{Deserialize, Serialize};

use crate::event::{Event, Kind};

/// Why a wake became a candidate.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Why {
    /// The wake before a human correction (V94 `human correction`).
    Correction,
    /// Random sample of silent/skipped wakes.
    Sample,
}

/// The human's label on a finding (V97).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Label {
    Miss,
    NotAMiss,
    AlreadySaid,
}

/// `audit` event payload (V94), appended to the audited session's log.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Op {
    Candidate {
        wake: u64,
        watcher: String,
        why: Why,
    },
    /// One thing the auditor says a reviewer should have raised on wake `wake`.
    Finding {
        id: String,
        wake: u64,
        watcher: String,
        focus: String,
        headline: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        detail: Vec<String>,
        auditor: String,
        /// USD, outside the session ceiling (V97).
        spend: f64,
    },
    Label {
        finding: String,
        label: Label,
    },
}

impl Op {
    pub fn event(&self) -> Event {
        Event::typed(Kind::Audit, self)
    }
}
