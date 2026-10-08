//! §T15 the Watcher process: wakes end to end against a mock model server, per-Focus cursors
//! (V10), failure classes and the poison guard (V86–V88), BLIND triggers (V89), restart rebuild
//! (V91), held backlog (V50, V70, V92), blocked wakes (V11, V40).

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use pair_watcher::escalation::Notifier;
use pair_watcher::event::{Event, Kind};
use pair_watcher::home::Home;
use pair_watcher::mailbox::{Hook, Mailbox};
use pair_watcher::placement::Placement;
use pair_watcher::status::{Coverage, Op, Outcome, Wake};
use pair_watcher::watch::{self, Inflight, Launch, Watch};
use serde_json::{Value, json};
use tempfile::TempDir;

const MIN: u64 = 60_000;

type Handler = dyn Fn(&Value) -> (u16, Value) + Send + Sync;

/// Answers every request with `handler(body)`, recording bodies.
struct Server {
    url: String,
    seen: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
}

impl Server {
    fn start(handler: impl Fn(&Value) -> (u16, Value) + Send + Sync + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, halt) = (Arc::clone(&seen), Arc::clone(&stop));
        let handler: Arc<Handler> = Arc::new(handler);
        thread::spawn(move || {
            while !halt.load(Ordering::Relaxed) {
                let Ok((stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let (log, handler) = (Arc::clone(&log), Arc::clone(&handler));
                thread::spawn(move || {
                    let mut stream = stream;
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut len = 0;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap();
                        }
                    }
                    let mut buf = vec![0; len];
                    reader.read_exact(&mut buf).unwrap();
                    let body: Value = serde_json::from_slice(&buf).unwrap();
                    let (status, reply) = handler(&body);
                    log.lock().unwrap().push(body);
                    let text = reply.to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                        text.len()
                    );
                });
            }
        });
        Server { url, seen, stop }
    }

    fn requests(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn reply(text: &str) -> (u16, Value) {
    (
        200,
        json!({
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1000, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "output_tokens": 100}
        }),
    )
}

fn input_of(body: &Value) -> String {
    body["messages"][0]["content"][0]["text"].as_str().unwrap().to_owned()
}

/// One `## <title>` section of a wake input.
fn section(input: &str, title: &str) -> String {
    let head = format!("\n## {title}\n");
    let Some(start) = input.find(&head) else { return String::new() };
    let body = &input[start + head.len()..];
    body[..body.find("\n## ").unwrap_or(body.len())].to_owned()
}

const ADD: &str = r#"{"op":"add","severity":"warn","focus":"approach","headline":"6th review round, 0 new findings"}"#;

struct Fx {
    _dir: TempDir,
    home: Home,
    mb: Mailbox,
    repo: PathBuf,
    transcript: PathBuf,
    url: String,
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

impl Fx {
    /// A Claude session in a git repo; `mode` = companion | swarm.
    fn new(mode: &str, server: &Server) -> Fx {
        let dir = TempDir::new().unwrap();
        let home = Home::new(dir.path().join(".pair-watcher"));
        home.ensure_layout().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        let mb = Mailbox::register(&home, "s1").unwrap();
        mb.log()
            .append(
                Event::new(Kind::Session)
                    .with("phase", "start")
                    .with("harness", "claude")
                    .with("repo", repo.to_string_lossy().as_ref())
                    .with("mode", mode)
                    .with("ceiling_usd", 5.0)
                    .with("focuses", json!(["approach", "correctness", "security"])),
            )
            .unwrap();
        watch::write_json(
            &mb.dir().join(watch::LAUNCH_FILE),
            &Launch { flags: vec![format!("mode={mode}")], only: vec![] },
        )
        .unwrap();
        let transcript = dir.path().join("t.jsonl");
        fs::write(&transcript, "").unwrap();
        Fx { _dir: dir, home, mb, repo, transcript, url: server.url.clone() }
    }

    fn open(&self) -> Watch {
        let url = self.url.clone();
        let env = move |k: &str| match k {
            "ANTHROPIC_API_KEY" => Some("k".to_owned()),
            "ANTHROPIC_BASE_URL" => Some(url.clone()),
            _ => None,
        };
        let mut w = Watch::open(&self.home, self.mb.clone(), &env).unwrap_or_else(|e| panic!("open: {}", e.0));
        w.notifier = Notifier::None;
        w.placement = Placement::Print;
        w.wake_clock = Duration::from_secs(5);
        w.blocked_clock = Duration::from_secs(5);
        w
    }

    fn hook(&self, event: &str, payload: Value) {
        self.mb.append_hook(&Hook { event: event.into(), payload }).unwrap();
    }

    fn start(&self) {
        self.hook("SessionStart", json!({"transcript_path": self.transcript, "source": "startup"}));
    }

    fn line(&self, v: Value) {
        let mut f = fs::OpenOptions::new().append(true).open(&self.transcript).unwrap();
        writeln!(f, "{v}").unwrap();
    }

    /// One whole turn: prompt, reply, turn end.
    fn turn(&self, n: u32, text: &str) {
        self.line(json!({"type": "user", "uuid": format!("u{n}"), "message": {"role": "user", "content": format!("prompt {n}")}}));
        self.line(
            json!({"type": "assistant", "uuid": format!("a{n}"), "message": {"id": format!("m{n}"), "role": "assistant",
            "content": [{"type": "text", "text": text}], "stop_reason": "end_turn"}}),
        );
    }

    fn stop(&self) {
        self.hook("Stop", json!({"transcript_path": self.transcript}));
    }

    fn events(&self) -> Vec<Event> {
        self.mb.log().read_from(0).unwrap().records.into_iter().map(|r| r.body).collect()
    }

    fn wakes(&self) -> Vec<Wake> {
        self.events().iter().filter(|e| e.k == Kind::Wake).map(|e| e.parse().unwrap()).collect()
    }

    fn status(&self) -> Vec<Op> {
        self.events().iter().filter(|e| e.k == Kind::Status).map(|e| e.parse().unwrap()).collect()
    }

    fn coverage(&self) -> Vec<Coverage> {
        self.events().iter().filter(|e| e.k == Kind::Coverage).map(|e| e.parse().unwrap()).collect()
    }
}

fn now() -> u64 {
    pair_watcher::jsonl::now_ms()
}

/// V1, V9, V10, V43: a turn end wakes the Companion once, with the prompt, every brief and the
/// turn; its Concern is logged; the cursor commits to the transcript end; the inbox settles.
#[test]
fn turn_end_wakes_and_commits() {
    let server = Server::start(|_| reply(ADD));
    let fx = Fx::new("companion", &server);
    let mut w = fx.open();
    fx.start();
    fx.turn(1, "Starting the 6th review round.");
    fx.stop();
    w.step(now()).unwrap();

    let reqs = server.requests();
    assert_eq!(reqs.len(), 1);
    let input = input_of(&reqs[0]);
    assert!(input.starts_with("You are a Watcher"));
    for f in ["## Focus: approach", "## Focus: correctness", "## Focus: security", "Starting the 6th review round."] {
        assert!(input.contains(f), "{f} missing");
    }
    let wakes = fx.wakes();
    assert_eq!(wakes.len(), 1);
    assert_eq!(
        (wakes[0].outcome, wakes[0].trigger.as_str(), wakes[0].watcher.as_str()),
        (Outcome::Concern, "turn-end", "companion")
    );
    assert!(wakes[0].spend > 0.0);
    assert_eq!(w.board().concerns.all().len(), 1);
    let code = w.state().code.clone();
    assert!(code.id.is_some());
    for f in ["approach", "correctness", "security"] {
        assert_eq!(w.state().focus[f].cursor, code, "{f} committed");
    }
    assert!(fx.mb.settled().unwrap() > 0);
    assert!(!fx.mb.dir().join(watch::INFLIGHT_FILE).exists());

    // Nothing new: no wake.
    w.step(now()).unwrap();
    assert_eq!(server.requests().len(), 1);
}

/// V10, V86, V87, V89, V92: in Swarm a Service failure holds only that Focus's cursor (lagging);
/// its re-wake replays the held slice for it alone, reviewed late.
#[test]
fn failing_focus_holds_alone_and_replays() {
    let fail_security = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&fail_security);
    let server = Server::start(move |b| {
        if input_of(b).contains("## Focus: security") && flag.load(Ordering::Relaxed) {
            (503, json!({"error": "overloaded"}))
        } else {
            reply("")
        }
    });
    let fx = Fx::new("swarm", &server);
    let mut w = fx.open();
    w.wake_clock = Duration::from_millis(500); // no room for the 2 s retry
    fx.start();
    fx.turn(1, "Done.");
    fx.stop();
    let t0 = now();
    w.step(t0).unwrap();

    let wakes = fx.wakes();
    assert_eq!(wakes.len(), 3);
    let sec = wakes.iter().find(|w| w.watcher == "security").unwrap();
    assert_eq!((sec.outcome, sec.class), (Outcome::Failed, Some(pair_watcher::status::Class::Service)));
    let code = w.state().code.clone();
    assert_eq!(w.state().focus["approach"].cursor, code);
    assert_eq!(w.state().focus["correctness"].cursor, code);
    assert_ne!(w.state().focus["security"].cursor, code, "held");
    assert!(fx.status().contains(&Op::Lagging { focus: Some("security".into()) }));

    // Before the 1 min re-wake nothing runs; after it, security alone replays its slice.
    fail_security.store(false, Ordering::Relaxed);
    let before = server.requests().len();
    w.step(t0 + 30_000).unwrap();
    assert_eq!(server.requests().len(), before);
    w.step(t0 + MIN + 1).unwrap();
    let reqs = server.requests();
    assert_eq!(reqs.len(), before + 1);
    assert!(input_of(&reqs[before]).contains("## Focus: security") && input_of(&reqs[before]).contains("Done."));
    let last = fx.wakes().pop().unwrap();
    assert_eq!((last.watcher.as_str(), last.replayed, last.trigger.as_str()), ("security", true, "catch-up"));
    assert_eq!(w.state().focus["security"].cursor, code);
    assert!(fx.status().contains(&Op::LaggingEnd { focus: Some("security".into()) }));
    assert!(matches!(fx.coverage().last(), Some(Coverage::Delayed { .. })));
}

/// V86, V88: invalid output twice on one slice, or a 400 twice, is a Slice failure; the second
/// advances the cursor and logs lost coverage; two such slices back to back are BLIND.
#[test]
fn poison_guard_skips_then_blind() {
    let server = Server::start(|_| (400, json!({"error": "prompt is too long"})));
    let fx = Fx::new("companion", &server);
    let mut w = fx.open();
    fx.start();
    fx.turn(1, "Done.");
    fx.stop();
    let t0 = now();
    w.step(t0).unwrap();
    assert_eq!(fx.wakes()[0].class, Some(pair_watcher::status::Class::Slice));
    assert_ne!(w.state().focus["approach"].cursor, w.state().code, "first failure holds");
    w.step(t0 + MIN + 1).unwrap();
    assert_eq!(w.state().focus["approach"].cursor, w.state().code, "second failure advances");
    assert!(matches!(fx.coverage().as_slice(), [Coverage::Lost { cause, .. }] if cause == "poison-skip"));

    fx.turn(2, "Again.");
    fx.stop();
    w.step(t0 + 2 * MIN).unwrap();
    w.step(t0 + 4 * MIN).unwrap();
    assert!(
        fx.status()
            .iter()
            .any(|s| matches!(s, Op::Blind { focus: None, reason } if reason.contains("2 slices skipped")))
    );
}

/// V86, V89: a Key failure is BLIND at once, no retry.
#[test]
fn key_failure_is_blind() {
    let server = Server::start(|_| (401, json!({"error": "invalid x-api-key"})));
    let fx = Fx::new("companion", &server);
    let mut w = fx.open();
    fx.start();
    fx.turn(1, "Done.");
    fx.stop();
    w.step(now()).unwrap();
    assert_eq!(server.requests().len(), 1, "no retry");
    assert!(
        fx.status().iter().any(|s| matches!(s, Op::Blind { focus: None, reason } if reason.starts_with("model key")))
    );
}

/// V89: a turn end whose transcript turn never shows, one rescan included, is BLIND after 10 s
/// and its span is lost (`transcript-unrecoverable`); the next accepted wake ends it.
#[test]
fn transcript_lost_is_blind() {
    let server = Server::start(|_| reply(""));
    let fx = Fx::new("companion", &server);
    let mut w = fx.open();
    fx.start();
    fx.stop();
    let t0 = now();
    w.step(t0).unwrap();
    w.step(t0 + 5_000).unwrap();
    assert!(!fx.status().iter().any(|s| matches!(s, Op::Blind { .. })));
    assert_eq!(fx.mb.settled().unwrap(), 0, "the waiting turn end is not settled");
    w.step(t0 + 10_000).unwrap();
    assert!(fx.status().contains(&Op::Blind { focus: None, reason: "transcript lost".into() }));
    assert!(matches!(fx.coverage().as_slice(), [Coverage::Lost { cause, .. }] if cause == "transcript-unrecoverable"));
    assert!(server.requests().is_empty());

    fx.turn(1, "Done.");
    fx.stop();
    w.step(t0 + 20_000).unwrap();
    assert!(fx.status().contains(&Op::BlindEnd { focus: None }));
}

/// V91: a restart folds the log and reloads committed cursors; a wake interrupted mid-flight
/// counts once against its slice (`interrupted`).
#[test]
fn restart_rebuilds_and_counts_interrupted_wake() {
    let server = Server::start(|_| reply(ADD));
    let fx = Fx::new("companion", &server);
    let mut w = fx.open();
    fx.start();
    fx.turn(1, "Done.");
    fx.stop();
    w.step(now()).unwrap();
    let state = w.state().clone();
    drop(w);

    let slices = BTreeMap::from([("companion".to_owned(), "u2".to_owned())]);
    watch::write_json(
        &fx.mb.dir().join(watch::INFLIGHT_FILE),
        &Inflight { started: now(), trigger: "turn-end".into(), slices, skip: BTreeMap::new() },
    )
    .unwrap();
    let w = fx.open();
    assert_eq!(w.board().concerns.all().len(), 1, "Concerns rebuilt from the log");
    assert_eq!(w.state().code, state.code);
    assert_eq!(w.state().focus, state.focus);
    let last = fx.wakes().pop().unwrap();
    assert_eq!((last.outcome, last.class), (Outcome::Interrupted, Some(pair_watcher::status::Class::Slice)));
    assert!(!fx.mb.dir().join(watch::INFLIGHT_FILE).exists());
}

/// V48, V50, V92: with the budget spent, wakes hold (`backlog paused`); `$` resumes with one
/// merged catch-up wake that shows the last turn and logs the rest `merge-omitted`.
#[test]
fn budget_hold_then_merged_catch_up() {
    let server = Server::start(|_| reply(""));
    let fx = Fx::new("companion", &server);
    let mut w = fx.open();
    fx.start();
    let spent = Wake {
        n: 0,
        watcher: "companion".into(),
        trigger: "turn-end".into(),
        outcome: Outcome::Silent,
        class: None,
        error: None,
        model: "m".into(),
        spend: 5.0,
        replayed: false,
        rulings: vec![],
    };
    fx.mb.log().append(spent.event()).unwrap();
    fx.turn(1, "First turn text.");
    fx.stop();
    let t0 = now();
    w.step(t0).unwrap();
    fx.turn(2, "Second turn text.");
    fx.stop();
    w.step(t0 + 1).unwrap();
    assert!(server.requests().is_empty());
    assert_eq!(fx.coverage().len(), 1, "backlog paused once");
    assert!(matches!(fx.coverage()[0], Coverage::BacklogPaused { .. }));

    fx.mb.log().append(Op::Raise.event()).unwrap();
    w.step(t0 + 2).unwrap();
    let reqs = server.requests();
    assert_eq!(reqs.len(), 1, "one merged wake");
    let input = input_of(&reqs[0]);
    let turn = section(&input, "Turn");
    assert!(turn.contains("Second turn text.") && !turn.contains("First turn text."), "{turn}");
    assert!(section(&input, "History").contains("First turn text."), "folded into the digest");
    let last = fx.wakes().pop().unwrap();
    assert_eq!(last.trigger, "catch-up");
    assert!(matches!(fx.coverage().last(), Some(Coverage::Lost { cause, .. }) if cause == "merge-omitted"));
    assert_eq!(w.state().focus["approach"].cursor, w.state().code);
    assert!(w.state().held.is_empty());
}

/// V70: `Z` stops wakes and spend, not BLIND; resume = one wake on the current state, nothing
/// logged lost.
#[test]
fn user_pause_then_one_wake() {
    let server = Server::start(|_| reply(""));
    let fx = Fx::new("companion", &server);
    let mut w = fx.open();
    fx.start();
    fx.mb.log().append(Op::UserPause.event()).unwrap();
    fx.turn(1, "One.");
    fx.stop();
    let t0 = now();
    w.step(t0).unwrap();
    assert!(server.requests().is_empty());
    fx.mb.log().append(Op::UserResume.event()).unwrap();
    w.step(t0 + 1).unwrap();
    assert_eq!(server.requests().len(), 1);
    assert!(fx.coverage().iter().all(|c| !matches!(c, Coverage::Lost { .. } | Coverage::BacklogPaused { .. })));
    assert!(!fx.status().iter().any(|s| matches!(s, Op::Blind { .. })));
}

/// V11, V40: a blocked hook wakes at once with the slim input, no tools; the consideration line
/// goes into a blocked Escalation.
#[test]
fn blocked_wake_raises_escalation() {
    let server = Server::start(|b| {
        assert!(b.get("tools").is_none(), "blocked wake: no tools");
        reply(r#"{"op":"consider","text":"the migration is irreversible"}"#)
    });
    let fx = Fx::new("companion", &server);
    let mut w = fx.open();
    fx.start();
    fx.hook("PermissionRequest", json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf data"}}));
    w.step(now()).unwrap();
    let input = input_of(&server.requests()[0]);
    assert!(input.contains("rm -rf data"));
    let raised: Vec<Event> =
        fx.events().into_iter().filter(|e| e.k == Kind::Escalation && e.str("op") == Some("raised")).collect();
    assert_eq!(raised.len(), 1);
    assert!(raised[0].body_text().contains("the migration is irreversible"));
    assert_eq!(fx.wakes()[0].outcome, Outcome::Escalation);
    assert_eq!(fx.wakes()[0].trigger, "blocked");
}

/// V90: a Swarm Focus task that crash-loops stays out of wakes until `attach` lifts it.
#[test]
fn crash_looped_focus_sits_out() {
    let server = Server::start(|_| reply(""));
    let fx = Fx::new("swarm", &server);
    let mut w = fx.open();
    let crashes = watch::Crashes { tasks: BTreeMap::new(), looped: vec!["security".into()] };
    watch::write_json(&fx.mb.dir().join(watch::CRASHES_FILE), &crashes).unwrap();
    fx.start();
    fx.turn(1, "Done.");
    fx.stop();
    w.step(now()).unwrap();
    let watchers: Vec<String> = fx.wakes().into_iter().map(|w| w.watcher).collect();
    assert_eq!(watchers, ["approach", "correctness"]);
}

/// V51: a Service failure on the primary switches to `fallback_model` from the next wake.
#[test]
fn service_failure_switches_to_fallback() {
    let server = Server::start(|b| if b["model"] == "claude-sonnet-5-5" { (529, json!({})) } else { reply("") });
    let fx = Fx::new("companion", &server);
    let launch = Launch { flags: vec!["fallback_model=claude-haiku-4-5".into()], only: vec![] };
    watch::write_json(&fx.mb.dir().join(watch::LAUNCH_FILE), &launch).unwrap();
    let mut w = fx.open();
    w.wake_clock = Duration::from_millis(500);
    fx.start();
    fx.turn(1, "Done.");
    fx.stop();
    let t0 = now();
    w.step(t0).unwrap();
    assert!(
        fx.status()
            .contains(&Op::Fallback { model: "claude-haiku-4-5".into(), class: pair_watcher::status::Class::Service })
    );
    w.step(t0 + MIN + 1).unwrap();
    let models: Vec<String> = server.requests().iter().map(|b| b["model"].as_str().unwrap().to_owned()).collect();
    assert_eq!(models, ["claude-sonnet-5-5", "claude-haiku-4-5"]);
    assert_eq!(fx.wakes().last().unwrap().outcome, Outcome::Silent);
    assert_eq!(w.state().focus["approach"].cursor, w.state().code);
}

/// V89 inputs quiet: the repo changes across two probes ≥6 min apart with no hook and no
/// transcript entry → BLIND; the next hook ends it.
#[test]
fn inputs_quiet_is_blind() {
    let server = Server::start(|_| reply(""));
    let fx = Fx::new("companion", &server);
    let mut w = fx.open();
    fx.start();
    let t0 = now();
    w.step(t0).unwrap();
    w.step(t0 + 3 * MIN).unwrap();
    fs::write(fx.repo.join("a.txt"), "two\n").unwrap();
    w.step(t0 + 6 * MIN).unwrap();
    fs::write(fx.repo.join("a.txt"), "three\n").unwrap();
    w.step(t0 + 9 * MIN).unwrap();
    assert!(!fx.status().iter().any(|s| matches!(s, Op::Blind { .. })), "first changed probe only");
    fs::write(fx.repo.join("a.txt"), "four\n").unwrap();
    w.step(t0 + 12 * MIN).unwrap();
    assert!(fx.status().contains(&Op::Blind { focus: None, reason: "inputs quiet".into() }));
    fx.hook("UserPromptSubmit", json!({}));
    w.step(t0 + 12 * MIN + 1).unwrap();
    assert!(fx.status().contains(&Op::BlindEnd { focus: None }));
}

/// V16, V17, V19, V28: a challenge-flagged Concern with verbatim evidence is queued as the
/// Challenge after the wake commits; the wake reads `challenge`.
#[test]
fn verified_concern_becomes_challenge() {
    let add = r#"{"op":"add","severity":"warn","focus":"approach","headline":"6th review round, 0 new findings","challenge":true,"evidence":[{"kind":"transcript","excerpt":"Starting the 6th review round with zero new findings."}],"alternative":"stop requesting reviews; ask the human to merge"}"#;
    let server = Server::start(move |_| reply(add));
    let fx = Fx::new("companion", &server);
    let mut w = fx.open();
    fx.start();
    fx.turn(1, "Starting the 6th review round with zero new findings.");
    fx.stop();
    w.step(now()).unwrap();
    let queued: Vec<Event> =
        fx.events().into_iter().filter(|e| e.k == Kind::Challenge && e.str("op") == Some("queued")).collect();
    assert_eq!(queued.len(), 1);
    assert_eq!(fx.wakes()[0].outcome, Outcome::Challenge);
    assert_eq!(fx.mb.outbox().pending().unwrap().len(), 1);
}

trait BodyText {
    fn body_text(&self) -> String;
}

impl BodyText for Event {
    fn body_text(&self) -> String {
        serde_json::to_string(self).unwrap()
    }
}

/// V86, V88: a wake interrupted twice on one slice (process killed mid-wake, restarted) lets the
/// poison guard skip it; the span is lost coverage.
#[test]
fn interrupted_twice_skips_the_slice() {
    let server = Server::start(|_| reply(""));
    let fx = Fx::new("companion", &server);
    let w = fx.open();
    let held = w.state().focus.get("approach").cloned().unwrap_or_default();
    drop(w);
    let next = watch::Position {
        cursor: pair_watcher::tailer::Cursor { id: Some("a1".into()), at: 99, turn_msg: None },
        ..held
    };
    let crash = || {
        let slices = BTreeMap::from([("companion".to_owned(), "u1".to_owned())]);
        let skip = BTreeMap::from([("companion".to_owned(), watch::Skip { next: next.clone(), from: 1, to: 2 })]);
        let inflight = Inflight { started: now(), trigger: "turn-end".into(), slices, skip };
        watch::write_json(&fx.mb.dir().join(watch::INFLIGHT_FILE), &inflight).unwrap();
        fx.open()
    };
    drop(crash());
    assert!(fx.coverage().is_empty(), "once holds");
    let w = crash();
    assert!(matches!(fx.coverage().as_slice(), [Coverage::Lost { cause, from: 1, to: 2 }] if cause == "poison-skip"));
    assert_eq!(w.state().focus["approach"].cursor, next.cursor);
}

/// V89: a turn end an earlier wake already read (it reached the transcript before its hook)
/// wakes at once, not after the 10 s transcript wait, and is not BLIND.
#[test]
fn turn_read_before_its_hook_is_not_lost() {
    let server = Server::start(|_| reply(""));
    let fx = Fx::new("companion", &server);
    let mut w = fx.open();
    fx.start();
    fx.turn(1, "Done.");
    fx.hook("PermissionRequest", json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}));
    let t0 = now();
    w.step(t0).unwrap();
    assert_eq!(server.requests().len(), 1, "blocked wake read the turn");
    fx.stop();
    w.step(t0 + 1).unwrap();
    assert_eq!(server.requests().len(), 2, "the turn end woke at once");
    w.step(t0 + 11_000).unwrap();
    assert!(!fx.status().iter().any(|s| matches!(s, Op::Blind { .. })));
}
