//! Per-session file mailbox `~/.pair-watcher/sessions/<id>/` (I4). The id is the Worker session id.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::{EventLog, Kind};
use crate::home::{Home, write_atomic};
use crate::jsonl::{self, Batch, Record};
use crate::outbox::Outbox;

/// Present while a Watcher follows the session; hooks no-op without it (V58).
const REGISTERED: &str = "registered";
/// Hook inbox offset the arbiter has stale-checked through (V21); idle deliverers wait on it.
const SETTLED: &str = "settled";
/// The session event log (I4).
pub const LOG_FILE: &str = "log.jsonl";

/// One hook invocation as appended to `events.jsonl` (I5).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Hook {
    pub event: String,
    /// Hook stdin: parsed JSON, else the raw text as a string.
    pub payload: Value,
}

#[derive(Clone, Debug)]
pub struct Mailbox {
    dir: PathBuf,
}

impl Mailbox {
    /// Creates the mailbox (0700) and marks a Watcher registered for the session.
    pub fn register(home: &Home, id: &str) -> io::Result<Mailbox> {
        let dir = session_dir(home, id)?;
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(dir.join(REGISTERED))?;
        Ok(Mailbox { dir })
    }

    /// The mailbox only if a Watcher is registered for `id`; an invalid id is never registered.
    pub fn open_registered(home: &Home, id: &str) -> Option<Mailbox> {
        let dir = session_dir(home, id).ok()?;
        dir.join(REGISTERED).is_file().then_some(Mailbox { dir })
    }

    /// A session's mailbox, live or ended (the pane can show an ended session's log).
    pub fn open(home: &Home, id: &str) -> Option<Mailbox> {
        let dir = session_dir(home, id).ok()?;
        dir.is_dir().then_some(Mailbox { dir })
    }

    /// A Watcher still follows the session.
    pub fn is_live(&self) -> bool {
        self.dir.join(REGISTERED).is_file()
    }

    pub fn id(&self) -> String {
        self.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// The repo from the `session` start event.
    pub fn repo(&self) -> Option<PathBuf> {
        let batch = self.log().read_from(0).ok()?;
        let start =
            batch.records.into_iter().find(|r| r.body.k == Kind::Session && r.body.str("phase") == Some("start"))?;
        start.body.str("repo").map(PathBuf::from)
    }

    /// Session end: hooks stop appending; the files stay until pruned.
    pub fn unregister(&self) -> io::Result<()> {
        match fs::remove_file(self.dir.join(REGISTERED)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn append_hook(&self, hook: &Hook) -> io::Result<()> {
        jsonl::append(&self.dir.join("events.jsonl"), &Record::now(hook))
    }

    /// Hook inbox from `offset` (the Watcher's inbox cursor, V10).
    pub fn read_hooks(&self, offset: u64) -> io::Result<Batch<Record<Hook>>> {
        jsonl::read_from(&self.dir.join("events.jsonl"), offset)
    }

    /// Arbiter: every hook record before inbox `offset` has had its wake committed and the
    /// waiting Challenge stale-checked (V21).
    pub fn settle(&self, offset: u64) -> io::Result<()> {
        write_atomic(&self.dir.join(SETTLED), offset.to_string().as_bytes())
    }

    /// The inbox offset last [`settle`](Self::settle)d; 0 before the first.
    pub fn settled(&self) -> io::Result<u64> {
        match fs::read_to_string(self.dir.join(SETTLED)) {
            Ok(text) => text.trim().parse().map_err(|_| io::Error::other(format!("bad {SETTLED} file"))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }

    pub fn outbox(&self) -> Outbox {
        Outbox::new(self.dir.join("outbox.jsonl"))
    }

    pub fn log(&self) -> EventLog {
        EventLog::new(self.dir.join(LOG_FILE))
    }
}

/// Live sessions, by id.
pub fn live(home: &Home) -> Vec<Mailbox> {
    let Ok(dir) = fs::read_dir(home.sessions_dir()) else { return Vec::new() };
    let mut out: Vec<Mailbox> = dir
        .filter_map(|e| e.ok())
        .filter_map(|e| Mailbox::open(home, &e.file_name().to_string_lossy()))
        .filter(Mailbox::is_live)
        .collect();
    out.sort_by_key(Mailbox::id);
    out
}

/// `--session` for session-scoped commands (I1): the named session (live, unless `ended_ok`),
/// else the only live session in `repo`; anything else is refused with the live sessions listed.
pub fn resolve(home: &Home, id: Option<&str>, repo: &Path, ended_ok: bool) -> Result<Mailbox, String> {
    let all = live(home);
    let list = || {
        let lines: Vec<String> = all
            .iter()
            .map(|m| format!("  {}  {}", m.id(), m.repo().map(|r| r.display().to_string()).unwrap_or_default()))
            .collect();
        if lines.is_empty() { "no live sessions".to_owned() } else { format!("live sessions:\n{}", lines.join("\n")) }
    };
    if let Some(id) = id {
        return match Mailbox::open(home, id) {
            Some(m) if ended_ok || m.is_live() => Ok(m),
            Some(_) => Err(format!("session {id} has ended; {}", list())),
            None => Err(format!("no session {id}; {}", list())),
        };
    }
    let here: Vec<&Mailbox> = all.iter().filter(|m| m.repo().as_deref() == Some(repo)).collect();
    match here.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(format!("no live session in {}; pass --session; {}", repo.display(), list())),
        _ => Err(format!("{} live sessions in {}; pass --session; {}", here.len(), repo.display(), list())),
    }
}

/// Session ids arrive in hook payloads (untrusted): one plain path segment only.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn session_dir(home: &Home, id: &str) -> io::Result<PathBuf> {
    if !valid_session_id(id) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("invalid session id {id:?}")));
    }
    Ok(home.sessions_dir().join(id))
}
