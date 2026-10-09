//! Retention (V96): wake records age out after `store.retention_days`; `store.cap_gb` drops the
//! oldest unpinned records first. Pinned: Challenge/Escalation wakes and audit-labelled wakes. A
//! session's snapshot store goes once the session has ended and none of its records remain (the
//! records' trees live there, V96 replay). Event logs stay until the user prunes them by hand;
//! Standing rulings are never touched.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::audit;
use crate::event::Kind;
use crate::home::Home;
use crate::mailbox::LOG_FILE;
use crate::record::{self, DIR};
use crate::status::{Outcome, Wake};

const DAY_MS: u64 = 86_400_000;
const GB: f64 = 1024.0 * 1024.0 * 1024.0;
/// The snapshot store of a session (`snapshot::Store`).
const STORE: [&str; 2] = ["snapshots.git", "snapshots.index"];
/// A live session's Watcher holds this (`mailbox`).
const REGISTERED: &str = "registered";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Policy {
    pub retention_days: u64,
    pub cap_gb: f64,
}

impl Policy {
    pub fn from(r: &crate::config::Resolved) -> Policy {
        Policy {
            retention_days: r
                .get("store.retention_days")
                .and_then(toml::Value::as_integer)
                .map_or(30, |d| d.max(0) as u64),
            cap_gb: r
                .get("store.cap_gb")
                .and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)))
                .unwrap_or(2.0),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub records: usize,
    pub stores: usize,
    pub bytes: u64,
}

struct Rec {
    path: PathBuf,
    session: usize,
    mtime: u64,
    size: u64,
}

struct Sess {
    dir: PathBuf,
    live: bool,
    records: usize,
    store_bytes: u64,
}

/// The records a session keeps whatever their age (V96).
fn pinned(dir: &Path) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(batch) = crate::event::EventLog::new(dir.join(LOG_FILE)).read_from(0) else { return out };
    let mut findings = std::collections::HashMap::new();
    for r in &batch.records {
        match r.body.k {
            Kind::Wake => {
                if let Some(w) = r.body.parse::<Wake>()
                    && matches!(w.outcome, Outcome::Challenge | Outcome::Escalation)
                {
                    out.extend(w.record);
                }
            }
            Kind::Audit => match r.body.parse::<audit::Op>() {
                Some(audit::Op::Finding { id, wake, watcher, .. }) => {
                    findings.insert(id, record::rel(wake, &watcher));
                }
                Some(audit::Op::Label { finding, .. }) => out.extend(findings.get(&finding).cloned()),
                _ => {}
            },
            _ => {}
        }
    }
    out
}

fn size(path: &Path) -> u64 {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {
            fs::read_dir(path).map_or(0, |d| d.filter_map(Result::ok).map(|e| size(&e.path())).sum())
        }
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

fn mtime_ms(m: &fs::Metadata) -> u64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64)
}

/// Applies retention, then the cap. Returns what went.
pub fn prune(home: &Home, policy: Policy, now: u64) -> io::Result<Pruned> {
    let mut sessions = Vec::new();
    let mut recs = Vec::new();
    let Ok(dirs) = fs::read_dir(home.sessions_dir()) else { return Ok(Pruned::default()) };
    for entry in dirs.filter_map(Result::ok) {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let pins = pinned(&dir);
        let i = sessions.len();
        let mut s = Sess {
            live: dir.join(REGISTERED).is_file(),
            records: 0,
            store_bytes: STORE.iter().map(|p| size(&dir.join(p))).sum(),
            dir: dir.clone(),
        };
        if let Ok(files) = fs::read_dir(dir.join(DIR)) {
            for f in files.filter_map(Result::ok) {
                let Ok(meta) = f.metadata() else { continue };
                s.records += 1;
                let rel = format!("{DIR}/{}", f.file_name().to_string_lossy());
                if !pins.contains(&rel) {
                    recs.push(Rec { path: f.path(), session: i, mtime: mtime_ms(&meta), size: meta.len() });
                }
            }
        }
        sessions.push(s);
    }

    let mut out = Pruned::default();
    let mut total: u64 = recs.iter().map(|r| r.size).sum::<u64>()
        + sessions.iter().map(|s| s.store_bytes).sum::<u64>()
        + pinned_bytes(&sessions, &recs);
    recs.sort_by_key(|r| r.mtime);
    let cutoff = now.saturating_sub(policy.retention_days * DAY_MS);
    let cap = (policy.cap_gb.max(0.0) * GB) as u64;
    for r in &recs {
        if r.mtime >= cutoff && total <= cap {
            break;
        }
        fs::remove_file(&r.path)?;
        out.records += 1;
        out.bytes += r.size;
        total = total.saturating_sub(r.size);
        let s = &mut sessions[r.session];
        s.records -= 1;
        if s.records == 0 && !s.live && s.store_bytes > 0 {
            drop_store(s, &mut out, &mut total)?;
        }
    }
    // Ended sessions with no records left whose store outlived retention.
    for s in &mut sessions {
        let old = fs::metadata(s.dir.join(STORE[0])).is_ok_and(|m| mtime_ms(&m) < cutoff);
        if s.records == 0 && !s.live && s.store_bytes > 0 && old {
            drop_store(s, &mut out, &mut total)?;
        }
    }
    Ok(out)
}

/// Bytes of the pinned records (counted toward the cap, never dropped).
fn pinned_bytes(sessions: &[Sess], unpinned: &[Rec]) -> u64 {
    let listed: HashSet<&Path> = unpinned.iter().map(|r| r.path.as_path()).collect();
    sessions
        .iter()
        .flat_map(|s| fs::read_dir(s.dir.join(DIR)).into_iter().flatten().filter_map(Result::ok))
        .filter(|f| !listed.contains(f.path().as_path()))
        .filter_map(|f| f.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn drop_store(s: &mut Sess, out: &mut Pruned, total: &mut u64) -> io::Result<()> {
    for p in STORE {
        let path = s.dir.join(p);
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(&path)?,
            Ok(_) => fs::remove_file(&path)?,
            Err(_) => {}
        }
    }
    out.stores += 1;
    out.bytes += s.store_bytes;
    *total = total.saturating_sub(s.store_bytes);
    s.store_bytes = 0;
    Ok(())
}
