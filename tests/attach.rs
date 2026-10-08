//! §T16 `attach` of a Worker not started by `run` (V58, V61): found by its transcript, refused
//! until its harness is installed; the first hook names the Worker's pid.

mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use pair_watcher::event::Kind;
use pair_watcher::harness::Harness;
use pair_watcher::home::Home;
use pair_watcher::hook::{self, GUARD_ENV, SESSION_ENV};
use pair_watcher::mailbox::Mailbox;
use pair_watcher::run::{self, ATTACH_EVENT, Found};
use pair_watcher::supervisor;
use serde_json::{Value, json};
use tempfile::TempDir;

const ID: &str = "0b5e7c1a-1111-2222-3333-444455556666";

fn env_of(home: &Path) -> impl Fn(&str) -> Option<String> + '_ {
    move |k: &str| (k == "HOME").then(|| home.to_string_lossy().into_owned())
}

#[test]
fn transcripts_are_found_per_harness() {
    let dir = TempDir::new().unwrap();
    let h = dir.path();
    let env = env_of(h);
    assert_eq!(run::find_transcript(&env, ID), None);
    assert_eq!(run::find_transcript(&env, "../etc"), None, "ids from the CLI are path segments only");

    let pi = h.join(format!(".pi/agent/sessions/--repo--/2026-10-08T00-00-00-000Z_{ID}.jsonl"));
    common::write(&pi, &format!("{}\n", json!({"type": "session", "id": ID, "cwd": "/repo/pi"})));
    assert_eq!(
        run::find_transcript(&env, ID),
        Some(Found { harness: Harness::Pi, transcript: pi.clone(), cwd: "/repo/pi".into() })
    );

    let codex = h.join(format!(".codex/sessions/2026/10/08/rollout-2026-10-08T00-00-00-{ID}.jsonl"));
    common::write(
        &codex,
        &format!("{}\n", json!({"type": "session_meta", "payload": {"id": ID, "cwd": "/repo/codex"}})),
    );
    assert_eq!(run::find_transcript(&env, ID).unwrap().harness, Harness::Codex, "Codex before pi");

    let claude = h.join(format!(".claude/projects/-repo/{ID}.jsonl"));
    let lines = [json!({"type": "summary"}), json!({"type": "user", "cwd": "/repo/claude"})];
    common::write(&claude, &lines.iter().map(|l| format!("{l}\n")).collect::<String>());
    let found = run::find_transcript(&env, ID).unwrap();
    assert_eq!(found, Found { harness: Harness::Claude, transcript: claude, cwd: "/repo/claude".into() });
}

#[test]
fn worker_is_the_nearest_ancestor_that_is_not_a_shell() {
    let chain = |c: &[(i32, &str)]| c.iter().map(|(p, n)| (*p, n.to_string())).collect::<Vec<_>>();
    assert_eq!(hook::pick_worker(chain(&[(40, "/bin/sh"), (30, "-zsh"), (20, "node")])), Some(20));
    assert_eq!(hook::pick_worker(chain(&[(40, "/Users/u/.local/share/claude/versions/2.1.294")])), Some(40));
    assert_eq!(hook::pick_worker(chain(&[(40, "bash"), (1, "launchd")])), None, "never init");
    assert_eq!(hook::pick_worker(Vec::new()), None);
}

struct World {
    dir: TempDir,
    home: Home,
    repo: PathBuf,
}

impl World {
    fn new() -> World {
        let dir = TempDir::new().unwrap();
        let home = Home::new(dir.path().join(".pair-watcher"));
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        World { dir, home, repo }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pair-watcher"));
        cmd.args(args)
            .env("HOME", self.dir.path())
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove(GUARD_ENV)
            .env_remove(SESSION_ENV)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("HERDR_PANE_ID")
            .env_remove("TMUX");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).stdin(Stdio::null()).output().unwrap()
    }

    fn hook(&self, event: &str, payload: &Value) -> Output {
        let mut child = self
            .cmd(&["hook", event])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }

    fn transcript(&self) -> PathBuf {
        let path = self.dir.path().join(format!(".claude/projects/-repo/{ID}.jsonl"));
        let line =
            json!({"type": "user", "uuid": "u1", "cwd": self.repo, "message": {"role": "user", "content": "hi"}});
        common::write(&path, &format!("{line}\n"));
        path
    }
}

fn wait(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn attach_registers_an_installed_worker_and_its_hook_names_the_pid() {
    let w = World::new();
    let transcript = w.transcript();

    let out = w.run(&["attach", ID]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("pair-watcher install claude"), "V58: refused until installed: {err}");
    assert!(Mailbox::open(&w.home, ID).is_none());

    assert!(w.run(&["install", "claude"]).status.success());
    let out = w.run(&["attach", ID]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("watching claude session"), "{out:?}");
    let mb = Mailbox::open_registered(&w.home, ID).unwrap();
    let start = mb.log().read_from(0).unwrap().records.into_iter().find(|r| r.body.k == Kind::Session).unwrap();
    assert_eq!(start.body.str("harness"), Some("claude"));
    assert_eq!(start.body.str("repo"), Some(w.repo.to_str().unwrap()));
    let hooks = mb.read_hooks(0).unwrap().records;
    assert_eq!(hooks[0].body.event, ATTACH_EVENT);
    assert_eq!(hooks[0].body.payload["transcript_path"], json!(transcript));
    assert_eq!(hooks[0].body.payload["source"], "resume", "earlier content predates the session");
    let dir = mb.dir().to_path_buf();
    wait("supervisor", || dir.join(supervisor::HEARTBEAT_FILE_SUPERVISOR).is_file());
    assert!(!dir.join(supervisor::WORKER_PID_FILE).exists(), "pid unknown until a hook");

    // The hook's parent here is this test process: the Worker stand-in.
    assert!(w.hook("Stop", &json!({"session_id": ID})).status.success());
    assert_eq!(supervisor::read_pid(&dir.join(supervisor::WORKER_PID_FILE)), Some(std::process::id() as i32));
    assert!(String::from_utf8_lossy(&w.run(&["attach", ID]).stdout).contains("already watched"));

    // Ending the session stops the supervisor (Worker exit takes the same path).
    mb.unregister().unwrap();
    wait("session end", || {
        mb.log()
            .read_from(0)
            .unwrap()
            .records
            .iter()
            .any(|r| r.body.k == Kind::Session && r.body.str("phase") == Some("end"))
    });
    let out = w.run(&["attach", ID]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("has ended"), "{out:?}");
}

#[test]
fn a_launched_session_hook_does_not_guess_the_worker() {
    let w = World::new();
    w.home.ensure_layout().unwrap();
    let mb = Mailbox::register(&w.home, "k1").unwrap();
    let mut child =
        w.cmd(&["hook", "Stop"]).env(SESSION_ENV, "k1").stdin(Stdio::piped()).stdout(Stdio::null()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(!mb.dir().join(supervisor::WORKER_PID_FILE).exists(), "`run` already knows it");
}
