//! Tool calls as code reads them: action signature (V4) and outcome (V3).

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::clip;

/// Signature = tool + command with ids, hashes and temp paths stripped, ≤120 B (V4).
pub fn signature(tool: &str, input: &str) -> String {
    let arg = command(input);
    let sig = if arg.is_empty() { tool.to_owned() } else { format!("{tool} {arg}") };
    clip::head(&strip(&sig.split_whitespace().collect::<Vec<_>>().join(" ")), 120)
}

/// The argument naming what a call does: a shell command, else its target.
pub fn command(input: &str) -> String {
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(input) {
        for key in ["command", "cmd", "file_path", "path", "pattern", "url", "query"] {
            match map.get(key) {
                Some(Value::String(s)) => return s.clone(),
                Some(Value::Array(a)) => {
                    return a
                        .iter()
                        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
                        .collect::<Vec<_>>()
                        .join(" ");
                }
                _ => {}
            }
        }
        return String::new();
    }
    // Codex `apply_patch`: the files it touches.
    let files: Vec<&str> = input
        .lines()
        .filter_map(|l| {
            ["*** Update File: ", "*** Add File: ", "*** Delete File: "].iter().find_map(|p| l.strip_prefix(p))
        })
        .collect();
    if !files.is_empty() {
        return files.join(" ");
    }
    // Codex `exec`: JS calling `tools.exec_command({cmd:"…"})`.
    for key in ["cmd:\"", "\"cmd\":\"", "command:\"", "\"command\":\""] {
        if let Some(at) = input.find(key) {
            return js_string(&input[at + key.len()..]);
        }
    }
    clip::first_line(input, 120)
}

/// A string literal body up to its closing quote, common escapes undone.
fn js_string(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(c) => out.push(c),
                None => break,
            },
            c => out.push(c),
        }
    }
    out
}

const TEMP_DIRS: [&str; 4] = ["/private/var/folders/", "/private/tmp/", "/var/folders/", "/tmp/"];

/// Temp paths → `<tmp>`, UUIDs → `<id>`, hex runs ≥7 with a digit and a letter → `<hash>`.
/// Plain numbers stay: a PR number is what makes repetition visible.
fn strip(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some((at, dir)) = TEMP_DIRS.iter().filter_map(|d| rest.find(d).map(|i| (i, d))).min() {
        out.push_str(&rest[..at]);
        out.push_str("<tmp>");
        let tail = &rest[at + dir.len()..];
        rest = &tail[tail.find(|c: char| c.is_whitespace() || "\"'`)".contains(c)).unwrap_or(tail.len())..];
    }
    out.push_str(rest);
    strip_hex(&out)
}

fn strip_hex(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| c.is_ascii_digit() || (b'a'..=b'f').contains(&c);
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if i + 36 <= b.len() && is_uuid(&b[i..i + 36]) && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            out.push_str("<id>");
            i += 36;
            continue;
        }
        if hex(b[i]) && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            let end = (i..b.len()).find(|&j| !hex(b[j])).unwrap_or(b.len());
            let run = &b[i..end];
            let bounded = end == b.len() || !b[end].is_ascii_alphanumeric();
            if bounded
                && run.len() >= 7
                && run.iter().any(u8::is_ascii_digit)
                && run.iter().any(u8::is_ascii_alphabetic)
            {
                out.push_str("<hash>");
                i = end;
                continue;
            }
        }
        let len = s[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&s[i..i + len]);
        i += len;
    }
    out
}

fn is_uuid(b: &[u8]) -> bool {
    b.iter().enumerate().all(|(i, &c)| if [8, 13, 18, 23].contains(&i) { c == b'-' } else { c.is_ascii_hexdigit() })
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    Exit(i64),
    Error,
}

/// Tools whose result carries a process exit status: Claude `Bash`, Codex `exec` (and older
/// `exec_command`/`shell`/`local_shell`), pi `bash`. Other results may be file content.
pub const SHELL_TOOLS: [&str; 6] = ["Bash", "bash", "exec", "exec_command", "shell", "local_shell"];

impl Outcome {
    pub fn of(tool: &str, output: &str, error: bool) -> Outcome {
        let exit = if SHELL_TOOLS.contains(&tool) { exit_code(output) } else { None };
        match exit {
            Some(n) if n != 0 => Outcome::Exit(n),
            _ if error => Outcome::Error,
            _ => Outcome::Ok,
        }
    }

    pub fn failed(self) -> bool {
        self != Outcome::Ok
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Outcome::Ok => f.write_str("ok"),
            Outcome::Exit(n) => write!(f, "exit {n}"),
            Outcome::Error => f.write_str("error"),
        }
    }
}

/// Exit status as harnesses print it near the start of a result (Codex `exit_code`/`Exit code:`,
/// Claude `Exit code N`); later text may be file content, so only the head is searched.
pub fn exit_code(output: &str) -> Option<i64> {
    let head = clip::prefix(output, 400).to_ascii_lowercase();
    ["\"exit_code\":", "exit code:", "exit code", "exited with code"].iter().find_map(|needle| {
        let at = head.find(needle)? + needle.len();
        let num = head[at..].trim_start();
        let end = num
            .char_indices()
            .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && c == '-')))
            .map_or(num.len(), |(i, _)| i);
        num[..end].parse().ok()
    })
}

const ERROR_WORDS: [&str; 10] =
    ["error", "failed", "failure", "fatal", "panic", "exception", "traceback", "denied", "not found", "no such file"];

/// First line that looks like an error, ≤160 B.
pub fn error_line(output: &str) -> Option<String> {
    output
        .lines()
        .find(|l| {
            let l = l.to_ascii_lowercase();
            ERROR_WORDS.iter().any(|w| l.contains(w))
        })
        .map(|l| clip::head(l.trim(), 160))
}
