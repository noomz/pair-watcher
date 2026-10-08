//! §T15 supervisor (V90): hang and restart rules, the first BLIND's one notification (V41),
//! Worker exit → drain → session end; pane `BLIND supervisor gone`; `attach` re-adopts.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use pair_watcher::escalation::Notifier;
use pair_watcher::event::{Event, Kind};
use pair_watcher::home::Home;
use pair_watcher::jsonl::now_ms;
use pair_watcher::mailbox::Mailbox;
use pair_watcher::pane::{self, Board};
use pair_watcher::status::{Op, WatcherOp};
use pair_watcher::supervisor::{self, PROGRESS_MS, RESTART_WINDOW_MS, SILENT_MS};
use pair_watcher::watch;
use tempfile::TempDir;

/// V90: heartbeat silent 90 s, or a wake 240 s past its start, is hung.
#[test]
fn hang_rules() {
    let now = 1_000_000;
    assert_eq!(supervisor::hung(now, now - SILENT_MS + 1, None), None);
    assert!(supervisor::hung(now, now - SILENT_MS, None).is_some());
    assert_eq!(supervisor::hung(now, now, Some(now - PROGRESS_MS + 1)), None);
    assert!(supervisor::hung(now, now, Some(now - PROGRESS_MS)).is_some());
}

/// V90: ≤3 restarts in 10 min, then stop.
#[test]
fn restart_budget() {
    let mut r = Vec::new();
    assert!(supervisor::may_restart(&mut r, 0));
    assert!(supervisor::may_restart(&mut r, 1));
    assert!(supervisor::may_restart(&mut r, 2));
    assert!(!supervisor::may_restart(&mut r, 3));
    assert!(supervisor::may_restart(&mut r, RESTART_WINDOW_MS + 1), "the window slides");
}

struct Fx {
    dir: TempDir,
    home: Home,
    mb: Mailbox,
}

impl Fx {
    fn new() -> Fx {
        let dir = TempDir::new().unwrap();
        let home = Home::new(dir.path().join(".pair-watcher"));
        home.ensure_layout().unwrap();
        let mb = Mailbox::register(&home, "s1").unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        mb.log()
            .append(
                Event::new(Kind::Session)
                    .with("phase", "start")
                    .with("harness", "claude")
                    .with("repo", repo.to_string_lossy().as_ref())
                    .with("mode", "companion")
                    .with("ceiling_usd", 5.0),
            )
            .unwrap();
        Fx { dir, home, mb }
    }

    fn status(&self) -> Vec<Op> {
        self.records().into_iter().filter(|e| e.k == Kind::Status).filter_map(|e| e.parse()).collect()
    }

    fn records(&self) -> Vec<Event> {
        self.mb.log().read_from(0).unwrap().records.into_iter().map(|r| r.body).collect()
    }

    fn wait(&self, what: &str, f: impl Fn(&Fx) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !f(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {:?}", self.records());
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn sleeper() -> Child {
    Command::new("sleep").arg("60").spawn().unwrap()
}

fn beat(path: &Path, t: u64) {
    fs::write(path, t.to_string()).unwrap();
}

/// V41, V90: the supervisor runs the Watcher; a Watcher that cannot run is BLIND, notified once
/// by the supervisor; Worker exit drains it and ends the session.
#[test]
fn supervise_runs_notifies_and_ends() {
    let fx = Fx::new();
    let mut worker = sleeper();
    let mut sup = Command::new(env!("CARGO_BIN_EXE_pair-watcher"))
        .args(["supervise", "s1", "--worker-pid", &worker.id().to_string()])
        .env("HOME", fx.dir.path())
        .env("HERDR_PANE_ID", "p1")
        .env("HERDR_BIN_PATH", "/usr/bin/true")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    fx.wait("BLIND", |f| f.status().iter().any(|s| matches!(s, Op::Blind { focus: None, .. })));
    fx.wait("blind notified", |f| f.status().contains(&Op::BlindNotified { by: "supervisor".into() }));
    let dir = fx.mb.dir();
    assert!(dir.join(supervisor::HEARTBEAT_FILE_SUPERVISOR).is_file());
    fx.wait("watcher heartbeat", |_| dir.join(watch::HEARTBEAT_FILE).is_file());
    let watcher = supervisor::read_pid(&dir.join(supervisor::WATCHER_PID_FILE)).unwrap();
    assert!(supervisor::alive(watcher));

    worker.kill().unwrap();
    worker.wait().unwrap();
    let status = sup.wait().unwrap();
    assert!(status.success());
    let ends = fx.records().iter().filter(|e| e.k == Kind::Session && e.str("phase") == Some("end")).count();
    assert_eq!(ends, 1);
    assert!(!fx.mb.is_live());
    thread::sleep(Duration::from_millis(300));
    assert!(!supervisor::alive(watcher) || zombie(watcher), "drained Watcher exited");
    let notified = fx.status().iter().filter(|s| matches!(s, Op::BlindNotified { .. })).count();
    assert_eq!(notified, 1, "one BLIND notification per session");
}

fn zombie(pid: i32) -> bool {
    Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim().starts_with('Z'))
}

/// V41, V89, V90: a silent supervisor heartbeat on a live session → the pane logs `supervisor
/// gone`, BLIND, and the session's one notification; once.
#[test]
fn pane_sees_supervisor_gone() {
    let fx = Fx::new();
    let hb = fx.mb.dir().join(supervisor::HEARTBEAT_FILE_SUPERVISOR);
    let now = now_ms();
    beat(&hb, now);
    let board = || Board::rebuild(0, &fx.mb.log().read_from(0).unwrap().records);
    assert!(!pane::supervisor_gone(&fx.mb, &board(), now, &Notifier::None).unwrap());
    beat(&hb, now - SILENT_MS);
    assert!(pane::supervisor_gone(&fx.mb, &board(), now, &Notifier::None).unwrap());
    assert!(!pane::supervisor_gone(&fx.mb, &board(), now + 1, &Notifier::None).unwrap(), "once");
    let records = fx.records();
    assert!(records.iter().any(|e| e.parse::<WatcherOp>() == Some(WatcherOp::SupervisorGone)));
    assert!(fx.status().contains(&Op::Blind { focus: None, reason: supervisor::GONE.into() }));
    assert_eq!(fx.status().iter().filter(|s| matches!(s, Op::BlindNotified { by } if by == "pane")).count(), 1);
    assert!(board().blind_notified);
}

/// V90: `attach` lifts a Focus crash-loop; a running supervisor is left alone.
#[test]
fn attach_lifts_crash_loop() {
    let fx = Fx::new();
    beat(&fx.mb.dir().join(supervisor::HEARTBEAT_FILE_SUPERVISOR), now_ms());
    let crashes = watch::Crashes { looped: vec!["security".into()], ..Default::default() };
    watch::write_json(&fx.mb.dir().join(watch::CRASHES_FILE), &crashes).unwrap();
    let exe = Path::new(env!("CARGO_BIN_EXE_pair-watcher"));
    let msg = supervisor::attach(&fx.home, "s1", exe).unwrap();
    assert_eq!(msg, "crash-looped Watchers resume");
    assert!(!fx.mb.dir().join(watch::CRASHES_FILE).exists());
    assert_eq!(supervisor::attach(&fx.home, "s1", exe).unwrap(), "already watched");
    assert!(supervisor::attach(&fx.home, "nope", exe).unwrap_err().contains("is not live"));
}
