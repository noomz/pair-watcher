//! CLI surface (I1). Commands owned by later §T tasks parse fully and report which task builds them.

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::delivery::{self, Route, Worker};
use crate::focus::FocusSource;
use crate::home::Home;
use crate::jsonl::now_ms;
use crate::mailbox::Mailbox;
use crate::outbox::Claim;
use crate::problem::{Problem, tilde};
use crate::{config, hook, models, run, supervisor};

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
    List,
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
    dispatch(Cli::parse().command)
}

fn dispatch(command: Command) -> ExitCode {
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
        Command::Attach { .. } => later("T15"),
        Command::Pane(_) | Command::Mute { .. } | Command::Nochallenge(_) | Command::Pause(_) => later("T12"),
        Command::Rule { .. } | Command::Undo(_) | Command::Keep(_) => later("T13"),
        Command::Rulings { .. } => later("T14"),
        Command::Install { .. } | Command::Uninstall { .. } | Command::Auth | Command::Doctor => later("T16"),
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
const PW_COMMANDS: [&str; 6] = ["rule", "undo", "keep", "mute", "nochallenge", "pause"];

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
        AdapterCmd::Poll { session, .. } | AdapterCmd::Ack { session, .. } | AdapterCmd::Pw { session, .. } => session,
    };
    let Some(mailbox) = Home::from_env().ok().and_then(|home| Mailbox::open_registered(&home, session)) else {
        return ExitCode::SUCCESS;
    };
    let now = now_ms();
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
        AdapterCmd::Ack { id, token, .. } => {
            let pending = mailbox.outbox().pending().map(|items| items.into_iter().find(|i| i.id == id));
            match pending {
                Ok(Some(item)) => mailbox.outbox().ack(&Claim { item, token }, now).map(drop),
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
                Ok(cli) => dispatch(cli.command),
                Err(e) => fail(e.to_string().lines().next().unwrap_or("bad /pw command")),
            };
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e.to_string()),
    }
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
