//! Strict JSONL Watcher output, one object per line (I10). Any bad line makes the whole output
//! invalid: a diagnostic, never a Concern. Empty output = silent wake. Shape only; who may touch
//! which Concern is the arbiter's call.

use serde::Deserialize;

use crate::concern::{Evidence, Resolution, Severity};

pub const HEADLINE_MAX: usize = 100;
pub const DETAIL_MAX: usize = 3;

#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Line {
    Add {
        severity: Severity,
        /// Required when the Watcher holds more than one Focus (Companion).
        #[serde(default)]
        focus: Option<String>,
        headline: String,
        #[serde(default)]
        detail: Vec<String>,
        #[serde(default)]
        refs: Vec<String>,
        #[serde(default)]
        challenge: bool,
        #[serde(default)]
        evidence: Vec<Evidence>,
        #[serde(default)]
        alternative: Option<String>,
    },
    /// New evidence on an own Concern; absent fields stay as they are.
    Update {
        id: String,
        #[serde(default)]
        severity: Option<Severity>,
        #[serde(default)]
        headline: Option<String>,
        #[serde(default)]
        detail: Option<Vec<String>>,
        #[serde(default)]
        refs: Option<Vec<String>>,
        #[serde(default)]
        challenge: Option<bool>,
        #[serde(default)]
        evidence: Option<Vec<Evidence>>,
        #[serde(default)]
        alternative: Option<String>,
    },
    Resolve {
        id: String,
        reason: Reason,
        /// The kept Concern, when `reason` = `duplicate`.
        #[serde(default)]
        of: Option<String>,
    },
    /// Blocked wake: this Watcher's one consideration line (V40).
    Consider { text: String },
    /// Reply owed in an open Debate (V31).
    Debate { decision: Decision, text: String },
    /// Turn-end wake: the human's prompt clearly takes a side on an Escalation (V71).
    InferredRuling { escalation: String, side: Side, prompt_ref: String },
    /// A Concern withheld because a Standing ruling covers it (V78).
    HeldBack { ruling: String },
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    Fixed,
    NoLongerApplies,
    Duplicate,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Concede,
    Rebut,
    Escalate,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Watcher,
    Worker,
}

impl Reason {
    /// `None` for `duplicate`, which closes as a duplicate rather than a resolution.
    pub fn resolution(self) -> Option<Resolution> {
        match self {
            Reason::Fixed => Some(Resolution::Fixed),
            Reason::NoLongerApplies => Some(Resolution::NoLongerApplies),
            Reason::Duplicate => None,
        }
    }
}

/// Lines in order, or the first problem as `line <n>: <why>`.
pub fn parse(text: &str) -> Result<Vec<Line>, String> {
    let mut lines = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let line = serde_json::from_str::<Line>(raw)
            .map_err(|e| e.to_string())
            .and_then(|l| check(&l).map(|()| l))
            .map_err(|e| format!("line {}: {e}", i + 1))?;
        lines.push(line);
    }
    Ok(lines)
}

fn check(line: &Line) -> Result<(), String> {
    match line {
        Line::Add { focus, headline, detail, refs, evidence, alternative, .. } => {
            if let Some(f) = focus {
                text("focus", f)?;
            }
            check_headline(headline)?;
            check_detail(detail)?;
            check_refs(refs)?;
            check_evidence(evidence)?;
            alternative.as_deref().map_or(Ok(()), |a| text("alternative", a))
        }
        Line::Update { id, headline, detail, refs, evidence, alternative, .. } => {
            text("id", id)?;
            headline.as_deref().map_or(Ok(()), check_headline)?;
            detail.as_deref().map_or(Ok(()), check_detail)?;
            refs.as_deref().map_or(Ok(()), check_refs)?;
            evidence.as_deref().map_or(Ok(()), check_evidence)?;
            alternative.as_deref().map_or(Ok(()), |a| text("alternative", a))
        }
        Line::Resolve { id, reason, of } => {
            text("id", id)?;
            match (reason, of) {
                (Reason::Duplicate, Some(of)) => text("of", of),
                (Reason::Duplicate, None) => Err("duplicate needs `of`".into()),
                (_, Some(_)) => Err("`of` only with reason duplicate".into()),
                (_, None) => Ok(()),
            }
        }
        Line::Consider { text: t } | Line::Debate { text: t, .. } => text("text", t),
        Line::InferredRuling { escalation, prompt_ref, .. } => {
            text("escalation", escalation)?;
            text("prompt_ref", prompt_ref)
        }
        Line::HeldBack { ruling } => text("ruling", ruling),
    }
}

/// Non-blank; multi-line values belong in `detail`.
fn text(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} is empty"));
    }
    Ok(())
}

fn one_line(field: &str, value: &str) -> Result<(), String> {
    text(field, value)?;
    if value.contains('\n') {
        return Err(format!("{field} spans lines"));
    }
    Ok(())
}

fn check_headline(h: &str) -> Result<(), String> {
    one_line("headline", h)?;
    if h.chars().count() > HEADLINE_MAX {
        return Err(format!("headline over {HEADLINE_MAX} chars"));
    }
    Ok(())
}

fn check_detail(detail: &[String]) -> Result<(), String> {
    if detail.len() > DETAIL_MAX {
        return Err(format!("detail over {DETAIL_MAX} lines"));
    }
    detail.iter().try_for_each(|d| one_line("detail line", d))
}

/// `file:line` or `file:start-end`.
fn check_refs(refs: &[String]) -> Result<(), String> {
    refs.iter().try_for_each(|r| if is_ref(r) { Ok(()) } else { Err(format!("ref {r:?} is not file:line")) })
}

fn is_ref(r: &str) -> bool {
    let Some((file, lines)) = r.rsplit_once(':') else { return false };
    let num = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    !file.trim().is_empty()
        && !r.contains(char::is_whitespace)
        && match lines.split_once('-') {
            Some((a, b)) => num(a) && num(b),
            None => num(lines),
        }
}

fn check_evidence(evidence: &[Evidence]) -> Result<(), String> {
    evidence.iter().try_for_each(|e| match e {
        Evidence::File { at, excerpt } => {
            if !is_ref(at) {
                return Err(format!("evidence ref {at:?} is not file:line"));
            }
            text("evidence excerpt", excerpt)
        }
        Evidence::Command { command, output } => {
            text("evidence command", command)?;
            text("evidence output", output)
        }
        Evidence::Transcript { excerpt } | Evidence::Hint { excerpt } => text("evidence excerpt", excerpt),
    })
}
