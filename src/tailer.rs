//! Transcript tailers: Claude JSONL, Codex rollout + guardian sibling rollout, pi session (I6).
//! A read is pure: entries after the cursor plus the next cursor, which the caller commits (V10).
//! Entries carry arrival time, never transcript timestamps (Codex rollout clocks run behind).

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::sha256_hex;
use crate::jsonl::now_ms;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Claude,
    Codex,
    /// The Codex guardian reviewer's sibling rollout (V56).
    Guardian,
    Pi,
}

/// Position in one transcript (V10).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    /// Last consumed entry; `None` before the first read.
    pub id: Option<String>,
    /// Byte offset where that entry starts.
    pub at: u64,
    /// Claude: message whose turn end was already emitted (one message spans several lines).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_msg: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub id: String,
    /// Arrival time (unix ms): when this read saw the entry.
    pub t: u64,
    pub items: Vec<Item>,
    /// Where a read resumes after this entry: a slice ending here commits it (V10, V92).
    pub next: Cursor,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// A prompt into the Worker (typed by the human or submitted by an adapter).
    Prompt(String),
    Text(String),
    ToolCall {
        id: String,
        name: String,
        input: String,
    },
    ToolResult {
        id: String,
        output: String,
        error: bool,
    },
    TurnEnd,
    /// Guardian auto-review started: no human is being asked (V56).
    ReviewStarted,
    /// Guardian auto-review finished; `allow` = the tool runs.
    ReviewDone {
        outcome: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reanchor {
    /// The cursor's entry moved (rewrite, resume copy); read resumed after it.
    Found,
    /// The cursor's entry is gone; the whole file was read.
    Lost,
}

#[derive(Debug)]
pub struct Tail {
    /// Entries after the cursor that carry items.
    pub entries: Vec<Entry>,
    pub cursor: Cursor,
    pub reanchor: Option<Reanchor>,
    /// One message per complete line that is not JSON.
    pub invalid: Vec<String>,
}

/// Reads complete lines after `cursor`; a torn last line stays unread. The line at `cursor.at`
/// must carry `cursor.id`, else the id re-anchors (rewind, compaction, resume).
pub fn read(format: Format, path: &Path, cursor: &Cursor) -> io::Result<Tail> {
    let t = now_ms();
    let mut file = File::open(path)?;
    let (lines, start, reanchor) = match &cursor.id {
        None => (lines_from(format, &mut file, 0)?, 0, None),
        Some(id) => {
            let tail = lines_from(format, &mut file, cursor.at)?;
            if tail.first().is_some_and(|l| l.at == cursor.at && l.id == *id) {
                (tail, 1, None)
            } else {
                let all = lines_from(format, &mut file, 0)?;
                match all.iter().rposition(|l| l.id == *id) {
                    Some(i) => (all, i + 1, Some(Reanchor::Found)),
                    None => (all, 0, Some(Reanchor::Lost)),
                }
            }
        }
    };

    let mut next = cursor.clone();
    match lines.last() {
        Some(last) => (next.id, next.at) = (Some(last.id.clone()), last.at),
        None if reanchor == Some(Reanchor::Lost) => (next.id, next.at) = (None, 0),
        None => {}
    }
    let mut tail = Tail { entries: Vec::new(), cursor: next, reanchor, invalid: Vec::new() };
    for line in lines.into_iter().skip(start) {
        match line.value {
            Err(e) => tail.invalid.push(format!("{}@{}: {e}", path.display(), line.at)),
            Ok(v) => {
                let items = match format {
                    Format::Claude => claude(&v, &mut tail.cursor.turn_msg),
                    Format::Codex => codex(&v),
                    Format::Guardian => guardian(&v),
                    Format::Pi => pi(&v),
                };
                if !items.is_empty() {
                    let next =
                        Cursor { id: Some(line.id.clone()), at: line.at, turn_msg: tail.cursor.turn_msg.clone() };
                    tail.entries.push(Entry { id: line.id, t, items, next });
                }
            }
        }
    }
    Ok(tail)
}

/// The Worker's guardian sibling rollout in `dir` (the Worker rollout's day dir): its
/// `session_meta` has `parent_thread_id` = Worker session id and `source.subagent.other: "guardian"`.
pub fn find_guardian(dir: &Path, worker_id: &str) -> io::Result<Option<PathBuf>> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
            continue;
        }
        let Ok(file) = File::open(&path) else { continue };
        let mut first = Vec::new();
        BufReader::new(file).read_until(b'\n', &mut first)?;
        let Ok(v) = serde_json::from_slice::<Value>(&first) else { continue };
        let meta = &v["payload"];
        if v["type"] == "session_meta"
            && meta["parent_thread_id"] == worker_id
            && meta["source"]["subagent"]["other"] == "guardian"
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

struct Line {
    at: u64,
    id: String,
    value: Result<Value, String>,
}

fn lines_from(format: Format, file: &mut File, offset: u64) -> io::Result<Vec<Line>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |e| e + 1);
    let mut lines = Vec::new();
    let mut at = offset;
    for raw in buf[..end].split_inclusive(|&b| b == b'\n') {
        let line_at = at;
        at += raw.len() as u64;
        let raw = raw.trim_ascii();
        if raw.is_empty() {
            continue;
        }
        let value = serde_json::from_slice::<Value>(raw).map_err(|e| e.to_string());
        lines.push(Line { at: line_at, id: entry_id(format, value.as_ref().ok(), raw), value });
    }
    Ok(lines)
}

/// Native id where the format has a unique one, else a hash of the line. Codex has none: item ids
/// repeat response ids and `ordinal` restarts per file. Claude `messageId` repeats a user `uuid`.
fn entry_id(format: Format, value: Option<&Value>, raw: &[u8]) -> String {
    let key = match format {
        Format::Claude => Some("uuid"),
        Format::Pi => Some("id"),
        Format::Codex | Format::Guardian => None,
    };
    key.and_then(|k| value?.get(k)?.as_str()).map_or_else(|| sha256_hex(raw)[..16].to_owned(), str::to_owned)
}

fn claude(v: &Value, turn_msg: &mut Option<String>) -> Vec<Item> {
    let msg = &v["message"];
    let mut items = Vec::new();
    match v["type"].as_str() {
        Some("user") if v["isMeta"] != true && v["isCompactSummary"] != true => {
            prompt(&msg["content"], &mut items);
            for b in blocks(msg) {
                if b["type"] == "tool_result" {
                    items.push(Item::ToolResult {
                        id: string(&b["tool_use_id"]),
                        output: text(&b["content"]),
                        error: b["is_error"] == true,
                    });
                }
            }
        }
        Some("assistant") => {
            for b in blocks(msg) {
                match b["type"].as_str() {
                    Some("text") => items.push(Item::Text(string(&b["text"]))),
                    Some("tool_use") => items.push(Item::ToolCall {
                        id: string(&b["id"]),
                        name: string(&b["name"]),
                        input: string(&b["input"]),
                    }),
                    _ => {}
                }
            }
            // Every line of the final message carries its stop reason; thinking comes first, so the
            // turn ends on its first line with a reply block.
            let reply = blocks(msg).any(|b| !matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking")));
            if reply && msg["stop_reason"].as_str().is_some_and(|s| s != "tool_use") {
                let id = msg["id"].as_str().map(str::to_owned);
                if id.is_none() || *turn_msg != id {
                    *turn_msg = id;
                    items.push(Item::TurnEnd);
                }
            }
        }
        _ => {}
    }
    items
}

/// Worker rollout. Prompts come from `item_completed` `UserMessage`: user-role response items
/// also carry harness context (`<environment_context>`).
fn codex(v: &Value) -> Vec<Item> {
    let p = &v["payload"];
    let mut items = Vec::new();
    match (v["type"].as_str(), p["type"].as_str()) {
        (Some("response_item"), Some("message")) if p["role"] == "assistant" => {
            for b in blocks(p) {
                if b["type"] == "output_text" {
                    items.push(Item::Text(string(&b["text"])));
                }
            }
        }
        (Some("response_item"), Some("custom_tool_call" | "function_call")) => items.push(Item::ToolCall {
            id: string(&p["call_id"]),
            name: string(&p["name"]),
            input: string(if p["input"].is_null() { &p["arguments"] } else { &p["input"] }),
        }),
        (Some("response_item"), Some("custom_tool_call_output" | "function_call_output")) => {
            items.push(Item::ToolResult { id: string(&p["call_id"]), output: text(&p["output"]), error: false })
        }
        (Some("event_msg"), Some("item_completed")) if p["item"]["type"] == "UserMessage" => {
            prompt(&p["item"]["content"], &mut items)
        }
        (Some("event_msg"), Some("task_complete")) => items.push(Item::TurnEnd),
        _ => {}
    }
    items
}

fn guardian(v: &Value) -> Vec<Item> {
    let p = &v["payload"];
    if v["type"] != "event_msg" {
        return Vec::new();
    }
    match p["type"].as_str() {
        Some("task_started") => vec![Item::ReviewStarted],
        Some("task_complete") => {
            let verdict = p["last_agent_message"].as_str().and_then(|m| serde_json::from_str::<Value>(m).ok());
            vec![Item::ReviewDone { outcome: verdict.and_then(|m| m["outcome"].as_str().map(str::to_owned)) }]
        }
        _ => Vec::new(),
    }
}

fn pi(v: &Value) -> Vec<Item> {
    let msg = &v["message"];
    let mut items = Vec::new();
    if v["type"] != "message" {
        return items;
    }
    match msg["role"].as_str() {
        Some("user") => prompt(&msg["content"], &mut items),
        Some("assistant") => {
            for b in blocks(msg) {
                match b["type"].as_str() {
                    Some("text") => items.push(Item::Text(string(&b["text"]))),
                    Some("toolCall") => items.push(Item::ToolCall {
                        id: string(&b["id"]),
                        name: string(&b["name"]),
                        input: string(&b["arguments"]),
                    }),
                    _ => {}
                }
            }
            if msg["stopReason"].as_str().is_some_and(|s| s != "toolUse") {
                items.push(Item::TurnEnd);
            }
        }
        Some("toolResult") => items.push(Item::ToolResult {
            id: string(&msg["toolCallId"]),
            output: text(&msg["content"]),
            error: msg["isError"] == true,
        }),
        _ => {}
    }
    items
}

fn blocks(msg: &Value) -> impl Iterator<Item = &Value> {
    msg["content"].as_array().into_iter().flatten()
}

fn prompt(content: &Value, items: &mut Vec<Item>) {
    let text = text(content);
    if !text.is_empty() {
        items.push(Item::Prompt(text));
    }
}

/// A string as is; content blocks as their `text` fields joined by newlines.
fn text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// A string as is; any other JSON compact (tool inputs).
fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}
