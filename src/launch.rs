//! Per-session Worker wiring (C3): Claude `--settings`, Codex `-c hooks.*` + one inline
//! `hooks.state` trust table (V57), pi `-e`. User config is never touched.

use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use toml::{Table, Value as Toml};

use crate::harness::Harness;
use crate::hook::shell_line;

/// Hooks reported through the shim (I5, V56). `SessionStart` carries the transcript path and the
/// Worker session id, including the new ones after Claude `/clear` or a resume.
pub const CLAUDE_EVENTS: [&str; 3] = ["SessionStart", "Stop", "PermissionRequest"];
pub const CODEX_EVENTS: [&str; 5] = ["SessionStart", "Stop", "PermissionRequest", "PreToolUse", "PostToolUse"];

const HOOK_TIMEOUT_S: i64 = 10;

/// Also runs untrusted project hooks from a cloned repo; never passed, never accepted (C10).
pub const CODEX_BYPASS_FLAG: &str = "--dangerously-bypass-hook-trust";

/// The Worker's final argv: our flags first, then the user's.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl Plan {
    pub fn new(worker: &[OsString], ours: Vec<String>) -> Plan {
        let mut args: Vec<OsString> = ours.into_iter().map(OsString::from).collect();
        args.extend(worker[1..].iter().cloned());
        Plan { program: worker[0].clone(), args }
    }
}

/// A reason `run` refuses this Worker command line.
pub fn refusal(harness: Harness, worker: &[OsString]) -> Option<String> {
    let bypass = worker[1..].iter().any(|a| a == CODEX_BYPASS_FLAG);
    (harness == Harness::Codex && bypass)
        .then(|| format!("refusing {CODEX_BYPASS_FLAG}: it also runs untrusted project hooks"))
}

/// `<shim> <event>`: no version or Cellar path, so Codex trust survives upgrades (V58).
pub fn hook_command(shim: &Path, event: &str) -> String {
    shell_line(&[&shim.to_string_lossy(), event])
}

fn hook_group(shim: &Path, event: &str) -> Toml {
    let mut hook = Table::new();
    hook.insert("type".into(), "command".into());
    hook.insert("command".into(), hook_command(shim, event).into());
    hook.insert("timeout".into(), HOOK_TIMEOUT_S.into());
    let mut group = Table::new();
    group.insert("hooks".into(), Toml::Array(vec![hook.into()]));
    Toml::Array(vec![group.into()])
}

/// Contents of the `--settings` file.
pub fn claude_settings(shim: &Path) -> Value {
    let hooks: serde_json::Map<String, Value> = CLAUDE_EVENTS
        .iter()
        .map(|ev| {
            let entry =
                json!([{"hooks": [{"type": "command", "command": hook_command(shim, ev), "timeout": HOOK_TIMEOUT_S}]}]);
            ((*ev).to_owned(), entry)
        })
        .collect();
    json!({ "hooks": hooks })
}

pub fn claude_args(settings_file: &Path) -> Vec<String> {
    vec!["--settings".into(), settings_file.to_string_lossy().into_owned()]
}

/// `-c hooks.<Event>=[…]` values, one per event.
pub fn codex_hook_values(shim: &Path) -> Vec<String> {
    CODEX_EVENTS.iter().map(|ev| format!("hooks.{ev}={}", hook_group(shim, ev))).collect()
}

/// The single `hooks.state={…}` value covering every key: a second `-c hooks.state=` replaces
/// the first, and the dotted `hooks.state."<key>".trusted_hash` form is rejected (V57).
pub fn codex_trust_value(trust: &[(String, String)]) -> String {
    let mut table = Table::new();
    for (key, hash) in trust {
        let mut entry = Table::new();
        entry.insert("trusted_hash".into(), hash.clone().into());
        table.insert(key.clone(), entry.into());
    }
    format!("hooks.state={}", Toml::Table(table))
}

pub fn codex_args(hook_values: &[String], trust_value: &str) -> Vec<String> {
    hook_values
        .iter()
        .chain(std::iter::once(&trust_value.to_owned()))
        .flat_map(|v| ["-c".to_owned(), v.clone()])
        .collect()
}

pub fn pi_args(extension: &Path) -> Vec<String> {
    vec!["-e".into(), extension.to_string_lossy().into_owned()]
}

/// One hook as `codex app-server` `hooks/list` reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct ListedHook {
    pub key: String,
    pub source: String,
    pub trust_status: String,
    pub current_hash: String,
    pub command: String,
}

/// Trust for our session hooks: called with only our `-c hooks.*` flags, so every
/// `sessionFlags` hook is ours. Hashes come from Codex, never a reimplemented normalisation.
pub fn codex_trust(listed: &[ListedHook]) -> Vec<(String, String)> {
    listed.iter().filter(|h| h.source == "sessionFlags").map(|h| (h.key.clone(), h.current_hash.clone())).collect()
}

/// `codex app-server <flags>` over stdio JSON-RPC: `initialize` → `initialized` → `hooks/list`.
pub fn codex_hooks_list(codex: &OsStr, flags: &[String], cwd: &Path, timeout: Duration) -> io::Result<Vec<ListedHook>> {
    let mut child = Command::new(codex)
        .arg("app-server")
        .args(flags.iter().flat_map(|f| ["-c", f.as_str()]))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + timeout;
    let result = (|| {
        let client = json!({"name": "pair-watcher", "title": null, "version": env!("CARGO_PKG_VERSION")});
        let caps = json!({"experimentalApi": true, "requestAttestation": false});
        send(
            &mut stdin,
            &json!({"id": 1, "method": "initialize", "params": {"clientInfo": client, "capabilities": caps}}),
        )?;
        response(&rx, 1, deadline)?;
        send(&mut stdin, &json!({"method": "initialized"}))?;
        let cwd = cwd.to_string_lossy();
        send(&mut stdin, &json!({"id": 2, "method": "hooks/list", "params": {"cwds": [cwd]}}))?;
        parse_hooks_list(&response(&rx, 2, deadline)?)
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn send(stdin: &mut impl Write, msg: &Value) -> io::Result<()> {
    let mut line = serde_json::to_vec(msg)?;
    line.push(b'\n');
    stdin.write_all(&line)?;
    stdin.flush()
}

fn response(rx: &mpsc::Receiver<io::Result<String>>, id: u64, deadline: Instant) -> io::Result<Value> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = match rx.recv_timeout(left) {
            Ok(line) => line?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "codex app-server timed out"));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "codex app-server exited"));
            }
        };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        if msg.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(err) = msg.get("error") {
            return Err(io::Error::other(format!("codex app-server: {err}")));
        }
        return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
    }
}

fn parse_hooks_list(result: &Value) -> io::Result<Vec<ListedHook>> {
    let field = |h: &Value, k: &str| h.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let data = result.get("data").and_then(Value::as_array).ok_or_else(|| io::Error::other("hooks/list: no data"))?;
    Ok(data
        .iter()
        .flat_map(|entry| entry.get("hooks").and_then(Value::as_array).into_iter().flatten())
        .map(|h| ListedHook {
            key: field(h, "key"),
            source: field(h, "source"),
            trust_status: field(h, "trustStatus"),
            current_hash: field(h, "currentHash"),
            command: field(h, "command"),
        })
        .collect())
}
