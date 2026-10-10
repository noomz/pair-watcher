//! Append-only JSONL files shared by the mailbox and the session event log (I4, V93).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Record schema major. Every line carries it; a reader rejects any other major (canary input).
pub const SCHEMA: u32 = 1;

/// One line: schema version, arrival time (unix ms), then the body's fields.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Record<B> {
    pub v: u32,
    pub t: u64,
    #[serde(flatten)]
    pub body: B,
}

impl<B> Record<B> {
    pub fn at(t: u64, body: B) -> Self {
        Record { v: SCHEMA, t, body }
    }

    pub fn now(body: B) -> Self {
        Record::at(now_ms(), body)
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Opens for append (and read), creating the file 0600.
pub fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).append(true).create(true).mode(0o600).open(path)
}

/// Appends one record under an exclusive lock.
pub fn append<T: Serialize>(path: &Path, record: &T) -> io::Result<()> {
    let file = open_append(path)?;
    file.lock()?;
    write_line(&file, record)
}

/// Writes one record as one line to a file the caller has locked. A torn last line left by a
/// crashed writer is terminated first, so it stays one invalid line instead of corrupting this one.
pub fn write_line<T: Serialize>(file: &File, record: &T) -> io::Result<()> {
    let mut line = Vec::new();
    let len = file.metadata()?.len();
    if len > 0 {
        let mut last = [0u8];
        file.read_exact_at(&mut last, len - 1)?;
        if last[0] != b'\n' {
            line.push(b'\n');
        }
    }
    serde_json::to_writer(&mut line, record)?;
    line.push(b'\n');
    let mut file = file;
    file.write_all(&line)
}

/// Records read from a byte offset. `next` is the offset after the last complete line.
#[derive(Debug)]
pub struct Batch<T> {
    pub records: Vec<T>,
    /// One message per complete line that failed to parse or has another schema major.
    pub invalid: Vec<String>,
    pub next: u64,
}

/// Reads complete lines from `offset`; a torn last line stays unread. A missing file reads empty.
pub fn read_from<T: DeserializeOwned>(path: &Path, offset: u64) -> io::Result<Batch<T>> {
    let mut batch = Batch { records: Vec::new(), invalid: Vec::new(), next: offset };
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(batch),
        Err(e) => return Err(e),
    };
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let Some(end) = buf.iter().rposition(|&b| b == b'\n') else { return Ok(batch) };
    batch.next = offset + end as u64 + 1;

    let mut at = offset;
    for line in buf[..end].split(|&b| b == b'\n') {
        let line_at = at;
        at += line.len() as u64 + 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match parse(line) {
            Ok(r) => batch.records.push(r),
            Err(e) => batch.invalid.push(format!("{}@{line_at}: {e}", path.display())),
        }
    }
    Ok(batch)
}

fn parse<T: DeserializeOwned>(line: &[u8]) -> Result<T, String> {
    let value: Value = serde_json::from_slice(line).map_err(|e| e.to_string())?;
    match value.get("v").and_then(Value::as_u64) {
        Some(v) if v == u64::from(SCHEMA) => {}
        Some(v) => return Err(format!("schema v{v} unsupported (this build reads v{SCHEMA})")),
        None => return Err("no schema version".into()),
    }
    serde_json::from_value(value).map_err(|e| e.to_string())
}
