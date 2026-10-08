//! CLI surface (I1). Commands owned by later §T tasks parse fully and report which task builds them.

use std::ffi::OsStr;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::credentials::{self, Keys};
use crate::delivery::{self, Route, Worker};
use crate::focus::FocusSource;
use crate::harness::{self, Harness};
use crate::home::Home;
use crate::human::Act;
use crate::jsonl::now_ms;
use crate::mailbox::Mailbox;
use crate::models::Vendor;
use crate::outbox::Claim;
use crate::problem::{Problem, tilde};
use crate::ruling::{self, Rulings, Surface, Via};
use crate::status::Op as StatusOp;
use crate::{config, doctor, hook, install, launch, mailbox, models, pane, repo, run, standing, supervisor};

#[derive(Parser, Debug)]
#[command(
    name = "pair-watcher",
    version,
    about = "A Watcher that follows one coding agent and speaks only when it matters"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Launch a Worker with a Watcher attached
    Run(RunArgs),
    /// Attach a pane to a running session
    Attach { session: String },
    /// Open the standalone pane
    Pane(SessionArg),
    /// Hook entry point (pure append, fail-silent); `hook adapter …` serves the Claude mod and pi
    /// extension
    Hook {
        event: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<String>,
    },
    /// Serve read tools over MCP for the `claude -p` backend
    Mcp,
    /// Install adapters into a harness's user config
    Install { target: Target },
    /// Remove installed adapters
    Uninstall { target: Target },
    /// Store API credentials
    Auth,
    /// Manage the model price table
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Check setup; non-zero only when the Watcher would be BLIND or unable to start
    Doctor,
    /// Inspect resolved config
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Manage repo Focuses
    Focus {
        #[command(subcommand)]
        action: FocusAction,
    },
    /// Mute a Focus for this session
    Mute {
        focus: String,
        #[command(flatten)]
        session: SessionArg,
    },
    /// Stop auto-Challenges for this session
    Nochallenge(SessionArg),
    /// Pause the Watcher for this session
    Pause(SessionArg),
    /// Back to the primary model after `pair-watcher auth` (V51)
    Primary(SessionArg),
    /// Rule on an Escalation
    Rule {
        id: String,
        side: Side,
        #[command(flatten)]
        session: SessionArg,
    },
    /// Undo the last ruling
    Undo(SessionArg),
    /// Keep the last ruling as a Standing ruling
    Keep(SessionArg),
    /// Manage Standing rulings
    Rulings {
        #[command(subcommand)]
        action: RulingsAction,
    },
    /// Ratio headlines over sessions
    Report {
        #[arg(long)]
        export: bool,
        #[arg(long)]
        explicit_only: bool,
    },
    /// Pre-filter tools
    Prefilter {
        #[command(subcommand)]
        action: PrefilterAction,
    },
    /// Re-run a wake set with a variant
    Replay {
        #[arg(long)]
        variant: Variant,
        wake_set: String,
    },
    /// Audit silent sessions for misses
    Audit {
        #[arg(long)]
        yes: bool,
        #[arg(long, default_value_t = 5.0)]
        max_usd: f64,
        #[arg(long, default_value_t = 20)]
        sample: usize,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        resume: bool,
    },
    /// Aggregate telemetry (off by default)
    Telemetry { action: TelemetryAction },
    /// Apply retention and the store cap
    Prune,
    /// Per-session supervisor, started detached by `run`
    #[command(hide = true)]
    Supervise {
        session: String,
        #[arg(long)]
        worker_pid: i32,
    },
    /// The Watcher process, run by the session's supervisor
    #[command(hide = true)]
    Watch { session: String },
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Config override `key=value` (repeatable)
    #[arg(short = 'c', value_name = "KEY=VALUE")]
    pub set: Vec<String>,
    #[arg(long)]
    pub mode: Option<Mode>,
    #[arg(long, value_name = "USD")]
    pub ceiling: Option<f64>,
    /// Run only the named Focuses this session (repeatable)
    #[arg(long = "focus", value_name = "NAME")]
    pub focus: Vec<String>,
    /// Worker harness and its arguments
    #[arg(last = true, required = true, value_name = "claude|codex|pi> <ARGS")]
    pub worker: Vec<String>,
}

impl RunArgs {
    /// The flag layer: `-c` entries, then shortcuts.
    pub fn flag_layer(&self) -> Vec<String> {
        let mut flags = self.set.clone();
        if let Some(mode) = self.mode {
            flags.push(format!("mode={}", mode.name()));
        }
        if let Some(c) = self.ceiling {
            flags.push(format!("ceiling_usd={c}"));
        }
        flags
    }
}

#[derive(Args, Debug)]
pub struct SessionArg {
    /// Session id; default = the only live session in the current repo
    #[arg(long)]
    pub session: Option<String>,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum Mode {
    Companion,
    Swarm,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Companion => "companion",
            Mode::Swarm => "swarm",
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum Target {
    Claude,
    Codex,
    Pi,
    All,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum Side {
    Watcher,
    Worker,
    Dismiss,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum Variant {
    Prompt,
    Focus,
    Model,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum TelemetryAction {
    On,
    Off,
    Preview,
}

#[derive(Subcommand, Debug)]
pub enum ModelsAction {
    Update {
        #[arg(long)]
        yes: bool,
    },
    Rollback,
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Print the resolved config
    Show {
        /// Show the layer that set each value
        #[arg(long)]
        origin: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum FocusAction {
    Approve {
        name: String,
        #[command(flatten)]
        session: SessionArg,
    },
}

#[derive(Subcommand, Debug)]
pub enum RulingsAction {
    /// List this repo's Standing rulings
    List,
    /// Remove a Standing ruling
    Rm { id: String },
}

#[derive(Subcommand, Debug)]
pub enum PrefilterAction {
    /// Fit `skip_below` into `prefilter/thresholds.json`
    Fit {
        #[arg(long)]
        target: Option<f64>,
    },
}

pub fn main() -> ExitCode {
    dispatch(Cli::parse().command, Via::Cli)
}

/// `via` = how a session command reached us: the terminal (`cli`) or the mod's `/pw` (`command`).
fn dispatch(command: Command, via: Via) -> ExitCode {
    match command {
        Command::Hook { event, rest } if event == ADAPTER => adapter(&rest),
        // Hooks never fail the Worker (V58).
        Command::Hook { event, .. } => {
            hook::run(&event);
            ExitCode::SUCCESS
        }
        Command::Config { action: ConfigAction::Show { origin } } => config_show(origin),
        Command::Run(args) => run::run(&args.flag_layer(), &args.focus, &args.worker),
        // Approval is per file hash, read by every session in the repo; `--session` names none.
        Command::Focus { action: FocusAction::Approve { name, .. } } => run::focus_approve(&name),
        Command::Supervise { session, worker_pid } => supervise(&session, worker_pid),
        Command::Attach { session } => attach(&session),
        Command::Watch { session } => watch(&session),
        Command::Pane(arg) => pane(arg),
        Command::Mute { focus, session } => session_fact(&session, |m| mute(m, &focus)),
        Command::Nochallenge(session) => session_fact(&session, |m| {
            m.log().append(Act::Nochallenge.event()).map(|()| "no auto-Challenge".to_owned())
        }),
        Command::Pause(session) => session_fact(&session, pause),
        Command::Primary(session) => session_fact(&session, |m| {
            m.log().append(Act::Primary.event()).map(|()| "primary model requested".to_owned())
        }),
        Command::Rule { id, side, session } => session_fact(&session, |m| rule(m, &id, side, via)),
        Command::Undo(session) => session_fact(&session, |m| undo(m, via)),
        Command::Keep(session) => session_fact(&session, |m| keep(m, via)),
        Command::Rulings { action } => rulings(action),
        Command::Install { target } => install_cmd(target, true),
        Command::Uninstall { target } => install_cmd(target, false),
        Command::Auth => auth(),
        Command::Doctor => doctor_cmd(),
        Command::Models { action } => models_cmd(action),
        Command::Prefilter { .. } => later("T17"),
        Command::Report { .. }
        | Command::Replay { .. }
        | Command::Audit { .. }
        | Command::Telemetry { .. }
        | Command::Prune => later("T18"),
        Command::Mcp => later("T20"),
    }
}

/// `pw-hook adapter …`: the adapters reach the binary through the stable shim (V58).
const ADAPTER: &str = "adapter";

/// What the Claude mod and the pi extension ask of the mailbox (I8, I9).
#[derive(Parser, Debug)]
#[command(name = "pw-hook adapter", no_binary_name = true)]
enum AdapterCmd {
    /// Status line + Commentary; claims the next item only when the Worker is idle
    Poll {
        #[arg(long)]
        session: String,
        #[arg(long)]
        route: AdapterRoute,
        #[arg(long)]
        worker: AdapterWorker,
    },
    /// The claimed item was handed to the Worker
    Ack {
        #[arg(long)]
        session: String,
        #[arg(long)]
        id: String,
        #[arg(long)]
        token: String,
    },
    /// The claimed item was not sent (the Worker turned busy): claimable again at once
    Release {
        #[arg(long)]
        session: String,
        #[arg(long)]
        id: String,
        #[arg(long)]
        token: String,
    },
    /// `/pw <command> …`: a session command for this session
    Pw {
        #[arg(long)]
        session: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum AdapterRoute {
    Mod,
    Pi,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum AdapterWorker {
    Idle,
    Busy,
    Draft,
}

/// Session commands `/pw` may run (I8).
const PW_COMMANDS: [&str; 7] = ["rule", "undo", "keep", "mute", "nochallenge", "pause", "primary"];

/// Fail-silent like a hook: a no-op under the Watcher guard or for an unregistered session.
fn adapter(args: &[String]) -> ExitCode {
    if std::env::var_os(hook::GUARD_ENV).is_some() {
        return ExitCode::SUCCESS;
    }
    let cmd = match AdapterCmd::try_parse_from(args) {
        Ok(cmd) => cmd,
        Err(e) => return fail(&e.to_string()),
    };
    let session = match &cmd {
        AdapterCmd::Poll { session, .. }
        | AdapterCmd::Ack { session, .. }
        | AdapterCmd::Release { session, .. }
        | AdapterCmd::Pw { session, .. } => session,
    };
    let Some(mailbox) = Home::from_env().ok().and_then(|home| Mailbox::open_registered(&home, session)) else {
        return ExitCode::SUCCESS;
    };
    let now = now_ms();
    let ack = matches!(cmd, AdapterCmd::Ack { .. });
    let result = match cmd {
        AdapterCmd::Poll { route, worker, .. } => {
            let route = match route {
                AdapterRoute::Mod => Route::Mod,
                AdapterRoute::Pi => Route::PiFollowUp,
            };
            let worker = match worker {
                AdapterWorker::Idle => Worker::Idle,
                AdapterWorker::Busy => Worker::Busy,
                AdapterWorker::Draft => Worker::Draft,
            };
            delivery::poll(&mailbox, route, worker, now).and_then(|poll| {
                println!("{}", serde_json::to_string(&poll)?);
                Ok(())
            })
        }
        AdapterCmd::Ack { id, token, .. } | AdapterCmd::Release { id, token, .. } => {
            let outbox = mailbox.outbox();
            match outbox.pending().map(|items| items.into_iter().find(|i| i.id == id)) {
                Ok(Some(item)) if ack => outbox.ack(&Claim { item, token }, now).map(drop),
                Ok(Some(item)) => outbox.release(&Claim { item, token }, now).map(drop),
                Ok(None) => Ok(()),
                Err(e) => Err(e),
            }
        }
        AdapterCmd::Pw { session, args } => {
            if !args.first().is_some_and(|c| PW_COMMANDS.contains(&c.as_str())) {
                return fail(&format!("/pw takes {}", PW_COMMANDS.join("|")));
            }
            let argv = std::iter::once("pair-watcher".to_owned()).chain(args).chain(["--session".to_owned(), session]);
            return match Cli::try_parse_from(argv) {
                Ok(cli) => dispatch(cli.command, Via::Command),
                Err(e) => fail(e.to_string().lines().next().unwrap_or("bad /pw command")),
            };
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e.to_string()),
    }
}

/// `--session`, else the only live session in the current repo (I1).
fn session(arg: &SessionArg, ended_ok: bool) -> Result<Mailbox, String> {
    let home = Home::from_env().map_err(|e| e.to_string())?;
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = repo::discover(&cwd).map_or(cwd, |r| r.root);
    mailbox::resolve(&home, arg.session.as_deref(), &root, ended_ok)
}

fn pane(arg: SessionArg) -> ExitCode {
    match session(&arg, true) {
        Ok(m) => match pane::run(&m) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(&e.to_string()),
        },
        Err(e) => fail(&e),
    }
}

/// A human fact for a live session (V43); prints what it did.
fn session_fact(arg: &SessionArg, write: impl FnOnce(&Mailbox) -> std::io::Result<String>) -> ExitCode {
    match session(arg, false) {
        Ok(m) => match write(&m) {
            Ok(done) => {
                println!("pair-watcher: {done} (session {})", m.id());
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e.to_string()),
        },
        Err(e) => fail(&e),
    }
}

/// `mute <focus>` (V70): an enabled Focus of the session only.
fn mute(m: &Mailbox, focus: &str) -> std::io::Result<String> {
    let board = pane::Board::rebuild(0, &m.log().read_from(0)?.records);
    if !board.focuses.iter().any(|f| f == focus) {
        return Err(std::io::Error::other(format!("no Focus {focus} in this session ({})", board.focuses.join(", "))));
    }
    m.log().append(Act::Mute { focus: focus.to_owned() }.event())?;
    Ok(format!("muted {focus}"))
}

fn surface(via: Via) -> Surface {
    if via == Via::Command { Surface::Mod } else { Surface::Cli }
}

fn board(m: &Mailbox) -> std::io::Result<pane::Board> {
    Ok(pane::Board::rebuild(crate::term::local_offset(), &m.log().read_from(0)?.records))
}

/// `rule <id> watcher|worker|dismiss` (V71–V73): the `ruled` fact.
fn rule(m: &Mailbox, id: &str, side: Side, via: Via) -> std::io::Result<String> {
    let b = board(m)?;
    let side = match side {
        Side::Watcher => ruling::Side::Watcher,
        Side::Worker => ruling::Side::Worker,
        Side::Dismiss => ruling::Side::Dismissed,
    };
    let ruled = Rulings::rule(&b.cx(), id, side, via, surface(via), now_ms()).map_err(std::io::Error::other)?;
    m.log().append(ruled.event())?;
    let sent = if ruled.ruling_sent { "; the Worker is told at its next idle" } else { "" };
    Ok(format!("{} {}{sent}", ruled.id, side.words()))
}

/// `undo` (V75): the last ruling of any origin.
fn undo(m: &Mailbox, via: Via) -> std::io::Result<String> {
    let event = pane::undo(&board(m)?, &m.outbox(), via, surface(via)).map_err(std::io::Error::other)?;
    let ruled = event.str("ruled").unwrap_or_default().to_owned();
    m.log().append(event)?;
    Ok(format!("ruling on {ruled} undone"))
}

/// `keep` (V77): the last ruling or dismissal as a Standing ruling for this repo.
fn keep(m: &Mailbox, via: Via) -> std::io::Result<String> {
    let b = board(m)?;
    // Nothing to keep says so before a missing store does.
    b.rulings.keep(&b.concerns, None).map_err(std::io::Error::other)?;
    let store = standing::session_path(m).ok_or_else(|| std::io::Error::other("the session's repo is unknown"))?;
    let event = pane::keep(&b, &store, None, now_ms(), via).map_err(std::io::Error::other)?;
    let (id, ruling) =
        (event.str("id").unwrap_or_default().to_owned(), event.str("ruling").unwrap_or_default().to_owned());
    m.log().append(event)?;
    Ok(format!("{id} kept for this repo as Standing ruling {ruling}"))
}

/// `rulings list|rm` (V78): the Standing rulings of the repo at the current directory. A removal
/// is logged in each live session on the repo; with none, the next session start sees it as an
/// edit of the file.
fn rulings(action: RulingsAction) -> ExitCode {
    let result = (|| -> Result<String, String> {
        let home = Home::from_env().map_err(|e| e.to_string())?;
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let key = standing::key(&cwd);
        let store = standing::path(&home, &key);
        match action {
            RulingsAction::List => {
                let held = standing::load(&store).map_err(|e| e.to_string())?;
                if held.is_empty() {
                    return Ok(format!("no Standing rulings for this repo ({})", tilde(&store)));
                }
                let lines: Vec<String> = held
                    .iter()
                    .map(|r| {
                        let reason = r.reason.as_ref().map(|x| format!(" — {x}")).unwrap_or_default();
                        format!("{}  {}  {}  {}  {}{reason}", r.id, r.date, r.focus, r.words(), r.headline)
                    })
                    .collect();
                Ok(format!("{}\n# {} of {} held · {}", lines.join("\n"), held.len(), standing::CAP, tilde(&store)))
            }
            RulingsAction::Rm { id } => {
                let sessions: Vec<Mailbox> = mailbox::live(&home)
                    .into_iter()
                    .filter(|m| m.repo().is_some_and(|r| standing::key(&r) == key))
                    .collect();
                let gone = standing::remove(&store, &id, !sessions.is_empty())
                    .map_err(|e| e.to_string())?
                    .map_err(|why| why.to_string())?;
                for m in &sessions {
                    m.log()
                        .append(Act::RulingRemoved { ruling: gone.id.clone(), via: Via::Cli }.event())
                        .map_err(|e| e.to_string())?;
                }
                Ok(format!("removed Standing ruling {}: {}", gone.id, gone.headline))
            }
        }
    })();
    match result {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

/// `pause` pauses a running Watcher and resumes a paused one (V70).
fn pause(m: &Mailbox) -> std::io::Result<String> {
    let board = pane::Board::rebuild(0, &m.log().read_from(0)?.records);
    let (op, done) =
        if board.user_paused { (StatusOp::UserResume, "resumed") } else { (StatusOp::UserPause, "paused (you)") };
    m.log().append(op.event())?;
    Ok(done.to_owned())
}

impl Target {
    fn harnesses(self) -> Vec<Harness> {
        match self {
            Target::Claude => vec![Harness::Claude],
            Target::Codex => vec![Harness::Codex],
            Target::Pi => vec![Harness::Pi],
            Target::All => Harness::ALL.to_vec(),
        }
    }
}

/// `install|uninstall <harness|all>` (V59); `install all` skips harnesses not on PATH.
fn install_cmd(target: Target, add: bool) -> ExitCode {
    let home = match Home::from_env().and_then(|h| h.ensure_layout().map(|()| h)) {
        Ok(h) => h,
        Err(e) => return fail(&e.to_string()),
    };
    let env = |k: &str| std::env::var(k).ok();
    let (paths, exe) = match install::Paths::from_env(&env).and_then(|p| Ok((p, std::env::current_exe()?))) {
        Ok(x) => x,
        Err(e) => return fail(&e.to_string()),
    };
    let cwd = home.root().parent().map_or_else(|| home.root().to_path_buf(), Path::to_path_buf);
    let codex_list = || launch::codex_hooks_list(OsStr::new("codex"), &[], &cwd, Duration::from_secs(10));
    let cx = install::Ctx { home: &home, paths: &paths, exe: &exe, codex_list: &codex_list };
    let mut failed = false;
    for h in target.harnesses() {
        if add && matches!(target, Target::All) && harness::probe(OsStr::new(h.name())).is_none() {
            println!("{}: not found on PATH, skipped", h.name());
            continue;
        }
        let result = if add { install::install(&cx, h) } else { install::uninstall(&home, &paths, h) };
        match result {
            Ok(lines) if lines.is_empty() => println!("{}: nothing of ours to remove", h.name()),
            Ok(lines) => lines.iter().for_each(|l| println!("{l}")),
            Err(e) => {
                eprintln!("pair-watcher: {} {}: {e}", if add { "install" } else { "uninstall" }, h.name());
                failed = true;
            }
        }
    }
    if add && !failed {
        println!("hooks stay silent until a session is watched: `pair-watcher attach <session id>`");
    }
    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// `auth` (I18): one key per vendor, read without echo on a terminal (a line each otherwise);
/// empty keeps the current one.
fn auth() -> ExitCode {
    use std::io::{BufRead, IsTerminal, Write};
    let home = match Home::from_env().and_then(|h| h.ensure_layout().map(|()| h)) {
        Ok(h) => h,
        Err(e) => return fail(&e.to_string()),
    };
    let env = |k: &str| std::env::var(k).ok();
    let current = Keys::load(&home, &env);
    let tty = std::io::stdin().is_terminal();
    let mut input = std::io::stdin().lock();
    let mut updates = Vec::new();
    for vendor in [Vendor::Anthropic, Vendor::Openai] {
        let name = credentials::env_name(vendor);
        let have = if current.get(vendor).is_some() { "set; enter keeps it" } else { "enter skips" };
        eprint!("{name} ({have}): ");
        let _ = std::io::stderr().flush();
        let quiet = if tty { crate::term::NoEcho::enter().ok() } else { None };
        let mut line = String::new();
        let read = input.read_line(&mut line);
        drop(quiet);
        if tty {
            eprintln!();
        }
        if let Err(e) = read {
            return fail(&e.to_string());
        }
        if !line.trim().is_empty() {
            updates.push((vendor, line.trim().to_owned()));
        }
    }
    if updates.is_empty() {
        println!("no keys changed");
        return ExitCode::SUCCESS;
    }
    if let Err(e) = credentials::store(&home, &updates) {
        return fail(&e.to_string());
    }
    println!("saved to {} (0600)", tilde(&home.credentials_file()));
    if !mailbox::live(&home).is_empty() {
        println!("a running session takes the new key with `pair-watcher primary` (pane F, /pw primary)");
    }
    ExitCode::SUCCESS
}

/// `doctor` (V85): non-zero only when the Watcher would be BLIND or cannot start.
fn doctor_cmd() -> ExitCode {
    let (home, cwd) = match Home::from_env().and_then(|h| Ok((h, std::env::current_dir()?))) {
        Ok(x) => x,
        Err(e) => return fail(&e.to_string()),
    };
    let env = |k: &str| std::env::var(k).ok();
    let user = home.root().parent().map_or_else(|| home.root().to_path_buf(), Path::to_path_buf);
    let probes = doctor::Probes {
        env: &env,
        version: &|h: Harness| harness::probe(OsStr::new(h.name())),
        codex_list: &|| launch::codex_hooks_list(OsStr::new("codex"), &[], &user, Duration::from_secs(10)),
        ollama: &doctor::ollama_models,
        now: std::time::SystemTime::now(),
    };
    let lines = doctor::check(&home, &cwd, &probes);
    lines.iter().for_each(|l| println!("{l}"));
    if doctor::failed(&lines) {
        println!("the Watcher would be BLIND or cannot start");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn later(task: &str) -> ExitCode {
    eprintln!("pair-watcher: not implemented yet ({task})");
    ExitCode::from(2)
}

fn config_show(origin: bool) -> ExitCode {
    let home = match Home::from_env() {
        Ok(h) => h,
        Err(e) => return fail(&e.to_string()),
    };
    if let Err(e) = home.ensure_layout() {
        return fail(&format!("{}: {e}", tilde(home.root())));
    }
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => return fail(&e.to_string()),
    };
    let run::Loaded { resolved, focuses, .. } = match run::load(&home, &cwd, &[], &[]) {
        Ok(l) => l,
        Err(unreadable) => return fail(&unreadable.to_string()),
    };

    for (key, entry) in resolved.entries() {
        let line = match &entry.value {
            Some(v) => format!("{key} = {v}"),
            None => format!("# {key} (unset)"),
        };
        if origin {
            println!("{line}  # {}", entry.origin);
        } else {
            println!("{line}");
        }
    }
    let names: Vec<String> = focuses.enabled.iter().map(|f| format!("{:?}", f.name)).collect();
    println!("# focus: [{}]", names.join(", "));
    if origin {
        for f in &focuses.enabled {
            let src = match &f.source {
                FocusSource::BuiltIn => "built-in".to_owned(),
                FocusSource::User(p) | FocusSource::Repo(p) => tilde(p),
            };
            println!("#   {} — {src}", f.name);
        }
    }
    for p in &focuses.pending {
        println!("# pending approval: {} ({})", p.name, tilde(&p.file));
    }
    println!("# config hash: {}", config::config_hash(&resolved, &focuses.enabled));

    let problems: Vec<&Problem> = resolved.problems.iter().chain(&focuses.problems).collect();
    for p in problems {
        eprintln!("{p}");
    }
    ExitCode::SUCCESS
}

fn models_cmd(action: ModelsAction) -> ExitCode {
    let home = match Home::from_env().and_then(|h| h.ensure_layout().map(|()| h)) {
        Ok(h) => h,
        Err(e) => return fail(&e.to_string()),
    };
    match action {
        ModelsAction::Update { yes } => {
            let fetched = match models::fetch() {
                Ok(t) => t,
                Err(e) => return fail(&e),
            };
            let diff = match models::prepare_update(&home, &fetched) {
                Ok((_, diff)) => diff,
                Err(e) => return fail(&e),
            };
            if diff.is_empty() {
                println!("model table up to date");
                return ExitCode::SUCCESS;
            }
            for line in &diff {
                println!("{line}");
            }
            if !yes && !confirm("apply? [y/N] ") {
                println!("not applied");
                return ExitCode::SUCCESS;
            }
            if let Err(e) = models::apply_update(&home, &fetched) {
                return fail(&format!("{}: {e}", tilde(&home.models_file())));
            }
            println!("applied; `pair-watcher models rollback` restores the previous table");
            ExitCode::SUCCESS
        }
        ModelsAction::Rollback => match models::rollback(&home) {
            Ok(true) => {
                println!("previous model table restored");
                ExitCode::SUCCESS
            }
            Ok(false) => fail("nothing to roll back"),
            Err(e) => fail(&format!("{}: {e}", tilde(&home.models_file()))),
        },
    }
}

/// `y` on an interactive terminal; anything else, or no terminal, is no.
fn confirm(question: &str) -> bool {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("{question}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && line.trim().eq_ignore_ascii_case("y")
}

fn attach(session: &str) -> ExitCode {
    let result = Home::from_env().map_err(|e| e.to_string()).and_then(|home| {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        supervisor::attach(&home, session, &exe)
    });
    match result {
        Ok(msg) => {
            println!("{session}: {msg}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn watch(session: &str) -> ExitCode {
    match Home::from_env().and_then(|home| crate::watch::watch(&home, session)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&format!("watch {session}: {e}")),
    }
}

fn supervise(session: &str, worker_pid: i32) -> ExitCode {
    let result = Home::from_env().and_then(|home| supervisor::supervise(&home, session, worker_pid));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&format!("supervise {session}: {e}")),
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("pair-watcher: {msg}");
    ExitCode::FAILURE
}
