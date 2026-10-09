use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use pair_watcher::credentials::Keys;
use pair_watcher::home::Home;
use pair_watcher::models::{Usage, Vendor};
use pair_watcher::provider::{Api, Call, Failure, ProviderError};
use pair_watcher::repo::Repo;
use pair_watcher::snapshot::Store;
use pair_watcher::tools::{Allowance, MAX_CALLS, MAX_READ, Snapshot};
use serde_json::{Value, json};
use tempfile::TempDir;

const KEY: &str = "sk-test-secret";

struct Request {
    headers: String,
    body: Value,
}

/// Serves `responses` in order, one connection each, recording every request.
struct Server {
    url: String,
    seen: Arc<Mutex<Vec<Request>>>,
    handle: JoinHandle<()>,
}

impl Server {
    fn start(responses: Vec<(u16, &'static str, Value)>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let handle = thread::spawn(move || {
            for (status, extra, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line.to_ascii_lowercase());
                }
                let len: usize = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map_or(0, |v| v.trim().parse().unwrap());
                let mut buf = vec![0; len];
                reader.read_exact(&mut buf).unwrap();
                log.lock().unwrap().push(Request { headers, body: serde_json::from_slice(&buf).unwrap() });
                let text = body.to_string();
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                )
                .unwrap();
            }
        });
        Server { url, seen, handle }
    }

    fn finish(self) -> Vec<Request> {
        self.handle.join().unwrap();
        Arc::try_unwrap(self.seen).ok().unwrap().into_inner().unwrap()
    }
}

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

struct World {
    _dir: TempDir,
    root: PathBuf,
    home: Home,
    store: Store,
    tree: String,
    head: Option<String>,
}

impl World {
    /// Commit `a.txt` = "committed"; snapshot with `a.txt` = "snap" + a Worker-excluded
    /// `secret.env`; afterwards the live tree moves on to "live".
    fn new() -> World {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("work");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "committed\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "first commit"]);
        fs::write(root.join("a.txt"), "snap\n").unwrap();
        fs::write(root.join("big.txt"), "x".repeat(30 * 1024)).unwrap();
        fs::write(root.join(".git/info/exclude"), "secret.env\n").unwrap();
        fs::write(root.join("secret.env"), "TOKEN=hunter2\n").unwrap();
        let mailbox = dir.path().join("mailbox");
        fs::create_dir_all(&mailbox).unwrap();
        let store = Store::open(&mailbox, &Repo { root: root.clone(), root_commit: None }).unwrap();
        let tree = store.take().unwrap();
        let head = store.head();
        fs::write(root.join("a.txt"), "live\n").unwrap();
        let home = Home::new(dir.path().join(".pair-watcher"));
        home.ensure_layout().unwrap();
        World { _dir: dir, root, home, store, tree, head }
    }

    fn snap(&self) -> Snapshot<'_> {
        Snapshot { store: &self.store, tree: &self.tree, head: self.head.as_deref() }
    }

    fn api(&self, server: &Server) -> Api {
        let url = server.url.clone();
        let env = move |k: &str| match k {
            "ANTHROPIC_API_KEY" | "OPENAI_API_KEY" => Some(KEY.to_owned()),
            "ANTHROPIC_BASE_URL" | "OPENAI_BASE_URL" => Some(url.clone()),
            _ => None,
        };
        Api::new(Keys::load(&self.home, &env), &env, Duration::from_secs(10))
    }
}

fn read_call(id: &str, args: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": "checking"}, {"type": "tool_use", "id": id, "name": "read_file", "input": args}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 100, "cache_read_input_tokens": 50, "cache_creation_input_tokens": 10, "output_tokens": 5}
    })
}

fn final_text(text: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "output_tokens": 2}
    })
}

fn tool_results(req: &Request) -> Vec<String> {
    let last = req.body["messages"].as_array().unwrap().last().unwrap();
    last["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "tool_result")
        .map(|b| b["content"].as_str().unwrap().to_owned())
        .collect()
}

/// V8: ≤6 calls, then output forced; I15: tools read the snapshot, not the live tree.
#[test]
fn anthropic_loop_caps_calls_then_forces_output() {
    let w = World::new();
    let mut responses: Vec<_> =
        (0..MAX_CALLS).map(|i| (200, "", read_call(&format!("t{i}"), json!({"path": "a.txt"})))).collect();
    responses.push((200, "", final_text("{\"silent\":true}")));
    let server = Server::start(responses);
    let reply = w
        .api(&server)
        .call(&Call {
            model: "claude-sonnet-5-5",
            vendor: Vendor::Anthropic,
            input: "WAKE INPUT",
            tools: Some(w.snap()),
            deadline: None,
            fallback: false,
        })
        .unwrap();
    let seen = server.finish();

    assert_eq!(reply.text, "{\"silent\":true}");
    assert_eq!((reply.requests, reply.tools.calls), (MAX_CALLS + 1, MAX_CALLS));
    assert_eq!(reply.usage, Usage { input: 601, cached: 300, cache_write: 60, output: 32 });
    for (i, req) in seen.iter().enumerate() {
        let forced = i == MAX_CALLS;
        assert_eq!(req.body["tool_choice"]["type"], if forced { "none" } else { "auto" }, "request {i}");
        assert!(req.headers.contains(&format!("x-api-key: {KEY}")) && req.headers.contains("anthropic-version: "));
    }
    let first = &seen[0].body["messages"][0]["content"][0];
    assert_eq!(
        (first["text"].as_str(), first["cache_control"]["type"].as_str()),
        (Some("WAKE INPUT"), Some("ephemeral"))
    );
    let result = &tool_results(&seen[1])[0];
    assert_eq!(result, "1\tsnap\n", "snapshot content, not the live tree");
    assert_eq!(fs::read_to_string(w.root.join("a.txt")).unwrap(), "live\n");
}

/// V8: ≤24 KB read per wake; the result is clipped and output forced next.
#[test]
fn read_allowance_clips_and_forces() {
    let w = World::new();
    let server =
        Server::start(vec![(200, "", read_call("t0", json!({"path": "big.txt"}))), (200, "", final_text("done"))]);
    let reply = w
        .api(&server)
        .call(&Call {
            model: "m",
            vendor: Vendor::Anthropic,
            input: "in",
            tools: Some(w.snap()),
            deadline: None,
            fallback: false,
        })
        .unwrap();
    let seen = server.finish();
    assert_eq!((reply.requests, reply.tools.read), (2, MAX_READ));
    let result = &tool_results(&seen[1])[0];
    assert!(result.ends_with("[clipped: tool read allowance reached]"), "{}", &result[result.len() - 60..]);
    assert!(result.len() <= MAX_READ + 64);
    assert_eq!(seen[1].body["tool_choice"]["type"], "none");
}

/// V11: blocked wake = no tools, one call.
#[test]
fn blocked_call_offers_no_tools() {
    let w = World::new();
    let server = Server::start(vec![(200, "", final_text("consideration"))]);
    let reply = w
        .api(&server)
        .call(&Call {
            model: "m",
            vendor: Vendor::Anthropic,
            input: "blocked",
            tools: None,
            deadline: None,
            fallback: false,
        })
        .unwrap();
    let seen = server.finish();
    assert_eq!((reply.text.as_str(), reply.requests), ("consideration", 1));
    assert!(seen[0].body.get("tools").is_none() && seen[0].body.get("tool_choice").is_none());
}

#[test]
fn openai_loop_round_trips_items_and_normalizes_usage() {
    let w = World::new();
    let server = Server::start(vec![
        (
            200,
            "",
            json!({
                "output": [
                    {"type": "reasoning", "id": "rs_1", "encrypted_content": "opaque", "summary": []},
                    {"type": "function_call", "call_id": "c1", "name": "grep", "arguments": "{\"pattern\":\"sn.p\"}"}
                ],
                "usage": {"input_tokens": 200, "input_tokens_details": {"cached_tokens": 150}, "output_tokens": 7}
            }),
        ),
        (
            200,
            "",
            json!({
                "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "{\"silent\":true}"}]}],
                "usage": {"input_tokens": 300, "input_tokens_details": {"cached_tokens": 200}, "output_tokens": 3}
            }),
        ),
    ]);
    let reply = w
        .api(&server)
        .call(&Call {
            model: "gpt-6.1-sol",
            vendor: Vendor::Openai,
            input: "WAKE",
            tools: Some(w.snap()),
            deadline: None,
            fallback: false,
        })
        .unwrap();
    let seen = server.finish();

    assert_eq!(reply.text, "{\"silent\":true}");
    assert_eq!(reply.usage, Usage { input: 150, cached: 350, cache_write: 0, output: 10 });
    assert!(seen[0].headers.contains(&format!("authorization: bearer {}", KEY.to_ascii_lowercase())));
    assert_eq!(seen[0].body["store"], false);
    assert_eq!(seen[0].body["tools"][0]["type"], "function");
    let input = seen[1].body["input"].as_array().unwrap();
    assert_eq!(input[0], json!({"role": "user", "content": "WAKE"}));
    assert_eq!(input[1]["encrypted_content"], "opaque", "reasoning item echoed back");
    let out = input.iter().find(|i| i["type"] == "function_call_output").unwrap();
    assert_eq!((out["call_id"].as_str(), out["output"].as_str()), (Some("c1"), Some("a.txt:1:snap\n")));
}

/// A failed request reports status + Retry-After for the supervisor and keeps the spend so far.
#[test]
fn http_error_keeps_status_retry_after_and_partial_usage() {
    let w = World::new();
    let server = Server::start(vec![
        (200, "", read_call("t0", json!({"path": "a.txt"}))),
        (429, "retry-after: 7\r\n", json!({"error": {"type": "rate_limit_error"}})),
    ]);
    let err = w
        .api(&server)
        .call(&Call {
            model: "m",
            vendor: Vendor::Anthropic,
            input: "in",
            tools: Some(w.snap()),
            deadline: None,
            fallback: false,
        })
        .unwrap_err();
    server.finish();
    let Failure { error, usage } = err;
    assert!(
        matches!(&error, ProviderError::Http { status: 429, retry_after: Some(d), body } if *d == Duration::from_secs(7) && body.contains("rate_limit")),
        "{error:?}"
    );
    assert_eq!(usage, Usage { input: 100, cached: 50, cache_write: 10, output: 5 });
    assert!(!error.to_string().contains(KEY));
}

#[test]
fn missing_key_fails_before_any_request() {
    let w = World::new();
    let api = Api::new(Keys::load(&w.home, &|_| None), &|_| None, Duration::from_secs(1));
    let err = api
        .call(&Call { model: "m", vendor: Vendor::Openai, input: "in", tools: None, deadline: None, fallback: false })
        .unwrap_err();
    assert_eq!(err.error, ProviderError::NoKey(Vendor::Openai));
}

/// I15: read tools never reach Worker-excluded files; git tools answer as of the snapshot's HEAD.
#[test]
fn tools_serve_snapshot_and_history_read_only() {
    let w = World::new();
    let git_dir = w.root.join(".git");
    let before = fs::read_dir(&git_dir).unwrap().count();
    let mut a = Allowance::default();
    let snap = w.snap();

    assert!(a.run(snap, "read_file", &json!({"path": "secret.env"})).contains("not in the snapshot"));
    assert_eq!(a.run(snap, "grep", &json!({"pattern": "hunter2"})), "no matches");
    assert!(a.run(snap, "git_log", &json!({})).contains("first commit"));
    assert!(a.run(snap, "git_show", &json!({"rev": "HEAD:a.txt"})).contains("committed"));
    assert!(a.run(snap, "git_show", &json!({"rev": "--output=/tmp/pw-x"})).starts_with("error: bad rev"));
    assert!(a.run(snap, "read_file", &json!({"path": "a.txt", "start_line": 5})).contains("no lines"));
    assert_eq!(a.calls, MAX_CALLS);
    assert!(a.run(snap, "read_file", &json!({"path": "a.txt"})).starts_with("not run"), "7th call refused");
    assert_eq!(a.calls, MAX_CALLS);

    assert_eq!(fs::read_dir(&git_dir).unwrap().count(), before, "Worker .git untouched");
    assert!(!Path::new("/tmp/pw-x").exists());
}
