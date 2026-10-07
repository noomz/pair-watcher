//! Session event log: one append-only file per session, the single source of truth (V93, I20).
//! Payload fields per kind (V94) are typed by the task that emits them.

use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::jsonl::{self, Batch, Record};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Session,
    Wake,
    Concern,
    Shown,
    Human,
    Challenge,
    Debate,
    Escalation,
    Ruled,
    Answered,
    Ruling,
    Unruled,
    ClosedStale,
    Status,
    Watcher,
    Coverage,
    Audit,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Event {
    pub k: Kind,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

impl Event {
    pub fn new(k: Kind) -> Self {
        Event { k, fields: Map::new() }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.fields.insert(key.to_owned(), value.into());
        self
    }

    /// An event whose fields are `body` serialized as a JSON object.
    pub fn typed<T: Serialize>(k: Kind, body: &T) -> Self {
        let fields = match serde_json::to_value(body) {
            Ok(Value::Object(map)) => map,
            other => panic!("event body must serialize to an object, got {other:?}"),
        };
        Event { k, fields }
    }

    /// The fields read back as `T`; `None` when they do not fit.
    pub fn parse<T: DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_value(Value::Object(self.fields.clone())).ok()
    }
}

#[derive(Clone, Debug)]
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        EventLog { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends, stamped with the current time.
    pub fn append(&self, event: Event) -> io::Result<()> {
        jsonl::append(&self.path, &Record::now(event))
    }

    /// Tails from `offset`; pass `batch.next` back to continue.
    pub fn read_from(&self, offset: u64) -> io::Result<Batch<Record<Event>>> {
        jsonl::read_from(&self.path, offset)
    }
}
