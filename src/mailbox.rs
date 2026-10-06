//! Per-session file mailbox `~/.pair-watcher/sessions/<id>/` (I4). The id is the Worker session id.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::EventLog;
use crate::home::Home;
use crate::jsonl::{self, Batch, Record};
use crate::outbox::Outbox;

/// Present while a Watcher follows the session; hooks no-op without it (V58).
const REGISTERED: &str = "registered";

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

    pub fn outbox(&self) -> Outbox {
        Outbox::new(self.dir.join("outbox.jsonl"))
    }

    pub fn log(&self) -> EventLog {
        EventLog::new(self.dir.join("log.jsonl"))
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
