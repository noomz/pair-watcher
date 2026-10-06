//! CLI surface (I1). Commands owned by later §T tasks parse fully and report which task builds them.

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::{self, Sources};
use crate::focus::{self, FocusSource};
use crate::home::Home;
use crate::problem::{Problem, tilde};
use crate::repo;

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
    Pane,
    /// Hook entry point (pure append, fail-silent)
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
    let cli = Cli::parse();
    match cli.command {
        // Hooks never fail the Worker (V58).
        Command::Hook { .. } => ExitCode::SUCCESS,
        Command::Config { action: ConfigAction::Show { origin } } => config_show(origin),
        Command::Run(_) | Command::Attach { .. } | Command::Focus { .. } => later("T4"),
        Command::Pane | Command::Mute { .. } | Command::Nochallenge(_) | Command::Pause(_) => later("T12"),
        Command::Rule { .. } | Command::Undo(_) | Command::Keep(_) => later("T13"),
        Command::Rulings { .. } => later("T14"),
        Command::Install { .. } | Command::Uninstall { .. } | Command::Auth | Command::Doctor => later("T16"),
        Command::Models { .. } => later("T6"),
        Command::Prefilter { .. } => later("T17"),
        Command::Report { .. }
        | Command::Replay { .. }
        | Command::Audit { .. }
        | Command::Telemetry { .. }
        | Command::Prune => later("T18"),
        Command::Mcp => later("T20"),
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
    let repo = std::env::current_dir().ok().and_then(|d| repo::discover(&d));
    let env = |var: &str| std::env::var(var).ok();
    let sources = Sources { home: &home, repo: repo.as_ref(), env: &env, flags: &[] };
    let resolved = match config::resolve(&sources) {
        Ok(r) => r,
        Err(unreadable) => return fail(&unreadable.to_string()),
    };
    let focuses = match focus::resolve(&home, repo.as_ref(), &resolved, &[]) {
        Ok(f) => f,
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

fn fail(msg: &str) -> ExitCode {
    eprintln!("pair-watcher: {msg}");
    ExitCode::FAILURE
}
