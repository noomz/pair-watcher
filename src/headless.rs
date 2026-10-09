//! Opt-in subscription backends (I15, V52): `claude -p` and `codex exec`, run headless once per
//! call. Read tools come from `pair-watcher mcp` over the wake's snapshot; the harness's own tools,
//! hooks and user config stay off, and the Watcher env guard is set so the Worker's hooks no-op
//! (V58). Codex launches never pass `--dangerously-bypass-hook-trust` (C10). `codex exec` still
//! routes like the user's Codex: their `model_provider` comes along, its secrets off argv (V110).

use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::hook::GUARD_ENV;
use crate::mcp;
use crate::models::Usage;
use crate::provider::{Call, Failure, ProviderError, Reply};

const MAX_MESSAGE: usize = 512;
const POLL: Duration = Duration::from_millis(50);
/// Output kept per stream when the deadline kills a call, for diagnosis (V87).
const TIMEOUT_TAIL: usize = 400;
/// How long a killed call's readers get to take in what was already written.
const TIMEOUT_DRAIN: Duration = Duration::from_millis(500);

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

    /// The config `backend` value.
    pub fn backend(self) -> &'static str {
        match self {
            Kind::ClaudeP => "claude-p",
            Kind::CodexExec => "codex-exec",
        }
    }
}

/// The user's Codex `model_provider`, carried past `--ignore-user-config` (V110): `-c` overrides
/// for argv, and the secret values they name by env var, for the child's env only.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Routing {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Env var prefix for provider secrets moved off argv.
pub const SECRET_ENV: &str = "PAIR_WATCHER_CODEX_SECRET";

impl Routing {
    pub fn for_kind(kind: Kind, env: &dyn Fn(&str) -> Option<String>) -> Routing {
        match kind {
            Kind::ClaudeP => Routing::default(),
            Kind::CodexExec => Routing::codex(env),
        }
    }

    /// `$CODEX_HOME/config.toml` (default `~/.codex`); unreadable → the built-in provider.
    pub fn codex(env: &dyn Fn(&str) -> Option<String>) -> Routing {
        let home = env("CODEX_HOME")
            .filter(|h| !h.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| env("HOME").map(|h| Path::new(&h).join(".codex")));
        home.and_then(|h| std::fs::read_to_string(h.join("config.toml")).ok())
            .map_or_else(Routing::default, |text| Routing::from_config(&text))
    }

    /// Only the top-level `model_provider` and its `[model_providers.<name>]` table. Literal
    /// `http_headers` become `env_http_headers`, `experimental_bearer_token` becomes `env_key`.
    pub fn from_config(text: &str) -> Routing {
        let Ok(config) = text.parse::<toml::Table>() else { return Routing::default() };
        let Some(name) = config.get("model_provider").and_then(toml::Value::as_str) else {
            return Routing::default();
        };
        let mut args = vec!["-c".to_owned(), format!("model_provider={}", toml::Value::from(name))];
        let table = config.get("model_providers").and_then(|t| t.get(name)).and_then(toml::Value::as_table);
        let mut env = Vec::new();
        if let Some(table) = table {
            // `-c` keys split on dots: a name that needs quoting cannot be addressed.
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                return Routing::default();
            }
            let mut table = table.clone();
            let mut secret = |value: toml::Value| {
                let var = format!("{SECRET_ENV}_{}", env.len());
                env.push((var.clone(), value.as_str().map_or_else(|| value.to_string(), str::to_owned)));
                toml::Value::from(var)
            };
            if let Some(toml::Value::Table(headers)) = table.remove("http_headers") {
                let mut named = match table.remove("env_http_headers") {
                    Some(toml::Value::Table(t)) => t,
                    _ => toml::Table::new(),
                };
                for (header, value) in headers {
                    named.insert(header, secret(value));
                }
                table.insert("env_http_headers".into(), toml::Value::Table(named));
            }
            if let Some(token) = table.remove("experimental_bearer_token") {
                table.insert("env_key".into(), secret(token));
            }
            args.extend(["-c".into(), format!("model_providers.{name}={}", toml::Value::Table(table))]);
        }
        Routing { args, env }
    }
}

/// A headless backend bound to one session.
#[derive(Clone)]
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
    /// `codex exec` only: the user's provider (V110).
    pub routing: Routing,
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
                        let mut args = codex_args(call.model, mcp.as_ref(), &schema, &last, &scratch);
                        let stdin = args.pop();
                        args.extend(self.routing.args.iter().cloned().chain(stdin));
                        cmd.args(args).envs(self.routing.env.iter().map(|(k, v)| (k, v)));
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
    let out = Pipe::read(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let errs = Pipe::read(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if deadline.is_some_and(|d| Instant::now() >= d) => {
                let _ = child.kill();
                let _ = child.wait();
                // A grandchild may hold a pipe open: take what arrived, never wait for EOF.
                let drained = Instant::now() + TIMEOUT_DRAIN;
                while !(out.done() && errs.done()) && Instant::now() < drained {
                    thread::sleep(POLL);
                }
                let tail = [("stderr", errs.text()), ("stdout", out.text())]
                    .into_iter()
                    .filter(|(_, s)| !s.trim().is_empty())
                    .map(|(name, s)| format!("{name} …{}", crate::clip::suffix(s.trim_end(), TIMEOUT_TAIL)))
                    .collect::<Vec<_>>()
                    .join(" | ");
                return Err(err(ProviderError::Timeout(tail)));
            }
            Ok(None) => thread::sleep(POLL),
            Err(e) => return Err(err(ProviderError::Cli(format!("{program}: {e}")))),
        }
    };
    let _ = writer.join();
    Ok(Output { status, stdout: out.finish(), stderr: errs.finish() })
}

/// One child stream read on its own thread into a buffer readable before EOF.
struct Pipe {
    buf: Arc<Mutex<Vec<u8>>>,
    reader: thread::JoinHandle<()>,
}

impl Pipe {
    fn read(pipe: Option<Box<dyn Read + Send>>) -> Pipe {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let into = Arc::clone(&buf);
        let reader = thread::spawn(move || {
            let Some(mut p) = pipe else { return };
            let mut chunk = [0; 8192];
            while let Ok(n @ 1..) = p.read(&mut chunk) {
                into.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(&chunk[..n]);
            }
        });
        Pipe { buf, reader }
    }

    fn done(&self) -> bool {
        self.reader.is_finished()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    }

    /// Everything, once the stream closes.
    fn finish(self) -> String {
        let Pipe { buf, reader } = self;
        let _ = reader.join();
        String::from_utf8_lossy(&buf.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    }
}

/// CLI auth failures that name their cause (V111). A failed call only: a reply may quote these.
const AUTH_CAUSES: [&str; 6] = [
    "refresh_token_expired",
    "log in again",
    "login again",
    "sign in again",
    "oauth session expired",
    "oauth token has expired",
];

/// A failed call whose stderr or events name an auth cause: the Key reason says so, never the
/// side error that came first (B7).
fn login_expired(kind: Kind, out: &Output) -> Option<ProviderError> {
    let all = format!("{}\n{}", out.stdout, out.stderr).to_ascii_lowercase();
    AUTH_CAUSES
        .iter()
        .any(|c| all.contains(c))
        .then(|| ProviderError::Cli(format!("{} login expired — re-login, then /pw primary", kind.backend())))
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
        if let Some(e) = login_expired(Kind::ClaudeP, out) {
            return Err((e, Usage::default()));
        }
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
        if let Some(e) = login_expired(Kind::ClaudeP, out) {
            return Err((e, usage));
        }
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
    if (error.is_some() || !out.status.success())
        && let Some(e) = login_expired(Kind::CodexExec, out)
    {
        return Err((e, usage));
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
