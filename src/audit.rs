//! Silent-session audit (V97): candidates, an auditor model's findings, the human's labels. Only
//! the labels count; `report` turns them into recall per Focus. Audit spend is reported outside
//! any session ceiling.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::event::{Event, EventLog, Kind};
use crate::home::{Home, write_atomic};
use crate::human::Act;
use crate::mailbox::LOG_FILE;
use crate::models::{Table, Usage};
use crate::provider::Api;
use crate::record;
use crate::replay;
use crate::report::Session;
use crate::status::{Coverage, Outcome, Wake};
use crate::tools::Snapshot;

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
    /// Wake `wake` of `watcher` audited by `auditor`; its findings follow.
    Candidate {
        wake: u64,
        watcher: String,
        why: Why,
        auditor: String,
        /// USD, outside the session ceiling (V97).
        spend: f64,
        /// The auditor failed: no findings, audit it again later.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
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

const PROMPT: &str = include_str!("audit_prompt.md");
/// Output tokens assumed per audit call for the estimate.
const OUT_ESTIMATE: u64 = 1_500;

/// One wake to audit.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Candidate {
    pub session: String,
    pub wake: u64,
    pub watcher: String,
    pub why: Why,
    pub auditor: String,
}

/// An interrupted run's queue (`--resume`) and what it spent.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Queue {
    pub left: Vec<Candidate>,
    pub spent: f64,
}

fn queue_path(home: &Home) -> PathBuf {
    home.root().join("audit.json")
}

pub fn load_queue(home: &Home) -> Queue {
    fs::read(queue_path(home)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_queue(home: &Home, q: &Queue) -> io::Result<()> {
    if q.left.is_empty() {
        return match fs::remove_file(queue_path(home)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    write_atomic(&queue_path(home), &serde_json::to_vec(q)?)
}

/// Default auditor (V97): the next tier up (by output price) from the mid-tier of a vendor other
/// than the audited Watcher's; that vendor's top model when none is above. Unpriced → refused.
pub fn auditor(table: &Table, audited: &str, chosen: Option<&str>) -> Result<String, String> {
    if let Some(m) = chosen {
        table.price(m).map_err(|u| u.to_string())?;
        return Ok(m.to_owned());
    }
    let own = table.price(audited).map(|p| p.vendor).map_err(|u| u.to_string())?;
    let (vendor, mid) =
        table.defaults.iter().find(|(v, _)| **v != own).ok_or_else(|| {
            format!("no model of a vendor other than {} in the model table: pass --model", own.name())
        })?;
    let floor = table.price(mid).map_err(|u| u.to_string())?.output;
    let mut theirs: Vec<(&String, f64)> =
        table.models.iter().filter(|(_, p)| p.vendor == *vendor).map(|(m, p)| (m, p.output)).collect();
    theirs.sort_by(|a, b| a.1.total_cmp(&b.1));
    let next = theirs.iter().find(|(_, o)| *o > floor).or(theirs.last()).map(|(m, _)| (*m).clone());
    next.ok_or_else(|| format!("no {} model priced", vendor.name()))
}

/// Wakes already audited (a `candidate` without error).
fn audited(s: &Session) -> HashSet<(u64, String)> {
    s.records
        .iter()
        .filter(|r| r.body.k == Kind::Audit)
        .filter_map(|r| match r.body.parse::<Op>()? {
            Op::Candidate { wake, watcher, error: None, .. } => Some((wake, watcher)),
            _ => None,
        })
        .collect()
}

/// `coverage lost` spans: activity never reviewed, so silence there certifies nothing (V97).
pub fn lost(s: &Session) -> Vec<(u64, u64)> {
    s.records
        .iter()
        .filter(|r| r.body.k == Kind::Coverage)
        .filter_map(|r| match r.body.parse::<Coverage>()? {
            Coverage::Lost { from, to, .. } => Some((from, to)),
            _ => None,
        })
        .collect()
}

/// Candidates (V97): the wake before each human correction, per Watcher, then a random `sample`
/// of silent/skipped wakes from auditable coverage. Ended sessions, recorded wakes, never one
/// already audited. A skipped wake is audited against its session's model.
pub fn candidates(
    all: &[Session],
    sample: usize,
    table: &Table,
    model: Option<&str>,
    seed: u64,
) -> Result<Vec<Candidate>, String> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut pool: Vec<(String, Candidate)> = Vec::new();
    for s in all.iter().filter(|s| s.ended()) {
        let done = audited(s);
        let wakes: Vec<(u64, Wake)> = s.wakes().filter(|(_, w)| w.record.is_some()).collect();
        let Some(session_model) = wakes.iter().map(|(_, w)| &w.model).find(|m| !m.is_empty()) else { continue };
        let candidate = |w: &Wake, why: Why| -> Result<Candidate, String> {
            let audited = if w.model.is_empty() { session_model } else { &w.model };
            let auditor = auditor(table, audited, model)?;
            Ok(Candidate { session: s.id.clone(), wake: w.n, watcher: w.watcher.clone(), why, auditor })
        };
        let open = |w: &Wake, out: &[Candidate]| {
            !done.contains(&(w.n, w.watcher.clone()))
                && !out.iter().any(|c| c.session == s.id && c.wake == w.n && c.watcher == w.watcher)
        };
        for r in s.records.iter().filter(|r| r.body.k == Kind::Human) {
            let Some(Act::Correction { wake, .. }) = r.body.parse::<Act>() else { continue };
            let mut before: HashMap<&str, &Wake> = HashMap::new();
            for (_, w) in wakes.iter().filter(|(_, w)| w.n < wake) {
                before.insert(&w.watcher, w);
            }
            let mut picked: Vec<&Wake> = before.into_values().collect();
            picked.sort_by_key(|w| (w.n, w.watcher.clone()));
            for w in picked {
                if open(w, &out) {
                    out.push(candidate(w, Why::Correction)?);
                }
            }
        }
        let lost = lost(s);
        for (t, w) in &wakes {
            let quiet = matches!(w.outcome, Outcome::Silent | Outcome::Skipped);
            let covered = !lost.iter().any(|(a, b)| (a..=b).contains(&t));
            if quiet && covered && open(w, &out) {
                let key = format!("{seed}:{}:{}:{}", s.id, w.n, w.watcher);
                pool.push((hex(&Sha256::digest(key.as_bytes())), candidate(w, Why::Sample)?));
            }
        }
    }
    pool.sort_by(|a, b| a.0.cmp(&b.0));
    out.extend(pool.into_iter().take(sample).map(|(_, c)| c));
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The auditor's input: its prompt, the wake as the Watcher saw it, the Watcher's answer.
fn input(rec: &record::WakeRecord) -> String {
    let answer = rec.output.as_deref().unwrap_or("").trim();
    let answer = if answer.is_empty() { "(silent)" } else { answer };
    format!(
        "{}\n\n# The wake\n\n{}\n\n# The Watcher's answer\n\n{answer}\n",
        PROMPT.trim(),
        rec.render(&rec.prompt, &rec.briefs).trim()
    )
}

fn session<'a>(all: &'a [Session], id: &str) -> Option<&'a Session> {
    all.iter().find(|s| s.id == id)
}

fn record_of(s: &Session, c: &Candidate) -> Option<record::WakeRecord> {
    let rel = s.wakes().find(|(_, w)| w.n == c.wake && w.watcher == c.watcher)?.1.record?;
    record::read(&s.dir, &rel).ok()
}

/// USD estimate for the queue: input ≈ bytes / 4 tokens.
pub fn estimate(all: &[Session], table: &Table, queue: &[Candidate]) -> f64 {
    queue.iter().map(|c| one_estimate(all, table, c)).sum()
}

fn one_estimate(all: &[Session], table: &Table, c: &Candidate) -> f64 {
    let Some(rec) = session(all, &c.session).and_then(|s| record_of(s, c)) else { return 0.0 };
    let u = Usage { input: (input(&rec).len() / 4) as u64, output: OUT_ESTIMATE, ..Usage::default() };
    table.price(&c.auditor).map_or(0.0, |p| p.cost(&u))
}

/// One finding line of the auditor's answer.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Found {
    focus: String,
    headline: String,
    #[serde(default)]
    detail: Vec<String>,
}

fn findings(text: &str) -> Result<Vec<Found>, String> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str::<Found>(l).map_err(|e| format!("invalid auditor output, line {}: {e}", i + 1))
        })
        .collect()
}

/// What a run did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ran {
    pub audited: usize,
    pub findings: usize,
    pub failed: usize,
    pub spent: f64,
    /// `--max-usd` reached with candidates left (`--resume` continues).
    pub stopped: bool,
}

/// Findings in a session log so far: ids run `A1`, `A2`, … per session.
fn findings_logged(log: &EventLog) -> io::Result<usize> {
    let batch = log.read_from(0)?;
    Ok(batch.records.iter().filter(|r| matches!(r.body.parse::<Op>(), Some(Op::Finding { .. }))).count())
}

/// Audits the queue in order, saving what is left after each wake; stops before a call whose
/// estimate would take the run past `max_usd` (V97 hard stop).
pub fn run(home: &Home, api: &Api, table: &Table, all: &[Session], mut q: Queue, max_usd: f64) -> io::Result<Ran> {
    let mut ran = Ran::default();
    while let Some(c) = q.left.first().cloned() {
        let found_session = session(all, &c.session);
        let rec = found_session.and_then(|s| record_of(s, &c));
        let (Some(s), Some(rec)) = (found_session, rec) else {
            q.left.remove(0);
            continue;
        };
        if q.spent + one_estimate(all, table, &c) > max_usd {
            ran.stopped = true;
            break;
        }
        let store = replay::store(s);
        let tools = match (store.as_ref(), rec.tree.as_deref()) {
            (Some(store), Some(tree)) => Some(Snapshot { store, tree, head: rec.head.as_deref() }),
            _ => None,
        };
        let (answer, usd) = replay::ask_once(api, table, &c.auditor, &input(&rec), tools);
        q.spent += usd;
        ran.spent += usd;
        let log = EventLog::new(s.dir.join(LOG_FILE));
        let found = answer.and_then(|text| findings(&text));
        let error = found.as_ref().err().cloned();
        let (wake, watcher, why, auditor) = (c.wake, c.watcher.clone(), c.why, c.auditor.clone());
        log.append(Op::Candidate { wake, watcher, why, auditor, spend: usd, error }.event())?;
        match found {
            Ok(found) => {
                let mut n = findings_logged(&log)?;
                for f in found {
                    n += 1;
                    let (id, watcher) = (format!("A{n}"), c.watcher.clone());
                    let (focus, headline, detail) = (f.focus, f.headline, f.detail);
                    log.append(Op::Finding { id, wake: c.wake, watcher, focus, headline, detail }.event())?;
                    ran.findings += 1;
                }
                ran.audited += 1;
            }
            Err(_) => ran.failed += 1,
        }
        q.left.remove(0);
        save_queue(home, &q)?;
    }
    save_queue(home, &q)?;
    Ok(ran)
}

/// A finding still waiting for the human's label.
#[derive(Clone, Debug, PartialEq)]
pub struct Unlabelled {
    pub session: String,
    pub dir: PathBuf,
    pub id: String,
    pub wake: u64,
    pub watcher: String,
    pub focus: String,
    pub headline: String,
    pub detail: Vec<String>,
}

pub fn unlabelled(all: &[Session]) -> Vec<Unlabelled> {
    let mut out = Vec::new();
    for s in all {
        let mut found: Vec<Unlabelled> = Vec::new();
        let mut labelled = HashSet::new();
        for r in s.records.iter().filter(|r| r.body.k == Kind::Audit) {
            match r.body.parse::<Op>() {
                Some(Op::Finding { id, wake, watcher, focus, headline, detail }) => found.push(Unlabelled {
                    session: s.id.clone(),
                    dir: s.dir.clone(),
                    id,
                    wake,
                    watcher,
                    focus,
                    headline,
                    detail,
                }),
                Some(Op::Label { finding, .. }) => {
                    labelled.insert(finding);
                }
                _ => {}
            }
        }
        out.extend(found.into_iter().filter(|f| !labelled.contains(&f.id)));
    }
    out
}

/// The human's label (V97): only these count.
pub fn label(f: &Unlabelled, label: Label) -> io::Result<()> {
    EventLog::new(f.dir.join(LOG_FILE)).append(Op::Label { finding: f.id.clone(), label }.event())
}
