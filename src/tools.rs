//! The Watcher's read tools (I15): read file, grep, `git log`, `git show`, served from the wake's
//! snapshot and never writing. ≤6 calls and ≤24 KB read per wake, separate from the input; then
//! output is forced (V8).

use serde_json::{Value, json};

use crate::clip;
use crate::snapshot::Store;

pub const MAX_CALLS: usize = 6;
pub const MAX_READ: usize = 24 * 1024;
const LOG_MAX: u64 = 20;
const SPENT: &str = "not run: this wake's tool allowance is spent; answer now";

/// One tool as offered to the model.
pub struct Def {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

pub fn defs() -> Vec<Def> {
    let path = json!({"type": "string", "description": "repo-relative path"});
    vec![
        Def {
            name: "read_file",
            description: "Read a file as the Worker's tree stood at this wake. Lines come numbered.",
            schema: json!({
                "type": "object",
                "properties": {
                    "path": path,
                    "start_line": {"type": "integer", "minimum": 1},
                    "end_line": {"type": "integer", "minimum": 1}
                },
                "required": ["path"]
            }),
        },
        Def {
            name: "grep",
            description: "Search text files of the tree at this wake (extended regex). Returns path:line:text.",
            schema: json!({
                "type": "object",
                "properties": {"pattern": {"type": "string"}, "path": path},
                "required": ["pattern"]
            }),
        },
        Def {
            name: "git_log",
            description: "One-line commit log of the Worker repo as of this wake, newest first.",
            schema: json!({
                "type": "object",
                "properties": {"path": path, "max": {"type": "integer", "minimum": 1, "maximum": LOG_MAX}}
            }),
        },
        Def {
            name: "git_show",
            description: "Show a commit (stat + patch) or `<rev>:<path>`. Use commit ids or HEAD, HEAD~n.",
            schema: json!({
                "type": "object",
                "properties": {"rev": {"type": "string"}},
                "required": ["rev"]
            }),
        },
    ]
}

/// The snapshot one wake's tools read.
#[derive(Clone, Copy)]
pub struct Snapshot<'a> {
    pub store: &'a Store,
    pub tree: &'a str,
    /// Worker HEAD recorded with the snapshot.
    pub head: Option<&'a str>,
}

/// One wake's tool use so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Allowance {
    pub calls: usize,
    pub read: usize,
}

impl Allowance {
    /// Calls or bytes used up: the next request forces output.
    pub fn spent(&self) -> bool {
        self.calls >= MAX_CALLS || self.read >= MAX_READ
    }

    /// Runs one call if the allowance has room; its result is clipped to the bytes left.
    pub fn run(&mut self, snap: Snapshot, name: &str, args: &Value) -> String {
        if self.spent() {
            return SPENT.to_owned();
        }
        self.calls += 1;
        let out = execute(snap, name, args).unwrap_or_else(|e| format!("error: {e}"));
        let left = MAX_READ - self.read;
        let out = if out.len() > left {
            format!("{}\n[clipped: tool read allowance reached]", clip::prefix(&out, left))
        } else {
            out
        };
        self.read = (self.read + out.len()).min(MAX_READ);
        out
    }
}

fn execute(snap: Snapshot, name: &str, args: &Value) -> Result<String, String> {
    let s = |k: &str| args.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
    let n = |k: &str| args.get(k).and_then(Value::as_u64);
    let need = |k: &str| s(k).ok_or_else(|| format!("missing `{k}`"));
    match name {
        "read_file" => {
            let path = need("path")?;
            let bytes = snap.store.read(snap.tree, path).map_err(|e| e.to_string())?;
            if bytes.contains(&0) {
                return Ok(format!("{path}: binary file, {} B", bytes.len()));
            }
            let text = String::from_utf8_lossy(&bytes);
            let from = n("start_line").unwrap_or(1).max(1) as usize;
            let to = n("end_line").map_or(usize::MAX, |e| e as usize);
            let lines: String = text
                .lines()
                .enumerate()
                .map(|(i, l)| (i + 1, l))
                .filter(|(i, _)| (from..=to).contains(i))
                .map(|(i, l)| format!("{i}\t{l}\n"))
                .collect();
            Ok(if lines.is_empty() { format!("{path}: no lines in range") } else { lines })
        }
        "grep" => {
            let out = snap.store.grep(snap.tree, need("pattern")?, s("path")).map_err(|e| e.to_string())?;
            Ok(if out.is_empty() { "no matches".into() } else { out })
        }
        "git_log" => {
            let head = snap.head.ok_or("the Worker repo has no commits")?;
            let max = n("max").unwrap_or(LOG_MAX).clamp(1, LOG_MAX) as usize;
            snap.store.log(head, s("path"), max).map_err(|e| e.to_string())
        }
        "git_show" => {
            let rev = need("rev")?;
            if rev.starts_with('-') {
                return Err(format!("bad rev `{rev}`"));
            }
            snap.store.show(snap.head, rev).map_err(|e| e.to_string())
        }
        other => Err(format!("unknown tool `{other}`")),
    }
}
