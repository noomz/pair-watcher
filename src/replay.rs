//! `replay --variant <prompt|focus|model> <wake-set>` (V98, V96): re-runs recorded wakes with one
//! piece swapped for what is current — the built-in prompt, the Focus briefs on disk, or the
//! configured `model` — and scores the answers against the audit labels and the human's
//! dismissals. Read tools are served from the session's snapshot store, never the live repo.
//! Spend is reported, outside any session ceiling.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use crate::audit::{self, Label};
use crate::concern::Change;
use crate::event::Kind;
use crate::home::Home;
use crate::human::Act;
use crate::models::Table;
use crate::output::{self, Line};
use crate::provider::{Api, Call};
use crate::record::{self, WakeRecord};
use crate::report::{Filter, Session, sessions};
use crate::snapshot::Store;
use crate::status::Wake;
use crate::tools::Snapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    Prompt,
    Focus,
    Model,
}

/// One recorded wake to re-run.
pub struct Item {
    pub session: usize,
    pub wake: Wake,
    pub record: WakeRecord,
}

/// The wake set: `labelled` (every audit-labelled wake), `<session>`, `<session>:<n>` or
/// `<session>:<n>-<m>`. Only wakes with a record.
pub fn select(home: &Home, set: &str) -> Result<(Vec<Session>, Vec<Item>), String> {
    let all = sessions(home, &Filter::default()).map_err(|e| e.to_string())?;
    let (id, range) = match set.split_once(':') {
        Some((id, r)) => {
            let (a, b) = r.split_once('-').unwrap_or((r, r));
            let n = |s: &str| s.parse::<u64>().map_err(|_| format!("wake set `{set}`: `{s}` is not a wake number"));
            (Some(id), Some((n(a)?, n(b)?)))
        }
        None if set == "labelled" => (None, None),
        None => (Some(set), None),
    };
    if let Some(id) = id
        && !all.iter().any(|s| s.id == id)
    {
        return Err(format!("no session `{id}` (wake set: labelled | <session>[:<n>[-<m>]])"));
    }
    let mut items = Vec::new();
    for (i, s) in all.iter().enumerate() {
        if id.is_some_and(|id| id != s.id) {
            continue;
        }
        let labelled = labelled(s);
        for (_, w) in s.wakes() {
            let in_range = range.is_none_or(|(a, b)| (a..=b).contains(&w.n));
            let wanted = if id.is_some() { in_range } else { labelled.contains_key(&(w.n, w.watcher.clone())) };
            let Some(rel) = w.record.clone().filter(|_| wanted) else { continue };
            if let Ok(record) = record::read(&s.dir, &rel) {
                items.push(Item { session: i, wake: w, record });
            }
        }
    }
    if items.is_empty() {
        return Err(format!("wake set `{set}`: no recorded wakes"));
    }
    Ok((all, items))
}

/// Labelled findings per (wake, Watcher): (Focus, label).
fn labelled(s: &Session) -> HashMap<(u64, String), Vec<(String, Label)>> {
    let mut findings = HashMap::new();
    let mut out: HashMap<(u64, String), Vec<(String, Label)>> = HashMap::new();
    for r in s.records.iter().filter(|r| r.body.k == Kind::Audit) {
        match r.body.parse::<audit::Op>() {
            Some(audit::Op::Finding { id, wake, watcher, focus, .. }) => {
                findings.insert(id, (wake, watcher, focus));
            }
            Some(audit::Op::Label { finding, label }) => {
                if let Some((wake, watcher, focus)) = findings.get(&finding) {
                    out.entry((*wake, watcher.clone())).or_default().push((focus.clone(), label));
                }
            }
            _ => {}
        }
    }
    out
}

/// What is current: the prompt, the briefs on disk, the configured model.
pub struct Current<'a> {
    pub prompt: &'a str,
    pub briefs: &'a HashMap<String, String>,
    pub model: Option<&'a str>,
}

/// One call to make.
#[derive(Debug)]
pub struct Job {
    pub item: usize,
    pub input: String,
    pub model: String,
}

pub fn jobs(items: &[Item], variant: Variant, cur: &Current) -> Result<Vec<Job>, String> {
    let model =
        match variant {
            Variant::Model => Some(cur.model.filter(|m| *m != "auto").ok_or(
                "replay --variant model: set `model` in config to the model to try (`auto` picks per Worker)",
            )?),
            _ => None,
        };
    let mut out = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let r = &it.record;
        let input = match variant {
            Variant::Prompt => r.render(cur.prompt, &r.briefs),
            Variant::Focus => {
                let briefs: Vec<(String, String)> = r
                    .briefs
                    .iter()
                    .map(|(n, b)| (n.clone(), cur.briefs.get(n).cloned().unwrap_or_else(|| b.clone())))
                    .collect();
                r.render(&r.prompt, &briefs)
            }
            Variant::Model => r.render(&r.prompt, &r.briefs),
        };
        let model = model.map(str::to_owned).unwrap_or_else(|| it.wake.model.clone());
        if model.is_empty() {
            continue;
        }
        out.push(Job { item: i, input, model });
    }
    Ok(out)
}

/// Output tokens assumed per call for estimates.
const OUT_ESTIMATE: u64 = 1_000;

/// USD estimate: input ≈ bytes / 4 tokens; unpriced → refused.
pub fn estimate(jobs: &[Job], table: &Table) -> Result<f64, String> {
    jobs.iter().try_fold(0.0, |sum, j| {
        let p = table.price(&j.model).map_err(|u| u.to_string())?;
        let u = crate::models::Usage { input: (j.input.len() / 4) as u64, output: OUT_ESTIMATE, ..Default::default() };
        Ok(sum + p.cost(&u))
    })
}

/// One model call outside any Watcher: (answer text or error, USD).
pub fn ask_once(
    api: &Api,
    table: &Table,
    model: &str,
    input: &str,
    tools: Option<Snapshot>,
) -> (Result<String, String>, f64) {
    let Ok(price) = table.price(model) else { return (Err(format!("model `{model}` is unpriced")), 0.0) };
    let deadline = Some(Instant::now() + crate::fail::WAKE_CLOCK);
    let call = Call { model, vendor: price.vendor, input, tools, deadline, fallback: false };
    match api.call(&call) {
        Ok(reply) => (Ok(reply.text), price.cost(&reply.usage)),
        Err(f) => (Err(f.error.to_string()), price.cost(&f.usage)),
    }
}

/// The session's snapshot store, when its repo is still there.
pub fn store(s: &Session) -> Option<Store> {
    let repo = s.start()?.body.str("repo").map(Path::new)?;
    Store::open(&s.dir, &crate::repo::discover(repo)?).ok()
}

/// A Concern the answer adds, with its Focus (a single-Focus Watcher's own when unnamed).
fn adds(text: &str, focuses: &[&str]) -> Option<Vec<(String, String)>> {
    let lines = output::parse(text).ok()?;
    Some(
        lines
            .into_iter()
            .filter_map(|l| match l {
                Line::Add { focus, headline, .. } => {
                    let f = focus.or_else(|| (focuses.len() == 1).then(|| focuses[0].to_owned()))?;
                    Some((f, headline))
                }
                _ => None,
            })
            .collect(),
    )
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Score {
    pub wakes: usize,
    /// Confirmed misses (audit label `miss`) whose Focus the variant now raises.
    pub misses_caught: usize,
    pub misses: usize,
    /// Concerns the human dismissed or muted, raised again (same Focus) by the variant.
    pub dismissed_again: usize,
    pub dismissed: usize,
    pub invalid: usize,
    pub failed: usize,
    pub original_usd: f64,
    pub variant_usd: f64,
    pub lines: Vec<String>,
}

/// Runs the jobs one by one; tools from each session's store.
pub fn run(api: &Api, table: &Table, all: &[Session], items: &[Item], jobs: &[Job]) -> Score {
    let mut stores: HashMap<usize, Option<Store>> = HashMap::new();
    let mut score = Score::default();
    for j in jobs {
        let it = &items[j.item];
        let s = &all[it.session];
        let store = stores.entry(it.session).or_insert_with(|| store(s));
        let tools = match (store.as_ref(), it.record.tree.as_deref()) {
            (Some(store), Some(tree)) => Some(Snapshot { store, tree, head: it.record.head.as_deref() }),
            _ => None,
        };
        let (answer, usd) = ask_once(api, table, &j.model, &j.input, tools);
        score.add(s, it, &answer, usd);
    }
    score
}

impl Score {
    fn add(&mut self, s: &Session, it: &Item, answer: &Result<String, String>, usd: f64) {
        self.wakes += 1;
        self.original_usd += it.wake.spend;
        self.variant_usd += usd;
        let focuses: Vec<&str> = it.record.focuses().collect();
        let before = it.record.output.as_deref().and_then(|o| adds(o, &focuses)).unwrap_or_default();
        let after = match answer {
            Ok(text) => match adds(text, &focuses) {
                Some(a) => a,
                None => {
                    self.invalid += 1;
                    Vec::new()
                }
            },
            Err(_) => {
                self.failed += 1;
                Vec::new()
            }
        };
        let raises = |f: &str| after.iter().any(|(af, _)| af == f);
        for (focus, label) in labelled(s).remove(&(it.wake.n, it.wake.watcher.clone())).unwrap_or_default() {
            if label == Label::Miss {
                self.misses += 1;
                self.misses_caught += usize::from(raises(&focus));
            }
        }
        let rejected = rejected(s);
        for (focus, headline) in &before {
            if rejected.contains(&(focus.clone(), headline.clone())) {
                self.dismissed += 1;
                self.dismissed_again += usize::from(raises(focus));
            }
        }
        let now = match answer {
            Err(e) => format!("failed ({e})"),
            Ok(_) if after.is_empty() => "silent".to_owned(),
            Ok(_) => format!(
                "{} Concerns: {}",
                after.len(),
                after.iter().map(|(_, h)| h.as_str()).collect::<Vec<_>>().join(" · ")
            ),
        };
        self.lines.push(format!(
            "{}:{} {} · recorded {} ({} Concerns, ${:.3}) → variant {now} (${usd:.3})",
            s.id,
            it.wake.n,
            it.wake.watcher,
            serde_json::to_value(it.wake.outcome).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default(),
            before.len(),
            it.wake.spend,
        ));
    }

    pub fn summary(&self) -> Vec<String> {
        let mut out = self.lines.clone();
        out.push(format!(
            "replayed {} wakes · confirmed misses now caught {}/{} · dismissed-like Concerns raised again {}/{} · invalid {} · failed {}",
            self.wakes, self.misses_caught, self.misses, self.dismissed_again, self.dismissed, self.invalid, self.failed
        ));
        out.push(format!(
            "cost: recorded ${:.3} → variant ${:.3} (outside any session ceiling)",
            self.original_usd, self.variant_usd
        ));
        out
    }
}

/// (Focus, headline) of Concerns the human dismissed or muted in the session.
fn rejected(s: &Session) -> Vec<(String, String)> {
    let mut by_id: HashMap<String, (String, String)> = HashMap::new();
    let mut out = Vec::new();
    for r in &s.records {
        match r.body.k {
            Kind::Concern => {
                if let Some(Change::Add { concern, .. }) = r.body.parse::<Change>() {
                    by_id.insert(concern.id.clone(), (concern.focus, concern.headline));
                }
            }
            Kind::Human => match r.body.parse::<Act>() {
                Some(Act::Dismiss { id }) => out.extend(by_id.get(&id).cloned()),
                Some(Act::Mute { focus }) => out.extend(by_id.values().filter(|(f, _)| *f == focus).cloned()),
                _ => {}
            },
            _ => {}
        }
    }
    out
}
