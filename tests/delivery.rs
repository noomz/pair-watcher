use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use pair_watcher::challenge::{Challenges, Origin};
use pair_watcher::concern::{Change, Concern, Severity, State};
use pair_watcher::delivery::{self, Route, View, Worker};
use pair_watcher::event::{Event, Kind};
use pair_watcher::home::Home;
use pair_watcher::hook::{GUARD_ENV, SESSION_ENV};
use pair_watcher::jsonl::now_ms;
use pair_watcher::mailbox::{Hook, Mailbox};
use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_pair-watcher");
const T: Duration = Duration::from_secs(5);

struct World {
    dir: TempDir,
    mb: Mailbox,
}

/// A registered session for `harness`, with Concern `c1` open and queued as Challenge `c1`.
fn world(harness: &str, observe_only: bool) -> World {
    let dir = TempDir::new().unwrap();
    let home = Home::new(dir.path().join(".pair-watcher"));
    home.ensure_layout().unwrap();
    let mb = Mailbox::register(&home, "s1").unwrap();
    let start =
        Event::new(Kind::Session).with("phase", "start").with("harness", harness).with("observe_only", observe_only);
    mb.log().append(start).unwrap();
    mb.log().append(Change::Add { watcher: "approach".into(), concern: concern() }.event()).unwrap();
    let w = World { dir, mb };
    w.queue();
    w
}

fn concern() -> Concern {
    Concern {
        id: "c1".into(),
        focus: "approach".into(),
        severity: Severity::Warn,
        state: State::Open,
        headline: "retry loop never converges".into(),
        detail: vec![],
        refs: vec![],
        challenge: true,
        evidence: vec![],
        alternative: Some("stop and bisect".into()),
        verified: true,
    }
}

impl World {
    fn queue(&self) {
        let events = self.events();
        let mut ch = Challenges::rebuild(&events);
        ch.queue(&self.mb.log(), &self.mb.outbox(), &concern(), Origin::Auto, None, now_ms()).unwrap();
    }

    fn events(&self) -> Vec<Event> {
        self.mb.log().read_from(0).unwrap().records.into_iter().map(|r| r.body).collect()
    }

    fn hook_record(&self, event: &str, payload: Value) -> u64 {
        self.mb.append_hook(&Hook { event: event.into(), payload }).unwrap();
        self.mb.read_hooks(0).unwrap().next
    }

    fn view(&self) -> View {
        View::read(&self.mb).unwrap()
    }

    fn cmd(&self, args: &[&str], stdin: &str, guard: bool) -> Output {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env("HOME", self.dir.path())
            .env_remove(GUARD_ENV)
            .env(SESSION_ENV, "s1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if guard {
            cmd.env(GUARD_ENV, "1");
        }
        let mut child = cmd.spawn().unwrap();
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }

    fn stop(&self, active: bool) -> Output {
        let payload = json!({"session_id": "worker-1", "hook_event_name": "Stop", "stop_hook_active": active});
        self.cmd(&["hook", "Stop"], &payload.to_string(), false)
    }

    fn sync_sent(&self) -> Option<String> {
        let mut ch = Challenges::rebuild(&self.events());
        ch.sync_sent(&self.mb.log(), &self.mb.outbox()).unwrap()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn claude_stop_hook_blocks_with_the_challenge_once() {
    let w = world("claude", false);
    let out = w.stop(false);
    assert!(out.status.success() && out.stderr.is_empty(), "{out:?}");
    let answer: Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert_eq!(answer["decision"], "block", "I7");
    let text = answer["reason"].as_str().unwrap();
    assert!(text.starts_with("[pair-watcher · Challenge c1 · from the Watcher, not the human]"), "{text}");

    // V28: delivered exactly once; the next Stop lets the Worker stop.
    let again = w.stop(false);
    assert!(again.status.success() && again.stdout.is_empty(), "{again:?}");
    let hooks = w.mb.read_hooks(0).unwrap().records;
    assert_eq!(hooks.len(), 2, "the Stop hook still appends first");

    // V43: the arbiter logs the send from the outbox ack, once.
    assert_eq!(w.sync_sent().as_deref(), Some("stop"));
    assert_eq!(w.sync_sent(), None);
    let sent: Vec<_> =
        w.events().into_iter().filter(|e| e.k == Kind::Challenge && e.str("op") == Some("sent")).collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].str("route"), Some("stop"));
    assert_eq!(w.view().status_line().as_deref(), Some("1 open · Challenge c1 in debate"));
}

#[test]
fn stop_hook_active_never_blocks_again() {
    let w = world("claude", false);
    let out = w.stop(true);
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
    assert_eq!(w.mb.outbox().pending().unwrap().len(), 1, "still pending for the next idle or Stop");
}

#[test]
fn codex_stop_is_not_a_delivery_route() {
    let w = world("codex", false);
    let out = w.stop(false);
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
}

#[test]
fn stale_challenge_is_dropped_at_the_stop_claim() {
    let w = world("claude", false);
    let mut updated = concern();
    updated.headline = "retry loop never converges (new evidence)".into();
    w.mb.log().append(Change::Update { watcher: "approach".into(), concern: updated }.event()).unwrap();
    let out = w.stop(false);
    assert!(out.stdout.is_empty(), "V21: nothing reaches the Worker");
    assert!(w.mb.outbox().pending().unwrap().is_empty());
}

#[test]
fn idle_routes_wait_for_the_stale_check_of_the_latest_turn_end() {
    let w = world("claude", false);
    let now = now_ms();
    let end = w.hook_record("Stop", json!({"session_id": "worker-1"}));
    let poll = delivery::poll(&w.mb, Route::Mod, Worker::Idle, now).unwrap();
    assert_eq!(poll.item, None, "V21: the turn-end wake has not committed yet");
    assert!(poll.commentary.contains(&"pending: Worker idle".to_owned()), "{:?}", poll.commentary);

    w.mb.settle(end).unwrap();
    let poll = delivery::poll(&w.mb, Route::Mod, Worker::Idle, now).unwrap();
    let item = poll.item.expect("settled: the mod may deliver");
    assert_eq!(item.id, "c1");
    assert_eq!(poll.status.as_deref(), Some("1 open · Challenge c1 pending"));
    assert_eq!(poll.commentary[0], "warn c1 [approach] retry loop never converges");
}

#[test]
fn draft_holds_delivery_and_names_it_once() {
    let w = world("claude", false);
    let now = now_ms();
    let outbox = w.dir.path().join(".pair-watcher/sessions/s1/outbox.jsonl");
    let lines = || fs::read_to_string(&outbox).unwrap().lines().count();

    let poll = delivery::poll(&w.mb, Route::Mod, Worker::Draft, now).unwrap();
    assert_eq!(poll.item, None, "V26: never with a human draft in the box");
    assert!(poll.commentary.contains(&"pending: human draft in prompt".to_owned()), "{:?}", poll.commentary);
    let after_first = lines();
    delivery::poll(&w.mb, Route::Mod, Worker::Draft, now).unwrap();
    assert_eq!(lines(), after_first, "a held reason is written per change, not per poll");

    let busy = delivery::poll(&w.mb, Route::Mod, Worker::Busy, now).unwrap();
    assert_eq!(busy.item, None, "V25: never mid-turn");
    assert!(busy.commentary.contains(&"pending: Worker idle".to_owned()));
    assert!(delivery::poll(&w.mb, Route::Mod, Worker::Idle, now).unwrap().item.is_some());
}

#[test]
fn observe_only_session_never_delivers() {
    let w = world("claude", true);
    assert!(w.stop(false).stdout.is_empty(), "V29");
    let poll = delivery::poll(&w.mb, Route::Mod, Worker::Idle, now_ms()).unwrap();
    assert_eq!(poll.item, None);
    assert!(poll.commentary.contains(&"pending: observe-only, not delivered".to_owned()));
    assert_eq!(poll.status.as_deref(), Some("observe-only · 1 open · Challenge c1 pending"));
    let view = w.view();
    for route in [Route::Stop, Route::Mod, Route::CodexQueue, Route::PiFollowUp] {
        assert!(delivery::claim(&w.mb, &view, route, now_ms()).unwrap().is_none(), "{route:?}");
    }
}

#[test]
fn no_session_start_delivers_nothing() {
    let dir = TempDir::new().unwrap();
    let home = Home::new(dir.path().join(".pair-watcher"));
    home.ensure_layout().unwrap();
    let mb = Mailbox::register(&home, "s1").unwrap();
    let view = View::read(&mb).unwrap();
    assert!(delivery::claim(&mb, &view, Route::Stop, now_ms()).unwrap().is_none());
}

fn fake_codex(dir: &Path, exit: i32) -> std::path::PathBuf {
    fake_codex_script(dir, &format!("exit {exit}"))
}

fn fake_codex_script(dir: &Path, tail: &str) -> std::path::PathBuf {
    let path = dir.join("codex");
    let argv = dir.join("codex.argv");
    fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n{tail}\n", argv.display())).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn codex_queue_after_stop_delivers_to_the_worker_thread() {
    let w = world("codex", false);
    let end = w.hook_record("Stop", json!({"session_id": "019a-thread"}));
    let codex = fake_codex(w.dir.path(), 0);
    let now = now_ms();
    assert_eq!(delivery::codex_queue(&w.mb, codex.as_os_str(), T, now).unwrap(), None, "V21: not before settle");

    w.mb.settle(end).unwrap();
    assert_eq!(delivery::codex_queue(&w.mb, codex.as_os_str(), T, now).unwrap().as_deref(), Some("c1"));
    let argv = fs::read_to_string(w.dir.path().join("codex.argv")).unwrap();
    let argv: Vec<&str> = argv.lines().collect();
    assert_eq!(&argv[..4], ["queue", "--thread", "019a-thread", "--message"], "I7");
    assert!(argv[4].starts_with("[pair-watcher · Challenge c1"));
    assert_eq!(w.sync_sent().as_deref(), Some("codex-queue"));
    assert_eq!(delivery::codex_queue(&w.mb, codex.as_os_str(), T, now).unwrap(), None, "V28: once");
}

#[test]
fn failed_codex_queue_keeps_the_item_for_after_the_lease() {
    let w = world("codex", false);
    let end = w.hook_record("Stop", json!({"session_id": "019a-thread"}));
    w.mb.settle(end).unwrap();
    let codex = fake_codex(w.dir.path(), 1);
    assert!(delivery::codex_queue(&w.mb, codex.as_os_str(), T, now_ms()).is_err());
    assert_eq!(w.mb.outbox().pending().unwrap().len(), 1);
    assert_eq!(w.sync_sent(), None, "not sent");
    let ok = fake_codex(w.dir.path(), 0);
    let later = now_ms() + pair_watcher::outbox::LEASE_MS;
    assert_eq!(delivery::codex_queue(&w.mb, ok.as_os_str(), T, later).unwrap().as_deref(), Some("c1"));
}

#[test]
fn adapter_poll_and_ack_over_the_cli() {
    let w = world("pi", false);
    let out = w.cmd(&["hook", "adapter", "poll", "--session", "s1", "--route", "pi", "--worker", "idle"], "", false);
    assert!(out.status.success(), "{out:?}");
    let poll: Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    let item = &poll["item"];
    assert_eq!(item["id"], "c1");
    let token = item["token"].as_str().unwrap();
    let ack = w.cmd(&["hook", "adapter", "ack", "--session", "s1", "--id", "c1", "--token", token], "", false);
    assert!(ack.status.success() && ack.stdout.is_empty(), "{ack:?}");
    assert_eq!(w.sync_sent().as_deref(), Some("pi-followup"));

    // Fail-silent like a hook: unregistered session, Watcher guard.
    let other =
        w.cmd(&["hook", "adapter", "poll", "--session", "nope", "--route", "mod", "--worker", "idle"], "", false);
    assert!(other.status.success() && other.stdout.is_empty(), "{other:?}");
    let guarded =
        w.cmd(&["hook", "adapter", "poll", "--session", "s1", "--route", "mod", "--worker", "idle"], "", true);
    assert!(guarded.status.success() && guarded.stdout.is_empty(), "{guarded:?}");
}

#[test]
fn adapter_pw_runs_only_session_commands() {
    let w = world("claude", false);
    let rule = w.cmd(&["hook", "adapter", "pw", "--session", "s1", "--", "rule", "c1", "watcher"], "", false);
    assert!(String::from_utf8_lossy(&rule.stderr).contains("not implemented yet (T13)"), "{rule:?}");
    let refused = w.cmd(&["hook", "adapter", "pw", "--session", "s1", "--", "run", "--", "claude"], "", false);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("/pw takes rule|undo|keep|mute|nochallenge|pause"));
}

#[test]
fn codex_queue_waits_while_a_new_turn_runs() {
    let w = world("codex", false);
    let end = w.hook_record("Stop", json!({"session_id": "019a-thread"}));
    w.mb.settle(end).unwrap();
    let codex = fake_codex(w.dir.path(), 0);
    // A text-only human turn shows only as its prompt; a tool turn as its tool hooks.
    for event in ["UserPromptSubmit", "PreToolUse"] {
        w.hook_record(event, json!({"session_id": "019a-thread"}));
        let got = delivery::codex_queue(&w.mb, codex.as_os_str(), T, now_ms()).unwrap();
        assert_eq!(got, None, "V21: queued behind this turn it would skip its turn-end stale check");
        assert!(!w.dir.path().join("codex.argv").exists());
    }
}

#[test]
fn hung_codex_queue_is_killed_and_counted_sent() {
    let w = world("codex", false);
    let end = w.hook_record("Stop", json!({"session_id": "019a-thread"}));
    w.mb.settle(end).unwrap();
    let codex = fake_codex_script(w.dir.path(), "exec sleep 30");
    let started = Instant::now();
    let err = delivery::codex_queue(&w.mb, codex.as_os_str(), Duration::from_millis(300), now_ms()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(5), "the arbiter loop is not held up");
    // It may have queued before hanging: counted sent, never queued a second time (V28).
    assert!(w.mb.outbox().pending().unwrap().is_empty());
    assert_eq!(w.sync_sent().as_deref(), Some("codex-queue"));
}

#[test]
fn worker_session_prefers_the_workers_own_hooks() {
    let w = world("codex", false);
    w.hook_record("Stop", json!({"session_id": "worker-thread"}));
    w.hook_record("PostToolUse", json!({"session_id": "other-thread"}));
    assert_eq!(delivery::worker_session(&w.mb).unwrap().as_deref(), Some("worker-thread"));
}

#[test]
fn released_claim_is_claimable_at_once() {
    let w = world("claude", false);
    let now = now_ms();
    let item = delivery::poll(&w.mb, Route::Mod, Worker::Idle, now).unwrap().item.unwrap();
    let release =
        w.cmd(&["hook", "adapter", "release", "--session", "s1", "--id", &item.id, "--token", &item.token], "", false);
    assert!(release.status.success(), "{release:?}");
    let out = w.stop(false);
    assert!(stdout(&out).contains("\"decision\":\"block\""), "the Stop hook need not wait out the lease: {out:?}");
}

#[test]
fn empty_box_clears_the_draft_label_even_when_nothing_is_claimed() {
    let w = world("claude", false);
    let now = now_ms();
    w.hook_record("Stop", json!({"session_id": "worker-1"}));
    delivery::poll(&w.mb, Route::Mod, Worker::Draft, now).unwrap();
    let poll = delivery::poll(&w.mb, Route::Mod, Worker::Idle, now).unwrap();
    assert_eq!(poll.item, None, "not settled yet");
    assert!(poll.commentary.contains(&"pending: Worker idle".to_owned()), "{:?}", poll.commentary);
}

#[test]
fn stale_challenge_gets_no_held_reason() {
    let w = world("claude", false);
    w.mb.log()
        .append(
            Change::Resolve {
                watcher: "approach".into(),
                id: "c1".into(),
                reason: pair_watcher::concern::Resolution::Fixed,
            }
            .event(),
        )
        .unwrap();
    delivery::poll(&w.mb, Route::Mod, Worker::Draft, now_ms()).unwrap();
    assert_eq!(w.mb.outbox().held("c1").unwrap(), None);
}
