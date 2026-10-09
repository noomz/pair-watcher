//! §T28 `claude-session` backend: the mailbox transport (V107), the code-owned text tool loop
//! (V8, V11), failure mapping and the unclaimed rule (V108), fallback on the API (V51).

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use pair_watcher::credentials::Keys;
use pair_watcher::fail::classify;
use pair_watcher::home::Home;
use pair_watcher::models::{Usage, Vendor};
use pair_watcher::provider::{Api, Call, ProviderError};
use pair_watcher::repo::Repo;
use pair_watcher::session::{Completion, Completions, ModelUsage, Request, Session, State};
use pair_watcher::snapshot::Store;
use pair_watcher::status::Class;
use pair_watcher::tools::{MAX_CALLS, Snapshot};
use tempfile::TempDir;

const READ_A: &str = r#"{"tool": "read_file", "args": {"path": "a.txt"}}"#;
const ADD: &str = r#"{"op":"add","severity":"warn","focus":"approach","headline":"h"}"#;

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
    dir: TempDir,
    store: Store,
    tree: String,
}

impl World {
    /// A repo whose snapshot has `a.txt` = "snap"; the live tree has moved on to "live".
    fn new() -> World {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("work");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "committed\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "first"]);
        fs::write(root.join("a.txt"), "snap\n").unwrap();
        let store = Store::open(&dir.path().join("mailbox"), &Repo { root: root.clone(), root_commit: None }).unwrap();
        let tree = store.take().unwrap();
        fs::write(root.join("a.txt"), "live\n").unwrap();
        World { dir, store, tree }
    }

    fn completions(&self) -> Completions {
        Completions::new(&self.dir.path().join("mailbox"))
    }

    fn session(&self, unclaimed: Duration) -> Session {
        // No API listens: a fallback call fails as network, never as the mod.
        let env = |k: &str| match k {
            "ANTHROPIC_API_KEY" => Some("sk".to_owned()),
            "ANTHROPIC_BASE_URL" => Some("http://127.0.0.1:9".to_owned()),
            _ => None,
        };
        let home = Home::new(self.dir.path().join(".pair-watcher"));
        let api = Api::new(Keys::load(&home, &env), &env, Duration::from_secs(2));
        Session { completions: self.completions(), api, unclaimed }
    }

    fn snap(&self) -> Snapshot<'_> {
        Snapshot { store: &self.store, tree: &self.tree, head: None }
    }
}

fn call<'a>(input: &'a str, tools: Option<Snapshot<'a>>, fallback: bool) -> Call<'a> {
    Call {
        model: "claude-sonnet-5-5",
        vendor: Vendor::Anthropic,
        input,
        tools,
        deadline: Some(Instant::now() + Duration::from_secs(10)),
        fallback,
    }
}

fn usage(input: u64, output: u64) -> ModelUsage {
    ModelUsage {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    }
}

fn answered(text: &str) -> Completion {
    Completion { is_answered: true, text: text.into(), usage: usage(100, 10), ..Default::default() }
}

fn failed(reason: &str, status: Option<u16>, error: &str) -> Completion {
    Completion {
        is_answered: false,
        reason: Some(reason.into()),
        status,
        error: Some(error.into()),
        usage: usage(0, 0),
        ..Default::default()
    }
}

/// A Claude mod: claims what the Watcher wrote and answers each request with `answer`, keeping
/// every request it ran.
struct Mod {
    seen: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
}

impl Mod {
    fn start(completions: Completions, answer: impl Fn(&Request, usize) -> Option<Completion> + Send + 'static) -> Mod {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, halt) = (Arc::clone(&seen), Arc::clone(&stop));
        thread::spawn(move || {
            while !halt.load(Ordering::Relaxed) {
                for r in completions.claim(0).unwrap() {
                    let n = log.lock().unwrap().len();
                    log.lock().unwrap().push(r.clone());
                    if let Some(c) = answer(&r, n) {
                        completions.result(&r.id, c, 0).unwrap();
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        Mod { seen, stop }
    }

    fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Mod {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// V107: requests are claimed once, results taken only for a claimed request, and an abandoned
/// one is never claimed or answered.
#[test]
fn store_claims_once_and_takes_results_for_claims_only() {
    let w = World::new();
    let c = w.completions();
    let req = |id: &str| Request {
        id: id.into(),
        model: "m".into(),
        system: String::new(),
        prompt: vec![],
        max_tokens: 1,
        timeout_ms: None,
    };
    c.request(req("r1"), 1).unwrap();
    c.request(req("r2"), 1).unwrap();
    assert!(!c.result("r1", answered("x"), 2).unwrap(), "not claimed yet");
    assert!(c.abandon("r2", "unclaimed", true, 2).unwrap());
    assert_eq!(c.claim(3).unwrap().iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["r1"]);
    assert_eq!(c.claim(3).unwrap(), vec![]);
    assert!(!c.abandon("r1", "unclaimed", true, 4).unwrap(), "claimed: not abandoned as unclaimed");
    assert!(c.result("r1", answered("x"), 4).unwrap());
    assert!(!c.result("r1", answered("y"), 5).unwrap());
    assert_eq!(c.state("r1").unwrap(), State::Done(answered("x")));
    assert_eq!(c.state("r2").unwrap(), State::Abandoned);
    assert!(!c.result("r2", answered("z"), 5).unwrap());
    assert_eq!(c.state("r3").unwrap(), State::Missing);
}

/// V8, V107: the model asks for a read tool in its reply; Rust serves it from the snapshot and the
/// next completion carries the result after the cached wake input.
#[test]
fn text_tool_loop_serves_the_snapshot() {
    let w = World::new();
    let m = Mod::start(w.completions(), |_, n| Some(answered(if n == 0 { READ_A } else { ADD })));
    let reply = w.session(Duration::from_secs(5)).call(&call("WAKE INPUT", Some(w.snap()), false)).unwrap();
    assert_eq!(reply.text, ADD);
    assert_eq!((reply.requests, reply.tools.calls), (2, 1));
    assert_eq!(reply.usage, Usage { input: 200, cached: 0, cache_write: 0, output: 20 });
    let reqs = m.requests();
    assert!(reqs[0].system.contains("read_file") && reqs[0].system.contains("grep"));
    assert_eq!(reqs[0].prompt.len(), 1);
    assert!(reqs[0].prompt[0].cache && reqs[0].prompt[0].text == "WAKE INPUT");
    assert_eq!(reqs[1].prompt[0], reqs[0].prompt[0], "same cached opening");
    let tail = &reqs[1].prompt[1].text;
    assert!(tail.contains(READ_A) && tail.contains("snap") && !tail.contains("live"), "{tail}");
    assert!(reqs[0].timeout_ms.is_some_and(|t| t > 0 && t <= 10_000));
}

/// V8: at most six calls, then output is forced; tool lines never reach the output parser.
#[test]
fn tool_allowance_then_forced() {
    let w = World::new();
    let m = Mod::start(w.completions(), |_, _| Some(answered(&format!("{READ_A}\n{ADD}"))));
    let reply = w.session(Duration::from_secs(5)).call(&call("in", Some(w.snap()), false)).unwrap();
    assert_eq!(reply.tools.calls, MAX_CALLS);
    assert_eq!(reply.requests, MAX_CALLS + 1);
    assert_eq!(reply.text, ADD);
    let last = m.requests().pop().unwrap();
    assert!(last.prompt[1].text.contains("allowance is spent"));
}

/// V11: a blocked wake is one completion with no tools offered or served.
#[test]
fn blocked_wake_is_one_completion_without_tools() {
    let w = World::new();
    let m = Mod::start(w.completions(), |_, _| Some(answered(&format!("{READ_A}\n{ADD}"))));
    let reply = w.session(Duration::from_secs(5)).call(&call("blocked", None, false)).unwrap();
    assert_eq!((reply.requests, reply.tools.calls, reply.text.as_str()), (1, 0, ADD));
    let reqs = m.requests();
    assert!(!reqs[0].system.contains("read_file"));
    assert_eq!(reqs[0].prompt.len(), 1);
}

/// V108: `api-error` by status per V86, `empty-reply` → Slice, `aborted` → Service; usage kept.
#[test]
fn failure_mapping() {
    let cases = [
        (failed("api-error", Some(529), "overloaded"), Class::Service),
        (failed("api-error", Some(429), "rate_limit"), Class::Service),
        (failed("api-error", Some(500), "server_error"), Class::Service),
        (failed("api-error", None, "unknown"), Class::Service),
        (failed("api-error", Some(401), "authentication_failed"), Class::Key),
        (failed("api-error", Some(403), "permission_error"), Class::Key),
        (failed("api-error", Some(400), "billing_error"), Class::Key),
        (failed("api-error", Some(400), "invalid_request"), Class::Slice),
        (failed("empty-reply", None, ""), Class::Slice),
        (failed("aborted", None, ""), Class::Service),
    ];
    for (c, class) in cases {
        let what = format!("{c:?}");
        let mut c = c;
        c.usage = usage(7, 3);
        let (e, u) = c.into_result().unwrap_err();
        assert_eq!(classify(&e), class, "{what}");
        assert_eq!(u, Usage { input: 7, cached: 0, cache_write: 0, output: 3 });
    }

    let w = World::new();
    let _m = Mod::start(w.completions(), |_, _| Some(failed("api-error", Some(529), "overloaded")));
    let f = w.session(Duration::from_secs(5)).call(&call("in", None, false)).unwrap_err();
    assert!(matches!(f.error, ProviderError::Http { status: 529, .. }));
}

/// V108: a request no mod claims in time is a Key failure, `claude-session: mod not answering`;
/// abandoned, so a mod that loads late never runs it.
#[test]
fn unclaimed_is_key_and_abandoned() {
    let w = World::new();
    let f = w.session(Duration::from_millis(200)).call(&call("in", Some(w.snap()), false)).unwrap_err();
    assert_eq!(f.error, ProviderError::Unanswered);
    assert_eq!(classify(&f.error), Class::Key);
    assert_eq!(f.error.to_string(), "claude-session: mod not answering");
    assert_eq!(w.completions().claim(0).unwrap(), vec![]);
}

/// V87: claimed but unanswered past the wake clock is a timeout (Service); a late result is not
/// taken.
#[test]
fn past_deadline_is_timeout_and_abandoned() {
    let w = World::new();
    let m = Mod::start(w.completions(), |_, _| None);
    let mut c = call("in", None, false);
    c.deadline = Some(Instant::now() + Duration::from_millis(400));
    let f = w.session(Duration::from_secs(5)).call(&c).unwrap_err();
    assert!(matches!(f.error, ProviderError::Timeout(_)));
    let id = m.requests()[0].id.clone();
    assert!(!w.completions().result(&id, answered(ADD), 0).unwrap());
}

/// V51, V108: a `fallback_model` wake runs on the direct API, never through the mod.
#[test]
fn fallback_runs_on_the_api() {
    let w = World::new();
    let f = w.session(Duration::from_millis(200)).call(&call("in", None, true)).unwrap_err();
    assert!(matches!(f.error, ProviderError::Network(_)), "{:?}", f.error);
    assert_eq!(w.completions().claim(0).unwrap(), vec![], "nothing written for the mod");
}
