//! `report` (V95, V98, V99): metrics computed at report time from the session event logs (V93),
//! no index, no live counters. A pure fold per session into [`Stats`]; `render` prints the
//! headlines, `export` the shareable aggregate JSON; telemetry sends the same ratios.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::audit::{self, Label};
use crate::challenge;
use crate::concern::{Change, Resolution};
use crate::debate;
use crate::escalation::{self, Cause};
use crate::event::{Event, Kind};
use crate::home::Home;
use crate::human::Act;
use crate::jsonl::Record;
use crate::mailbox::LOG_FILE;
use crate::ruling::{self, Ruled, Side, Via};
use crate::standing::{self, Standing};
use crate::status::{Coverage, Op, Wake};

/// Ratios print only from this many items (V95).
pub const MIN_N: usize = 20;
/// H1 flags a Focus whose noise is over this (V95).
const NOISE_FLAG: f64 = 0.25;
/// A Standing ruling uncited this long is a prune candidate (V98).
const UNCITED_DAYS: u64 = 30;
const DAY_MS: u64 = 86_400_000;

/// Which sessions, by their `session` start date (`YYYY-MM-DD`, UTC), and whether inferred
/// rulings count (V95 `--explicit-only`).
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub since: Option<String>,
    pub until: Option<String>,
    pub explicit_only: bool,
}

impl Filter {
    fn admits(&self, start_ms: u64) -> bool {
        let day = standing::date(start_ms, 0);
        self.since.as_ref().is_none_or(|s| day >= *s) && self.until.as_ref().is_none_or(|u| day <= *u)
    }
}

/// `YYYY-MM-DD` or an error naming the flag.
pub fn check_date(flag: &str, d: &str) -> Result<(), String> {
    let ok =
        d.len() == 10 && d.char_indices().all(|(i, c)| if i == 4 || i == 7 { c == '-' } else { c.is_ascii_digit() });
    if ok { Ok(()) } else { Err(format!("--{flag} {d}: expected YYYY-MM-DD")) }
}

/// One session's log, read once.
#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub dir: PathBuf,
    pub records: Vec<Record<Event>>,
}

impl Session {
    pub fn start(&self) -> Option<&Record<Event>> {
        self.records.iter().find(|r| r.body.k == Kind::Session && r.body.str("phase") == Some("start"))
    }

    pub fn ended(&self) -> bool {
        self.records.iter().any(|r| r.body.k == Kind::Session && r.body.str("phase") == Some("end"))
    }

    pub fn wakes(&self) -> impl Iterator<Item = (u64, Wake)> + '_ {
        self.records.iter().filter(|r| r.body.k == Kind::Wake).filter_map(|r| Some((r.t, r.body.parse::<Wake>()?)))
    }
}

/// Every session under `sessions/*/` whose start passes `filter`, oldest first (V98: no index).
pub fn sessions(home: &Home, filter: &Filter) -> io::Result<Vec<Session>> {
    let mut out = Vec::new();
    let Ok(dirs) = fs::read_dir(home.sessions_dir()) else { return Ok(out) };
    for dir in dirs.filter_map(Result::ok) {
        let path = dir.path().join(LOG_FILE);
        if !path.is_file() {
            continue;
        }
        let records = crate::event::EventLog::new(&path).read_from(0)?.records;
        let s = Session { id: dir.file_name().to_string_lossy().into_owned(), dir: dir.path(), records };
        if s.start().is_some_and(|st| filter.admits(st.t)) {
            out.push(s);
        }
    }
    out.sort_by_key(|s| s.start().map_or(0, |r| r.t));
    Ok(out)
}

/// What a headline is grouped by (V95).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Dims {
    pub watcher: String,
    pub focus: String,
    pub model: String,
    pub mode: String,
}

/// A numerator over a denominator; `inferred` = part of `num` that only an inferred ruling gave.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ratio {
    pub num: usize,
    pub den: usize,
    pub inferred: usize,
}

impl Ratio {
    fn add(&mut self, hit: bool, inferred: bool) {
        self.den += 1;
        if hit {
            self.num += 1;
            if inferred {
                self.inferred += 1;
            }
        }
    }

    pub fn value(&self) -> Option<f64> {
        (self.den >= MIN_N).then(|| self.num as f64 / self.den as f64)
    }

    fn show(&self) -> String {
        match self.value() {
            None => "n<20".into(),
            Some(v) if self.inferred > 0 => format!("{v:.2} (n {}, {} inferred)", self.den, self.inferred),
            Some(v) => format!("{v:.2} (n {})", self.den),
        }
    }

    fn json(&self) -> Value {
        json!({ "ratio": self.value(), "n": self.den, "inferred": self.inferred })
    }

    fn merge(&mut self, o: &Ratio) {
        self.num += o.num;
        self.den += o.den;
        self.inferred += o.inferred;
    }
}

/// H1–H4 for one group (V95).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Headlines {
    /// (dismissed + muted) ÷ shown Concerns.
    pub h1: Ratio,
    /// (`+` or promoted or side=watcher) ÷ shown Concerns.
    pub h2: Ratio,
    /// side ∈ {watcher, worker} ÷ notified unresolved-Debate Escalations − closed-stale.
    pub h3: Ratio,
    /// (worker conceded and later resolved fixed, or side=watcher) ÷ Challenges sent.
    pub h4: Ratio,
    /// watcher concedes ÷ Challenges sent, printed beside H4.
    pub watcher_concedes: Ratio,
}

impl Headlines {
    fn merge(&mut self, o: &Headlines) {
        for (a, b) in [
            (&mut self.h1, &o.h1),
            (&mut self.h2, &o.h2),
            (&mut self.h3, &o.h3),
            (&mut self.h4, &o.h4),
            (&mut self.watcher_concedes, &o.watcher_concedes),
        ] {
            a.merge(b);
        }
    }
}

/// Runtime availability (V98), ms per state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Availability {
    pub watched: u64,
    pub lagging: u64,
    pub blind: u64,
    pub paused: u64,
    pub paused_by_user: u64,
}

/// Review coverage (V98), ms: lost = unknown, never successful silence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReviewCoverage {
    pub on_time: u64,
    pub late: u64,
    pub lost: u64,
}

/// Blocked Escalations: own report line (V95).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockedLine {
    pub shown: usize,
    pub helpful: usize,
    pub dismissed: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub sessions: usize,
    /// Sessions with no pane: in no ratio (audit only, V95).
    pub paneless: usize,
    /// Sessions with lost spans: silence not certifiable (V97).
    pub with_lost: usize,
    pub groups: BTreeMap<Dims, Headlines>,
    pub blocked: BlockedLine,
    pub availability: Availability,
    pub coverage: ReviewCoverage,
    /// Audit labels per Focus: (already said, miss, not a miss).
    pub audit: BTreeMap<String, (usize, usize, usize)>,
    /// harness → versions seen; pair-watcher versions; Focus names.
    pub harnesses: BTreeMap<String, BTreeSet<String>>,
    pub versions: BTreeSet<String>,
    pub focuses: BTreeSet<String>,
    /// Standing rulings cited by wakes in range: (session dir, ruling id, time).
    pub cited: Vec<(PathBuf, String, u64)>,
    /// Standing rulings kept / removed in range (telemetry counts, V99).
    pub rulings_added: usize,
    pub rulings_removed: usize,
}

impl Stats {
    pub fn fold(sessions: &[Session], filter: &Filter) -> Stats {
        let mut stats = Stats::default();
        for s in sessions {
            stats.session(s, filter);
        }
        stats
    }

    /// All groups merged along one dimension.
    pub fn by(&self, dim: fn(&Dims) -> &str) -> BTreeMap<String, Headlines> {
        let mut out: BTreeMap<String, Headlines> = BTreeMap::new();
        for (d, h) in &self.groups {
            out.entry(dim(d).to_owned()).or_default().merge(h);
        }
        out
    }

    pub fn total(&self) -> Headlines {
        let mut all = Headlines::default();
        self.groups.values().for_each(|h| all.merge(h));
        all
    }

    fn session(&mut self, s: &Session, filter: &Filter) {
        let Some(start) = s.start() else { return };
        self.sessions += 1;
        let field = |k: &str| start.body.str(k).unwrap_or_default().to_owned();
        let mode = field("mode");
        if !field("harness").is_empty() {
            let v = start.body.str("harness_version").unwrap_or("unknown").to_owned();
            self.harnesses.entry(field("harness")).or_default().insert(v);
        }
        if !field("version").is_empty() {
            self.versions.insert(field("version"));
        }
        if let Some(Value::Array(fs)) = start.body.fields.get("focuses") {
            self.focuses.extend(fs.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        let f = Fold::run(s);
        self.time(s, &f, start.t);
        for (t, w) in s.wakes() {
            self.cited.extend(w.rulings.iter().map(|r| (s.dir.clone(), r.clone(), t)));
        }
        self.rulings_added += f.kept;
        self.rulings_removed += f.removed;
        for (finding, label) in &f.labels {
            let focus = f.findings.get(finding).cloned().unwrap_or_default();
            let e = self.audit.entry(focus).or_default();
            match label {
                Label::AlreadySaid => e.0 += 1,
                Label::Miss => e.1 += 1,
                Label::NotAMiss => e.2 += 1,
            }
        }
        if f.shown.is_empty() {
            self.paneless += 1;
            return;
        }
        let ruling = |id: &str| f.effective(id, filter.explicit_only);
        let on_concern = |id: &str| f.on_concern(id, filter.explicit_only);
        let dims = |c: &str| -> Dims {
            let info = f.concerns.get(c).cloned().unwrap_or_default();
            let model = f.models.get(&info.watcher).cloned().unwrap_or_default();
            Dims { watcher: info.watcher, focus: info.focus, model, mode: mode.clone() }
        };

        for id in f.concerns.keys() {
            if !f.shown.contains(id) {
                continue;
            }
            let h = self.groups.entry(dims(id)).or_default();
            let ruled = on_concern(id);
            let noise =
                f.dismissed.contains(id) || f.muted.contains(id) || ruled.is_some_and(|r| r.side == Side::Dismissed);
            h.h1.add(noise, false);
            let sided = ruled.filter(|r| r.side == Side::Watcher);
            let marked = f.helpful.contains(id) || f.promoted.contains(id);
            h.h2.add(marked || sided.is_some(), !marked && sided.is_some_and(|r| r.via == Via::Prompt));
        }
        for e in &f.escalations {
            if !f.notified.contains(&e.id) || f.closed_stale.contains(&e.id) {
                continue;
            }
            let r = ruling(&e.id);
            match e.cause {
                Cause::Unresolved => {
                    let h = self.groups.entry(dims(e.concern.as_deref().unwrap_or_default())).or_default();
                    let acted = r.filter(|r| matches!(r.side, Side::Watcher | Side::Worker));
                    h.h3.add(acted.is_some(), acted.is_some_and(|r| r.via == Via::Prompt));
                }
                Cause::Blocked => {
                    self.blocked.shown += 1;
                    self.blocked.helpful += usize::from(f.helpful.contains(&e.id));
                    self.blocked.dismissed += usize::from(r.is_some_and(|r| r.side == Side::Dismissed));
                }
            }
        }
        for (ch, concern) in &f.challenges {
            if !f.sent.contains(ch) {
                continue;
            }
            let h = self.groups.entry(dims(concern)).or_default();
            let end = f.ends.get(ch);
            let conceded_fixed = end == Some(&debate::Outcome::WorkerConcedes) && f.fixed.contains(concern);
            let sided = ruling(ch).or_else(|| on_concern(concern)).filter(|r| r.side == Side::Watcher);
            h.h4.add(conceded_fixed || sided.is_some(), !conceded_fixed && sided.is_some_and(|r| r.via == Via::Prompt));
            h.watcher_concedes.add(end == Some(&debate::Outcome::WatcherConcedes), false);
        }
    }

    /// Availability and coverage over the session's span (V98).
    fn time(&mut self, s: &Session, f: &Fold, from: u64) {
        let end = s.records.last().map_or(from, |r| r.t);
        let a = &mut self.availability;
        for w in f.spans.windows(2) {
            let ((t0, st), (t1, _)) = (w[0], w[1]);
            let slot = match st {
                St::Blind => &mut a.blind,
                St::UserPaused => &mut a.paused_by_user,
                St::Paused => &mut a.paused,
                St::Lagging => &mut a.lagging,
                St::Watched => &mut a.watched,
            };
            *slot += t1.saturating_sub(t0);
        }
        let total = end.saturating_sub(from);
        let late: u64 = f.delayed.iter().map(|(a, b)| b.saturating_sub(*a)).sum();
        let lost: u64 = f.lost.iter().map(|(a, b)| b.saturating_sub(*a)).sum();
        self.coverage.late += late;
        self.coverage.lost += lost;
        self.coverage.on_time += total.saturating_sub(late + lost);
        if !f.lost.is_empty() {
            self.with_lost += 1;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum St {
    Watched,
    Lagging,
    Paused,
    UserPaused,
    Blind,
}

#[derive(Clone, Debug, Default)]
struct ConcernInfo {
    watcher: String,
    focus: String,
    open: bool,
}

/// One ruling as it stands: `undone` ones never count (V95).
#[derive(Clone, Debug)]
struct Ruling {
    side: Side,
    via: Via,
    concern: Option<String>,
    /// Log order across items.
    seq: usize,
    undone: bool,
}

impl Ruling {
    fn counts(&self, explicit_only: bool) -> bool {
        !self.undone && !(explicit_only && self.via == Via::Prompt)
    }
}

/// One session's log folded into the facts the headlines read.
#[derive(Default)]
struct Fold {
    concerns: BTreeMap<String, ConcernInfo>,
    models: HashMap<String, String>,
    shown: HashSet<String>,
    dismissed: HashSet<String>,
    muted: HashSet<String>,
    helpful: HashSet<String>,
    promoted: HashSet<String>,
    fixed: HashSet<String>,
    escalations: Vec<escalation::Escalation>,
    notified: HashSet<String>,
    closed_stale: HashSet<String>,
    rulings: HashMap<String, Vec<Ruling>>,
    /// Challenge id → Concern id.
    challenges: BTreeMap<String, String>,
    sent: HashSet<String>,
    /// Last `debate end` per Challenge.
    ends: HashMap<String, debate::Outcome>,
    spans: Vec<(u64, St)>,
    delayed: Vec<(u64, u64)>,
    lost: Vec<(u64, u64)>,
    findings: HashMap<String, String>,
    labels: BTreeMap<String, Label>,
    kept: usize,
    removed: usize,
}

impl Fold {
    fn run(s: &Session) -> Fold {
        let mut f = Fold::default();
        let mut esc = escalation::Escalations::default();
        let (mut lagging, mut blind) = (HashSet::<Option<String>>::new(), HashSet::<Option<String>>::new());
        let (mut user_paused, mut spend, mut ceiling) = (false, 0.0, 0.0);
        for r in &s.records {
            let e = &r.body;
            match e.k {
                Kind::Session if e.str("phase") == Some("start") => {
                    ceiling = e.fields.get("ceiling_usd").and_then(Value::as_f64).unwrap_or(5.0);
                }
                Kind::Concern => f.concern(e),
                Kind::Human => f.human(e),
                Kind::Shown => f.shown.extend(e.str("id").map(str::to_owned)),
                Kind::Escalation => {
                    esc.apply(e);
                    if let Some(escalation::Op::Notified { id, .. }) = e.parse() {
                        f.notified.insert(id);
                    }
                }
                Kind::ClosedStale => f.closed_stale.extend(e.str("id").map(str::to_owned)),
                Kind::Ruled => {
                    if let Some(r) = e.parse::<Ruled>() {
                        let seq = f.rulings.values().map(Vec::len).sum();
                        let x = Ruling { side: r.side, via: r.via, concern: r.concern, seq, undone: false };
                        f.rulings.entry(r.id).or_default().push(x);
                    }
                }
                Kind::Ruling => {
                    if let Some(ruling::Op::Undone { ruled, nth, .. }) = e.parse()
                        && let Some(x) = f.rulings.get_mut(&ruled).and_then(|v| v.get_mut(nth))
                    {
                        x.undone = true;
                    }
                }
                Kind::Challenge => match e.parse::<challenge::Op>() {
                    Some(challenge::Op::Queued { id, concern, .. }) => {
                        f.challenges.insert(id, concern);
                    }
                    Some(challenge::Op::Sent { id, .. }) => {
                        f.sent.insert(id);
                    }
                    _ => {}
                },
                Kind::Debate => {
                    if let Some(debate::Op::End { challenge, outcome, .. }) = e.parse() {
                        f.ends.insert(challenge, outcome);
                    }
                }
                Kind::Wake => {
                    if let Some(w) = e.parse::<Wake>() {
                        spend += w.spend;
                        if !w.model.is_empty() {
                            f.models.entry(w.watcher).or_insert(w.model);
                        }
                    }
                }
                Kind::Status => match e.parse::<Op>() {
                    Some(Op::Model { watcher, model, .. }) => {
                        f.models.insert(watcher, model);
                    }
                    Some(Op::Raise) => ceiling += 5.0,
                    Some(Op::UserPause) => user_paused = true,
                    Some(Op::UserResume) => user_paused = false,
                    Some(Op::Lagging { focus }) => {
                        lagging.insert(focus);
                    }
                    Some(Op::LaggingEnd { focus }) => {
                        lagging.remove(&focus);
                    }
                    Some(Op::Blind { focus, .. }) => {
                        blind.insert(focus);
                    }
                    Some(Op::BlindEnd { focus }) => {
                        blind.remove(&focus);
                    }
                    _ => {}
                },
                Kind::Coverage => match e.parse::<Coverage>() {
                    Some(Coverage::Delayed { from, to }) => f.delayed.push((from, to)),
                    Some(Coverage::Lost { from, to, .. }) => f.lost.push((from, to)),
                    _ => {}
                },
                Kind::Audit => match e.parse::<audit::Op>() {
                    Some(audit::Op::Finding { id, focus, .. }) => {
                        f.findings.insert(id, focus);
                    }
                    Some(audit::Op::Label { finding, label }) => {
                        f.labels.insert(finding, label);
                    }
                    _ => {}
                },
                _ => {}
            }
            let st = if !blind.is_empty() {
                St::Blind
            } else if user_paused {
                St::UserPaused
            } else if ceiling > 0.0 && spend >= ceiling - 1e-9 {
                St::Paused
            } else if !lagging.is_empty() {
                St::Lagging
            } else {
                St::Watched
            };
            if f.spans.last().is_none_or(|(_, last)| *last != st) {
                f.spans.push((r.t, st));
            }
        }
        if let Some(last) = s.records.last() {
            f.spans.push((last.t, St::Watched));
        }
        f.escalations = esc.all().to_vec();
        f
    }

    fn concern(&mut self, e: &Event) {
        match e.parse::<Change>() {
            Some(Change::Add { watcher, concern }) => {
                let info = ConcernInfo { watcher, focus: concern.focus, open: true };
                self.concerns.insert(concern.id, info);
            }
            Some(Change::Resolve { id, reason, .. }) => {
                if reason == Resolution::Fixed {
                    self.fixed.insert(id.clone());
                }
                self.close(&id);
            }
            Some(Change::Duplicate { id, .. }) => self.close(&id),
            _ => {}
        }
    }

    fn close(&mut self, id: &str) {
        if let Some(c) = self.concerns.get_mut(id) {
            c.open = false;
        }
    }

    fn human(&mut self, e: &Event) {
        match e.parse::<Act>() {
            Some(Act::Dismiss { id }) => {
                self.close(&id);
                self.dismissed.insert(id);
            }
            Some(Act::Mute { focus }) => {
                let open: Vec<String> = self
                    .concerns
                    .iter()
                    .filter(|(_, c)| c.open && c.focus == focus)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in open {
                    self.close(&id);
                    self.muted.insert(id);
                }
            }
            Some(Act::Helpful { id }) => {
                self.helpful.insert(id);
            }
            Some(Act::Promote { id, .. }) => {
                self.promoted.insert(id);
            }
            Some(Act::RulingRemembered { .. }) => self.kept += 1,
            Some(Act::RulingRemoved { .. }) => self.removed += 1,
            _ => {}
        }
    }

    /// The ruling standing on item `id`: the last not undone; inferred ones dropped when
    /// `explicit_only` (V95).
    fn effective(&self, id: &str, explicit_only: bool) -> Option<&Ruling> {
        self.rulings.get(id)?.iter().rev().find(|r| r.counts(explicit_only))
    }

    /// The latest standing ruling on Concern `id`, whichever item (Escalation, Challenge) it was
    /// made on.
    fn on_concern(&self, id: &str, explicit_only: bool) -> Option<&Ruling> {
        self.rulings
            .values()
            .flatten()
            .filter(|r| r.concern.as_deref() == Some(id) && r.counts(explicit_only))
            .max_by_key(|r| r.seq)
    }
}

type Dim = (&'static str, fn(&Dims) -> &str);
const DIMS: [Dim; 4] =
    [("Watcher", |d| &d.watcher), ("Focus", |d| &d.focus), ("model", |d| &d.model), ("mode", |d| &d.mode)];

fn pct(part: u64, total: u64) -> String {
    if total == 0 { "–".into() } else { format!("{:.0}%", part as f64 * 100.0 / total as f64) }
}

fn headline_line(name: &str, h: &Headlines, flag_noise: bool) -> String {
    let flag = flag_noise && h.h1.value().is_some_and(|v| v > NOISE_FLAG);
    format!(
        "  {name:<18} H1 noise {}{} · H2 helpful {} · H3 Escalation {} · H4 Challenge {} (watcher concedes {})",
        h.h1.show(),
        if flag { " ⚑ over 25%" } else { "" },
        h.h2.show(),
        h.h3.show(),
        h.h4.show(),
        h.watcher_concedes.show(),
    )
}

/// The printed report (V95, V97, V98).
pub fn render(stats: &Stats, prune: &[String], filter: &Filter) -> Vec<String> {
    let mut out = Vec::new();
    let range = match (&filter.since, &filter.until) {
        (None, None) => "all sessions".to_owned(),
        (s, u) => format!("{} … {}", s.as_deref().unwrap_or("start"), u.as_deref().unwrap_or("today")),
    };
    let explicit = if filter.explicit_only { " · explicit rulings only" } else { "" };
    out.push(format!(
        "pair-watcher report · {range} · {} sessions ({} without a pane: audit only){explicit}",
        stats.sessions, stats.paneless
    ));
    out.push(headline_line("all", &stats.total(), false));
    for (title, dim) in DIMS {
        let groups = stats.by(dim);
        if groups.is_empty() || groups.len() < 2 && title != "Focus" {
            continue;
        }
        out.push(format!("by {title}"));
        for (name, h) in &groups {
            out.push(headline_line(if name.is_empty() { "?" } else { name }, h, title == "Focus"));
        }
    }
    let b = &stats.blocked;
    out.push(format!(
        "blocked Escalations: {} shown · {} helpful (+) · {} not worth the ping (d)",
        b.shown, b.helpful, b.dismissed
    ));

    let a = &stats.availability;
    let total = a.watched + a.lagging + a.blind + a.paused + a.paused_by_user;
    out.push(format!(
        "availability: watched {} · lagging {} · blind {} · paused {} · paused by you {} ({:.1} h)",
        pct(a.watched, total),
        pct(a.lagging, total),
        pct(a.blind, total),
        pct(a.paused, total),
        pct(a.paused_by_user, total),
        total as f64 / 3_600_000.0
    ));
    let c = &stats.coverage;
    let total = c.on_time + c.late + c.lost;
    out.push(format!(
        "coverage: on time {} · reviewed late {} · lost {} (lost = unknown, never successful silence)",
        pct(c.on_time, total),
        pct(c.late, total),
        pct(c.lost, total)
    ));

    if stats.audit.is_empty() {
        out.push("recall: no audit labels yet (pair-watcher audit)".into());
    } else {
        out.push("recall, measured over auditable coverage (caught ÷ caught + confirmed misses):".into());
        for (focus, (caught, miss, not)) in &stats.audit {
            let r = Ratio { num: *caught, den: caught + miss, inferred: 0 };
            out.push(format!("  {focus:<18} {} · {not} not a miss", r.show()));
        }
    }
    if stats.with_lost > 0 {
        out.push(format!("silence not certifiable: {} sessions with lost spans", stats.with_lost));
    }
    if !prune.is_empty() {
        out.push(format!("Standing rulings uncited {UNCITED_DAYS} days (prune candidates, `rulings rm`):"));
        out.extend(prune.iter().map(|p| format!("  {p}")));
    }
    out
}

/// Standing rulings no wake cited in the last 30 days, kept at least that long (V98). Scans every
/// rulings store; citations come from every session, whatever the range.
pub fn prune_candidates(home: &Home, all: &[Session], now: u64) -> io::Result<Vec<String>> {
    let cutoff = now.saturating_sub(UNCITED_DAYS * DAY_MS);
    let cited: HashSet<String> =
        all.iter().flat_map(|s| s.wakes().filter(|(t, _)| *t >= cutoff).flat_map(|(_, w)| w.rulings)).collect();
    let old = standing::date(cutoff, 0);
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir(home.repos_dir()) else { return Ok(out) };
    let mut stores: Vec<PathBuf> = dir.filter_map(Result::ok).map(|e| e.path()).collect();
    stores.sort();
    for p in stores {
        let Some(name) = p.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".rulings.toml")) else {
            continue;
        };
        for r in standing::load(&p)? {
            if r.date <= old && !cited.contains(&r.id) {
                out.push(format!("{name} {} [{}] {} (since {})", r.id, r.focus, r.headline, r.date));
            }
        }
    }
    Ok(out)
}

/// The rulings the exported wakes cite, from each session's repo store (V99).
fn cited_rulings(home: &Home, stats: &Stats) -> Vec<Standing> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (dir, id, _) in &stats.cited {
        let Some(session) = dir.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(repo) = crate::mailbox::Mailbox::open(home, session).and_then(|mb| mb.repo()) else { continue };
        let path = standing::path(home, &standing::key(&repo));
        if seen.insert((path.clone(), id.clone()))
            && let Some(r) = standing::load(&path).ok().and_then(|v| v.into_iter().find(|r| &r.id == id))
        {
            out.push(r);
        }
    }
    out
}

/// Ratios with n per group, harness/model/version, Focus names (V99). `focus` maps names
/// (telemetry: custom → `custom`).
pub fn aggregate(stats: &Stats, focus: &dyn Fn(&str) -> String) -> Value {
    let mut groups = Map::new();
    for (title, dim) in DIMS {
        let mut by: BTreeMap<String, Headlines> = BTreeMap::new();
        for (d, h) in &stats.groups {
            let key = dim(d);
            let key = if matches!(title, "Watcher" | "Focus") { focus(key) } else { key.to_owned() };
            by.entry(key).or_default().merge(h);
        }
        let values: Map<String, Value> = by.iter().map(|(k, h)| (k.clone(), headlines_json(h))).collect();
        groups.insert(title.to_lowercase(), Value::Object(values));
    }
    let audit: Map<String, Value> = stats
        .audit
        .iter()
        .map(|(f, (caught, miss, not))| {
            let r = Ratio { num: *caught, den: caught + miss, inferred: 0 };
            (focus(f), json!({ "recall": r.json(), "not_a_miss": not }))
        })
        .collect();
    json!({
        "sessions": stats.sessions,
        "all": headlines_json(&stats.total()),
        "groups": groups,
        "blocked": { "shown": stats.blocked.shown, "helpful": stats.blocked.helpful, "dismissed": stats.blocked.dismissed },
        "availability_ms": {
            "watched": stats.availability.watched, "lagging": stats.availability.lagging,
            "blind": stats.availability.blind, "paused": stats.availability.paused,
            "paused_by_user": stats.availability.paused_by_user,
        },
        "coverage_ms": { "on_time": stats.coverage.on_time, "late": stats.coverage.late, "lost": stats.coverage.lost },
        "audit": audit,
        "harnesses": stats.harnesses,
        "versions": stats.versions,
        "focuses": stats.focuses.iter().map(|f| focus(f)).collect::<BTreeSet<_>>(),
    })
}

fn headlines_json(h: &Headlines) -> Value {
    json!({
        "h1_noise": h.h1.json(), "h2_helpful": h.h2.json(), "h3_escalation": h.h3.json(),
        "h4_challenge": h.h4.json(), "watcher_concedes": h.watcher_concedes.json(),
    })
}

/// `report --export`: the aggregate + only the rulings cited in exported wakes; shared by hand.
pub fn export(home: &Home, stats: &Stats) -> Value {
    let mut v = aggregate(stats, &|f| f.to_owned());
    let rulings: Vec<Value> = cited_rulings(home, stats)
        .iter()
        .map(|r| json!({ "id": r.id, "focus": r.focus, "headline": r.headline, "verdict": r.verdict, "reason": r.reason, "date": r.date }))
        .collect();
    v["rulings"] = Value::Array(rulings);
    v
}

/// `report`: lines to print, or the export JSON.
pub fn report_cmd(home: &Home, filter: &Filter, export_json: bool, now: u64) -> io::Result<Vec<String>> {
    let picked = sessions(home, filter)?;
    let stats = Stats::fold(&picked, filter);
    if export_json {
        return Ok(vec![serde_json::to_string_pretty(&export(home, &stats))?]);
    }
    let all = sessions(home, &Filter::default())?;
    Ok(render(&stats, &prune_candidates(home, &all, now)?, filter))
}
