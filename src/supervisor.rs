//! Per-session supervisor (V61, V90), detached (`setsid`) from the launch terminal so closing it
//! or the Worker exiting never kills it mid-drain. It holds no Watcher logic and makes no model
//! calls: it runs the Watcher process, kills and restarts it when it crashes or hangs (≤3 times
//! in 10 min, then BLIND), sends the session's first BLIND notification (V41), and on Worker exit
//! drains the Watcher and ends the session.

use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use crate::escalation::Notifier;
use crate::event::{Event, EventLog, Kind};
use crate::home::{Home, write_atomic};
use crate::hook::{GUARD_ENV, SESSION_ENV};
use crate::jsonl::now_ms;
use crate::mailbox::Mailbox;
use crate::outbox::Withdraw;
use crate::pane::Board;
use crate::status::{Op, WatcherOp};
use crate::watch::{
    self, BLOCKED_INFLIGHT_FILE, DRAIN_CLOCK, DRAIN_FILE, HEARTBEAT, HEARTBEAT_FILE, INFLIGHT_FILE, Inflight,
};

const POLL: Duration = Duration::from_millis(250);
pub const PID_FILE: &str = "supervisor.pid";
pub const HEARTBEAT_FILE_SUPERVISOR: &str = "supervisor.hb";
/// The Worker's pid, for `attach` to re-adopt the session (V90).
pub const WORKER_PID_FILE: &str = "worker.pid";
pub const WATCHER_PID_FILE: &str = "watcher.pid";
/// A heartbeat silent this long is a hung process (V90).
pub const SILENT_MS: u64 = 90_000;
/// An outstanding wake with no result this long after its start is hung: 180 s wake clock +
/// 60 s grace (V90).
pub const PROGRESS_MS: u64 = 240_000;
pub const RESTART_WINDOW_MS: u64 = 10 * 60_000;
pub const RESTART_MAX: usize = 3;
/// Past the drain wake's clock, the Watcher is killed.
const DRAIN_GRACE: Duration = Duration::from_secs(15);
pub const CRASHED: &str = "watcher crashed";
/// Present once `/pw stop` ended the session (V112): `/pw watch` may watch it again.
pub const STOPPED_FILE: &str = "stopped";
/// Session end reason and outbox drop reason for `/pw stop` (V112).
pub const STOPPED: &str = "stopped";
/// `session` phase: watched again after a stop, same session (V112).
pub const REWATCH: &str = "rewatch";
pub const GONE: &str = "supervisor gone";

/// Starts `<exe> supervise <session>` in its own session, double-forked so it is reparented to
/// init and the Worker never inherits a child to reap. Returns once it has exec'd.
pub fn spawn(exe: &Path, mailbox: &Mailbox, session: &str, worker_pid: u32) -> io::Result<()> {
    let log =
        fs::OpenOptions::new().append(true).create(true).mode(0o600).open(mailbox.dir().join("supervisor.log"))?;
    let mut cmd = Command::new(exe);
    cmd.args(["supervise", session, "--worker-pid", &worker_pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        // Watcher subprocesses inherit the guard so their own harness calls never wake them (V58).
        .env(GUARD_ENV, "1")
        .env_remove(SESSION_ENV);
    // SAFETY: setsid, fork and _exit are async-signal-safe; nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            match libc::fork() {
                -1 => Err(io::Error::last_os_error()),
                0 => Ok(()),
                _ => libc::_exit(0),
            }
        });
    }
    cmd.spawn()?.wait()?;
    Ok(())
}

/// Why a running Watcher process counts as hung, if it does (V90). `beat` = its last heartbeat
/// (or its start, whichever is later); `inflight` = start of the wake under way.
pub fn hung(now: u64, beat: u64, inflight: Option<u64>) -> Option<&'static str> {
    if now.saturating_sub(beat) >= SILENT_MS {
        return Some("heartbeat silent 90 s");
    }
    match inflight {
        Some(started) if now.saturating_sub(started) >= PROGRESS_MS => Some("wake past its progress deadline"),
        _ => None,
    }
}

/// Restarts so far in the window (pruned) admit one more (V90).
pub fn may_restart(restarts: &mut Vec<u64>, now: u64) -> bool {
    restarts.retain(|t| now.saturating_sub(*t) < RESTART_WINDOW_MS);
    if restarts.len() >= RESTART_MAX {
        return false;
    }
    restarts.push(now);
    true
}

/// The session's first BLIND gets one notification (V41): by the supervisor, or by the pane
/// when the supervisor is gone; both look for an earlier `blind notified` first.
pub fn notify_first_blind(board: &Board, log: &EventLog, notifier: &Notifier, by: &str) -> io::Result<bool> {
    let Some((_, reason)) = board.blind.values().next() else { return Ok(false) };
    if board.blind_notified {
        return Ok(false);
    }
    let body = format!("{reason}: not watching; silence ≠ all clear");
    if let Err(e) = notifier.show("pair-watcher BLIND", &body) {
        eprintln!("pair-watcher: BLIND notification: {e}");
    }
    log.append(Op::BlindNotified { by: by.to_owned() }.event())?;
    Ok(true)
}

/// The `supervise` command: runs the Watcher until the Worker exits, then drains it, logs
/// session end and unregisters the mailbox so later hooks no-op.
pub fn supervise(home: &Home, session: &str, worker_pid: i32) -> io::Result<()> {
    let mailbox = Mailbox::open_registered(home, session)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("session {session} is not registered")))?;
    let dir = mailbox.dir().to_path_buf();
    write_atomic(&dir.join(PID_FILE), std::process::id().to_string().as_bytes())?;
    // 0 = attached, pid unknown: the next hook writes it (`hook::learn_worker`).
    if worker_pid > 0 {
        write_atomic(&dir.join(WORKER_PID_FILE), worker_pid.to_string().as_bytes())?;
    }
    beat(dir.join(HEARTBEAT_FILE_SUPERVISOR));
    let log = mailbox.log();
    let env = |k: &str| std::env::var(k).ok();
    let notifier = Notifier::detect(&env);
    let exe = std::env::current_exe()?;
    let _ = fs::remove_file(dir.join(DRAIN_FILE));
    adopt(&mailbox)?;

    let mut restarts: Vec<u64> = Vec::new();
    let mut child = Some(start(&exe, &mailbox)?);
    let mut started = now_ms();
    let (mut board, mut offset) = (Board::new(0), 0);
    let mut worker = worker_pid;
    while (worker <= 0 || alive(worker)) && mailbox.is_live() {
        if worker <= 0 {
            worker = read_pid(&dir.join(WORKER_PID_FILE)).unwrap_or(0);
        }
        let batch = log.read_from(offset)?;
        offset = batch.next;
        batch.records.iter().for_each(|r| board.apply(r));
        if notify_first_blind(&board, &log, &notifier, "supervisor")? {
            board.blind_notified = true;
        }
        if let Some(c) = child.as_mut() {
            let now = now_ms();
            let why = match c.try_wait()? {
                Some(status) => Some((format!("exited: {}", describe(status)), false)),
                None => {
                    let hb = watch::beat_of(&dir.join(HEARTBEAT_FILE)).unwrap_or(0).max(started);
                    let inflight = [INFLIGHT_FILE, BLOCKED_INFLIGHT_FILE]
                        .iter()
                        .filter_map(|f| watch::read_json::<Inflight>(&dir.join(f)).ok().flatten())
                        .map(|i| i.started)
                        .min();
                    hung(now, hb, inflight).map(|why| (why.to_owned(), true))
                }
            };
            if let Some((why, hang)) = why {
                if hang {
                    let _ = c.kill();
                    let _ = c.wait();
                    log.append(WatcherOp::HangKill.event())?;
                } else {
                    log.append(WatcherOp::Crash { focus: None, why: why.clone() }.event())?;
                }
                child = None;
                if may_restart(&mut restarts, now) {
                    log.append(WatcherOp::Restart { why }.event())?;
                    child = Some(start(&exe, &mailbox)?);
                    started = now_ms();
                } else {
                    log.append(WatcherOp::CrashLoop { focus: None }.event())?;
                    log.append(Op::Blind { focus: None, reason: CRASHED.to_owned() }.event())?;
                }
            }
        }
        thread::sleep(POLL);
    }
    // `/pw stop`: the human wants it off now, so no final wake.
    let stopped = dir.join(STOPPED_FILE).is_file();
    if let Some(c) = child.as_mut() {
        if stopped {
            let _ = c.kill();
            let _ = c.wait();
        } else {
            drain(&mailbox, c)?;
        }
    }
    end(&mailbox, stopped)
}

/// Session end: logged, unregistered so later hooks no-op. A stop drops what is still queued
/// for the Worker (V112).
fn end(mailbox: &Mailbox, stopped: bool) -> io::Result<()> {
    let mut event = Event::new(Kind::Session).with("phase", "end");
    if stopped {
        let (outbox, now) = (mailbox.outbox(), now_ms());
        let mut dropped = Vec::new();
        for item in outbox.pending()? {
            if outbox.withdraw(&item.id, STOPPED, now)? == Withdraw::Withdrawn {
                dropped.push(item.id);
            }
        }
        event = event.with("reason", STOPPED).with("dropped", dropped);
    }
    mailbox.log().append(event)?;
    mailbox.unregister()?;
    for f in [PID_FILE, WATCHER_PID_FILE, HEARTBEAT_FILE_SUPERVISOR] {
        let _ = fs::remove_file(mailbox.dir().join(f));
    }
    Ok(())
}

/// `/pw stop` (V112): ends the Watcher for the session. A running supervisor sees the session
/// unregistered and ends it; with none, the session ends here.
pub fn stop(home: &Home, session: &str) -> Result<String, String> {
    let Some(mailbox) = Mailbox::open_registered(home, session) else {
        return Ok("not watched".into());
    };
    let result = (|| -> io::Result<()> {
        write_atomic(&mailbox.dir().join(STOPPED_FILE), b"")?;
        let supervised = running(&mailbox, now_ms());
        mailbox.unregister()?;
        if !supervised {
            kill_orphan(&mailbox);
            end(&mailbox, true)?;
        }
        Ok(())
    })();
    result.map(|()| "stopped watching; /pw watch to resume".into()).map_err(|e| e.to_string())
}

/// `/pw watch` after `/pw stop` (V112): the same session dir, watched again from now on. The
/// `attach` hook re-reads the transcript from its end; the turns since the stop stay unread.
fn rewatch(home: &Home, mailbox: &Mailbox, exe: &Path) -> Result<String, String> {
    let (id, dir) = (mailbox.id(), mailbox.dir());
    let worker = read_pid(&dir.join(WORKER_PID_FILE)).unwrap_or(0);
    if worker > 0 && !alive(worker) {
        return Err(format!("session {id}: its Worker is gone"));
    }
    let result = (|| -> io::Result<()> {
        let hooks = mailbox.read_hooks(0)?.records;
        let transcript = hooks.iter().rev().find_map(|r| {
            ["transcript_path", "session_file"].iter().find_map(|k| r.body.payload.get(*k)?.as_str().map(str::to_owned))
        });
        Mailbox::register(home, &id)?;
        let _ = fs::remove_file(dir.join(STOPPED_FILE));
        mailbox.log().append(Event::new(Kind::Session).with("phase", REWATCH))?;
        if let Some(path) = transcript {
            let payload = serde_json::json!({"session_id": id, "transcript_path": path, "source": "resume"});
            mailbox.append_hook(&crate::mailbox::Hook { event: crate::run::ATTACH_EVENT.into(), payload })?;
        }
        spawn(exe, mailbox, &id, worker.max(0) as u32)
    })();
    if let Err(e) = result {
        let _ = mailbox.unregister();
        return Err(e.to_string());
    }
    Ok("watching again from now".into())
}

/// Re-adopting a session (`attach`, V90): an orphaned Watcher from a dead supervisor is stopped,
/// and a `supervisor gone` BLIND ends.
fn adopt(mailbox: &Mailbox) -> io::Result<()> {
    kill_orphan(mailbox);
    let log = mailbox.log();
    let board = Board::rebuild(0, &log.read_from(0)?.records);
    if board.blind.get(&None).is_some_and(|(_, r)| r == GONE) {
        log.append(Op::BlindEnd { focus: None }.event())?;
    }
    Ok(())
}

/// A Watcher left by a dead supervisor.
fn kill_orphan(mailbox: &Mailbox) {
    if let Some(pid) = read_pid(&mailbox.dir().join(WATCHER_PID_FILE))
        && alive(pid)
    {
        // SAFETY: plain kill(2) on a pid this session recorded.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

fn start(exe: &Path, mailbox: &Mailbox) -> io::Result<Child> {
    let child = Command::new(exe).args(["watch", &mailbox.id()]).stdin(Stdio::null()).stdout(Stdio::null()).spawn()?;
    write_atomic(&mailbox.dir().join(WATCHER_PID_FILE), child.id().to_string().as_bytes())?;
    Ok(child)
}

/// Session end (V90): one final wake ≤60 s, then the Watcher stops.
fn drain(mailbox: &Mailbox, child: &mut Child) -> io::Result<()> {
    write_atomic(&mailbox.dir().join(DRAIN_FILE), b"")?;
    let deadline = std::time::Instant::now() + DRAIN_CLOCK + DRAIN_GRACE;
    while std::time::Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        thread::sleep(POLL);
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

fn describe(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(sig)) => format!("signal {sig}"),
        _ => status.to_string(),
    }
}

/// Own heartbeat file, every 30 s from its own thread (V90).
fn beat(path: std::path::PathBuf) {
    let write = move || {
        let _ = write_atomic(&path, now_ms().to_string().as_bytes());
    };
    write();
    thread::spawn(move || {
        loop {
            thread::sleep(HEARTBEAT);
            write();
        }
    });
}

pub fn read_pid(path: &Path) -> Option<i32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    found || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The supervisor's heartbeat is fresh: it still runs (V90).
pub fn running(mailbox: &Mailbox, now: u64) -> bool {
    watch::beat_of(&mailbox.dir().join(HEARTBEAT_FILE_SUPERVISOR)).is_some_and(|b| now.saturating_sub(b) < SILENT_MS)
}

/// `attach <session>` (V90): re-adopts a live session whose supervisor is gone and lifts Focus
/// crash-loop BLIND. Returns what it did.
/// `hint`: harness and cwd from an in-session `/pw watch` (V113).
pub fn attach(
    home: &Home,
    session: &str,
    exe: &Path,
    hint: Option<(crate::harness::Harness, std::path::PathBuf)>,
) -> Result<String, String> {
    let Some(mailbox) = Mailbox::open_registered(home, session) else {
        return match Mailbox::open(home, session) {
            Some(m) if m.dir().join(STOPPED_FILE).is_file() => rewatch(home, &m, exe),
            Some(_) => Err(format!("session {session} has ended")),
            None => crate::run::attach_installed(home, session, exe, hint),
        };
    };
    let lifted = fs::remove_file(mailbox.dir().join(watch::CRASHES_FILE)).is_ok();
    if lifted {
        let _ = mailbox.log().append(WatcherOp::Restart { why: "attach: crash-loop lifted".into() }.event());
    }
    if running(&mailbox, now_ms()) {
        return Ok(if lifted { "crash-looped Watchers resume".into() } else { "already watched".into() });
    }
    // An attached session may not know its Worker's pid yet.
    let worker = read_pid(&mailbox.dir().join(WORKER_PID_FILE)).unwrap_or(0);
    if worker > 0 && !alive(worker) {
        return Err(format!("session {session}: its Worker is gone"));
    }
    spawn(exe, &mailbox, session, worker.max(0) as u32).map_err(|e| e.to_string())?;
    Ok("supervisor restarted".into())
}
