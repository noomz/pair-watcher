//! Per-wake record files (V93, V96): one zstd-compressed JSON file per Watcher per wake under
//! `sessions/<id>/wakes/`, referenced from its `wake` event. They hold what the event log does
//! not: the shared input sections, the snapshot the read tools were served from, the model's
//! answer. `replay` and `audit` read them back; `prune` ages them out.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::home::write_atomic;
use crate::wake::Input;

/// Under the session dir.
pub const DIR: &str = "wakes";
const LEVEL: i32 = 9;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WakeRecord {
    pub n: u64,
    pub watcher: String,
    pub trigger: String,
    /// The Focuses whose briefs the input carried; the prompt + briefs are not stored, `replay`
    /// renders them again (current or recorded, V98).
    pub focuses: Vec<String>,
    pub input: Input,
    /// Snapshot tree the read tools were served from (V96); `None` = no tools (blocked wake).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The model's accepted answer; `None` when the wake failed or was skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// Session-relative path of wake `n` of `watcher`.
pub fn rel(n: u64, watcher: &str) -> String {
    format!("{DIR}/{n}-{watcher}.json.zst")
}

/// Writes the record; returns its session-relative ref.
pub fn write(session: &Path, record: &WakeRecord) -> io::Result<String> {
    let rel = rel(record.n, &record.watcher);
    let path = session.join(&rel);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec(record)?;
    write_atomic(&path, &zstd::encode_all(json.as_slice(), LEVEL)?)?;
    Ok(rel)
}

pub fn read(session: &Path, rel: &str) -> io::Result<WakeRecord> {
    let bytes = zstd::decode_all(fs::File::open(session.join(rel))?)?;
    serde_json::from_slice(&bytes).map_err(io::Error::from)
}
