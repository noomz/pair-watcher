//! Per-session supervisor (V61, V90), detached (`setsid`) from the launch terminal so closing it
//! or the Worker exiting never kills it mid-drain. It holds no Watcher logic and makes no model
//! calls. Here: record its pid, end the session when the Worker exits. Watcher children,
//! heartbeats, restarts and the final drain are T15.

use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::event::{Event, Kind};
use crate::home::{Home, write_atomic};
use crate::hook::{GUARD_ENV, SESSION_ENV};
use crate::mailbox::Mailbox;

const POLL: Duration = Duration::from_millis(250);
pub const PID_FILE: &str = "supervisor.pid";

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

/// The `supervise` command: waits for the Worker to exit, then logs session end and unregisters
/// the mailbox so later hooks no-op.
pub fn supervise(home: &Home, session: &str, worker_pid: i32) -> io::Result<()> {
    let mailbox = Mailbox::open_registered(home, session)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("session {session} is not registered")))?;
    let pid_file = mailbox.dir().join(PID_FILE);
    write_atomic(&pid_file, std::process::id().to_string().as_bytes())?;
    while alive(worker_pid) {
        thread::sleep(POLL);
    }
    mailbox.log().append(Event::new(Kind::Session).with("phase", "end"))?;
    mailbox.unregister()?;
    let _ = fs::remove_file(pid_file);
    Ok(())
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    found || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
