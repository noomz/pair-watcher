use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use pair_watcher::home::Home;
use pair_watcher::hook::{GUARD_ENV, write_shim};
use pair_watcher::mailbox::Mailbox;
use serde_json::json;
use tempfile::TempDir;

struct World {
    dir: TempDir,
    home: Home,
}

impl World {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let home = Home::new(dir.path().join(".pair-watcher"));
        home.ensure_layout().unwrap();
        World { dir, home }
    }

    fn run(&self, program: &Path, args: &[&str], stdin: &str, guard: bool) -> Output {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .env("HOME", self.dir.path())
            .env_remove(GUARD_ENV)
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

    fn hook(&self, event: &str, stdin: &str) -> Output {
        self.run(Path::new(env!("CARGO_BIN_EXE_pair-watcher")), &["hook", event], stdin, false)
    }

    fn inbox(&self, id: &str) -> Option<String> {
        fs::read_to_string(self.home.sessions_dir().join(id).join("events.jsonl")).ok()
    }
}

fn silent_success(out: &Output) {
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
}

#[test]
fn hook_appends_payload_for_registered_session() {
    let w = World::new();
    let mb = Mailbox::register(&w.home, "s1").unwrap();
    let payload = json!({"session_id": "s1", "hook_event_name": "Stop", "stop_hook_active": false});
    silent_success(&w.hook("Stop", &payload.to_string()));
    silent_success(&w.hook("PermissionRequest", r#"{"session_id":"s1","tool_input":{"command":"ls"}}"#));

    let records = mb.read_hooks(0).unwrap().records;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].body.event, "Stop");
    assert_eq!(records[0].body.payload, payload);
    assert_eq!(records[1].body.payload["tool_input"]["command"], "ls");
    assert!(records[0].t <= records[1].t);
}

#[test]
fn hook_is_noop_without_registered_watcher() {
    let w = World::new();
    silent_success(&w.hook("Stop", r#"{"session_id":"nobody"}"#));
    assert!(!w.home.sessions_dir().join("nobody").exists(), "hook never creates a mailbox");

    let mb = Mailbox::register(&w.home, "s1").unwrap();
    mb.unregister().unwrap();
    silent_success(&w.hook("Stop", r#"{"session_id":"s1"}"#));
    assert_eq!(w.inbox("s1"), None);
}

#[test]
fn hook_is_noop_under_watcher_guard() {
    let w = World::new();
    Mailbox::register(&w.home, "s1").unwrap();
    let bin = Path::new(env!("CARGO_BIN_EXE_pair-watcher"));
    silent_success(&w.run(bin, &["hook", "Stop"], r#"{"session_id":"s1"}"#, true));
    assert_eq!(w.inbox("s1"), None);
}

#[test]
fn hook_fails_silent_on_bad_input() {
    let w = World::new();
    Mailbox::register(&w.home, "s1").unwrap();
    for stdin in ["", "not json", r#"{"no_session":1}"#, r#"{"session_id":"../s1"}"#, r#"{"session_id":7}"#] {
        silent_success(&w.hook("Stop", stdin));
    }
    assert_eq!(w.inbox("s1"), None);
    assert!(!w.dir.path().join("s1").exists());
}

#[test]
fn hook_fails_silent_when_mailbox_unwritable() {
    let w = World::new();
    let mb = Mailbox::register(&w.home, "s1").unwrap();
    fs::create_dir(mb.dir().join("events.jsonl")).unwrap();
    silent_success(&w.hook("Stop", r#"{"session_id":"s1"}"#));
}

#[test]
fn shim_execs_binary_and_stays_silent_once_removed() {
    let w = World::new();
    let mb = Mailbox::register(&w.home, "s1").unwrap();

    // A copy stands in for an installed binary at a path with a quote in it.
    let bin_dir = w.dir.path().join("it's bin");
    fs::create_dir(&bin_dir).unwrap();
    let exe = bin_dir.join("pair-watcher");
    fs::copy(env!("CARGO_BIN_EXE_pair-watcher"), &exe).unwrap();

    let shim = write_shim(&w.home, &exe).unwrap();
    assert_eq!(shim, w.home.root().join("bin/pw-hook"));
    assert_eq!(fs::metadata(&shim).unwrap().permissions().mode() & 0o777, 0o700);

    silent_success(&w.run(&shim, &["Stop"], r#"{"session_id":"s1"}"#, false));
    assert_eq!(mb.read_hooks(0).unwrap().records[0].body.event, "Stop");

    fs::remove_file(&exe).unwrap();
    silent_success(&w.run(&shim, &["Stop"], r#"{"session_id":"s1"}"#, false));
    assert_eq!(mb.read_hooks(0).unwrap().records.len(), 1);

    // Rewriting (next `run`/upgrade) replaces the shim in place.
    write_shim(&w.home, Path::new(env!("CARGO_BIN_EXE_pair-watcher"))).unwrap();
    silent_success(&w.run(&shim, &["Stop"], r#"{"session_id":"s1"}"#, false));
    assert_eq!(mb.read_hooks(0).unwrap().records.len(), 2);
}
