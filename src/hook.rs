//! `pair-watcher hook <event>` and the `pw-hook` shim harness hooks call (I5, V58).

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::delivery;
use crate::home::Home;
use crate::jsonl::now_ms;
use crate::mailbox::{Hook, Mailbox};

/// Set for every Watcher subprocess; hooks no-op under it so a Watcher never triggers itself.
/// Not a config key: config reads only `PAIR_WATCHER_<known key>`.
pub const GUARD_ENV: &str = "PAIR_WATCHER_GUARD";

/// Session key `run` mints and passes in the Worker env; hooks route by it before the payload's
/// `session_id`, which changes on Claude `/clear` and is unknown for Codex at launch (V58).
pub const SESSION_ENV: &str = "PAIR_WATCHER_SESSION";

/// Pure append, fail-silent: never fails the Worker. Prints only a Claude `Stop` block carrying a
/// ready Challenge (I7, V26). The block is claimed and acked before the `Stop` record is appended,
/// so the wake that record triggers already sees the delivery (`debate` reads the reply from it).
pub fn run(event: &str) {
    if std::env::var_os(GUARD_ENV).is_some() {
        return;
    }
    let launched = std::env::var(SESSION_ENV).ok();
    let attached = launched.is_none();
    let Ok(Some((mailbox, payload))) = registered(io::stdin().lock(), launched) else {
        return;
    };
    if attached {
        learn_worker(&mailbox);
    }
    if event == "Stop" {
        // A panic must not cost the record below.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            delivery::stop_block(&mailbox, &payload, &mut io::stdout().lock(), now_ms())
        }));
    }
    let _ = mailbox.append_hook(&Hook { event: event.to_owned(), payload });
}

/// The registered mailbox the hook routes to, with the parsed payload.
fn registered(mut input: impl Read, launched: Option<String>) -> io::Result<Option<(Mailbox, Value)>> {
    let home = Home::from_env()?;
    let mut raw = Vec::new();
    input.read_to_end(&mut raw)?;
    let payload =
        serde_json::from_slice(&raw).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&raw).into_owned()));
    let id = match &launched {
        Some(key) => key.as_str(),
        None => match payload.get("session_id").and_then(Value::as_str) {
            Some(id) => id,
            None => return Ok(None),
        },
    };
    let Some(mailbox) = Mailbox::open_registered(&home, id) else { return Ok(None) };
    Ok(Some((mailbox, payload)))
}

/// Names between a hook and its Worker: shells, `env`, the shim.
const SHELLS: [&str; 8] = ["sh", "bash", "zsh", "dash", "fish", "ksh", "env", "pw-hook"];

/// The Worker among a hook's ancestors (nearest first, `(pid, command name)`): the first that is
/// not a shell.
pub fn pick_worker(chain: impl IntoIterator<Item = (i32, String)>) -> Option<i32> {
    chain
        .into_iter()
        .find(|(_, name)| {
            let base = name.rsplit('/').next().unwrap_or(name).trim_start_matches('-');
            !SHELLS.contains(&base)
        })
        .map(|(pid, _)| pid)
        .filter(|pid| *pid > 1)
}

/// An attached session learns its Worker's pid from its first hook, so the supervisor can end the
/// session when the Worker exits (V90). Once written, later hooks skip the walk.
fn learn_worker(mailbox: &Mailbox) {
    let path = mailbox.dir().join(crate::supervisor::WORKER_PID_FILE);
    if path.exists() {
        return;
    }
    // SAFETY: getppid has no preconditions.
    let parent = unsafe { libc::getppid() };
    let chain = std::iter::successors(Some(parent), |pid| parent_of(*pid).map(|(ppid, _)| ppid))
        .take(8)
        .filter_map(|pid| Some((pid, parent_of(pid)?.1)));
    if let Some(pid) = pick_worker(chain) {
        let _ = crate::home::write_atomic(&path, pid.to_string().as_bytes());
    }
}

/// `(ppid, command name)` of `pid`.
fn parent_of(pid: i32) -> Option<(i32, String)> {
    let out = std::process::Command::new("ps").args(["-o", "ppid=,comm=", "-p", &pid.to_string()]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (ppid, name) = text.trim().split_once(char::is_whitespace)?;
    Some((ppid.trim().parse().ok()?, name.trim().to_owned()))
}

/// Writes `bin/pw-hook`: execs `exe` while it exists, else exits 0, so a removed binary stays
/// silent. Hook config names only the shim, never a versioned path.
pub fn write_shim(home: &Home, exe: &Path) -> io::Result<PathBuf> {
    let exe = exe.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "binary path is not UTF-8"))?;
    let script = format!(
        "#!/bin/sh\n# pair-watcher hook shim: run the current binary, else stay silent.\nexe={}\n[ -x \"$exe\" ] || exit 0\nexec \"$exe\" hook \"$@\"\n",
        sh_quote(exe)
    );
    let path = home.hook_shim();
    let tmp = path.with_extension("tmp");
    let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o700).open(&tmp)?;
    file.write_all(script.as_bytes())?;
    file.set_permissions(fs::Permissions::from_mode(0o700))?;
    // Rename so a hook firing mid-write never runs a partial script.
    fs::rename(&tmp, &path)?;
    Ok(path)
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// One shell line; words quoted only when they need it, so plain commands stay readable.
pub fn shell_line<S: AsRef<str>>(words: &[S]) -> String {
    let word = |w: &S| {
        let w = w.as_ref();
        let plain = !w.is_empty() && w.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+=:,@".contains(&b));
        if plain { w.to_owned() } else { sh_quote(w) }
    };
    words.iter().map(word).collect::<Vec<_>>().join(" ")
}
