//! Plain-code arbiter (V43): no model; sole writer of the Concern list, ids, the per-wake cap and
//! the `concern` events. Companion = Swarm of one: the same path runs one Watcher holding every
//! brief, or one Watcher per Focus.

use std::collections::{HashSet, VecDeque};
use std::io;

use crate::concern::{Change, Concern, ConcernList, Severity, State};
use crate::event::EventLog;
use crate::focus::Focus;
use crate::output::Line;

/// New-or-updated Concerns shown per wake per Worker; resolutions are free (V14).
pub const CAP: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Watcher {
    /// `companion`, or the Focus name in Swarm.
    pub name: String,
    /// Focus names in enabled order. The Watcher gets their briefs and owns their Concerns (V44).
    pub focuses: Vec<String>,
    /// Per-Focus model from a user Focus file, Swarm only (V45); `None` = the default `model`.
    pub model: Option<String>,
}

/// `swarm` = mode `swarm` and not collapsed by the budget ladder (V48): one Watcher per Focus.
/// Otherwise one Companion with every enabled brief on the default model.
pub fn roster(enabled: &[Focus], swarm: bool) -> Vec<Watcher> {
    if swarm {
        return enabled
            .iter()
            .map(|f| Watcher { name: f.name.clone(), focuses: vec![f.name.clone()], model: f.model.clone() })
            .collect();
    }
    vec![Watcher { name: "companion".into(), focuses: enabled.iter().map(|f| f.name.clone()).collect(), model: None }]
}

/// One call per Watcher, all in parallel; results in roster order (V43).
pub fn run<T: Send>(watchers: &[Watcher], call: impl Fn(&Watcher) -> T + Sync) -> Vec<T> {
    let call = &call;
    std::thread::scope(|s| {
        let tasks: Vec<_> = watchers.iter().map(|w| s.spawn(move || call(w))).collect();
        tasks.into_iter().map(|t| t.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))).collect()
    })
}

/// One Watcher's valid output for this wake (`output::parse`).
pub struct Output<'a> {
    pub watcher: &'a Watcher,
    pub lines: Vec<Line>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Merged {
    /// `concern` events in log order: resolutions, then shown adds/updates, then trimmed.
    pub changes: Vec<Change>,
    /// Lines for Debate, Escalation and ruling handling, with their Watcher's name.
    pub rest: Vec<(String, Line)>,
    /// Ops the lifecycle rules dropped, as `<watcher>: <why>`.
    pub dropped: Vec<String>,
}

struct Candidate {
    watcher: usize,
    concern: Concern,
    update: bool,
}

/// Applies one wake's outputs (roster order) to `list`: each Watcher adds, updates its own with new
/// evidence, or resolves its own (V13, V44); at most [`CAP`] adds/updates are shown (V14).
pub fn merge(list: &ConcernList, outputs: &[Output]) -> Merged {
    let mut merged = Merged::default();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut touched: HashSet<&str> = HashSet::new();
    for (wi, out) in outputs.iter().enumerate() {
        let w = out.watcher;
        for line in &out.lines {
            let result = match line {
                Line::Add { .. } => added(list, &candidates, w, line)
                    .map(|concern| candidates.push(Candidate { watcher: wi, concern, update: false })),
                Line::Update { id, .. } => own(list, w, id, &touched).and_then(|c| {
                    let concern = updated(c, line).ok_or_else(|| format!("{id}: update without new evidence"))?;
                    touched.insert(&c.id);
                    candidates.push(Candidate { watcher: wi, concern, update: true });
                    Ok(())
                }),
                Line::Resolve { id, reason, of } => own(list, w, id, &touched).and_then(|c| {
                    let watcher = w.name.clone();
                    let change = match (reason.resolution(), of) {
                        (Some(reason), _) => Change::Resolve { watcher, id: c.id.clone(), reason },
                        (None, Some(of)) if of != id && list.get(of).is_some() => {
                            Change::Duplicate { watcher, id: c.id.clone(), of: of.clone() }
                        }
                        (None, _) => {
                            return Err(format!("{id}: duplicate of unknown {}", of.as_deref().unwrap_or("?")));
                        }
                    };
                    touched.insert(&c.id);
                    merged.changes.push(change);
                    Ok(())
                }),
                other => {
                    merged.rest.push((w.name.clone(), other.clone()));
                    Ok(())
                }
            };
            if let Err(why) = result {
                merged.dropped.push(format!("{}: {why}", w.name));
            }
        }
    }

    let picked = pick(&candidates, outputs.len());
    let mut minted = 0;
    for &i in &picked {
        let Candidate { watcher, concern, update } = &candidates[i];
        let watcher = outputs[*watcher].watcher.name.clone();
        let mut concern = concern.clone();
        merged.changes.push(if *update {
            Change::Update { watcher, concern }
        } else {
            concern.id = list.mint(minted);
            minted += 1;
            Change::Add { watcher, concern }
        });
    }
    let trimmed = candidates.iter().enumerate().filter(|(i, _)| !picked.contains(i)).map(|(_, c)| c);
    for c in trimmed {
        merged.changes.push(Change::Trimmed {
            watcher: outputs[c.watcher].watcher.name.clone(),
            focus: c.concern.focus.clone(),
            severity: c.concern.severity,
            headline: c.concern.headline.clone(),
            id: c.update.then(|| c.concern.id.clone()),
        });
    }
    merged
}

/// Writes each change to the event log, then folds it into the list.
pub fn commit(list: &mut ConcernList, log: &EventLog, changes: &[Change]) -> io::Result<()> {
    for change in changes {
        let event = change.event();
        log.append(event.clone())?;
        list.apply(&event);
    }
    Ok(())
}

/// The open Concern `id`, if this Watcher owns it and nothing else changed it this wake.
fn own<'l>(list: &'l ConcernList, w: &Watcher, id: &str, touched: &HashSet<&str>) -> Result<&'l Concern, String> {
    let c = list.get(id).ok_or_else(|| format!("{id}: no such Concern"))?;
    if !w.focuses.contains(&c.focus) {
        return Err(format!("{id}: owned by Focus {}", c.focus));
    }
    match c.state {
        State::Dismissed => Err(format!("{id}: dismissed, never reopens")),
        State::Resolved => Err(format!("{id}: already resolved")),
        State::Open if touched.contains(id) => Err(format!("{id}: already changed this wake")),
        State::Open => Ok(c),
    }
}

/// A new Concern, unless it repeats one already listed or raised this wake (V13, V44). A resolved
/// Concern may come back only with new evidence.
fn added(list: &ConcernList, pending: &[Candidate], w: &Watcher, line: &Line) -> Result<Concern, String> {
    let Line::Add { severity, focus, headline, detail, refs, challenge, evidence, alternative } = line else {
        unreachable!("added() takes add lines")
    };
    let focus = match focus {
        Some(f) if w.focuses.contains(f) => f.clone(),
        Some(f) => return Err(format!("add for Focus {f}, not this Watcher's")),
        None if w.focuses.len() == 1 => w.focuses[0].clone(),
        None => return Err("add without focus".into()),
    };
    let concern = Concern {
        id: String::new(),
        focus,
        severity: *severity,
        state: State::Open,
        headline: headline.clone(),
        detail: detail.clone(),
        refs: refs.clone(),
        challenge: *challenge,
        evidence: evidence.clone(),
        alternative: alternative.clone(),
    };
    let key = norm(headline);
    for c in list.all().iter().filter(|c| norm(&c.headline) == key) {
        let same = c.detail == concern.detail && c.refs == concern.refs && c.evidence == concern.evidence;
        match c.state {
            State::Open => return Err(format!("repeat of {}", c.id)),
            State::Dismissed => return Err(format!("repeat of dismissed {}", c.id)),
            State::Resolved if same => return Err(format!("re-raise of {} without new evidence", c.id)),
            State::Resolved => {}
        }
    }
    if pending.iter().any(|p| !p.update && norm(&p.concern.headline) == key) {
        return Err(format!("repeat of another add this wake: {headline}"));
    }
    Ok(concern)
}

/// `c` with the update's fields applied; `None` when nothing changes.
fn updated(c: &Concern, line: &Line) -> Option<Concern> {
    let Line::Update { severity, headline, detail, refs, challenge, evidence, alternative, .. } = line else {
        unreachable!("updated() takes update lines")
    };
    let mut next = c.clone();
    if let Some(s) = severity {
        next.severity = *s;
    }
    if let Some(h) = headline {
        next.headline = h.clone();
    }
    if let Some(d) = detail {
        next.detail = d.clone();
    }
    if let Some(r) = refs {
        next.refs = r.clone();
    }
    if let Some(ch) = challenge {
        next.challenge = *ch;
    }
    if let Some(e) = evidence {
        next.evidence = e.clone();
    }
    if let Some(a) = alternative {
        next.alternative = Some(a.clone());
    }
    (next != *c).then_some(next)
}

/// Shown candidates in fill order: challenge-flagged, then `warn`, then `note`; within each class
/// round-robin across Watchers, each in its own ranking (output order).
fn pick(candidates: &[Candidate], watchers: usize) -> Vec<usize> {
    let class = |c: &Concern| match (c.challenge, c.severity) {
        (true, _) => 0,
        (false, Severity::Warn) => 1,
        (false, Severity::Note) => 2,
    };
    let mut picked = Vec::new();
    for k in 0..3 {
        let mut queues: Vec<VecDeque<usize>> = (0..watchers)
            .map(|w| {
                (0..candidates.len())
                    .filter(|&i| candidates[i].watcher == w && class(&candidates[i].concern) == k)
                    .collect()
            })
            .collect();
        while picked.len() < CAP && queues.iter().any(|q| !q.is_empty()) {
            for q in &mut queues {
                if picked.len() < CAP
                    && let Some(i) = q.pop_front()
                {
                    picked.push(i);
                }
            }
        }
    }
    picked
}

fn norm(headline: &str) -> String {
    headline.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}
