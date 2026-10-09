//! Opt-in subscription backends (I15, V52): `claude -p` and `codex exec`, run headless once per
//! call. Read tools come from `pair-watcher mcp` over the wake's snapshot; the harness's own tools,
//! hooks and user config stay off, and the Watcher env guard is set so the Worker's hooks no-op
//! (V58). Codex launches never pass `--dangerously-bypass-hook-trust` (C10).

use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::hook::GUARD_ENV;
use crate::mcp;
use crate::models::Usage;
use crate::provider::{Call, Failure, ProviderError, Reply};

const MAX_MESSAGE: usize = 512;
const POLL: Duration = Duration::from_millis(50);

/// The Watcher instructions travel in the wake input; this only replaces Claude Code's own.
const CLAUDE_SYSTEM: &str = "You are a read-only reviewer. Follow the user message exactly. \
Your only tools are the pair-watcher read tools; answer in the format the message asks for.";

/// Codex answers in one schema-checked JSON object: each Watcher output line as a string.
const CODEX_FORMAT: &str = "\n\nAnswer with one JSON object {\"lines\": [...]}: each item is one \
line of the JSONL output above, as a string; an empty list when you have nothing to add.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    ClaudeP,
    CodexExec,
}

impl Kind {
    pub fn program(self) -> &'static str {
        match self {
            Kind::ClaudeP => "claude",
            Kind::CodexExec => "codex",
        }
    }
}

/// A headless backend bound to one session.
pub struct Cli {
    pub kind: Kind,
    /// The harness binary (`claude`, `codex`, or a path).
    pub program: PathBuf,
    /// The `pair-watcher` binary that serves the read tools.
    pub exe: PathBuf,
    /// Session mailbox: holds the snapshot store; scratch files go under `cli/`.
    pub mailbox: PathBuf,
    /// The Worker repo; `None` = no tools (no repo to snapshot).
    pub repo: Option<PathBuf>,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

impl Cli {
    pub fn call(&self, call: &Call) -> Result<Reply, Failure> {
        let fail = |error| Failure { error, usage: Usage::default() };
        let scratch = self.mailbox.join("cli");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&scratch)
            .map_err(|e| fail(ProviderError::Cli(format!("scratch dir: {e}"))))?;
        let tag = format!("{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
        let stats = scratch.join(format!("{tag}.stats.json"));
        let mcp = match (&call.tools, &self.repo) {
            (Some(snap), Some(repo)) => {
                let mut args: Vec<String> = vec!["mcp".into(), "--mailbox".into(), path(&self.mailbox)];
                args.extend(["--repo".into(), path(repo), "--tree".into(), snap.tree.to_owned()]);
                if let Some(head) = snap.head {
                    args.extend(["--head".into(), head.to_owned()]);
                }
                args.extend(["--stats".into(), path(&stats)]);
                Some(Server { command: path(&self.exe), args })
            }
            _ => None,
        };
        let mut cmd = Command::new(&self.program);
        cmd.current_dir(&scratch);
        let result = match self.kind {
            Kind::ClaudeP => {
                cmd.args(claude_args(call.model, mcp.as_ref()));
                run(cmd, call.input, call.deadline).and_then(|out| parse_claude(&out))
            }
            Kind::CodexExec => {
                let schema = scratch.join(format!("{tag}.schema.json"));
                let last = scratch.join(format!("{tag}.last.json"));
                let wrote = std::fs::write(&schema, codex_schema().to_string());
                let result = wrote
                    .map_err(|e| (ProviderError::Cli(format!("schema file: {e}")), Usage::default()))
                    .and_then(|()| {
                        cmd.args(codex_args(call.model, mcp.as_ref(), &schema, &last, &scratch));
                        let input = format!("{}{CODEX_FORMAT}", call.input);
                        run(cmd, &input, call.deadline)
                            .and_then(|out| parse_codex(&out, std::fs::read_to_string(&last).ok()))
                    });
                let _ = std::fs::remove_file(&schema);
                let _ = std::fs::remove_file(&last);
                result
            }
        };
        let tools = mcp::read_stats(&stats);
        let _ = std::fs::remove_file(&stats);
        match result {
            Ok((text, usage, requests)) => Ok(Reply { text, usage, tools, requests }),
            Err((error, usage)) => Err(Failure { error, usage }),
        }
    }
}

/// The read-tools server a headless call starts.
pub struct Server {
    pub command: String,
    pub args: Vec<String>,
}

fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `claude -p`: built-in tools off, slash commands off, hooks off, only our MCP server (I15).
pub fn claude_args(model: &str, mcp: Option<&Server>) -> Vec<String> {
    let mut a: Vec<String> =
        ["-p", "--output-format", "json", "--model", model, "--no-session-persistence"].map(str::to_owned).to_vec();
    a.extend(["--system-prompt".into(), CLAUDE_SYSTEM.into(), "--tools".into(), String::new()]);
    a.extend(["--disable-slash-commands".into(), "--strict-mcp-config".into()]);
    a.extend(["--settings".into(), json!({"disableAllHooks": true}).to_string()]);
    if let Some(s) = mcp {
        let config = json!({"mcpServers": {mcp::SERVER: {"command": s.command, "args": s.args}}});
        a.extend(["--mcp-config".into(), config.to_string()]);
        a.extend(["--allowedTools".into(), format!("mcp__{}", mcp::SERVER)]);
    }
    a
}

/// `codex exec`: read-only sandbox, user config and rules ignored, schema-checked answer (I15).
pub fn codex_args(model: &str, mcp: Option<&Server>, schema: &Path, last: &Path, cwd: &Path) -> Vec<String> {
    let mut a: Vec<String> = [
        "exec",
        "--sandbox",
        "read-only",
        "--ignore-user-config",
        "--ignore-rules",
        "--ephemeral",
        "--skip-git-repo-check",
        "--json",
        "--color",
        "never",
    ]
    .map(str::to_owned)
    .to_vec();
    a.extend(["--output-schema".into(), path(schema), "-o".into(), path(last)]);
    a.extend(["-m".into(), model.into(), "-C".into(), path(cwd)]);
    if let Some(s) = mcp {
        // `-c` values parse as TOML; JSON strings and string arrays are valid TOML.
        let key = format!("mcp_servers.{}", mcp::SERVER);
        a.extend(["-c".into(), format!("{key}.command={}", json!(s.command))]);
        a.extend(["-c".into(), format!("{key}.args={}", json!(s.args))]);
        // Read-only tools of our own: no approval prompt (`codex exec` runs approval `never`).
        a.extend(["-c".into(), format!("{key}.default_tools_approval_mode=\"approve\"")]);
    }
    a.push("-".into());
    a
}

/// Strict-mode compatible: every property required, no extras.
pub fn codex_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"lines": {"type": "array", "items": {"type": "string"}}},
        "required": ["lines"],
        "additionalProperties": false
    })
}

pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

type Parsed = Result<(String, Usage, usize), (ProviderError, Usage)>;

/// Runs `cmd` with `input` on stdin under the env guard; killed at `deadline` (V87).
fn run(mut cmd: Command, input: &str, deadline: Option<Instant>) -> Result<Output, (ProviderError, Usage)> {
    let err = |e| (e, Usage::default());
    let program = cmd.get_program().to_string_lossy().into_owned();
    cmd.env(GUARD_ENV, "1").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| {
        err(ProviderError::Cli(if e.kind() == std::io::ErrorKind::NotFound {
            format!("{program}: not found (install it or set backend = \"api\")")
        } else {
            format!("{program}: {e}")
        }))
    })?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = input.to_owned();
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_string(&mut s);
            }
            s
        })
    };
    let out = reader(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let errs = reader(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if deadline.is_some_and(|d| Instant::now() >= d) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err(ProviderError::Timeout));
            }
            Ok(None) => thread::sleep(POLL),
            Err(e) => return Err(err(ProviderError::Cli(format!("{program}: {e}")))),
        }
    };
    let _ = writer.join();
    Ok(Output { status, stdout: out.join().unwrap_or_default(), stderr: errs.join().unwrap_or_default() })
}

fn message(s: &str) -> String {
    crate::clip::head(s.trim(), MAX_MESSAGE)
}

fn n(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

/// `claude -p --output-format json`: one result object.
pub fn parse_claude(out: &Output) -> Parsed {
    let Ok(v) = serde_json::from_str::<Value>(out.stdout.trim()) else {
        let why = if out.stderr.trim().is_empty() { &out.stdout } else { &out.stderr };
        return Err((
            ProviderError::Cli(format!("claude -p exited {}: {}", out.status, message(why))),
            Usage::default(),
        ));
    };
    let u = &v["usage"];
    let usage = Usage {
        input: n(u, "input_tokens"),
        cached: n(u, "cache_read_input_tokens"),
        cache_write: n(u, "cache_creation_input_tokens"),
        output: n(u, "output_tokens"),
    };
    let text = v["result"].as_str().unwrap_or_default().to_owned();
    if v["is_error"].as_bool().unwrap_or(false) || !out.status.success() {
        let why = if text.is_empty() { v["subtype"].as_str().unwrap_or("error").to_owned() } else { text };
        return Err((ProviderError::Cli(format!("claude -p: {}", message(&why))), usage));
    }
    if v.get("result").is_none() {
        return Err((ProviderError::Protocol("no `result`".into()), usage));
    }
    Ok((text, usage, n(&v, "num_turns").max(1) as usize))
}

/// `codex exec --json`: event lines on stdout; the answer is in the `-o` file.
pub fn parse_codex(out: &Output, last: Option<String>) -> Parsed {
    let mut usage = Usage::default();
    let mut turns = 0;
    let mut error = None;
    for v in out.stdout.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        match v["type"].as_str().unwrap_or_default() {
            "turn.completed" => {
                let u = &v["usage"];
                let cached = n(u, "cached_input_tokens");
                usage += Usage {
                    input: n(u, "input_tokens").saturating_sub(cached),
                    cached,
                    cache_write: 0,
                    output: n(u, "output_tokens"),
                };
                turns += 1;
            }
            "turn.failed" => error = v["error"]["message"].as_str().map(str::to_owned),
            "error" => error = error.or_else(|| v["message"].as_str().map(str::to_owned)),
            _ => {}
        }
    }
    if let Some(e) = error {
        return Err((ProviderError::Cli(format!("codex exec: {}", message(&e))), usage));
    }
    if !out.status.success() {
        return Err((ProviderError::Cli(format!("codex exec exited {}: {}", out.status, message(&out.stderr))), usage));
    }
    let answer: Value = last
        .as_deref()
        .and_then(|s| serde_json::from_str(s.trim()).ok())
        .ok_or_else(|| (ProviderError::Protocol("no schema answer".into()), usage))?;
    let lines = answer["lines"].as_array().ok_or_else(|| (ProviderError::Protocol("no `lines`".into()), usage))?;
    let text = lines.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n");
    Ok((text, usage, turns.max(1)))
}
