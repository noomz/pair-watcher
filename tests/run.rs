//! `pair-watcher run` end to end against stand-in harness scripts (V57, V58, V60, V61, V82, C3, C10).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::{executable, fake_codex, write};
use pair_watcher::event::{Event, Kind};
use pair_watcher::home::Home;
use pair_watcher::hook::{GUARD_ENV, SESSION_ENV};
use pair_watcher::jsonl::Record;
use pair_watcher::launch::CODEX_BYPASS_FLAG;
use pair_watcher::mailbox::Mailbox;
use serde_json::Value;
use tempfile::TempDir;

struct World {
    dir: TempDir,
    home: Home,
    bin: PathBuf,
    repo: PathBuf,
}

/// The stand-in records argv, the session env and its pid, then fires one hook through the shim
/// the way a harness would, with a payload session id of its own.
fn fake_harness(dir: &Path, name: &str, version: &str) -> PathBuf {
    let path = dir.join(name);
    executable(
        &path,
        &format!(
            r#"#!/bin/sh
d="$(dirname "$0")"
if [ "$1" = "--version" ]; then echo "{version}"; exit 0; fi
printf '%s\n' "$@" > "$d/{name}.args"
printf '%s' "${{{SESSION_ENV}-unset}}" > "$d/{name}.session"
printf '%s' "$$" > "$d/{name}.pid"
echo '{{"session_id":"worker-1","hook_event_name":"Stop"}}' | "$HOME/.pair-watcher/bin/pw-hook" Stop 2>/dev/null
exit 0
"#
        ),
    );
    path
}

impl World {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let home = Home::new(dir.path().join(".pair-watcher"));
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        for args in [
            &["init", "-q"][..],
            &["-c", "user.email=a@b", "-c", "user.name=a", "commit", "-q", "--allow-empty", "-m", "init"],
        ] {
            assert!(Command::new("git").args(args).current_dir(&repo).status().unwrap().success());
        }
        fake_harness(&bin, "claude", "2.1.289 (Claude Code)");
        fake_harness(&bin, "pi", "1.0.4");
        World { dir, home, bin, repo }
    }

    fn pw(&self, args: &[&str]) -> (Output, u32) {
        let child = Command::new(env!("CARGO_BIN_EXE_pair-watcher"))
            .args(args)
            .current_dir(&self.repo)
            .env("HOME", self.dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove(GUARD_ENV)
            .env_remove(SESSION_ENV)
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_BIN_PATH")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        (child.wait_with_output().unwrap(), pid)
    }

    fn run(&self, worker: &[&str]) -> (Output, u32) {
        let mut args = vec!["run", "--"];
        args.extend(worker);
        self.pw(&args)
    }

    fn harness(&self, name: &str) -> String {
        self.bin.join(name).to_string_lossy().into_owned()
    }

    fn recorded(&self, name: &str, what: &str) -> String {
        fs::read_to_string(self.bin.join(format!("{name}.{what}"))).unwrap_or_default()
    }

    fn args(&self, name: &str) -> Vec<String> {
        self.recorded(name, "args").lines().map(str::to_owned).collect()
    }

    fn session(&self, name: &str) -> String {
        self.recorded(name, "session")
    }

    fn mailbox_dir(&self, session: &str) -> PathBuf {
        self.home.sessions_dir().join(session)
    }

    fn log(&self, session: &str) -> Vec<Record<Event>> {
        pair_watcher::event::EventLog::new(self.mailbox_dir(session).join("log.jsonl")).read_from(0).unwrap().records
    }

    /// The supervisor ends the session once the Worker has exited (V61).
    fn wait_for_end(&self, session: &str) -> Vec<Record<Event>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let log = self.log(session);
            if log.iter().any(|r| r.body.k == Kind::Session && r.body.fields["phase"] == "end") {
                return log;
            }
            assert!(Instant::now() < deadline, "no session end: {log:?}");
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn start(log: &[Record<Event>]) -> &Event {
    let e = &log[0].body;
    assert_eq!((e.k, &e.fields["phase"]), (Kind::Session, &Value::from("start")));
    e
}

#[test]
fn claude_runs_in_place_with_session_settings_and_supervisor() {
    let w = World::new();
    let (out, pid) = w.run(&[&w.harness("claude"), "--resume", "x"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(w.recorded("claude", "pid"), pid.to_string(), "exec in place: same pid, same terminal");

    let session = w.session("claude");
    let settings = w.mailbox_dir(&session).join("claude-settings.json");
    let mod_dir = w.home.adapters_dir().join(env!("CARGO_PKG_VERSION")).join("claude");
    let expected =
        ["--settings", settings.to_str().unwrap(), "--plugin-dir", mod_dir.to_str().unwrap(), "--resume", "x"];
    assert_eq!(w.args("claude"), expected, "C3: per-session flags first, user args kept");
    // I8: the mod rides `--plugin-dir`, written from the binary's embedded copy.
    let register = fs::read_to_string(mod_dir.join("hooks/register.ts")).unwrap();
    assert_eq!(register, include_str!("../adapters/claude/hooks/register.ts"));
    assert!(mod_dir.join(".claude-plugin/plugin.json").is_file() && mod_dir.join("hooks/hooks.json").is_file());
    assert!(!mod_dir.join("tests").exists(), "mod tests are not shipped");
    let json: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    let shim = w.home.hook_shim();
    assert_eq!(json["hooks"]["Stop"][0]["hooks"][0]["command"], format!("{} Stop", shim.display()));
    assert!(shim.is_file(), "V58: shim written by run");

    // The Worker's hook carried its own session id; the launch key routed it (V58).
    let hooks = Mailbox::register(&w.home, &session).unwrap().read_hooks(0).unwrap().records;
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].body.payload["session_id"], "worker-1");

    let log = w.wait_for_end(&session);
    let s = start(&log);
    assert_eq!(s.fields["harness"], "claude");
    assert_eq!(s.fields["harness_version"], "2.1.289");
    assert_eq!(s.fields["observe_only"], false);
    assert_eq!(s.fields["focuses"], serde_json::json!(["approach", "correctness", "security"]));
    assert!(s.fields["config_hash"].as_str().is_some_and(|h| h.len() == 64));
    assert!(
        stderr(&out)
            .contains(&format!("pair-watcher: pane: {} pane --session {session}", env!("CARGO_BIN_EXE_pair-watcher")))
    );
}

#[test]
fn supervisor_ends_session_and_unregisters() {
    let w = World::new();
    let (out, _) = w.run(&[&w.harness("claude")]);
    assert!(out.status.success(), "{}", stderr(&out));
    let session = w.session("claude");
    w.wait_for_end(&session);
    assert!(Mailbox::open_registered(&w.home, &session).is_none(), "later hooks no-op");
    assert!(!w.mailbox_dir(&session).join("supervisor.pid").exists());
    let log = fs::read_to_string(w.mailbox_dir(&session).join("supervisor.log")).unwrap();
    assert!(log.is_empty(), "{log}");
}

#[test]
fn codex_gets_hook_flags_and_one_trust_table() {
    let w = World::new();
    let codex = fake_codex(&w.bin, None);
    let (out, _) = w.run(&[codex.to_str().unwrap(), "resume", "--last"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let calls = fs::read_to_string(w.bin.join("codex.argv")).unwrap();
    let worker = calls.lines().find(|l| l.contains("resume")).expect("worker argv");
    assert_eq!(worker.matches("hooks.state=").count(), 1, "V57: exactly one inline trust table");
    assert!(!worker.contains("hooks.state.\""), "V57: never the dotted form");
    assert!(!calls.contains(CODEX_BYPASS_FLAG), "C10");
    for ev in ["SessionStart", "Stop", "PermissionRequest", "PreToolUse", "PostToolUse"] {
        assert!(worker.contains(&format!("hooks.{ev}=")), "{ev} missing: {worker}");
        assert!(worker.contains(&format!("\"sha256:{ev}\"")), "{ev} not trusted: {worker}");
    }
    assert!(worker.ends_with("resume --last"), "{worker}");
    assert!(!worker.contains("sha256:u"), "user hooks keep their own trust");
    assert!(calls.lines().any(|l| l.starts_with("app-server -c hooks.")), "hashes read from app-server hooks/list");

    let session = fs::read_dir(w.home.sessions_dir()).unwrap().next().unwrap().unwrap().file_name();
    let log = w.log(session.to_str().unwrap());
    assert_eq!(start(&log).fields["observe_only"], false);
}

#[test]
fn codex_without_trust_runs_observe_only() {
    let w = World::new();
    let codex = fake_codex(&w.bin, Some(r#"{"id":2,"error":{"code":1,"message":"boom"}}"#));
    let (out, _) = w.run(&[codex.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("codex hook trust unavailable"), "{}", stderr(&out));
    let calls = fs::read_to_string(w.bin.join("codex.argv")).unwrap();
    assert_eq!(calls.lines().last(), Some(""), "Worker started with the user's args only: {calls}");
    let session = fs::read_dir(w.home.sessions_dir()).unwrap().next().unwrap().unwrap().file_name();
    assert_eq!(start(&w.log(session.to_str().unwrap())).fields["observe_only"], true);
}

#[test]
fn codex_bypass_flag_is_refused() {
    let w = World::new();
    let codex = fake_codex(&w.bin, None);
    let (out, _) = w.run(&[codex.to_str().unwrap(), CODEX_BYPASS_FLAG]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("refusing --dangerously-bypass-hook-trust"));
    assert!(!w.bin.join("codex.argv").exists(), "C10: Worker never started");
}

#[test]
fn pi_loads_the_embedded_extension() {
    let w = World::new();
    let (out, _) = w.run(&[&w.harness("pi"), "--model", "m"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let ext = w.home.adapters_dir().join(env!("CARGO_PKG_VERSION")).join("pi/pair-watcher.ts");
    assert_eq!(w.args("pi"), ["-e", ext.to_str().unwrap(), "--model", "m"]);
    assert_eq!(fs::read_to_string(&ext).unwrap(), include_str!("../adapters/pi/pair-watcher.ts"));
    assert!(stderr(&out).contains("pair-watcher: untested pi 1.0.4"), "V60: newer than last-tested");
    let log = w.wait_for_end(&w.session("pi"));
    assert_eq!(start(&log).fields["notes"], serde_json::json!(["untested pi 1.0.4"]));
}

#[test]
fn unsupported_worker_is_refused() {
    let w = World::new();
    let (out, _) = w.run(&["aider"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("unsupported Worker `aider`"));
}

#[test]
fn unreadable_config_starts_worker_unwatched() {
    let w = World::new();
    write(&w.home.config_file(), "model = = 1\n");
    let (out, _) = w.run(&[&w.harness("claude"), "-p", "hi"]);
    assert!(out.status.success(), "V83: config never blocks the Worker");
    assert!(stderr(&out).contains("BLIND — config unreadable"), "{}", stderr(&out));
    assert_eq!(w.args("claude"), ["-p", "hi"]);
    assert_eq!(w.session("claude"), "unset");
}

#[test]
fn repo_focus_without_tty_waits_for_focus_approve() {
    let w = World::new();
    write(&w.repo.join(".pair-watcher/focus/perf.md"), "Attend to: slow loops.\n");
    let (out, _) = w.run(&[&w.harness("claude")]);
    assert!(out.status.success());
    assert!(
        stderr(&out).contains("repo Focus `perf` not approved — pair-watcher focus approve perf"),
        "{}",
        stderr(&out)
    );
    let log = w.wait_for_end(&w.session("claude"));
    assert!(
        !start(&log).fields["focuses"].as_array().unwrap().contains(&"perf".into()),
        "V82: unapproved does not run"
    );

    let (out, _) = w.pw(&["focus", "approve", "perf"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let (out, _) = w.pw(&["config", "show"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"perf\""), "approved Focus enabled");
    let (out, _) = w.pw(&["focus", "approve", "nope"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no repo Focus `nope` awaiting approval"));
}
