//! `pair-watcher run -- <claude|codex|pi> <args…>` (I1): wires the Worker for this session only
//! (C3), starts the supervisor (V61), then execs the Worker in place: same pid, same terminal.
//! Watcher setup failures never block the Worker; it starts unwatched with one line saying why.

use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use serde_json::{Value, json};

use crate::adapters;
use crate::approval::{self, Answer};
use crate::config::{self, Resolved, Sources};
use crate::event::{Event, Kind};
use crate::focus::{self, FocusSet, PendingApproval};
use crate::harness::{self, Harness, Support, Version};
use crate::home::{Home, write_atomic};
use crate::hook::{self, SESSION_ENV, shell_line};
use crate::jsonl::now_ms;
use crate::launch::{self, Plan};
use crate::mailbox::Mailbox;
use crate::placement::{Placement, pane_command};
use crate::problem::{Problem, Unreadable, ticker_line, tilde};
use crate::repo::{self, Repo};
use crate::supervisor;

const CODEX_LIST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Loaded {
    pub repo: Option<Repo>,
    pub resolved: Resolved,
    pub focuses: FocusSet,
}

/// Config + enabled Focuses for the current dir; `Err` only for an unparseable user file (V83).
pub fn load(home: &Home, cwd: &Path, flags: &[String], only: &[String]) -> Result<Loaded, Unreadable> {
    let repo = repo::discover(cwd);
    let env = |var: &str| std::env::var(var).ok();
    let resolved = config::resolve(&Sources { home, repo: repo.as_ref(), env: &env, flags })?;
    let focuses = focus::resolve(home, repo.as_ref(), &resolved, only)?;
    Ok(Loaded { repo, resolved, focuses })
}

pub fn run(flags: &[String], only: &[String], worker: &[String]) -> ExitCode {
    let worker: Vec<OsString> = worker.iter().map(OsString::from).collect();
    let Some(harness) = Harness::from_program(&worker[0]) else {
        eprintln!("pair-watcher: unsupported Worker `{}` (claude, codex or pi)", worker[0].to_string_lossy());
        return ExitCode::from(2);
    };
    if let Some(reason) = launch::refusal(harness, &worker) {
        eprintln!("pair-watcher: {reason}");
        return ExitCode::from(2);
    }
    match prepare(harness, &worker, flags, only) {
        Ok((plan, session)) => exec(&plan, Some(&session)),
        Err(reason) => {
            eprintln!("pair-watcher: BLIND — {reason}; starting {} unwatched", harness.name());
            exec(&Plan::new(&worker, Vec::new()), None)
        }
    }
}

fn prepare(harness: Harness, worker: &[OsString], flags: &[String], only: &[String]) -> Result<(Plan, String), String> {
    let home = Home::from_env().map_err(|e| e.to_string())?;
    home.ensure_layout().map_err(|e| format!("{}: {e}", tilde(home.root())))?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut loaded = load(&home, &cwd, flags, only).map_err(|u| u.to_string())?;
    if approve_pending(&home, loaded.repo.as_ref(), &loaded.focuses.pending).map_err(|e| e.to_string())? {
        loaded = load(&home, &cwd, flags, only).map_err(|u| u.to_string())?;
    }
    let problems: Vec<Problem> = loaded.resolved.problems.iter().chain(&loaded.focuses.problems).cloned().collect();
    if let Some(line) = ticker_line(&problems) {
        eprintln!("pair-watcher: {line}");
    }

    let session = format!("{:x}-{:x}", now_ms(), std::process::id());
    let mailbox = Mailbox::register(&home, &session).map_err(|e| e.to_string())?;
    let launch = Launch {
        home: &home,
        mailbox: &mailbox,
        session: &session,
        harness,
        version: harness::probe(&worker[0]),
        worker,
        cwd: &cwd,
        loaded: &loaded,
    };
    let wired = wire(&launch);
    if wired.is_err() {
        let _ = mailbox.unregister();
    }
    let (plan, notes) = wired.map_err(|e| e.to_string())?;

    let env = |var: &str| std::env::var(var).ok();
    if Placement::detect(&env) == Placement::Print {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pair-watcher"));
        eprintln!("pair-watcher: pane: {}", shell_line(&pane_command(&exe, &session)));
    }
    for note in notes {
        eprintln!("pair-watcher: {note}");
    }
    Ok((plan, session))
}

struct Launch<'a> {
    home: &'a Home,
    mailbox: &'a Mailbox,
    session: &'a str,
    harness: Harness,
    version: Option<Version>,
    worker: &'a [OsString],
    cwd: &'a Path,
    loaded: &'a Loaded,
}

/// Shim, adapters, per-harness flags, `session` start event, supervisor. Returns the plan and
/// pane notes.
fn wire(l: &Launch) -> io::Result<(Plan, Vec<String>)> {
    let Launch { home, mailbox, harness, worker, cwd, .. } = *l;
    let support = Support::of(harness, l.version);
    let exe = std::env::current_exe()?;
    let shim = hook::write_shim(home, &exe)?;
    let adapters = adapters::write(home)?;
    let mut notes: Vec<String> = support.note(harness).into_iter().collect();

    let ours = if !support.hooks() {
        Vec::new()
    } else {
        match harness {
            Harness::Claude => {
                let file = mailbox.dir().join("claude-settings.json");
                write_atomic(&file, &serde_json::to_vec_pretty(&launch::claude_settings(&shim))?)?;
                launch::claude_args(&file, &adapters.join(adapters::CLAUDE_MOD))
            }
            Harness::Codex => {
                let values = launch::codex_hook_values(&shim);
                match launch::codex_hooks_list(&worker[0], &values, cwd, CODEX_LIST_TIMEOUT) {
                    Ok(listed) => {
                        let trust = launch::codex_trust(&listed);
                        if trust.len() == values.len() {
                            launch::codex_args(&values, &launch::codex_trust_value(&trust))
                        } else {
                            notes.push(format!(
                                "codex trusted {} of {} hooks → observe-only",
                                trust.len(),
                                values.len()
                            ));
                            Vec::new()
                        }
                    }
                    Err(e) => {
                        notes.push(format!("codex hook trust unavailable ({e}) → observe-only"));
                        Vec::new()
                    }
                }
            }
            Harness::Pi => launch::pi_args(&adapters.join(adapters::PI_EXTENSION)),
        }
    };
    let observe_only = ours.is_empty();
    let plan = Plan::new(worker, ours);

    mailbox.log().append(session_start(l, observe_only, &notes))?;
    supervisor::spawn(&exe, mailbox, l.session, std::process::id())?;
    Ok((plan, notes))
}

/// `session` start (V94); Worker model is read later from the transcript, rulings hash is T14.
fn session_start(l: &Launch, observe_only: bool, notes: &[String]) -> Event {
    let loaded = l.loaded;
    let mode = loaded.resolved.get("mode").and_then(toml::Value::as_str).unwrap_or("companion");
    let focuses: Vec<&str> = loaded.focuses.enabled.iter().map(|f| f.name.as_str()).collect();
    let layers: Vec<String> = loaded.resolved.layer_files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let repo = loaded.repo.as_ref().map_or_else(|| l.cwd.to_path_buf(), |r| r.root.clone());
    Event::new(Kind::Session)
        .with("phase", "start")
        .with("repo", repo.to_string_lossy().into_owned())
        .with("harness", l.harness.name())
        .with("harness_version", l.version.map_or(Value::Null, |v| v.to_string().into()))
        .with("mode", mode)
        .with("ceiling_usd", loaded.resolved.get("ceiling_usd").and_then(toml::Value::as_float).unwrap_or(5.0))
        .with("focuses", json!(focuses))
        .with("config_hash", config::config_hash(&loaded.resolved, &loaded.focuses.enabled))
        .with("layer_files", json!(layers))
        .with("version", env!("CARGO_PKG_VERSION"))
        .with("observe_only", observe_only)
        .with("notes", json!(notes))
}

/// V82: asks on a terminal before the Worker starts; otherwise one line per Focus naming
/// `focus approve`. Returns whether any Focus was approved.
fn approve_pending(home: &Home, repo: Option<&Repo>, pending: &[PendingApproval]) -> io::Result<bool> {
    if pending.is_empty() {
        return Ok(false);
    }
    let root_commit = repo.and_then(|r| r.root_commit.as_deref());
    let (Some(rc), true) = (root_commit, io::stdin().is_terminal()) else {
        for p in pending {
            eprintln!("pair-watcher: {}", approval::ticker_line(p));
        }
        return Ok(false);
    };
    let mut approved = false;
    let (mut input, mut out) = (io::stdin().lock(), io::stderr());
    for p in pending {
        match approval::ask(p, &mut input, &mut out)? {
            Answer::Approve => {
                approval::record(home, rc, p, true)?;
                approved = true;
            }
            Answer::Never => approval::record(home, rc, p, false)?,
            Answer::Skip => {}
        }
    }
    Ok(approved)
}

/// `focus approve <name>`: approves the repo Focus file as it is now (V82).
pub fn focus_approve(name: &str) -> ExitCode {
    let result = (|| -> Result<String, String> {
        let home = Home::from_env().map_err(|e| e.to_string())?;
        home.ensure_layout().map_err(|e| e.to_string())?;
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let loaded = load(&home, &cwd, &[], &[]).map_err(|u| u.to_string())?;
        let rc = loaded
            .repo
            .as_ref()
            .and_then(|r| r.root_commit.clone())
            .ok_or("not in a git repo with commits; approvals are keyed by root commit")?;
        if let Some(p) = loaded.focuses.pending.iter().find(|p| p.name == name) {
            approval::record(&home, &rc, p, true).map_err(|e| e.to_string())?;
            return Ok(format!("approved repo Focus `{name}` ({})", tilde(&p.file)));
        }
        if loaded.focuses.enabled.iter().any(|f| f.name == name) {
            return Ok(format!("Focus `{name}` is already enabled"));
        }
        Err(format!("no repo Focus `{name}` awaiting approval"))
    })();
    match result {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("pair-watcher: {e}");
            ExitCode::FAILURE
        }
    }
}

fn exec(plan: &Plan, session: Option<&str>) -> ExitCode {
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args);
    match session {
        Some(s) => cmd.env(SESSION_ENV, s),
        None => cmd.env_remove(SESSION_ENV),
    };
    let err = cmd.exec();
    eprintln!("pair-watcher: cannot start {}: {err}", plan.program.to_string_lossy());
    ExitCode::from(127)
}
