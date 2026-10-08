//! Standing rulings store (V77, V78): `~/.pair-watcher/repos/<key>.rulings.toml`, human-editable,
//! keyed by the repo's root commit (path fallback). What `s` / `/pw keep` / `pair-watcher keep`
//! add (refused at the cap) and `x` / `rulings rm` remove, the wake section, the rulings hash, and
//! hand edits seen at session start: a `.rulings.seen` sidecar holds what pair-watcher last knew,
//! so the next session start names what changed by hand.

use std::fs::{self, DirBuilder, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::sha256_hex;
use crate::home::{Home, write_atomic};
use crate::human::Act;
use crate::mailbox::Mailbox;
use crate::repo;
use crate::ruling::Keep;

/// Hard cap (V78): a new remember past it is refused.
pub const CAP: usize = 30;

/// One Standing ruling as the file holds it (V78): Focus, headline, verdict, reason, date.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Standing {
    pub id: String,
    pub focus: String,
    pub headline: String,
    /// `watcher`, `worker` or `dismissed`.
    pub verdict: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `YYYY-MM-DD`.
    pub date: String,
}

impl Standing {
    /// The verdict as the human made it.
    pub fn words(&self) -> &str {
        match self.verdict.as_str() {
            "watcher" => "sided with the Watcher",
            "worker" => "sided with the Worker",
            "dismissed" => "dismissed",
            other => other,
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    /// The highest id number ever given out, so an id is never reused (a `held back by ruling`
    /// citation names one ruling for good).
    #[serde(default, skip_serializing_if = "is_zero")]
    last_id: usize,
    #[serde(default)]
    ruling: Vec<Standing>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The store key of the repo at `dir`: its root commit, else a hash of its path (V78).
pub fn key(dir: &Path) -> String {
    match repo::discover(dir) {
        Some(repo::Repo { root_commit: Some(rc), .. }) => rc,
        Some(r) => path_key(&r.root),
        None => path_key(dir),
    }
}

fn path_key(dir: &Path) -> String {
    format!("path-{}", &sha256_hex(dir.to_string_lossy().as_bytes())[..16])
}

pub fn path(home: &Home, key: &str) -> PathBuf {
    home.repos_dir().join(format!("{key}.rulings.toml"))
}

/// The store of the session's repo (from its `session` start); `None` when either is unknown.
pub fn session_path(mailbox: &Mailbox) -> Option<PathBuf> {
    let home = Home::from_env().ok()?;
    Some(path(&home, &key(&mailbox.repo()?)))
}

/// What pair-watcher last knew the store to hold.
fn seen_path(path: &Path) -> PathBuf {
    path.with_extension("seen")
}

fn parse(path: &Path) -> io::Result<Option<Vec<Standing>>> {
    parse_store(path).map(|s| s.map(|s| s.ruling))
}

fn parse_store(path: &Path) -> io::Result<Option<Store>> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str::<Store>(&text)
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The rulings held; a missing file holds none.
pub fn load(path: &Path) -> io::Result<Vec<Standing>> {
    parse(path).map(Option::unwrap_or_default)
}

fn write(path: &Path, held: &[Standing]) -> io::Result<()> {
    write_store(path, &Store { last_id: 0, ruling: held.to_vec() })
}

fn write_store(path: &Path, store: &Store) -> io::Result<()> {
    write_atomic(path, toml::to_string(store).map_err(io::Error::other)?.as_bytes())
}

fn canonical(held: &[Standing]) -> String {
    toml::to_string(&Store { last_id: 0, ruling: held.to_vec() }).expect("plain strings serialize")
}

/// The rulings hash logged at session start (V84): what the rulings say, not how the file is
/// laid out, so a reformatted or re-commented file keeps its hash.
pub fn hash(held: &[Standing]) -> String {
    sha256_hex(canonical(held).as_bytes())
}

/// The wake input section (V7, V78): every ruling with its id, never trimmed. Empty when none.
pub fn render(held: &[Standing]) -> String {
    if held.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "The human ruled on these in earlier sessions on this repo. Never raise a Concern one of them \
         covers (judge by meaning, not wording); name it as `held back by ruling <id>`. Siding with the \
         Watcher never lowers the Challenge bar.\n",
    );
    for r in held {
        out.push_str(&format!("{} · {} · {} · {}: {}", r.id, r.date, r.focus, r.words(), r.headline));
        if let Some(reason) = &r.reason {
            out.push_str(&format!(" — {reason}"));
        }
        out.push('\n');
    }
    out
}

/// Holds the store's lock while `f` runs; creates the repos dir 0700.
fn locked<T>(path: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    if let Some(dir) = path.parent() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let lock = File::options().create(true).append(true).mode(0o600).open(path.with_extension("lock"))?;
    lock.lock()?;
    f()
}

/// Applies a logged tool change to the seen sidecar, so the next session start does not count
/// it as a hand edit. No sidecar yet = nothing known to update.
fn track(path: &Path, change: impl FnOnce(&mut Vec<Standing>)) -> io::Result<()> {
    let seen = seen_path(path);
    // An unreadable sidecar is replaced at the next session start.
    if let Ok(Some(mut held)) = parse(&seen) {
        change(&mut held);
        write(&seen, &held)?;
    }
    Ok(())
}

/// Appends `keep` as a new Standing ruling dated `date`. `Ok(Err(why))` = refused: the cap is
/// reached (V78). An unreadable file is never overwritten. `logged`: the caller logs the
/// `human` event, so the change is not a hand edit.
pub fn remember(path: &Path, keep: &Keep, date: &str, logged: bool) -> io::Result<Result<String, String>> {
    locked(path, || {
        let mut store = parse_store(path)?.unwrap_or_default();
        if store.ruling.len() >= CAP {
            return Ok(Err(format!(
                "{CAP} Standing rulings held; remove one first (r → x, or pair-watcher rulings rm)"
            )));
        }
        let n = store
            .ruling
            .iter()
            .filter_map(|r| r.id.strip_prefix('r')?.parse::<usize>().ok())
            .fold(store.last_id, usize::max)
            + 1;
        store.last_id = n;
        let ruling = Standing {
            id: format!("r{n}"),
            focus: keep.focus.clone(),
            headline: keep.headline.clone(),
            verdict: keep.side.name().to_owned(),
            reason: keep.reason.clone(),
            date: date.to_owned(),
        };
        store.ruling.push(ruling.clone());
        write_store(path, &store)?;
        if logged {
            track(path, |seen| seen.push(ruling.clone()))?;
        }
        Ok(Ok(ruling.id))
    })
}

/// Removes ruling `id` (`x`, `rulings rm`). `Ok(Err(why))` = no such ruling. An unreadable file
/// is never overwritten. `logged` as for `remember`.
pub fn remove(path: &Path, id: &str, logged: bool) -> io::Result<Result<Standing, String>> {
    locked(path, || {
        let mut store = parse_store(path)?.unwrap_or_default();
        let Some(i) = store.ruling.iter().position(|r| r.id == id) else {
            let ids: Vec<&str> = store.ruling.iter().map(|r| r.id.as_str()).collect();
            let have = if ids.is_empty() { "none held".to_owned() } else { format!("held: {}", ids.join(" ")) };
            return Ok(Err(format!("no Standing ruling {id} ({have})")));
        };
        let gone = store.ruling.remove(i);
        write_store(path, &store)?;
        if logged {
            track(path, |seen| seen.retain(|r| r.id != id))?;
        }
        Ok(Ok(gone))
    })
}

/// What session start read from the store (V78, V84).
#[derive(Debug, Default, PartialEq)]
pub struct Start {
    pub held: Vec<Standing>,
    /// `None` when the file is unreadable.
    pub hash: Option<String>,
    /// Hand edits since pair-watcher last knew the store: `human rulings-edited`.
    pub edited: Option<Act>,
    /// Ticker notes (an unreadable file).
    pub notes: Vec<String>,
}

/// Session start: loads the store and diffs it against what pair-watcher last knew; hand edits
/// become one `rulings-edited` fact and the sidecar catches up. The first sight of a store is the
/// baseline, not an edit. An unreadable file loads no rulings and is never overwritten.
pub fn start(path: &Path) -> io::Result<Start> {
    locked(path, || {
        let held = match load(path) {
            Ok(held) => held,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                return Ok(Start {
                    notes: vec![format!("rulings unreadable: {e}; none loaded until it is fixed")],
                    ..Start::default()
                });
            }
            Err(e) => return Err(e),
        };
        let seen = seen_path(path);
        let before = parse(&seen).ok().flatten();
        let edited = before.as_deref().and_then(|before| edits(before, &held));
        if before.is_none() || edited.is_some() {
            write(&seen, &held)?;
        }
        Ok(Start { hash: Some(hash(&held)), held, edited, notes: Vec::new() })
    })
}

fn edits(before: &[Standing], now: &[Standing]) -> Option<Act> {
    let ids = |list: &[Standing], other: &[Standing]| -> Vec<String> {
        list.iter().filter(|r| !other.iter().any(|o| o.id == r.id)).map(|r| r.id.clone()).collect()
    };
    let changed: Vec<String> =
        now.iter().filter(|r| before.iter().any(|b| b.id == r.id && b != *r)).map(|r| r.id.clone()).collect();
    let (added, removed) = (ids(now, before), ids(before, now));
    if added.is_empty() && removed.is_empty() && changed.is_empty() {
        return None;
    }
    Some(Act::RulingsEdited { added, removed, changed })
}

/// `YYYY-MM-DD` of `t_ms` at `tz` seconds east of UTC.
pub fn date(t_ms: u64, tz: i64) -> String {
    let days = ((t_ms / 1000) as i64 + tz).div_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}
