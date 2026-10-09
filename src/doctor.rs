//! `doctor` (V85): one line per check; exit non-zero only when the Watcher would be BLIND or
//! unable to start. Everything else is advice and never blocks.

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::credentials::Keys;
use crate::focus::FocusSource;
use crate::harness::{Harness, Support, Version};
use crate::home::Home;
use crate::launch::{CODEX_EVENTS, ListedHook};
use crate::models::{self, Table};
use crate::prefilter::{self, Tag};
use crate::problem::tilde;
use crate::{install, run};

/// A user `models.toml` older than this is likely missing new models or prices.
pub const STALE_DAYS: u64 = 90;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    /// The Watcher would be BLIND or cannot start.
    Fail,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub level: Level,
    pub text: String,
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tag = match self.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        };
        write!(f, "{tag} {}", self.text)
    }
}

/// What `doctor` looks at beyond files: probes, injectable for tests.
pub struct Probes<'a> {
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// `<harness> --version`; `None` = not on PATH.
    pub version: &'a dyn Fn(Harness) -> Option<Version>,
    /// Codex `hooks/list` over the user config.
    pub codex_list: &'a dyn Fn() -> io::Result<Vec<ListedHook>>,
    /// The models an Ollama server at `url` has pulled.
    pub ollama: &'a dyn Fn(&str) -> Result<Vec<Tag>, String>,
    pub now: SystemTime,
}

pub fn check(home: &Home, cwd: &Path, p: &Probes) -> Vec<Line> {
    let mut out = Vec::new();
    let mut push = |level, text: String| out.push(Line { level, text });
    if let Err(e) = home.ensure_layout() {
        push(Level::Fail, format!("{}: {e}", tilde(home.root())));
        return out;
    }

    // Config, ignored repo values, repo Focus approvals (V79–V83).
    let loaded = match run::load(home, cwd, &[], &[]) {
        Ok(l) => l,
        Err(unreadable) => {
            push(Level::Fail, format!("config unreadable: {unreadable} → the Watcher is BLIND"));
            return out;
        }
    };
    // V106: a `claude-session` backend some Worker harness would refuse.
    let refused: Vec<_> = Harness::ALL.iter().filter_map(|h| loaded.resolved.backend_for(h.name()).1).collect();
    let problems: Vec<_> = loaded.resolved.problems.iter().chain(&loaded.focuses.problems).chain(&refused).collect();
    if problems.is_empty() {
        push(Level::Ok, "config".into());
    }
    for problem in problems {
        push(Level::Warn, problem.to_string());
    }
    for f in &loaded.focuses.pending {
        push(Level::Warn, format!("repo Focus `{}` awaits approval: pair-watcher focus approve {}", f.name, f.name));
    }

    // Keys, price table, models (V47, I18).
    let keys = Keys::load(home, p.env);
    if let Some(problem) = &keys.problem {
        push(Level::Warn, format!("credentials: {problem}"));
    }
    let text = |key: &str| loaded.resolved.get(key).and_then(toml::Value::as_str).map(str::to_owned);
    match Table::load(home) {
        Err(e) => push(Level::Fail, format!("price table: {e}")),
        Ok(table) => {
            let configured = keys.configured();
            let model = text("model").unwrap_or_else(|| "auto".into());
            match models::choose(&table, &model, None, &configured) {
                Ok(c) => push(Level::Ok, format!("model {} ({})", c.model, c.vendor.name())),
                Err(e) => push(Level::Fail, format!("model {model}: {e} → the Watcher cannot start")),
            }
            if let Some(fallback) = text("fallback_model").filter(|m| !m.trim().is_empty())
                && let Err(e) = models::choose(&table, &fallback, None, &configured)
            {
                push(Level::Warn, format!("fallback_model {fallback}: {e}"));
            }
            // `[harness.<h>]` models (V109).
            for h in Harness::ALL {
                for (key, level) in [("model", Level::Fail), ("fallback_model", Level::Warn)] {
                    let Some(m) = text(&format!("harness.{}.{key}", h.name())) else { continue };
                    if let Err(e) = models::choose(&table, &m, None, &configured) {
                        push(level, format!("harness.{}.{key} {m}: {e}", h.name()));
                    }
                }
            }
            let swarm = text("mode").as_deref() == Some("swarm");
            for f in &loaded.focuses.enabled {
                let (Some(m), FocusSource::User(file)) = (&f.model, &f.source) else { continue };
                if !swarm {
                    push(Level::Warn, format!("Focus `{}` model {m} ignored in Companion ({})", f.name, tilde(file)));
                } else if let Err(e) = models::choose(&table, m, None, &configured) {
                    push(Level::Fail, format!("Focus `{}` model {m}: {e} → that Watcher cannot start", f.name));
                }
            }
        }
    }
    if let Some(days) = age_days(&home.models_file(), p.now).filter(|d| *d > STALE_DAYS) {
        push(Level::Warn, format!("price table updated {days} days ago: pair-watcher models update"));
    }

    // Harnesses vs the compat table (V60), installed adapters, Codex trust (V59).
    for h in Harness::ALL {
        let installed = install::installed(home, h);
        let Some(v) = (p.version)(h) else {
            if installed.is_some() {
                push(Level::Warn, format!("{}: installed but not on PATH", h.name()));
            }
            continue;
        };
        let (minimum, tested) = h.compat();
        match Support::of(h, Some(v)) {
            Support::Tested => push(Level::Ok, format!("{} {v} (minimum {minimum}, tested {tested})", h.name())),
            Support::BelowMinimum(_) => {
                push(Level::Warn, format!("{} {v} below minimum {minimum} → observe-only", h.name()));
            }
            Support::Untested(_) => push(Level::Warn, format!("{} {v} untested (last tested {tested})", h.name())),
        }
        let Some(installed) = installed else { continue };
        if installed.version != env!("CARGO_PKG_VERSION") {
            push(
                Level::Warn,
                format!(
                    "{} adapters installed by {} (binary {}): pair-watcher install {}",
                    h.name(),
                    installed.version,
                    env!("CARGO_PKG_VERSION"),
                    h.name()
                ),
            );
        }
        if h == Harness::Codex {
            out_codex_trust(home, p, &mut push);
        }
    }

    // Pre-filter (V53, V55).
    let mode = text("prefilter.mode").unwrap_or_else(|| "auto".into());
    if mode != "off" {
        let url = text("prefilter.url").unwrap_or_else(|| "http://localhost:11434".into());
        let model = text("prefilter.model").unwrap_or_else(|| "nimble".into());
        if !prefilter::local(&url) {
            push(Level::Warn, format!("prefilter.url {url}: {}", prefilter::LEAVES));
        }
        match (p.ollama)(&url) {
            Ok(tags) if prefilter::pulled(&tags, &model).is_some() => {
                push(Level::Ok, format!("pre-filter {model} at {url}"));
            }
            Ok(_) => push(Level::Warn, format!("pre-filter: {model} not pulled at {url}: ollama pull {model}")),
            Err(e) => push(Level::Warn, format!("pre-filter: Ollama not reachable at {url} ({e}); pre-filter off")),
        }
        if mode == "skip" && !prefilter::fitted_for(&prefilter::load_thresholds(home), &url) {
            push(
                Level::Warn,
                "prefilter.mode = skip refused: no fitted skip_below (pair-watcher prefilter fit); runs shadow".into(),
            );
        }
    }

    // Store vs cap (V96).
    let cap = loaded.resolved.get("store.cap_gb").and_then(|v| v.as_float().or(v.as_integer().map(|i| i as f64)));
    let size = dir_size(&home.sessions_dir()) as f64 / 1e9;
    match cap {
        Some(cap) if size > cap => {
            push(Level::Warn, format!("store {size:.2} GB over cap {cap} GB: pair-watcher prune"))
        }
        Some(cap) => push(Level::Ok, format!("store {size:.2} GB of {cap} GB")),
        None => {}
    }
    out
}

fn out_codex_trust(home: &Home, p: &Probes, push: &mut impl FnMut(Level, String)) {
    match (p.codex_list)() {
        Err(e) => push(Level::Warn, format!("codex hook trust unreadable: {e}")),
        Ok(listed) => {
            let ours: Vec<&ListedHook> = listed.iter().filter(|h| install::is_ours(home, &h.command)).collect();
            let untrusted: Vec<&str> =
                ours.iter().filter(|h| h.trust_status != "trusted").map(|h| h.key.as_str()).collect();
            if ours.len() < CODEX_EVENTS.len() || !untrusted.is_empty() {
                let detail = if untrusted.is_empty() {
                    format!("{} of {} hooks present", ours.len(), CODEX_EVENTS.len())
                } else {
                    format!("not trusted: {}", untrusted.join(", "))
                };
                push(Level::Warn, format!("codex hooks {detail}: pair-watcher install codex"));
            } else {
                push(Level::Ok, "codex hooks trusted".into());
            }
        }
    }
}

/// The Watcher would be BLIND or cannot start.
pub fn failed(lines: &[Line]) -> bool {
    lines.iter().any(|l| l.level == Level::Fail)
}

fn age_days(path: &Path, now: SystemTime) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(now.duration_since(modified).unwrap_or(Duration::ZERO).as_secs() / 86_400)
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries
        .filter_map(Result::ok)
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}
