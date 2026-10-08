//! Standing rulings store (V77, V78): `~/.pair-watcher/repos/<key>.rulings.toml`, human-editable,
//! keyed by the repo's root commit (path fallback). Here: what `s` / `/pw keep` / `pair-watcher
//! keep` write, refused at the cap. Loading into wakes, `r`/`x`, `rulings list|rm`, edit
//! detection and the rulings hash are T14.

use std::fs::{self, DirBuilder, File};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::sha256_hex;
use crate::home::{Home, write_atomic};
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

#[derive(Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    ruling: Vec<Standing>,
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

/// The rulings held; a missing file holds none.
pub fn load(path: &Path) -> io::Result<Vec<Standing>> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str::<Store>(&text)
            .map(|s| s.ruling)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Appends `keep` as a new Standing ruling dated `date`. `Ok(Err(why))` = refused: the cap is
/// reached (V78). An unreadable file is never overwritten.
pub fn remember(path: &Path, keep: &Keep, date: &str) -> io::Result<Result<String, String>> {
    if let Some(dir) = path.parent() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let lock = File::options().create(true).append(true).mode(0o600).open(path.with_extension("lock"))?;
    lock.lock()?;
    let mut held = load(path)?;
    if held.len() >= CAP {
        return Ok(Err(format!("{CAP} Standing rulings held; remove one first (r → x, or pair-watcher rulings rm)")));
    }
    let n = held.iter().filter_map(|r| r.id.strip_prefix('r')?.parse::<usize>().ok()).max().unwrap_or(0) + 1;
    let id = format!("r{n}");
    held.push(Standing {
        id: id.clone(),
        focus: keep.focus.clone(),
        headline: keep.headline.clone(),
        verdict: keep.side.name().to_owned(),
        reason: keep.reason.clone(),
        date: date.to_owned(),
    });
    let text = toml::to_string(&Store { ruling: held }).map_err(io::Error::other)?;
    write_atomic(path, text.as_bytes())?;
    Ok(Ok(id))
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
