//! §T20 opt-in subscription backends (I15, V52, C10): `pair-watcher mcp` serves the read tools
//! over a wake's snapshot under the V8 allowance; `claude -p` and `codex exec` run headless with
//! their own tools, hooks and user config off, under the env guard, killed at the wake clock.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pair_watcher::config::{self, Sources};
use pair_watcher::fail;
use pair_watcher::headless::{Cli, Kind};
use pair_watcher::home::Home;
use pair_watcher::models::Vendor;
use pair_watcher::problem::Problem;
use pair_watcher::provider::{Call, ProviderError};
use pair_watcher::repo::Repo;
use pair_watcher::snapshot::Store;
use pair_watcher::status::Class;
use pair_watcher::tools::{MAX_CALLS, Snapshot};
use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_pair-watcher");

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

/// A repo whose snapshot has `a.txt` = "snap"; the live tree then moves on to "live".
struct World {
    dir: TempDir,
    root: PathBuf,
    mailbox: PathBuf,
    store: Store,
    tree: String,
    head: Option<String>,
}

impl World {
    fn new() -> World {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("work");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "committed\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "first commit"]);
        fs::write(root.join("a.txt"), "snap\n").unwrap();
        let mailbox = dir.path().join("mailbox");
        fs::create_dir_all(&mailbox).unwrap();
        let store = Store::open(&mailbox, &Repo { root: root.clone(), root_commit: None }).unwrap();
        let tree = store.take().unwrap();
        let head = store.head();
        fs::write(root.join("a.txt"), "live\n").unwrap();
        World { dir, root, mailbox, store, tree, head }
    }

    fn snap(&self) -> Snapshot<'_> {
        Snapshot { store: &self.store, tree: &self.tree, head: self.head.as_deref() }
    }

    /// An executable fake harness: `body` is a sh script.
    fn fake(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn cli(&self, kind: Kind, program: PathBuf) -> Cli {
        Cli { kind, program, exe: PathBuf::from(BIN), mailbox: self.mailbox.clone(), repo: Some(self.root.clone()) }
    }

    fn out(&self, name: &str) -> String {
        fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
    }
}

/// Records argv (one per line), stdin and the env guard into `<dir>/{args,stdin,guard}`.
fn recorder(dir: &Path) -> String {
    let d = dir.display();
    format!(
        "printf '%s\\n' \"$@\" > '{d}/args'\ncat > '{d}/stdin'\nprintf '%s' \"$PAIR_WATCHER_GUARD\" > '{d}/guard'\n"
    )
}

fn call<'a>(input: &'a str, tools: Option<Snapshot<'a>>) -> Call<'a> {
    Call { model: "claude-sonnet-5-5", vendor: Vendor::Anthropic, input, tools, deadline: None, fallback: false }
}

struct Mcp {
    child: std::process::Child,
    out: BufReader<std::process::ChildStdout>,
    next: u64,
}

impl Mcp {
    fn start(w: &World, stats: &Path) -> Mcp {
        let mut cmd = Command::new(BIN);
        cmd.args(["mcp", "--mailbox"]).arg(&w.mailbox).arg("--repo").arg(&w.root).args(["--tree", &w.tree]);
        if let Some(h) = &w.head {
            cmd.args(["--head", h]);
        }
        let mut child = cmd.arg("--stats").arg(stats).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Mcp { child, out, next: 1 }
    }

    fn send(&mut self, v: Value) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{v}").unwrap();
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let mut line = String::new();
        self.out.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], id, "replies in order, notifications unanswered");
        v
    }

    fn tool(&mut self, name: &str, args: Value) -> String {
        let r = self.rpc("tools/call", json!({"name": name, "arguments": args}));
        r["result"]["content"][0]["text"].as_str().unwrap().to_owned()
    }
}

/// I15, V8: the MCP server serves the four read tools from the snapshot, never the live tree,
/// spends the wake's allowance, and reports it in the stats file.
#[test]
fn mcp_serves_snapshot_tools_under_allowance() {
    let w = World::new();
    let stats = w.dir.path().join("stats.json");
    let mut m = Mcp::start(&w, &stats);
    let init = m.rpc("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}}));
    assert_eq!(init["result"]["serverInfo"]["name"], "pair-watcher");
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let list = m.rpc("tools/list", json!({}));
    let names: Vec<&str> =
        list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["read_file", "grep", "git_log", "git_show"]);
    assert!(list["result"]["tools"][0]["inputSchema"]["properties"]["path"].is_object());

    assert_eq!(m.tool("read_file", json!({"path": "a.txt"})), "1\tsnap\n");
    assert!(m.tool("git_log", json!({})).contains("first commit"));
    assert!(m.tool("git_show", json!({"rev": "HEAD:a.txt"})).contains("committed"));
    assert_eq!(pair_watcher::mcp::read_stats(&stats).calls, 3);
    for _ in 3..MAX_CALLS {
        m.tool("grep", json!({"pattern": "snap"}));
    }
    assert!(m.tool("read_file", json!({"path": "a.txt"})).contains("allowance is spent"));
    let unknown = m.rpc("resources/list", json!({}));
    assert_eq!(unknown["error"]["code"], -32601);
    drop(m.child.stdin.take());
    assert!(m.child.wait().unwrap().success());
    let a = pair_watcher::mcp::read_stats(&stats);
    assert_eq!(a.calls, MAX_CALLS);
    assert_eq!(fs::read_to_string(w.root.join("a.txt")).unwrap(), "live\n", "never writes");
}

/// I15: `claude -p` with built-ins off (`--tools ""`), hooks off, only our MCP server; input on
/// stdin, env guard set; usage and the read-tool count come back.
#[test]
fn claude_p_args_input_and_usage() {
    let w = World::new();
    let result = json!({"type": "result", "subtype": "success", "is_error": false, "result": "{\"op\":\"none\"}",
        "num_turns": 3, "usage": {"input_tokens": 900, "cache_read_input_tokens": 50, "cache_creation_input_tokens": 10, "output_tokens": 70}});
    let fake = w.fake("claude", &format!("{}echo '{result}'\n", recorder(w.dir.path())));
    let reply = w.cli(Kind::ClaudeP, fake).call(&call("WAKE INPUT", Some(w.snap()))).unwrap();
    assert_eq!(reply.text, "{\"op\":\"none\"}");
    assert_eq!((reply.usage.input, reply.usage.cached, reply.usage.cache_write, reply.usage.output), (900, 50, 10, 70));
    assert_eq!(reply.requests, 3);
    assert_eq!(w.out("stdin"), "WAKE INPUT");
    assert_eq!(w.out("guard"), "1");
    let args: Vec<String> = w.out("args").lines().map(str::to_owned).collect();
    let after = |flag: &str| args[args.iter().position(|a| a == flag).unwrap_or_else(|| panic!("{flag}")) + 1].clone();
    assert_eq!(args[0], "-p");
    assert_eq!(after("--output-format"), "json");
    assert_eq!(after("--model"), "claude-sonnet-5-5");
    assert_eq!(after("--tools"), "", "built-in tools off");
    assert_eq!(serde_json::from_str::<Value>(&after("--settings")).unwrap(), json!({"disableAllHooks": true}));
    for flag in ["--strict-mcp-config", "--no-session-persistence", "--disable-slash-commands"] {
        assert!(args.iter().any(|a| a == flag), "{flag}");
    }
    assert_eq!(after("--allowedTools"), "mcp__pair-watcher");
    let mcp: Value = serde_json::from_str(&after("--mcp-config")).unwrap();
    let server = &mcp["mcpServers"]["pair-watcher"];
    assert_eq!(server["command"], BIN);
    let sargs: Vec<&str> = server["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
    assert_eq!(sargs[0], "mcp");
    assert!(sargs.windows(2).any(|p| p == ["--tree", w.tree.as_str()]));
    assert!(sargs.contains(&"--stats"));
    assert!(fs::read_dir(w.mailbox.join("cli")).unwrap().next().is_none(), "scratch files removed");
}

/// V11: a blocked wake offers no tools: no MCP server at all.
#[test]
fn claude_p_blocked_call_has_no_mcp() {
    let w = World::new();
    let result = json!({"type": "result", "is_error": false, "result": "x", "usage": {}});
    let fake = w.fake("claude", &format!("{}echo '{result}'\n", recorder(w.dir.path())));
    let reply = w.cli(Kind::ClaudeP, fake).call(&call("blocked", None)).unwrap();
    assert_eq!((reply.tools.calls, reply.requests), (0, 1));
    let args = w.out("args");
    assert!(!args.contains("--mcp-config") && !args.contains("--allowedTools"));
    assert!(args.contains("--strict-mcp-config"));
}

/// V86: a spent subscription or lost login is a Key failure; other errors are Service.
#[test]
fn claude_p_errors_classify() {
    let w = World::new();
    let limit = json!({"type": "result", "subtype": "success", "is_error": true, "result": "Claude usage limit reached",
        "usage": {"input_tokens": 5, "output_tokens": 0}});
    let fake = w.fake("claude", &format!("cat > /dev/null\necho '{limit}'\n"));
    let f = w.cli(Kind::ClaudeP, fake).call(&call("x", None)).unwrap_err();
    assert!(matches!(&f.error, ProviderError::Cli(m) if m.contains("usage limit")));
    assert_eq!(f.usage.input, 5, "a failed call still spent");
    assert_eq!(fail::classify(&f.error), Class::Key);

    let fake = w.fake("claude2", "cat > /dev/null\necho 'boom: overloaded' >&2\nexit 1\n");
    let f = w.cli(Kind::ClaudeP, fake).call(&call("x", None)).unwrap_err();
    assert!(matches!(&f.error, ProviderError::Cli(m) if m.contains("overloaded")), "{:?}", f.error);
    assert_eq!(fail::classify(&f.error), Class::Service);

    let missing = w.dir.path().join("no-such-claude");
    let f = w.cli(Kind::ClaudeP, missing).call(&call("x", None)).unwrap_err();
    assert!(matches!(&f.error, ProviderError::Cli(m) if m.contains("not found")));
    assert_eq!(fail::classify(&f.error), Class::Key);
}

/// V87: the wake clock kills a headless call that runs past it.
#[test]
fn deadline_kills_the_cli() {
    let w = World::new();
    let fake = w.fake("claude", "sleep 20\n");
    let started = Instant::now();
    let mut c = call("x", None);
    c.deadline = Some(Instant::now() + Duration::from_millis(300));
    let f = w.cli(Kind::ClaudeP, fake).call(&c).unwrap_err();
    assert_eq!(f.error, ProviderError::Timeout(String::new()));
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// V87: a killed call keeps the tail of what it printed, for diagnosis (live 2026-10-09: a
/// `claude -p` hung to the 180 s clock left no trace), even while a grandchild holds the pipes.
#[test]
fn deadline_keeps_output_tail() {
    let w = World::new();
    let noise = "x".repeat(2_000);
    let fake =
        w.fake("claude", &format!("echo '{noise}' >&2\necho 'waiting on MCP server' >&2\necho partial\nsleep 20\n"));
    let started = Instant::now();
    let mut c = call("x", None);
    c.deadline = Some(Instant::now() + Duration::from_millis(500));
    let f = w.cli(Kind::ClaudeP, fake).call(&c).unwrap_err();
    let ProviderError::Timeout(tail) = &f.error else { panic!("{:?}", f.error) };
    assert!(tail.contains("stderr …") && tail.ends_with("waiting on MCP server | stdout …partial"), "{tail}");
    assert!(tail.len() < 1_000, "clipped: {}", tail.len());
    assert!(f.error.to_string().starts_with("timed out; last output: "));
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// I15, C10: `codex exec` read-only sandbox, user config and rules ignored, `--output-schema`,
/// MCP server via `-c`, never `--dangerously-bypass-hook-trust`; the schema answer's lines are
/// the Watcher output.
#[test]
fn codex_exec_args_schema_and_answer() {
    let w = World::new();
    let d = w.dir.path().display();
    let events = [
        json!({"type": "thread.started", "thread_id": "t"}),
        json!({"type": "turn.completed", "usage": {"input_tokens": 1200, "cached_input_tokens": 200, "output_tokens": 80}}),
    ];
    let answer = json!({"lines": ["{\"op\":\"none\"}", "{\"op\":\"x\"}"]});
    let script = format!(
        "{rec}prev=''\nfor a in \"$@\"; do\n  case \"$prev\" in\n    -o) last=\"$a\";;\n    --output-schema) cp \"$a\" '{d}/schema';;\n  esac\n  prev=\"$a\"\ndone\nprintf '%s' '{answer}' > \"$last\"\necho '{e0}'\necho '{e1}'\n",
        rec = recorder(w.dir.path()),
        e0 = events[0],
        e1 = events[1],
    );
    let fake = w.fake("codex", &script);
    let mut c = call("WAKE INPUT", Some(w.snap()));
    c.model = "gpt-6.1-sol";
    c.vendor = Vendor::Openai;
    let reply = w.cli(Kind::CodexExec, fake).call(&c).unwrap();
    assert_eq!(reply.text, "{\"op\":\"none\"}\n{\"op\":\"x\"}");
    assert_eq!((reply.usage.input, reply.usage.cached, reply.usage.output), (1000, 200, 80));
    assert!(w.out("stdin").starts_with("WAKE INPUT\n\nAnswer with one JSON object {\"lines\""));
    assert_eq!(w.out("guard"), "1");
    let schema: Value = serde_json::from_str(&w.out("schema")).unwrap();
    assert_eq!(schema["required"], json!(["lines"]));
    assert_eq!(schema["additionalProperties"], false);
    let args: Vec<String> = w.out("args").lines().map(str::to_owned).collect();
    let after = |flag: &str| args[args.iter().position(|a| a == flag).unwrap_or_else(|| panic!("{flag}")) + 1].clone();
    assert_eq!(args[0], "exec");
    assert_eq!(after("--sandbox"), "read-only");
    assert_eq!(after("-m"), "gpt-6.1-sol");
    for flag in ["--ignore-user-config", "--ignore-rules", "--ephemeral", "--json"] {
        assert!(args.iter().any(|a| a == flag), "{flag}");
    }
    assert!(!args.iter().any(|a| a.starts_with("--dangerously")), "C10");
    assert_eq!(args.last().unwrap(), "-", "prompt on stdin");
    let configs: Vec<&String> = args.windows(2).filter(|p| p[0] == "-c").map(|p| &p[1]).collect();
    assert_eq!(configs[0], &format!("mcp_servers.pair-watcher.command=\"{BIN}\""));
    assert!(configs[1].starts_with("mcp_servers.pair-watcher.args=[\"mcp\""));
    assert_eq!(
        configs[2], "mcp_servers.pair-watcher.default_tools_approval_mode=\"approve\"",
        "exec approval is never"
    );
    assert!(fs::read_dir(w.mailbox.join("cli")).unwrap().next().is_none(), "scratch files removed");
}

#[test]
fn codex_exec_turn_failed_is_an_error() {
    let w = World::new();
    let failed = json!({"type": "turn.failed", "error": {"message": "You have hit your usage limit."}});
    let fake = w.fake("codex", &format!("cat > /dev/null\necho '{failed}'\nexit 1\n"));
    let f = w.cli(Kind::CodexExec, fake).call(&call("x", None)).unwrap_err();
    assert!(matches!(&f.error, ProviderError::Cli(m) if m.contains("usage limit")));
    assert_eq!(fail::classify(&f.error), Class::Key);
}

/// C10: a pi backend is refused, saying why.
#[test]
fn pi_backend_is_refused() {
    let dir = TempDir::new().unwrap();
    let home = Home::new(dir.path());
    let flags = ["backend=pi".to_owned()];
    let r = config::resolve(&Sources { home: &home, repo: None, env: &|_| None, flags: &flags }).unwrap();
    assert_eq!(r.get("backend").and_then(|v| v.as_str()), Some("api"));
    let why = r
        .problems
        .iter()
        .find_map(|p| match p {
            Problem::BadValue { key: "backend", expected, .. } => Some(expected.clone()),
            _ => None,
        })
        .expect("bad value");
    assert!(why.contains("subscription login is refused"), "{why}");
}
