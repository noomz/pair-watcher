//! The Watcher process (V43, V61, V90): `pair-watcher watch <session>`, child of the session's
//! supervisor. One task per Watcher per wake, all in parallel, plus the plain-code arbiter: sole
//! writer of Concerns, the Challenge slot, cursor commits and the event log. Its view of the log
//! is one fold ([`Board`]); arbiter calls run on copies of its parts and the fold re-reads what
//! they appended, so every reader agrees with the log (V93).
//!
//! Restart rebuilds from files (V91): the fold from the log, the committed cursors from
//! [`STATE_FILE`], and an interrupted wake from [`INFLIGHT_FILE`] (a Slice failure, once).
//! Cursors are per Focus (V10): a wake reads under its pending cursor and commits only what the
//! arbiter accepted; a failing Focus replays its held slice alone, oldest first, one bounded
//! slice per wake (V92).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::arbiter::{self, Output, Watcher};
use crate::budget::Need;
use crate::challenge::{self, Auto, Origin, Sources};
use crate::concern::{Change, Concern};
use crate::config::Resolved;
use crate::credentials::{self, Keys};
use crate::debate::EscalateAt;
use crate::delivery;
use crate::escalation::{self, Notifier, Surfaces};
use crate::event::{Event, EventLog};
use crate::fail::{self, Effect, Fallback, Health};
use crate::focus::Focus;
use crate::harness::Harness;
use crate::history::History;
use crate::home::{Home, write_atomic};
use crate::human::Act;
use crate::jsonl::now_ms;
use crate::mailbox::Mailbox;
use crate::models::{Choice, Table, Usage, Vendor};
use crate::outbox::Outbox;
use crate::output::{self, Line};
use crate::pane::{self, Board};
use crate::placement::{Placement, pane_command};
use crate::prefilter::{self, Gate, Tag, Verdict, Wakeup};
use crate::provider::{Api, Backend, Call, Provider, ProviderError};
use crate::record::{self, WakeRecord};
use crate::repo;
use crate::snapshot::Store;
use crate::status::{Class, Coverage, Op, Outcome, Wake, WatcherOp};
use crate::tailer::{self, Cursor, Entry, Format, Item};
use crate::tools::Snapshot;
use crate::trigger::{self, CHECKPOINT_FLOOR_MS, Due, Signal, Trigger, Triggers};
use crate::wake::{self, Context, Diff, Input, TURN_MAX};
use crate::{models, run, standing, supervisor};

pub const STATE_FILE: &str = "watch.json";
/// The wake under way: its start (the supervisor's progress deadline) and slices (V88, V90).
pub const INFLIGHT_FILE: &str = "inflight.json";
/// A blocked wake under way beside a normal one (V102).
pub const BLOCKED_INFLIGHT_FILE: &str = "inflight-blocked.json";
pub const HEARTBEAT_FILE: &str = "watcher.hb";
/// Focus task crashes; `attach` removes it (V90).
pub const CRASHES_FILE: &str = "crashes.json";
/// The supervisor asks for the final drain (V90).
pub const DRAIN_FILE: &str = "drain";
/// `run`'s config flags and `--focus` list, which the Watcher loads with (I1).
pub const LAUNCH_FILE: &str = "launch.json";
const PANE_OPENED: &str = "pane.opened";

pub const HEARTBEAT: Duration = Duration::from_secs(30);
pub const CRASH_WINDOW_MS: u64 = 10 * 60_000;
/// Task crashes per Focus in [`CRASH_WINDOW_MS`] before it stays BLIND (V90).
pub const CRASH_MAX: usize = 3;
/// Final drain wake (V90).
pub const DRAIN_CLOCK: Duration = Duration::from_secs(60);
const TICK: Duration = Duration::from_millis(250);
/// How often a normal wake in flight looks for a blocked one to run beside it (V102).
const ALONGSIDE_POLL: Duration = Duration::from_millis(50);
/// A turn end with no transcript turn this long, one rescan included, is BLIND (V89).
const TRANSCRIPT_WAIT_MS: u64 = 10_000;
const RESCAN_MS: u64 = TRANSCRIPT_WAIT_MS / 2;
/// Inputs quiet: repo changes seen by two probes this far apart with no hook or entry (V89).
const QUIET_SPAN_MS: u64 = 6 * 60_000;
/// Item text one replay slice holds before it ends at a turn end (V92).
const SLICE_MAX: usize = TURN_MAX;
/// How often a due checkpoint looks for repo changes (each look is a snapshot).
const CHANGE_POLL_MS: u64 = 30_000;
const CODEX_QUEUE_TIMEOUT: Duration = Duration::from_secs(10);
pub const PROMPT: &str = include_str!("watch_prompt.md");

/// One Focus's committed place (V10): transcript cursor, the snapshot its last diff ended at,
/// the digest state, and the arrival time of the last entry it reviewed.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Position {
    pub cursor: Cursor,
    pub tree: Option<String>,
    pub history: History,
    pub t: u64,
}

/// Wakes held back for a Focus (V50, V70): the next one that runs merges what came since.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hold {
    pub since: u64,
    /// Budget: what the merge leaves out is lost (`merge-omitted`); the user's own pause is
    /// outside coverage.
    pub budget: bool,
}

/// Everything the loop commits ([`STATE_FILE`]).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Committed {
    /// Hook inbox offset consumed.
    pub inbox: u64,
    pub transcript: Option<PathBuf>,
    /// The code observers' cursor (Debate, rulings): the live tail, not a Focus's.
    pub code: Cursor,
    pub guardian: Option<(PathBuf, Cursor)>,
    /// Session start snapshot: base of the session `--stat` (V5).
    pub base: Option<String>,
    pub focus: BTreeMap<String, Position>,
    pub health: BTreeMap<String, Health>,
    pub fallback: Fallback,
    pub held: BTreeMap<String, Hold>,
    /// Turn-end hooks seen and transcript turn ends the code observers read: a turn end due
    /// is written once `read` catches up (V89).
    pub turns: (u64, u64),
    /// Wakes so far.
    pub n: u64,
    /// `primary` requests answered (V51).
    #[serde(default)]
    pub primary: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Inflight {
    pub started: u64,
    pub trigger: String,
    /// Watcher → key of the slice it is reviewing.
    pub slices: BTreeMap<String, String>,
    /// Watcher → where its cursor goes if the poison guard skips the slice (V88).
    #[serde(default)]
    pub skip: BTreeMap<String, Skip>,
}

/// A slice the poison guard may skip: the cursor past it and its span.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Skip {
    pub next: Position,
    pub from: u64,
    pub to: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Crashes {
    pub tasks: BTreeMap<String, Vec<u64>>,
    /// Crash-looped Watchers: BLIND until `attach` or session end (V90).
    pub looped: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Launch {
    pub flags: Vec<String>,
    pub only: Vec<String>,
}

/// A boolean config key as this session resolves it: its `run` flags over the config layers of
/// the session's repo (V79). `default` when unset or unreadable.
pub fn session_flag(
    home: &Home,
    mailbox: &Mailbox,
    records: &[crate::jsonl::Record<Event>],
    key: &str,
    default: bool,
) -> bool {
    let start =
        records.iter().find(|r| r.body.k == crate::event::Kind::Session && r.body.str("phase") == Some("start"));
    let repo = start.and_then(|r| r.body.str("repo")).and_then(|r| repo::discover(Path::new(r)));
    let launch: Launch = read_json(&mailbox.dir().join(LAUNCH_FILE)).ok().flatten().unwrap_or_default();
    let env = |var: &str| std::env::var(var).ok();
    let sources = crate::config::Sources { home, repo: repo.as_ref(), env: &env, flags: &launch.flags };
    crate::config::resolve(&sources).ok().and_then(|r| r.get(key).and_then(toml::Value::as_bool)).unwrap_or(default)
}

pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    write_atomic(path, &serde_json::to_vec(value)?)
}

/// Process-wide settings read once at start.
#[derive(Clone, Debug)]
struct Cfg {
    checkpoints: bool,
    infer_rulings: bool,
    auto_challenge: bool,
    escalate: EscalateAt,
    band_digits: bool,
    model: String,
    fallback: Option<String>,
    backend: Backend,
    telemetry: bool,
    retention: crate::retention::Policy,
}

impl Cfg {
    fn from(r: &Resolved) -> Cfg {
        let flag = |k: &str, d: bool| r.get(k).and_then(toml::Value::as_bool).unwrap_or(d);
        let text = |k: &str| r.get(k).and_then(toml::Value::as_str).map(str::to_owned);
        Cfg {
            checkpoints: flag("checkpoints", true),
            infer_rulings: flag("infer_rulings", true),
            auto_challenge: flag("auto_challenge", true),
            escalate: EscalateAt::from_config(&text("escalate_unresolved").unwrap_or_else(|| "warn".into())),
            band_digits: flag("band_digits", true),
            model: text("model").unwrap_or_else(|| "auto".into()),
            fallback: text("fallback_model").filter(|m| !m.trim().is_empty()),
            backend: Backend::from_config(&text("backend").unwrap_or_default()),
            telemetry: flag("telemetry", false),
            retention: crate::retention::Policy::from(r),
        }
    }
}

/// A turn end waiting for its transcript turn (V89 transcript lost).
#[derive(Clone, Debug)]
struct TurnWait {
    due: Due,
    since: u64,
    rescanned: bool,
}

/// One Watcher's call result (runs on its task).
struct Asked {
    result: Result<Vec<Line>, (Class, String, bool)>,
    /// The accepted answer's text, for the wake record.
    text: Option<String>,
    spend: f64,
    usage: Usage,
    tools: usize,
    latency_ms: u64,
    model: String,
    /// Ran on the primary while on a Service fallback (V51 probe).
    probe: bool,
}

/// One Watcher's input for this wake.
#[derive(Clone)]
struct Built {
    input: String,
    /// The shared sections `input` rendered, for the wake record (V93).
    sections: Input,
    /// Entries shown or folded; empty for a blocked wake.
    slice: Vec<Entry>,
    /// Entries a merged wake folded into history without showing (V50).
    omitted: Vec<Entry>,
    next: Position,
    replayed: bool,
    merged: Option<Hold>,
}

pub struct Watch {
    mailbox: Mailbox,
    log: EventLog,
    outbox: Outbox,
    harness: Harness,
    store: Option<Store>,
    focuses: Vec<Focus>,
    cfg: Cfg,
    instructions: Vec<String>,
    /// From config `backend` (I15); tests point a subscription CLI at a fake.
    pub provider: Provider,
    table: Table,
    keys: Keys,
    /// Reloads keys for `primary`: the home and the launch env's key vars.
    key_source: (Home, HashMap<String, String>),
    pub notifier: Notifier,
    pub placement: Placement,
    pub exe: PathBuf,
    /// Clock of normal wakes; tests shorten it.
    pub wake_clock: Duration,
    pub blocked_clock: Duration,
    board: Board,
    offset: u64,
    triggers: Triggers,
    state: Committed,
    /// Every transcript entry so far: Challenge evidence (V17).
    transcript: Vec<Entry>,
    last_wake: u64,
    change_polled: u64,
    last_input: u64,
    probe: Option<(u64, String)>,
    quiet_since: Option<u64>,
    turn_wait: Option<TurnWait>,
    /// Owner Watcher's last wake cost (V24).
    cost: HashMap<String, f64>,
    pane_opened: bool,
    /// The wake pre-filter; `None` = off (V53).
    prefilter: Option<Gate>,
    /// The final drain's wake: never skipped (V54).
    draining: bool,
    /// A normal wake's calls are out; blocked wakes run beside it (V102).
    mid_wake: bool,
    /// A transcript hooked mid-wake, adopted once that wake commits (V10).
    adopt_after: Option<(PathBuf, bool)>,
}

/// Why the Watcher cannot run at all: logged as BLIND (V89).
pub struct Unrunnable(pub String);

impl Watch {
    /// Rebuilds the Watcher from the session's files (V91). `env` = the launch environment.
    pub fn open(home: &Home, mailbox: Mailbox, env: &dyn Fn(&str) -> Option<String>) -> Result<Watch, Unrunnable> {
        Self::open_with(home, mailbox, env, &prefilter::ollama_tags)
    }

    /// `open` with the pre-filter's Ollama probe given (tests).
    pub fn open_with(
        home: &Home,
        mailbox: Mailbox,
        env: &dyn Fn(&str) -> Option<String>,
        probe: &dyn Fn(&str) -> Result<Vec<Tag>, String>,
    ) -> Result<Watch, Unrunnable> {
        let log = mailbox.log();
        let records = log.read_from(0).map_err(|e| Unrunnable(format!("event log: {e}")))?;
        let board = Board::rebuild(0, &records.records);
        let start = records
            .records
            .iter()
            .find(|r| r.body.k == crate::event::Kind::Session && r.body.str("phase") == Some("start"))
            .map(|r| r.body.clone())
            .ok_or_else(|| Unrunnable("no session start".into()))?;
        let harness = start
            .str("harness")
            .and_then(|h| Harness::from_program(h.as_ref()))
            .ok_or_else(|| Unrunnable("unknown harness".into()))?;
        let repo = PathBuf::from(start.str("repo").unwrap_or("."));
        let launch: Launch = read_json(&mailbox.dir().join(LAUNCH_FILE)).ok().flatten().unwrap_or_default();
        let loaded = run::load(home, &repo, &launch.flags, &launch.only)
            .map_err(|u| Unrunnable(format!("config unreadable: {u}")))?;
        let table = Table::load(home).map_err(|e| Unrunnable(format!("price table: {e}")))?;
        let keys = Keys::load(home, env);
        let worker_repo = repo::discover(&repo);
        let store = worker_repo.as_ref().and_then(|r| Store::open(mailbox.dir(), r).ok());
        let mut state: Committed = read_json(&mailbox.dir().join(STATE_FILE))
            .map_err(|e| Unrunnable(format!("{STATE_FILE}: {e}")))?
            .unwrap_or_default();
        if state.base.is_none() {
            state.base = store.as_ref().and_then(|s| s.take().ok());
        }
        let transcript = match &state.transcript {
            Some(path) => tailer::read(format(harness), path, &Cursor::default())
                .map(|t| t.entries.into_iter().filter(|e| e.next.at <= state.code.at).collect())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let text = |k: &str| loaded.resolved.get(k).and_then(toml::Value::as_str).map(str::to_owned);
        let url = text("prefilter.url").unwrap_or_else(|| prefilter::DEFAULT_URL.into());
        let key = (!prefilter::local(&url)).then(|| credentials::named(home, env, prefilter::KEY_ENV)).flatten();
        let gate = Gate::open(
            home,
            &text("prefilter.mode").unwrap_or_else(|| "auto".into()),
            &url,
            &text("prefilter.model").unwrap_or_else(|| prefilter::DEFAULT_MODEL.into()),
            key,
            probe,
        );
        for note in gate.iter().flat_map(|g| &g.notes) {
            eprintln!("pair-watcher: {note}");
        }
        let cfg = Cfg::from(&loaded.resolved);
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pair-watcher"));
        let provider = match cfg.backend.headless() {
            None => Provider::Api(Api::new(keys.clone(), env, fail::WAKE_CLOCK)),
            Some(kind) => Provider::Cli(crate::headless::Cli {
                kind,
                program: PathBuf::from(kind.program()),
                exe: exe.clone(),
                mailbox: mailbox.dir().to_owned(),
                repo: worker_repo.map(|r| r.root),
            }),
        };
        let mut w = Watch {
            log,
            outbox: mailbox.outbox(),
            harness,
            instructions: wake::instruction_files(&repo),
            store,
            focuses: loaded.focuses.enabled,
            cfg,
            provider,
            table,
            keys,
            key_source: (home.clone(), key_env(env)),
            notifier: Notifier::detect(env),
            placement: Placement::detect(env),
            exe,
            wake_clock: fail::WAKE_CLOCK,
            blocked_clock: fail::BLOCKED_CLOCK,
            board,
            offset: records.next,
            triggers: Triggers::default(),
            state,
            transcript,
            last_wake: now_ms(),
            change_polled: 0,
            last_input: now_ms(),
            probe: None,
            quiet_since: None,
            turn_wait: None,
            cost: HashMap::new(),
            pane_opened: mailbox.dir().join(PANE_OPENED).exists(),
            prefilter: gate,
            draining: false,
            mid_wake: false,
            adopt_after: None,
            mailbox,
        };
        w.choose_models()?;
        w.interrupted().map_err(|e| Unrunnable(format!("restart: {e}")))?;
        Ok(w)
    }

    pub fn board(&self) -> &Board {
        &self.board
    }

    pub fn state(&self) -> &Committed {
        &self.state
    }

    /// Logs each Watcher's model (V46); a model that cannot run is BLIND.
    fn choose_models(&mut self) -> Result<(), Unrunnable> {
        for w in self.roster() {
            let choice = self.primary(&w).map_err(Unrunnable)?;
            let op = Op::Model {
                watcher: w.name.clone(),
                model: choice.model,
                same_vendor: choice.same_vendor,
                shares_quota: self.cfg.backend.subscription(),
            };
            if self.board.models.get(&w.name).map(|m| m.0.as_str()) != Some(op_model(&op)) {
                self.append(op.event()).map_err(|e| Unrunnable(e.to_string()))?;
            }
        }
        Ok(())
    }

    fn primary(&self, w: &Watcher) -> Result<Choice, String> {
        let setting = w.model.as_deref().unwrap_or(&self.cfg.model);
        self.choose(setting).map_err(|e| format!("model {setting}: {e}"))
    }

    fn fallback_choice(&self) -> Option<Choice> {
        self.choose(self.cfg.fallback.as_deref()?).ok()
    }

    /// A subscription backend runs its own vendor's models only, on its login (I15).
    fn choose(&self, setting: &str) -> Result<Choice, String> {
        let backend = self.cfg.backend;
        let configured = backend.vendor().map_or_else(|| self.keys.configured(), |v| vec![v]);
        models::choose(&self.table, setting, worker_vendor(self.harness), &configured).map_err(|e| match e {
            models::ChoiceError::NoKey(v) if backend.subscription() => {
                format!("backend {} runs no {} models", backend.name(), v.name())
            }
            e => e.to_string(),
        })
    }

    /// Wakes cut off by a crash or kill: each counts once against its slices (V86, V88).
    fn interrupted(&mut self) -> io::Result<()> {
        for file in [INFLIGHT_FILE, BLOCKED_INFLIGHT_FILE] {
            self.interrupted_in(&self.mailbox.dir().join(file))?;
        }
        Ok(())
    }

    fn interrupted_in(&mut self, path: &Path) -> io::Result<()> {
        let Some(inflight) = read_json::<Inflight>(path)? else { return Ok(()) };
        let now = now_ms();
        let blocked = inflight.trigger == "blocked";
        for (watcher, slice) in &inflight.slices {
            self.append(
                Wake {
                    n: self.state.n + 1,
                    watcher: watcher.clone(),
                    trigger: inflight.trigger.clone(),
                    outcome: Outcome::Interrupted,
                    class: Some(Class::Slice),
                    error: Some("interrupted".into()),
                    model: String::new(),
                    spend: 0.0,
                    replayed: false,
                    rulings: vec![],
                    prefilter: None,
                    ..Default::default()
                }
                .event(),
            )?;
            // A blocked wake reviews no slice.
            if blocked {
                continue;
            }
            let effects =
                self.state.health.entry(watcher.clone()).or_default().failed(Class::Slice, slice, "interrupted", now);
            self.effects(watcher, &effects, inflight.skip.get(watcher))?;
        }
        self.state.n += 1;
        self.save()?;
        fs::remove_file(path)
    }

    /// The Watchers of the next wake (V45, V48, V70): mode and ladder, muted Focuses out.
    fn roster(&self) -> Vec<Watcher> {
        let enabled: Vec<Focus> =
            self.focuses.iter().filter(|f| !self.board.muted.contains(&f.name)).cloned().collect();
        if enabled.is_empty() {
            return Vec::new();
        }
        arbiter::roster(&enabled, self.board.swarm && !self.board.budget.swarm_collapsed())
    }

    /// The `status` focus for Watcher `w`: Swarm per Focus, Companion = every Watcher (V89).
    fn status_focus(w: &str) -> Option<String> {
        (w != "companion").then(|| w.to_owned())
    }

    // ---- the loop ----

    /// Runs until the supervisor asks for the drain (V90).
    pub fn run(&mut self) -> io::Result<()> {
        heartbeat(self.mailbox.dir().join(HEARTBEAT_FILE));
        while !self.mailbox.dir().join(DRAIN_FILE).exists() {
            self.step(now_ms())?;
            thread::sleep(TICK);
        }
        self.drain()
    }

    /// One loop: read what arrived, answer the human, run due wakes, check the clocks.
    pub fn step(&mut self, now: u64) -> io::Result<()> {
        self.refresh()?;
        self.hooks(now)?;
        self.guardian()?;
        self.primary_asked(now)?;
        self.answer(now)?;
        let mut due = self.due(now);
        // A turn end waits for its transcript turn before it wakes (V89); a later one joins it.
        if let Some(i) = due.iter().position(|d| d.trigger == Trigger::TurnEnd)
            && (self.turn_wait.is_some() || !self.turn_written()?)
        {
            let d = due.remove(i);
            self.turn_wait.get_or_insert(TurnWait { due: d, since: now, rescanned: false });
        }
        if let Some(wait) = self.turn_wait.take() {
            self.await_turn(wait, now)?;
        }
        for d in &due {
            self.round(d, None, now)?;
        }
        self.resume_held(now);
        let rewake: Vec<String> =
            self.state.health.iter().filter(|(_, h)| h.rewake_due(now)).map(|(w, _)| w.clone()).collect();
        if !rewake.is_empty() && due.iter().all(|d| d.trigger == Trigger::Blocked) {
            let d = Due { trigger: Trigger::Checkpoint, payload: None, at: now };
            self.round(&d, Some(&rewake), now)?;
        }
        self.clocks(now)?;
        // A turn end taken in beside a wake (V102) has not woken yet (V21).
        if self.turn_wait.is_none() && !self.triggers.turn_end_held() {
            self.mailbox.settle(self.state.inbox)?;
        }
        self.save()
    }

    /// `primary` (V51), each request once: keys reload (after `auth`), a Key fallback ends, and a
    /// Watcher BLIND on its key wakes at once; its first accepted wake ends the BLIND (V89).
    fn primary_asked(&mut self, now: u64) -> io::Result<()> {
        if self.board.primary_asked <= self.state.primary {
            return Ok(());
        }
        self.state.primary = self.board.primary_asked;
        let (home, vars) = &self.key_source;
        self.keys = Keys::load(home, &|k| vars.get(k).cloned());
        self.provider.set_keys(self.keys.clone());
        if self.state.fallback.active == Some(Class::Key) {
            self.state.fallback.recovered();
            self.append(Op::Primary.event())?;
        }
        for h in self.state.health.values_mut() {
            if h.blind.as_ref().is_some_and(|(_, r)| r.starts_with(fail::KEY_BLIND)) {
                h.rewake_at = Some(now);
            }
        }
        Ok(())
    }

    /// Final drain (V90): one last wake on what arrived, open escalations unruled, exit.
    pub fn drain(&mut self) -> io::Result<()> {
        let now = now_ms();
        self.refresh()?;
        self.hooks(now)?;
        let fresh = self.peek()?;
        if !fresh.is_empty() || self.turn_wait.is_some() {
            let trigger = if fresh.iter().flat_map(|e| &e.items).any(|i| *i == Item::TurnEnd) {
                Trigger::TurnEnd
            } else {
                Trigger::Checkpoint
            };
            self.turn_wait = None;
            let clock = self.wake_clock;
            self.wake_clock = DRAIN_CLOCK.min(clock);
            self.draining = true;
            self.round(&Due { trigger, payload: None, at: now }, None, now)?;
            self.draining = false;
            self.wake_clock = clock;
        }
        let mut rulings = self.board.rulings.clone();
        rulings.unruled(&self.log, &self.board.escalations, now)?;
        self.refresh()?;
        self.save()
    }

    /// Folds what the log gained; opens the pane on the first event that calls for it (V67).
    fn refresh(&mut self) -> io::Result<()> {
        let batch = self.log.read_from(self.offset)?;
        self.offset = batch.next;
        for r in &batch.records {
            self.board.apply(r);
            if !self.pane_opened && pane::opens(&r.body) {
                self.open_pane();
            }
        }
        Ok(())
    }

    fn open_pane(&mut self) {
        self.pane_opened = true;
        let _ = fs::write(self.mailbox.dir().join(PANE_OPENED), b"");
        if self.placement != Placement::Print
            && let Err(e) = self.placement.open(&pane_command(&self.exe, &self.mailbox.id()))
        {
            eprintln!("pair-watcher: pane: {e}");
        }
    }

    fn append(&mut self, e: Event) -> io::Result<()> {
        self.log.append(e)?;
        self.refresh()
    }

    fn save(&self) -> io::Result<()> {
        write_json(&self.mailbox.dir().join(STATE_FILE), &self.state)
    }

    /// New hook records: triggers, the transcript path, input activity (V1, V56, V89).
    fn hooks(&mut self, now: u64) -> io::Result<()> {
        let batch = self.mailbox.read_hooks(self.state.inbox)?;
        self.state.inbox = batch.next;
        for r in &batch.records {
            self.input(now)?;
            let p = &r.body.payload;
            let path = ["transcript_path", "session_file"].iter().find_map(|k| p.get(*k)?.as_str()).map(PathBuf::from);
            if let Some(path) = path
                && self.state.transcript.as_ref() != Some(&path)
            {
                let resumed = p.get("source").and_then(Value::as_str) == Some("resume");
                if self.mid_wake {
                    self.adopt_after = Some((path, resumed));
                } else {
                    self.adopt(path, resumed);
                }
            }
            if let Some(signal) = trigger::signal(self.harness, &r.body.event) {
                self.triggers.observe(r.t, signal, p);
                if signal == Signal::TurnEnd {
                    self.state.turns.0 += 1;
                }
            }
        }
        Ok(())
    }

    /// A new transcript (start, `/clear`, resume): read from its start, or its end when resumed
    /// (that content predates the session). Histories and snapshots carry over.
    fn adopt(&mut self, path: PathBuf, resumed: bool) {
        let start = if resumed {
            tailer::read(format(self.harness), &path, &Cursor::default()).map(|t| t.cursor).unwrap_or_default()
        } else {
            Cursor::default()
        };
        self.state.code = start.clone();
        for pos in self.state.focus.values_mut() {
            pos.cursor = start.clone();
        }
        for f in &self.focuses {
            self.state.focus.entry(f.name.clone()).or_insert_with(|| Position {
                cursor: start.clone(),
                tree: self.state.base.clone(),
                ..Position::default()
            });
        }
        self.state.transcript = Some(path);
        self.state.guardian = None;
    }

    /// A Codex guardian review holds the blocked timer; its verdict cancels it, its end with no
    /// verdict re-arms it (V56, V101).
    fn guardian(&mut self) -> io::Result<()> {
        if self.harness != Harness::Codex {
            return Ok(());
        }
        if self.state.guardian.is_none() {
            let (Some(rollout), Some(id)) = (&self.state.transcript, delivery::worker_session(&self.mailbox)?) else {
                return Ok(());
            };
            let Some(dir) = rollout.parent() else { return Ok(()) };
            if let Some(path) = tailer::find_guardian(dir, &id)? {
                self.state.guardian = Some((path, Cursor::default()));
            }
        }
        let Some((path, cursor)) = &self.state.guardian else { return Ok(()) };
        let Ok(tail) = tailer::read(Format::Guardian, path, cursor) else { return Ok(()) };
        for e in &tail.entries {
            for i in &e.items {
                let signal = match i {
                    Item::ReviewStarted => Signal::Review,
                    Item::ReviewDone { outcome: Some(_) } => Signal::Activity,
                    Item::ReviewDone { outcome: None } => Signal::NoVerdict,
                    _ => continue,
                };
                self.triggers.observe(e.t, signal, &Value::Null);
            }
        }
        self.state.guardian = Some((path.clone(), tail.cursor));
        Ok(())
    }

    fn input(&mut self, now: u64) -> io::Result<()> {
        self.last_input = now;
        self.quiet_since = None;
        if self.board.blind.get(&None).is_some_and(|(_, r)| r == QUIET) {
            self.append(Op::BlindEnd { focus: None }.event())?;
        }
        Ok(())
    }

    /// Every loop, not only on wakes (V43): promote requests, rulings, blocked answers, sent
    /// deliveries, stale Challenges, Escalations to notify (restart re-check, V91).
    fn answer(&mut self, now: u64) -> io::Result<()> {
        let b = &self.board;
        let (mut ch, mut debate) = (b.challenges.clone(), b.debate.clone());
        for line in ch.answer_requests(&self.log, &self.outbox, &b.concerns, now)? {
            eprintln!("pair-watcher: {line}");
        }
        self.refresh()?;
        let b = &self.board;
        let (mut rulings, mut ch) = (b.rulings.clone(), b.challenges.clone());
        rulings.answer(&self.log, &self.outbox, &mut debate, &mut ch, &b.concerns, &b.escalations, now)?;
        self.refresh()?;
        let (mailbox, harness) = (&self.mailbox, self.harness);
        let answered = |at| escalation::blocked_answered(mailbox, harness, at);
        let mut rulings = self.board.rulings.clone();
        rulings.answer_blocked(&self.log, &self.board.escalations, answered, now)?;
        self.refresh()?;
        self.sync_sent()?;
        let mut ch = self.board.challenges.clone();
        if let Some(line) = ch.drop_stale(&self.log, &self.outbox, now)? {
            eprintln!("pair-watcher: {line}");
        }
        self.refresh()?;
        self.deliver()
    }

    fn sync_sent(&mut self) -> io::Result<()> {
        let mut ch = self.board.challenges.clone();
        ch.sync_sent(&self.log, &self.outbox)?;
        self.refresh()?;
        let mut debate = self.board.debate.clone();
        debate.sync_sent(&self.log, &self.outbox)?;
        self.refresh()
    }

    fn deliver(&mut self) -> io::Result<()> {
        let (mailbox, harness) = (&self.mailbox, self.harness);
        let answered = |at| escalation::blocked_answered(mailbox, harness, at);
        let mut esc = self.board.escalations.clone();
        esc.deliver(&self.log, &self.board.concerns, answered, &self.notifier)?;
        self.refresh()
    }

    fn due(&mut self, now: u64) -> Vec<Due> {
        let checkpoints = self.cfg.checkpoints && self.board.budget.admits(Need::Checkpoint);
        let (store, last) = (&self.store, self.state.focus.values().filter_map(|p| p.tree.clone()).next());
        // Each look is a snapshot: once per CHANGE_POLL_MS, not every poll.
        let polled = &mut self.change_polled;
        let changed = || match (store, last) {
            (Some(s), Some(last)) if now.saturating_sub(*polled) >= CHANGE_POLL_MS => {
                *polled = now;
                s.take().is_ok_and(|t| t != last)
            }
            _ => false,
        };
        self.triggers.due(now, self.last_wake, checkpoints, changed)
    }

    /// Every turn end hooked so far is in the transcript, read or past the code cursor (an
    /// earlier checkpoint wake may have read it before its hook arrived).
    fn turn_written(&self) -> io::Result<bool> {
        let (hooked, read) = self.state.turns;
        Ok(read + turn_ends(&self.peek()?) >= hooked)
    }

    fn peek(&self) -> io::Result<Vec<Entry>> {
        let Some(path) = &self.state.transcript else { return Ok(Vec::new()) };
        match tailer::read(format(self.harness), path, &self.state.code) {
            Ok(t) => Ok(t.entries),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// V89 transcript lost: the turn shows within 10 s, one rescan included, else BLIND.
    fn await_turn(&mut self, mut wait: TurnWait, now: u64) -> io::Result<()> {
        if self.turn_written()? {
            return self.round(&wait.due, None, now);
        }
        let waited = now.saturating_sub(wait.since);
        if waited >= RESCAN_MS && !wait.rescanned {
            // Re-anchor by id from the file start (rewrite, compaction).
            wait.rescanned = true;
            self.state.code.at = 0;
            if self.turn_written()? {
                return self.round(&wait.due, None, now);
            }
        }
        if waited < TRANSCRIPT_WAIT_MS {
            self.turn_wait = Some(wait);
            return Ok(());
        }
        self.blind(None, "transcript lost")?;
        self.state.turns.1 = self.state.turns.0;
        let from = self.state.focus.values().map(|p| p.t).min().unwrap_or(wait.since);
        self.append(Coverage::Lost { from, to: now, cause: "transcript-unrecoverable".into() }.event())
    }

    /// Time-driven checks: lagging ≥10 min (V89), inputs quiet (V89).
    fn clocks(&mut self, now: u64) -> io::Result<()> {
        let names: Vec<String> = self.state.health.keys().cloned().collect();
        for w in names {
            let effects = self.state.health.get_mut(&w).map(|h| h.check(now)).unwrap_or_default();
            self.effects(&w, &effects, None)?;
        }
        let Some(store) = &self.store else { return Ok(()) };
        if self.board.observe_only || self.state.transcript.is_none() {
            return Ok(());
        }
        let due = self.probe.as_ref().is_none_or(|(at, _)| now.saturating_sub(*at) >= CHECKPOINT_FLOOR_MS);
        if !due {
            return Ok(());
        }
        let Ok(tree) = store.take() else { return Ok(()) };
        if let Some((at, before)) = &self.probe
            && *before != tree
            && self.last_input < *at
        {
            match self.quiet_since {
                Some(first) if now.saturating_sub(first) >= QUIET_SPAN_MS => self.blind(None, QUIET)?,
                Some(_) => {}
                None => self.quiet_since = Some(now),
            }
        }
        self.probe = Some((now, tree));
        Ok(())
    }

    fn blind(&mut self, focus: Option<String>, reason: &str) -> io::Result<()> {
        if self.board.blind.contains_key(&focus) {
            return Ok(());
        }
        self.append(Op::Blind { focus, reason: reason.to_owned() }.event())
    }

    /// Logs what a [`Health`] change means; `skip` = the slice to skip on [`Effect::Advance`].
    fn effects(&mut self, w: &str, effects: &[Effect], skip: Option<&Skip>) -> io::Result<()> {
        let focus = Self::status_focus(w);
        for e in effects {
            match e {
                Effect::Lagging if !self.board.lagging.contains_key(&focus) => {
                    self.append(Op::Lagging { focus: focus.clone() }.event())?
                }
                Effect::LaggingEnd if self.board.lagging.contains_key(&focus) => {
                    self.append(Op::LaggingEnd { focus: focus.clone() }.event())?
                }
                Effect::Blind(reason) => self.blind(focus.clone(), reason)?,
                Effect::BlindEnd if self.board.blind.contains_key(&focus) => {
                    self.append(Op::BlindEnd { focus: focus.clone() }.event())?
                }
                Effect::Advance => {
                    if let Some(Skip { next, from, to }) = skip {
                        self.commit(w, next);
                        let (from, to) = (*from, *to);
                        self.append(Coverage::Lost { from, to, cause: "poison-skip".into() }.event())?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    // ---- one wake ----

    /// One wake for `only` Watchers (re-wakes) or the whole roster (V1, V43).
    fn round(&mut self, due: &Due, only: Option<&[String]>, now: u64) -> io::Result<()> {
        self.refresh()?;
        let blocked = due.trigger == Trigger::Blocked;
        let mut roster = self.roster();
        let crashes: Crashes = read_json(&self.mailbox.dir().join(CRASHES_FILE))?.unwrap_or_default();
        roster.retain(|w| !crashes.looped.contains(&w.name));
        if let Some(names) = only {
            roster.retain(|w| names.contains(&w.name));
        }
        if roster.is_empty() {
            return Ok(());
        }
        let need = match due.trigger {
            _ if blocked || self.board.debate.owes() => Need::Reserve,
            // Re-wakes replay or merge what was held: normal wakes (V50, V92).
            Trigger::Checkpoint if only.is_none() => Need::Checkpoint,
            _ => Need::Normal,
        };
        if self.board.user_paused || !self.board.budget.admits(need) {
            if !blocked {
                self.hold(&roster, !self.board.user_paused, now)?;
            }
            // `resume_held` wakes them again once that changes.
            for w in &roster {
                self.state.health.entry(w.name.clone()).or_default().rewake_at = None;
            }
            return Ok(());
        }
        if !blocked {
            self.last_wake = now;
        }

        // Code observers read the live tail first (debate.rs order).
        let tree = self.store.as_ref().and_then(|s| s.take().ok());
        let fresh = self.peek()?;
        if let Some(last) = fresh.last() {
            self.state.turns.1 += turn_ends(&fresh);
            self.state.code = last.next.clone();
            self.transcript.extend(fresh.iter().cloned());
            self.input(now)?;
        }
        self.sync_sent()?;
        let mut debate = self.board.debate.clone();
        debate.observe(&self.log, &self.outbox, &fresh, now)?;
        self.refresh()?;
        let mut rulings = self.board.rulings.clone();
        rulings.drop_on_prompt(&self.log, &self.outbox, &fresh, now)?;
        self.refresh()?;

        let mut built: Vec<Built> =
            roster.iter().map(|w| self.build(w, due, tree.as_deref())).collect::<io::Result<_>>()?;
        let wakeup =
            Wakeup { blocked, owes_reply: self.board.debate.owes(), catch_up: only.is_some() || self.draining };
        let mut verdicts = self.prefilter(&built, wakeup);
        // Skipped Watchers: logged with their input, cursor held, no model call (V55).
        let mut skipped = Vec::new();
        for i in (0..roster.len()).rev() {
            if verdicts[i].as_ref().is_some_and(|v| v.skipped) {
                skipped.push((roster.remove(i), built.remove(i), verdicts.remove(i)));
            }
        }
        let slices = roster.iter().zip(&built).map(|(w, b)| (w.name.clone(), slice_key(b))).collect();
        let skip = roster.iter().zip(&built).map(|(w, b)| (w.name.clone(), skip_of(b, now))).collect();
        let inflight = self.mailbox.dir().join(if blocked { BLOCKED_INFLIGHT_FILE } else { INFLIGHT_FILE });
        let trigger = trigger_name(due.trigger, false).into();
        write_json(&inflight, &Inflight { started: now, trigger, slices, skip })?;

        let clock = if blocked { self.blocked_clock } else { self.wake_clock };
        let deadline = Instant::now() + clock;
        let fallback = self.fallback_choice().filter(|_| self.state.fallback.active.is_some());
        let probe = fallback.is_some() && self.state.fallback.probe(now);
        let plans: Vec<(Choice, bool)> = roster
            .iter()
            .map(|w| match (&fallback, probe) {
                (Some(f), false) => Ok((f.clone(), false)),
                _ => self.primary(w).map(|c| (c, fallback.is_some())),
            })
            .collect::<Result<_, String>>()
            .map_err(io::Error::other)?;
        let head = self.store.as_ref().and_then(Store::head);
        let (provider, table, store) = (self.provider.clone(), self.table.clone(), self.store.clone());
        let (watchers, inputs, snapshot) =
            (roster.clone(), built.iter().map(|b| b.input.clone()).collect::<Vec<_>>(), tree.clone());
        let calls = thread::spawn(move || {
            arbiter::run(&watchers, |w| {
                let i = watchers.iter().position(|r| r.name == w.name).expect("in roster");
                let tools = match (&store, &snapshot) {
                    (Some(store), Some(tree)) if !blocked => Some(Snapshot { store, tree, head: head.as_deref() }),
                    _ => None,
                };
                let (choice, probe) = &plans[i];
                ask(&provider, &table, choice, &inputs[i], tools, deadline, !probe, *probe)
            })
        });
        let results = if blocked { join(calls)? } else { self.alongside(calls)? };

        // The arbiter takes accepted outputs only (V10, V43).
        let mut ran: Vec<Watcher> = Vec::new();
        let mut outputs: Vec<(usize, Vec<Line>)> = Vec::new();
        let mut wakes: Vec<Wake> = Vec::new();
        let mut spend = 0.0;
        let mut failed: Vec<(usize, Class, String)> = Vec::new();
        for (i, (w, r)) in roster.iter().zip(results).enumerate() {
            let trigger = trigger_name(due.trigger, built[i].replayed || built[i].merged.is_some());
            let mut wake = Wake {
                n: self.state.n + 1,
                watcher: w.name.clone(),
                trigger: trigger.into(),
                outcome: Outcome::Silent,
                class: None,
                error: None,
                model: String::new(),
                spend: 0.0,
                replayed: built[i].replayed,
                rulings: vec![],
                prefilter: verdicts[i].clone(),
                diff_bytes: built[i].sections.diff_bytes,
                ..Default::default()
            };
            let tree = tree.as_deref().filter(|_| !blocked);
            match r {
                Err(panic) => {
                    wake.outcome = Outcome::Failed;
                    wake.error = Some(format!("crashed: {panic}"));
                    wake.record = Some(self.record(w, trigger, &built[i], tree, None)?);
                    self.crashed(&w.name, &panic, now)?;
                }
                Ok(asked) => {
                    spend += asked.spend;
                    wake.spend = asked.spend;
                    wake.model = asked.model;
                    wake.tokens = Some(asked.usage);
                    wake.tools = asked.tools;
                    wake.latency_ms = Some(asked.latency_ms);
                    wake.record = Some(self.record(w, trigger, &built[i], tree, asked.text)?);
                    match asked.result {
                        Ok(lines) => {
                            if asked.probe {
                                self.state.fallback.recovered();
                                self.append(Op::Primary.event())?;
                            }
                            wake.rulings = lines
                                .iter()
                                .filter_map(|l| match l {
                                    Line::HeldBack { ruling } => Some(ruling.clone()),
                                    _ => None,
                                })
                                .collect();
                            self.cost.insert(w.name.clone(), asked.spend);
                            ran.push(w.clone());
                            outputs.push((i, lines));
                        }
                        Err((class, error, invalid)) => {
                            wake.outcome = if invalid { Outcome::Invalid } else { Outcome::Failed };
                            wake.class = Some(class);
                            wake.error = Some(error.clone());
                            failed.push((i, class, error));
                        }
                    }
                }
            }
            wakes.push(wake);
        }

        let outs: Vec<Output> =
            outputs.iter().map(|(i, lines)| Output { watcher: &roster[*i], lines: lines.clone() }).collect();
        let mut merged = arbiter::merge(&self.board.concerns, &outs);
        let history = outputs.first().map(|(i, _)| built[*i].next.history.clone()).unwrap_or_default();
        let unverifiable = match (&self.store, &tree) {
            (Some(store), Some(tree)) => challenge::verify(
                &mut merged.changes,
                &Sources { store, tree, transcript: &self.transcript, history: &history },
            ),
            _ => Vec::new(),
        };
        let mut list = self.board.concerns.clone();
        arbiter::commit(&mut list, &self.log, &merged.changes)?;
        arbiter::commit(&mut list, &self.log, &unverifiable)?;
        for d in &merged.dropped {
            eprintln!("pair-watcher: dropped {d}");
        }
        if due.trigger == Trigger::TurnEnd
            && let Some((watcher, prompt_ref)) = merged.rest.iter().find_map(|(w, l)| match l {
                Line::Correction { prompt_ref } => Some((w.clone(), prompt_ref.clone())),
                _ => None,
            })
        {
            self.append(Act::Correction { prompt_ref, watcher, wake: self.state.n + 1 }.event())?;
        }
        self.refresh()?;
        for (i, w) in roster.iter().enumerate() {
            let shown = |c: &Change| matches!(c, Change::Add { watcher, .. } | Change::Update { watcher, .. } if *watcher == w.name);
            if merged.changes.iter().any(shown) {
                wakes[i].outcome = Outcome::Concern;
            }
        }

        // Cursors: accepted slices commit, failed ones hold or are skipped (V10, V88).
        for (i, _) in &outputs {
            let w = &roster[*i].name;
            self.commit(w, &built[*i].next);
            let mut effects = self.state.health.entry(w.clone()).or_default().accepted();
            // A Focus crash BLIND ends with its next accepted wake (V89, V90).
            if !effects.contains(&Effect::BlindEnd) && Self::status_focus(w).is_some() {
                effects.push(Effect::BlindEnd);
            }
            self.effects(w, &effects, None)?;
            self.session_seen()?;
            self.covered(&built[*i])?;
        }
        for (i, class, error) in &failed {
            wakes[*i].poison_advance = self.failure(&roster[*i], &built[*i], *class, error, blocked, now)?;
        }

        // Then Debate, Escalation, rulings, the next Challenge, delivery (escalation.rs order).
        let mut escalated: Vec<String> = Vec::new();
        let swarm = roster.len() > 1 || roster.first().is_some_and(|w| w.name != "companion");
        let surfaces = Surfaces { harness: Some(self.harness), band_digits: self.cfg.band_digits };
        if blocked {
            let payload = due.payload.clone().unwrap_or(Value::Null);
            let mut esc = self.board.escalations.clone();
            let step = esc.raise_blocked(&self.log, due.at, &payload, &ran, &merged.rest, swarm)?;
            if step.raised.is_some() {
                escalated.extend(
                    merged.rest.iter().filter(|(_, l)| matches!(l, Line::Consider { .. })).map(|(w, _)| w.clone()),
                );
            }
        } else {
            let mut debate = self.board.debate.clone();
            let step = debate.decide(
                &self.log,
                &self.outbox,
                &self.board.concerns,
                &ran,
                &merged.rest,
                self.cfg.escalate,
                now,
            )?;
            for t in step.ticker.iter().chain(&step.dropped) {
                eprintln!("pair-watcher: {t}");
            }
            self.refresh()?;
            let mut esc = self.board.escalations.clone();
            for id in esc.raise_unresolved(&self.log, &self.board.concerns, swarm, surfaces)? {
                if let Some(c) = self.board.escalations.get(&id).and_then(|e| e.concern.clone()) {
                    escalated.extend(self.owner(&roster, &c));
                }
            }
            self.refresh()?;
            if due.trigger == Trigger::TurnEnd {
                let mut rulings = self.board.rulings.clone();
                let b = &self.board;
                rulings.infer(
                    &self.log,
                    &merged.rest,
                    true,
                    self.cfg.infer_rulings,
                    &fresh,
                    &b.escalations,
                    &b.concerns,
                    now,
                )?;
                self.refresh()?;
            }
            if let Some(owner) = self.auto_challenge(&roster, spend, tree.as_deref(), &history, now)?
                && let Some(i) = roster.iter().position(|w| w.name == owner)
            {
                wakes[i].outcome = Outcome::Challenge;
            }
        }
        for (i, w) in roster.iter().enumerate() {
            if escalated.contains(&w.name) {
                wakes[i].outcome = Outcome::Escalation;
            }
        }
        // Skipped Watchers: logged with their input, cursor held, no model call (V55).
        for (w, b, v) in skipped {
            let trigger = trigger_name(due.trigger, false);
            let record = self.record(&w, trigger, &b, tree.as_deref(), None)?;
            let wake = Wake {
                n: self.state.n + 1,
                watcher: w.name,
                trigger: trigger.into(),
                outcome: Outcome::Skipped,
                prefilter: v,
                diff_bytes: b.sections.diff_bytes,
                record: Some(record),
                ..Default::default()
            };
            self.append(wake.event())?;
        }
        for wake in wakes {
            self.append(wake.event())?;
        }
        self.state.n += 1;
        self.save()?;
        let _ = fs::remove_file(inflight);

        let mut ch = self.board.challenges.clone();
        if let Some(line) = ch.drop_stale(&self.log, &self.outbox, now)? {
            eprintln!("pair-watcher: {line}");
        }
        self.refresh()?;
        self.deliver()?;
        if due.trigger == Trigger::TurnEnd && self.harness == Harness::Codex && !self.board.observe_only {
            if let Err(e) = delivery::codex_queue(&self.mailbox, "codex".as_ref(), CODEX_QUEUE_TIMEOUT, now) {
                eprintln!("pair-watcher: codex queue: {e}");
            }
            self.sync_sent()?;
        }
        // A slice that did not reach the live tail is replayed next loop (V92).
        for (i, w) in roster.iter().enumerate() {
            let behind = !blocked && built[i].next.cursor != self.state.code && failed.iter().all(|(j, ..)| *j != i);
            if behind {
                self.state.health.entry(w.name.clone()).or_default().rewake_at = Some(now);
            }
        }
        if !blocked && let Some((path, resumed)) = self.adopt_after.take() {
            self.adopt(path, resumed);
        }
        Ok(())
    }

    /// A blocked wake never waits on a normal one (V102): until the normal wake's calls return,
    /// the loop takes in hooks and runs each blocked wake that comes due beside it, under its
    /// own clock. A new transcript waits until the normal wake commits.
    fn alongside<T>(&mut self, calls: thread::JoinHandle<T>) -> io::Result<T> {
        self.mid_wake = true;
        let mut waited = || -> io::Result<()> {
            while !calls.is_finished() {
                let now = now_ms();
                self.hooks(now)?;
                self.guardian()?;
                if let Some(due) = self.triggers.blocked_due(now) {
                    self.round(&due, None, now)?;
                }
                thread::sleep(ALONGSIDE_POLL);
            }
            Ok(())
        };
        let r = waited();
        self.mid_wake = false;
        r?;
        join(calls)
    }

    /// Scores each eligible Watcher's input in parallel (V53–V55); `None` = not scored. A failure
    /// fails open and reaches the ticker once until an answer comes back (V86).
    fn prefilter(&mut self, built: &[Built], wakeup: Wakeup) -> Vec<Option<Verdict>> {
        let Some(gate) = self.prefilter.as_mut().filter(|_| wakeup.eligible()) else {
            return vec![None; built.len()];
        };
        let verdicts: Vec<Option<Verdict>> = thread::scope(|s| {
            let gate = &*gate;
            let tasks: Vec<_> = built
                .iter()
                // A replayed or merged slice is catch-up, never skipped (V54).
                .map(|b| (!b.replayed && b.merged.is_none()).then(|| s.spawn(|| gate.score(&b.input))))
                .collect();
            tasks.into_iter().map(|t| t.and_then(|t| t.join().ok())).collect()
        });
        match verdicts.iter().flatten().find_map(|v| v.error.as_deref()) {
            Some(e) if !gate.down => {
                gate.down = true;
                eprintln!("pair-watcher: pre-filter down ({e}): waking");
            }
            None if verdicts.iter().flatten().any(|v| v.score.is_some()) => gate.down = false,
            _ => {}
        }
        verdicts
    }

    /// Wakes the budget or the user held back (V50, V70, V92): the cursor stays; the next wake
    /// that runs merges the span.
    fn hold(&mut self, roster: &[Watcher], budget: bool, now: u64) -> io::Result<()> {
        let mut fresh = false;
        for w in roster {
            for f in &w.focuses {
                if !self.state.held.contains_key(f) {
                    self.state.held.insert(f.clone(), Hold { since: now, budget });
                    fresh = true;
                }
            }
        }
        if fresh && budget {
            self.append(Coverage::BacklogPaused { from: now, to: now }.event())?;
        }
        Ok(())
    }

    /// `Z` resumed or `$` raised (V50, V70): held Watchers wake at once.
    fn resume_held(&mut self, now: u64) {
        if self.state.held.is_empty() || self.board.user_paused || !self.board.budget.admits(Need::Normal) {
            return;
        }
        for w in self.roster() {
            if w.focuses.iter().any(|f| self.state.held.contains_key(f)) {
                let h = self.state.health.entry(w.name.clone()).or_default();
                h.rewake_at.get_or_insert(now);
            }
        }
    }

    /// This Watcher's input (V9: identical across Watchers at one position, plus its briefs).
    fn build(&self, w: &Watcher, due: &Due, tree: Option<&str>) -> io::Result<Built> {
        let pos = self.position(w);
        let held: Option<Hold> =
            w.focuses.iter().filter_map(|f| self.state.held.get(f).copied()).min_by_key(|h| h.since);
        let standing = standing::session_path(&self.mailbox)
            .map(|p| standing::load(&p).map(|h| standing::render(&h)).unwrap_or_default())
            .unwrap_or_default();
        let debate = self.board.debate.render(&self.board.concerns);
        let awaiting = self.board.rulings.awaiting(&self.board.escalations);
        let ctx = Context {
            concerns: self.board.concerns.all(),
            debate: debate.as_deref(),
            awaiting: awaiting.as_deref(),
            rulings: &standing,
            instructions: &self.instructions,
        };
        let briefs: Vec<&Focus> = self.focuses.iter().filter(|f| w.focuses.contains(&f.name)).collect();
        let all = match &self.state.transcript {
            Some(path) => tailer::read(format(self.harness), path, &pos.cursor).map(|t| t.entries).unwrap_or_default(),
            None => Vec::new(),
        };
        if due.trigger == Trigger::Blocked {
            // The open turn's prompts and actions are seen, not committed: the cursor stays (V11).
            let seen = pos.history.ingest(&all);
            let input = wake::blocked(&seen, due.payload.as_ref().unwrap_or(&Value::Null), &ctx);
            return Ok(Built {
                input: input.render(PROMPT, &briefs),
                sections: input,
                slice: vec![],
                omitted: vec![],
                next: pos,
                replayed: false,
                merged: None,
            });
        }
        let (slice, omitted, merged) = cut(all, held);
        let replayed = !merged.is_some() && slice.last().is_some_and(|e| e.next != self.state.code)
            || self.state.health.get(&w.name).is_some_and(|h| h.lagging_since.is_some() || h.rewake_at.is_some());
        let history = pos.history.ingest(&omitted);
        let (store, base) = (self.store.as_ref(), self.state.base.as_deref());
        let wake_diff = match (store, pos.tree.as_deref(), tree) {
            (Some(s), Some(a), Some(b)) => s.diff(a, b).unwrap_or_default(),
            _ => Vec::new(),
        };
        let session = match (store, base, tree) {
            (Some(s), Some(a), Some(b)) => s.stat(a, b).unwrap_or_default(),
            _ => Vec::new(),
        };
        let diff = (tree.is_some() && store.is_some()).then_some(Diff { wake: &wake_diff, session: &session });
        let trigger = if merged.is_some() { Trigger::Checkpoint } else { due.trigger };
        let (input, next_history) = wake::normal(trigger, &history, &slice, diff.as_ref(), &ctx);
        let last = slice.last().or(omitted.last());
        let next = Position {
            cursor: last.map_or(pos.cursor.clone(), |e| e.next.clone()),
            tree: tree.map(str::to_owned).or(pos.tree.clone()),
            history: next_history,
            t: last.map_or(pos.t, |e| e.t),
        };
        Ok(Built { input: input.render(PROMPT, &briefs), sections: input, slice, omitted, next, replayed, merged })
    }

    /// Writes `w`'s record of the coming wake (V93); returns its session-relative ref.
    fn record(
        &self,
        w: &Watcher,
        trigger: &str,
        b: &Built,
        tree: Option<&str>,
        output: Option<String>,
    ) -> io::Result<String> {
        let rec = WakeRecord {
            n: self.state.n + 1,
            watcher: w.name.clone(),
            trigger: trigger.into(),
            prompt: PROMPT.to_owned(),
            briefs: self
                .focuses
                .iter()
                .filter(|f| w.focuses.contains(&f.name))
                .map(|f| (f.name.clone(), f.brief.clone()))
                .collect(),
            input: b.sections.clone(),
            tree: tree.map(str::to_owned),
            head: self.store.as_ref().and_then(Store::head),
            output,
        };
        record::write(self.mailbox.dir(), &rec)
    }

    /// The oldest committed position among `w`'s Focuses (V10).
    fn position(&self, w: &Watcher) -> Position {
        w.focuses
            .iter()
            .map(|f| {
                self.state.focus.get(f).cloned().unwrap_or_else(|| Position {
                    cursor: self.state.code.clone(),
                    tree: self.state.base.clone(),
                    ..Position::default()
                })
            })
            .min_by_key(|p| (p.cursor.id.is_some(), p.cursor.at))
            .unwrap_or_default()
    }

    fn commit(&mut self, w: &str, next: &Position) {
        let focuses: Vec<String> = match w {
            "companion" => self.focuses.iter().map(|f| f.name.clone()).collect(),
            f => vec![f.to_owned()],
        };
        for f in focuses {
            self.state.focus.insert(f, next.clone());
        }
    }

    /// Accepted: a replayed slice is `delayed`, a budget merge's left-out span `merge-omitted`.
    fn covered(&mut self, b: &Built) -> io::Result<()> {
        if let Some(hold) = b.merged {
            for f in self.state.held.clone().keys() {
                if self.state.held.get(f).is_some_and(|h| h.since == hold.since) {
                    self.state.held.remove(f);
                }
            }
            if let (true, Some(first), Some(last)) = (hold.budget, b.omitted.first(), b.omitted.last()) {
                self.append(Coverage::Lost { from: first.t, to: last.t, cause: "merge-omitted".into() }.event())?;
            }
        } else if b.replayed
            && let (Some(first), Some(last)) = (b.slice.first(), b.slice.last())
        {
            self.append(Coverage::Delayed { from: first.t, to: last.t }.event())?;
        }
        Ok(())
    }

    /// An accepted wake ends a session-wide BLIND that waits for one (V89).
    fn session_seen(&mut self) -> io::Result<()> {
        let lasting = [QUIET, supervisor::CRASHED, supervisor::GONE];
        if self.board.blind.get(&None).is_some_and(|(_, r)| !lasting.contains(&r.as_str())) {
            self.append(Op::BlindEnd { focus: None }.event())?;
        }
        Ok(())
    }

    fn failure(
        &mut self,
        w: &Watcher,
        b: &Built,
        class: Class,
        error: &str,
        blocked: bool,
        now: u64,
    ) -> io::Result<bool> {
        if self.state.fallback.failed(class, self.fallback_choice().is_some(), now) {
            let model = self.cfg.fallback.clone().unwrap_or_default();
            self.append(Op::Fallback { model, class }.event())?;
            if class == Class::Key {
                // The fallback takes the next wake; the slice waits for it.
                self.state.health.entry(w.name.clone()).or_default().rewake_at = Some(now);
                return Ok(false);
            }
        }
        if blocked && class != Class::Key {
            return Ok(false);
        }
        let effects = self.state.health.entry(w.name.clone()).or_default().failed(class, &slice_key(b), error, now);
        self.effects(&w.name, &effects, Some(&skip_of(b, now)))?;
        Ok(effects.contains(&Effect::Advance))
    }

    /// A Focus task panicked (V90): BLIND (crashed); the third in 10 min keeps it BLIND.
    fn crashed(&mut self, w: &str, why: &str, now: u64) -> io::Result<()> {
        let path = self.mailbox.dir().join(CRASHES_FILE);
        let mut crashes: Crashes = read_json(&path)?.unwrap_or_default();
        let times = crashes.tasks.entry(w.to_owned()).or_default();
        times.retain(|t| now.saturating_sub(*t) < CRASH_WINDOW_MS);
        times.push(now);
        let looped = times.len() >= CRASH_MAX;
        if looped && !crashes.looped.iter().any(|l| l == w) {
            crashes.looped.push(w.to_owned());
        }
        write_json(&path, &crashes)?;
        let focus = Self::status_focus(w);
        self.append(WatcherOp::Crash { focus: focus.clone(), why: why.to_owned() }.event())?;
        self.blind(focus.clone(), "crashed")?;
        if looped {
            self.append(WatcherOp::CrashLoop { focus }.event())?;
        }
        Ok(())
    }

    /// Watchers of `roster` that own Concern `id` (V39).
    fn owner(&self, roster: &[Watcher], id: &str) -> Vec<String> {
        let Some(c) = self.board.concerns.get(id) else { return Vec::new() };
        roster.iter().filter(|w| w.focuses.contains(&c.focus)).map(|w| w.name.clone()).collect()
    }

    /// The next auto-Challenge after this wake (V16, V19, V22–V24). Returns its owner Watcher.
    fn auto_challenge(
        &mut self,
        roster: &[Watcher],
        spend: f64,
        tree: Option<&str>,
        history: &History,
        now: u64,
    ) -> io::Result<Option<String>> {
        let (Some(store), Some(tree)) = (&self.store, tree) else { return Ok(None) };
        let mut budget = self.board.budget.clone();
        budget.record(spend);
        let auto = self.cfg.auto_challenge && !self.board.nochallenge;
        let wake_usd = |c: &Concern| {
            roster
                .iter()
                .find(|w| w.focuses.contains(&c.focus))
                .and_then(|w| self.cost.get(&w.name).copied())
                .unwrap_or(f64::NAN)
        };
        let sources = Sources { store, tree, transcript: &self.transcript, history };
        let mut ch = self.board.challenges.clone();
        match ch.next_auto(&self.board.concerns, auto, &budget, wake_usd, &sources) {
            Some(Auto::Ready(c)) => {
                let c = c.clone();
                ch.queue(&self.log, &self.outbox, &c, Origin::Auto, None, now)?;
                self.refresh()?;
                Ok(self.owner(roster, &c.id).into_iter().next())
            }
            Some(Auto::Unverifiable(change)) => {
                let mut list = self.board.concerns.clone();
                arbiter::commit(&mut list, &self.log, &[*change])?;
                self.refresh()?;
                Ok(None)
            }
            None => Ok(None),
        }
    }
}

const QUIET: &str = "inputs quiet";

fn op_model(op: &Op) -> &str {
    match op {
        Op::Model { model, .. } => model,
        _ => "",
    }
}

fn format(h: Harness) -> Format {
    match h {
        Harness::Claude => Format::Claude,
        Harness::Codex => Format::Codex,
        Harness::Pi => Format::Pi,
    }
}

/// The Worker's vendor for `model = auto` (V46); pi's is unknown here.
fn worker_vendor(h: Harness) -> Option<Vendor> {
    match h {
        Harness::Claude => Some(Vendor::Anthropic),
        Harness::Codex => Some(Vendor::Openai),
        Harness::Pi => None,
    }
}

fn trigger_name(t: Trigger, catch_up: bool) -> &'static str {
    match t {
        _ if catch_up => "catch-up",
        Trigger::TurnEnd => "turn-end",
        Trigger::Checkpoint => "checkpoint",
        Trigger::Blocked => "blocked",
    }
}

fn skip_of(b: &Built, now: u64) -> Skip {
    let span = b.omitted.iter().chain(&b.slice);
    let from = span.clone().map(|e| e.t).min().unwrap_or(now);
    Skip { next: b.next.clone(), from, to: span.map(|e| e.t).max().unwrap_or(now) }
}

/// The held slice's identity for the poison guard (V88): where it starts.
fn slice_key(b: &Built) -> String {
    b.slice.first().or(b.omitted.first()).map_or_else(|| "empty".to_owned(), |e| e.id.clone())
}

/// Splits what a Focus has not reviewed (V92): entries before a hold replay oldest first in
/// slices of whole turns up to [`SLICE_MAX`]; from the hold on, one merged wake shows the last
/// turn and folds the rest into history (V50, V70). Returns (shown, omitted, merged hold).
fn cut(all: Vec<Entry>, held: Option<Hold>) -> (Vec<Entry>, Vec<Entry>, Option<Hold>) {
    let before = held.map_or(all.len(), |h| all.iter().position(|e| e.t >= h.since).unwrap_or(all.len()));
    if before == 0 && held.is_some() && !all.is_empty() {
        let last_turn =
            all[..all.len() - 1].iter().rposition(|e| e.items.contains(&Item::TurnEnd)).map_or(0, |i| i + 1);
        let mut all = all;
        let shown = all.split_off(last_turn);
        return (shown, all, held);
    }
    let mut end = 0;
    let mut size = 0;
    for (i, e) in all[..before].iter().enumerate() {
        size += e.items.iter().map(item_len).sum::<usize>();
        if e.items.contains(&Item::TurnEnd) {
            if end > 0 && size > SLICE_MAX {
                break;
            }
            end = i + 1;
        }
    }
    // The live tail: an unfinished turn goes with the turns before it when nothing is held.
    if end == 0 || (held.is_none() && size <= SLICE_MAX) {
        end = before;
    }
    let mut all = all;
    all.truncate(end);
    (all, Vec::new(), None)
}

fn turn_ends(entries: &[Entry]) -> u64 {
    entries.iter().flat_map(|e| &e.items).filter(|i| **i == Item::TurnEnd).count() as u64
}

fn item_len(i: &Item) -> usize {
    match i {
        Item::Prompt(s) | Item::Text(s) => s.len(),
        Item::ToolCall { input, .. } => input.len(),
        Item::ToolResult { output, .. } => output.len(),
        _ => 0,
    }
}

fn join<T>(calls: thread::JoinHandle<T>) -> io::Result<T> {
    calls.join().map_err(|_| io::Error::other("wake calls panicked"))
}

/// One Watcher's call inside the wake clock (V86, V87): Service errors retried, invalid output
/// asked again once, then failed with its class. `retries = false` for a primary probe (V51).
#[allow(clippy::too_many_arguments)]
fn ask(
    provider: &Provider,
    table: &Table,
    choice: &Choice,
    input: &str,
    tools: Option<Snapshot>,
    deadline: Instant,
    retries: bool,
    probe: bool,
) -> Asked {
    let price = table.price(&choice.model).ok();
    let cost = |u: &Usage| price.map_or(0.0, |p| p.cost(u));
    let started = Instant::now();
    let (mut spend, mut usage, mut calls, mut text) = (0.0, Usage::default(), 0, None);
    let mut tries = 0;
    let mut invalid = None;
    let result = loop {
        let call = Call { model: &choice.model, vendor: choice.vendor, input, tools, deadline: Some(deadline) };
        match provider.call(&call) {
            Ok(reply) => {
                spend += cost(&reply.usage);
                usage += reply.usage;
                calls += reply.tools.calls;
                match output::parse(&reply.text) {
                    Ok(lines) => {
                        text = Some(reply.text);
                        break Ok(lines);
                    }
                    // Invalid twice is a Slice failure (V86).
                    Err(why) if invalid.is_some() => break Err((Class::Slice, format!("invalid output: {why}"), true)),
                    Err(why) => invalid = Some(why),
                }
            }
            Err(f) => {
                spend += cost(&f.usage);
                usage += f.usage;
                let wait = retries.then(|| fail::retry_delay(tries, &f.error, fail::jitter())).flatten();
                match wait {
                    Some(wait) if Instant::now() + wait < deadline => {
                        tries += 1;
                        thread::sleep(wait);
                    }
                    _ => break Err((fail::classify(&f.error), f.error.to_string(), false)),
                }
            }
        }
        if Instant::now() >= deadline {
            break Err((Class::Service, ProviderError::Timeout(String::new()).to_string(), false));
        }
    };
    let latency_ms = started.elapsed().as_millis() as u64;
    Asked { result, text, spend, usage, tools: calls, latency_ms, model: choice.model.clone(), probe }
}

/// The Watcher process's heartbeat (V90): its own thread, sharing no lock with wake tasks.
fn heartbeat(path: PathBuf) {
    let beat = move || {
        let _ = write_atomic(&path, now_ms().to_string().as_bytes());
    };
    beat();
    thread::spawn(move || {
        loop {
            thread::sleep(HEARTBEAT);
            beat();
        }
    });
}

/// Unix ms in a heartbeat file, or `None` if absent or unreadable.
pub fn beat_of(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// `pair-watcher watch <session>` (V61): runs until drained. A Watcher that cannot run logs
/// BLIND once and waits for the drain, so the supervisor does not restart it in a loop.
pub fn watch(home: &Home, session: &str) -> io::Result<()> {
    let mailbox = Mailbox::open_registered(home, session)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("session {session} is not registered")))?;
    let env = |k: &str| std::env::var(k).ok();
    match Watch::open(home, mailbox.clone(), &env) {
        Ok(mut w) => {
            upkeep(home.clone(), w.cfg.retention, w.cfg.telemetry);
            w.run()
        }
        Err(Unrunnable(reason)) => {
            heartbeat(mailbox.dir().join(HEARTBEAT_FILE));
            let log = mailbox.log();
            let board = Board::rebuild(0, &log.read_from(0)?.records);
            if !board.blind.contains_key(&None) {
                log.append(Op::Blind { focus: None, reason }.event())?;
            }
            while !mailbox.dir().join(DRAIN_FILE).exists() {
                thread::sleep(TICK);
            }
            Ok(())
        }
    }
}

/// Store housekeeping off the wake path (V96, V99): retention + cap, then a due weekly telemetry
/// send. Failures only print; nothing here may stop the Watcher.
fn upkeep(home: Home, policy: crate::retention::Policy, telemetry: bool) {
    thread::spawn(move || {
        let now = now_ms();
        if let Err(e) = crate::retention::prune(&home, policy, now) {
            eprintln!("pair-watcher: prune: {e}");
        }
        if telemetry {
            match crate::telemetry::weekly(&home, now, &crate::telemetry::post) {
                Ok(crate::telemetry::Sent::Dropped(why)) => eprintln!("pair-watcher: telemetry dropped: {why}"),
                Err(e) => eprintln!("pair-watcher: telemetry: {e}"),
                _ => {}
            }
        }
    });
}

/// The launch env's API key vars, for reloading keys later.
fn key_env(env: &dyn Fn(&str) -> Option<String>) -> HashMap<String, String> {
    [Vendor::Anthropic, Vendor::Openai]
        .into_iter()
        .map(crate::credentials::env_name)
        .filter_map(|k| Some((k.to_owned(), env(k)?)))
        .collect()
}
