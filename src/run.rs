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
use crate::{standing, supervisor};

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
    let refused = loaded.resolved.backend_for(harness.name()).1;
    let problems: Vec<Problem> =
        loaded.resolved.problems.iter().chain(&loaded.focuses.problems).chain(&refused).cloned().collect();
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
        flags,
        only,
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
    flags: &'a [String],
    only: &'a [String],
}

/// Shim, adapters, per-harness flags, `session` start event, supervisor. Returns the plan and
/// pane notes.
fn wire(l: &Launch) -> io::Result<(Plan, Vec<String>)> {
    let Launch { home, mailbox, harness, worker, cwd, .. } = *l;
    let launch = crate::watch::Launch { flags: l.flags.to_vec(), only: l.only.to_vec() };
    crate::watch::write_json(&mailbox.dir().join(crate::watch::LAUNCH_FILE), &launch)?;
    let support = Support::of(harness, l.version);
    let exe = std::env::current_exe()?;
    let shim = hook::write_shim(home, &exe)?;
    let adapters = adapters::write(home)?;
    let mut notes: Vec<String> = support.note(harness).into_iter().collect();
    // Installed hooks already route by the session env var; per-session ones would fire twice.
    let installed = crate::install::installed(home, harness).is_some();

    let ours = if !support.hooks() || installed {
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
    let observe_only = ours.is_empty() && !(installed && support.hooks());
    let plan = Plan::new(worker, ours);

    // Never blocks the Worker: a store that cannot be read loads no rulings (V78).
    let rulings = standing::start(&standing::path(l.home, &standing::key(l.cwd))).unwrap_or_else(|e| standing::Start {
        notes: vec![format!("rulings unavailable: {e}")],
        ..standing::Start::default()
    });
    notes.extend(rulings.notes.iter().cloned());
    let start = Start { cwd: l.cwd, harness, version: l.version, observe_only, notes: &notes, rulings: &rulings };
    mailbox.log().append(session_start(l.loaded, &start))?;
    if let Some(edited) = &rulings.edited {
        mailbox.log().append(edited.event())?;
    }
    supervisor::spawn(&exe, mailbox, l.session, std::process::id())?;
    Ok((plan, notes))
}

struct Start<'a> {
    cwd: &'a Path,
    harness: Harness,
    version: Option<Version>,
    observe_only: bool,
    notes: &'a [String],
    rulings: &'a standing::Start,
}

/// `session` start (V94); Worker model is read later from the transcript.
fn session_start(loaded: &Loaded, l: &Start) -> Event {
    let (observe_only, notes, rulings) = (l.observe_only, l.notes, l.rulings);
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
        .with("rulings", rulings.held.len())
        .with("rulings_hash", rulings.hash.clone().map_or(Value::Null, Value::from))
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

/// A Worker session found by its id in its harness's transcript store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub harness: Harness,
    pub transcript: PathBuf,
    pub cwd: PathBuf,
}

/// The transcript of Worker session `id`: Claude `projects/*/<id>.jsonl`, Codex
/// `sessions/**/rollout-*-<id>.jsonl`, pi `sessions/*/*_<id>.jsonl`; harness dirs follow the
/// same env vars as `install`.
pub fn find_transcript(env: &dyn Fn(&str) -> Option<String>, id: &str) -> Option<Found> {
    if !crate::mailbox::valid_session_id(id) {
        return None;
    }
    let home = PathBuf::from(env("HOME").filter(|h| !h.is_empty())?);
    let dir =
        |var: &str, default: &str| env(var).filter(|v| !v.is_empty()).map_or_else(|| home.join(default), PathBuf::from);
    let found = |harness, transcript: PathBuf, pointer: &str| {
        let cwd = first_cwd(&transcript, pointer)?;
        Some(Found { harness, transcript, cwd })
    };
    let claude = dir("CLAUDE_CONFIG_DIR", ".claude").join("projects");
    let name = format!("{id}.jsonl");
    if let Some(f) = files(&claude, 1).into_iter().find(|f| f.file_name().is_some_and(|n| n == name.as_str())) {
        return found(Harness::Claude, f, "/cwd");
    }
    let suffix = format!("-{id}.jsonl");
    let codex = dir("CODEX_HOME", ".codex").join("sessions");
    let is_rollout = |n: &str| n.starts_with("rollout-") && n.ends_with(&suffix);
    if let Some(f) =
        files(&codex, 3).into_iter().find(|f| f.file_name().and_then(|n| n.to_str()).is_some_and(is_rollout))
    {
        return found(Harness::Codex, f, "/payload/cwd");
    }
    let pi = env("PI_CODING_AGENT_SESSION_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(|| dir("PI_CODING_AGENT_DIR", ".pi/agent").join("sessions"), PathBuf::from);
    let suffix = format!("_{id}.jsonl");
    let f = files(&pi, 1)
        .into_iter()
        .find(|f| f.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(&suffix)))?;
    found(Harness::Pi, f, "/cwd")
}

/// Files `depth` dirs below `root`.
fn files(root: &Path, depth: usize) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    let paths = entries.filter_map(|e| e.ok()).map(|e| e.path());
    if depth == 0 {
        return paths.filter(|p| p.is_file()).collect();
    }
    paths.filter(|p| p.is_dir()).flat_map(|p| files(&p, depth - 1)).collect()
}

/// The first `pointer` value among the transcript's first lines (Claude puts `cwd` on entries,
/// Codex on `session_meta`, pi on its header).
fn first_cwd(path: &Path, pointer: &str) -> Option<PathBuf> {
    let file = std::fs::File::open(path).ok()?;
    io::BufRead::lines(io::BufReader::new(file))
        .take(200)
        .map_while(Result::ok)
        .find_map(|line| serde_json::from_str::<Value>(&line).ok()?.pointer(pointer)?.as_str().map(PathBuf::from))
}

/// `attach <id>` for a Worker started without `run` (V58, V61): its harness must be installed so
/// its hooks route by payload session id. Registers the mailbox under the Worker session id, logs
/// the session start, adopts the transcript from its end (earlier content predates the session)
/// and starts the supervisor; the next hook tells it the Worker's pid.
/// `hint`: the Worker's harness and cwd as an in-session `/pw watch` knows them (V113). A Worker
/// with no transcript yet (Claude writes it at the first prompt) is watched from them; its first
/// hook names the transcript, read from its start since nothing predates the watch.
pub fn attach_installed(home: &Home, id: &str, exe: &Path, hint: Option<(Harness, PathBuf)>) -> Result<String, String> {
    let env = |k: &str| std::env::var(k).ok();
    let (harness, cwd, transcript) = match (find_transcript(&env, id), hint) {
        (Some(f), _) => (f.harness, f.cwd, Some(f.transcript)),
        (None, Some((harness, cwd))) => (harness, cwd, None),
        (None, None) => return Err(format!("no live session {id} and no Claude, Codex or pi transcript for it")),
    };
    if crate::install::installed(home, harness).is_none() {
        return Err(format!(
            "session {id} is a {0} session not started by `run`: `pair-watcher install {0}` first, then restart it",
            harness.name()
        ));
    }
    home.ensure_layout().map_err(|e| e.to_string())?;
    let loaded = load(home, &cwd, &[], &[]).map_err(|u| u.to_string())?;
    let mailbox = Mailbox::register(home, id).map_err(|e| e.to_string())?;
    let result = (|| -> io::Result<()> {
        crate::watch::write_json(&mailbox.dir().join(crate::watch::LAUNCH_FILE), &crate::watch::Launch::default())?;
        let version = harness::probe(std::ffi::OsStr::new(harness.name()));
        let mut notes: Vec<String> = Support::of(harness, version).note(harness).into_iter().collect();
        // Late attach asks nothing: an unapproved repo Focus waits for `focus approve` (V82).
        notes.extend(loaded.focuses.pending.iter().map(approval::ticker_line));
        let rulings = standing::start(&standing::path(home, &standing::key(&cwd))).unwrap_or_else(|e| {
            standing::Start { notes: vec![format!("rulings unavailable: {e}")], ..standing::Start::default() }
        });
        notes.extend(rulings.notes.iter().cloned());
        let observe_only = !Support::of(harness, version).hooks();
        let start = Start { cwd: &cwd, harness, version, observe_only, notes: &notes, rulings: &rulings };
        mailbox.log().append(session_start(&loaded, &start))?;
        if let Some(edited) = &rulings.edited {
            mailbox.log().append(edited.event())?;
        }
        if let Some(transcript) = &transcript {
            let payload = json!({"session_id": id, "transcript_path": transcript, "cwd": cwd, "source": "resume"});
            mailbox.append_hook(&crate::mailbox::Hook { event: ATTACH_EVENT.into(), payload })?;
        }
        supervisor::spawn(exe, &mailbox, id, 0)
    })();
    if let Err(e) = result {
        let _ = mailbox.unregister();
        return Err(e.to_string());
    }
    Ok(format!("watching {} session in {}", harness.name(), cwd.display()))
}

/// The hook record `attach` writes: the transcript to tail, from its end. It never wakes.
pub const ATTACH_EVENT: &str = "attach";
